//! Drive Newznab search, the grab handler and post-processing against local
//! RPC fixtures. The SQLite database and both media mounts are temporary.
use axum::{Json, extract::State};
use ryokan::{
    AppState,
    handlers::search::grab_release,
    models::{download_clients, grabbed_torrents, migrations, series},
    services::{download_client, indexers, post_processing},
    test_support,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{basic_auth, method, path, query_param},
};

const TITLE: &str = "[Fixture] Space Brothers - 07 [1080p]";
const OTHER: &str = "nzbget-2-111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111";

#[derive(Default)]
struct Daemon {
    queue: Vec<Value>,
    history: Vec<Value>,
    edits: Vec<Value>,
    adds: usize,
    offline: bool,
}

impl Daemon {
    fn finish(&mut self, key: &str, kind: &str, status: &str) {
        let index = self
            .queue
            .iter()
            .position(|job| job["DupeKey"] == key)
            .unwrap();
        let mut job = self.queue.remove(index);
        job["Kind"] = json!(kind);
        job["Status"] = json!(status);
        job["NZBName"] = json!(TITLE);
        job["DestDir"] = json!(format!("/remote/ryokan/{TITLE}"));
        self.history.push(job);
    }

    fn rpc(&mut self, request: &Request) -> ResponseTemplate {
        if self.offline {
            return ResponseTemplate::new(503);
        }
        let body: Value = request.body_json().unwrap();
        let p = &body["params"];
        let result = match body["method"].as_str().unwrap() {
            "version" => json!("26.0"),
            "listgroups" => {
                assert_eq!(p, &json!([0]));
                json!(self.queue)
            }
            "history" => {
                assert_eq!(p, &json!([false]));
                json!(self.history)
            }
            "append" => {
                let key = p[6].as_str().unwrap();
                assert!(key.starts_with("nzbget-2-"));
                assert_eq!(
                    p,
                    &json!([
                        "",
                        p[1],
                        "ryokan",
                        0,
                        false,
                        false,
                        key,
                        0,
                        "SCORE",
                        false,
                        []
                    ])
                );
                self.adds += 1;
                let id = self.adds + 9;
                self.queue.push(
                    json!({"NZBID":id,"Kind":"URL","Category":"ryokan","DupeKey":key,
                    "URL":p[1],"Status":"FETCHING"}),
                );
                json!(id)
            }
            "editqueue" => {
                self.edits.push(p.clone());
                json!(true)
            }
            other => panic!("unexpected RPC {other}"),
        };
        ResponseTemplate::new(200).set_body_json(json!({"version":"1.1","result":result,"id":1}))
    }
}

async fn boot(db_url: &str) -> AppState {
    let db = SqlitePool::connect(db_url).await.unwrap();
    migrations::migrate(&db).await.unwrap();
    let mut app = test_support::build_test_app_state(db.clone(), None);
    download_client::rebuild_clients_cache(&app.download_clients, &db).await;
    app.indexers = indexers::rebuild_cache(&db).await;
    app
}

async fn configure(db: &SqlitePool, server: &str, mount: &Path, media: &Path, remove: bool) {
    sqlx::query("INSERT INTO config(id,media_root,post_processing_enabled,post_processing_mode,auto_redownload_failed,default_allow_upgrades) VALUES(1,?,1,'copy',0,0)")
        .bind(media.to_str().unwrap()).execute(db).await.unwrap();
    let series_id = test_support::seed_series(db, 42, "Space Brothers").await;
    series::update_allow_upgrades(db, series_id, false)
        .await
        .unwrap();
    for (name, kind, default) in [("SAB", "sabnzbd", true), ("NZBGet", "nzbget", false)] {
        download_clients::insert(
            db,
            download_clients::DownloadClientForm {
                name,
                kind,
                url: server,
                username: "test",
                password: "fixture",
                label: "ryokan",
                download_path: mount.to_str().unwrap(),
                enabled: true,
                is_default: default,
            },
        )
        .await
        .unwrap();
    }
    download_clients::set_remove_completed(db, 2, remove)
        .await
        .unwrap();
    download_clients::set_remove_failed(db, 2, remove)
        .await
        .unwrap();
    for pin in [Some(2_i64), None] {
        sqlx::query("INSERT INTO indexers(name,kind,url,min_seeders,download_client_id) VALUES('Fixture','newznab',?,0,?)")
            .bind(format!("{server}/indexer")).bind(pin).execute(db).await.unwrap();
    }
}

// Cargo runs these fixtures in one process. Serialize only the calls that
// share the production try_lock, so each fixture gets its requested tick.
async fn tick(app: &AppState) {
    static TICKS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = TICKS.lock().await;
    post_processing::run_once(app).await;
}

struct Fixture {
    _server: MockServer,
    tmp: tempfile::TempDir,
    daemon: Arc<Mutex<Daemon>>,
    foreign: Vec<Value>,
    db_url: String,
    app: AppState,
    release_url: String,
    hash: String,
}

async fn fixture(remove: bool) -> Fixture {
    let server = MockServer::start().await;
    let release_url = format!("{}/release.nzb", server.uri());
    let rss = format!(
        "<rss><channel><item><title>{TITLE}</title><guid>release-7</guid><link>{release_url}</link><enclosure url=\"{release_url}\" length=\"100\" type=\"application/x-nzb\"/></item></channel></rss>"
    );
    Mock::given(method("GET"))
        .and(query_param("t", "tvsearch"))
        .respond_with(ResponseTemplate::new(200).set_body_string(rss))
        .expect(1)
        .mount(&server)
        .await;
    let daemon = Arc::new(Mutex::new(Daemon::default()));
    let foreign = vec![
        json!({"NZBID":81,"Kind":"NZB","Status":"DOWNLOADING","DupeKey":"sonarr-key","Category":"ryokan"}),
        json!({"NZBID":82,"Kind":"NZB","Status":"DOWNLOADING","DupeKey":OTHER,"Category":"radarr"}),
    ];
    daemon.lock().unwrap().queue = foreign.clone();
    let rpc = daemon.clone();
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(basic_auth("test", "fixture"))
        .respond_with(move |r: &Request| rpc.lock().unwrap().rpc(r))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mount = tmp.path().join("completed");
    let media = tmp.path().join("library");
    let sibling = mount.join("Sonarr");
    fs::create_dir_all(&sibling).unwrap();
    fs::create_dir(&media).unwrap();
    fs::write(sibling.join("Other - 07.mkv"), b"foreign").unwrap();
    let db_url = format!(
        "sqlite://{}?mode=rwc",
        tmp.path().join("ryokan.db").display()
    );
    let db = SqlitePool::connect(&db_url).await.unwrap();
    migrations::migrate(&db).await.unwrap();
    configure(&db, &server.uri(), &mount, &media, remove).await;
    db.close().await;
    let app = boot(&db_url).await;
    let client = app.client_for_indexer(Some(1)).await.unwrap();
    assert_eq!(client.test().await.unwrap(), "NZBGet 26.0");
    assert_eq!(
        app.client_for_indexer(Some(2))
            .await
            .unwrap()
            .sonarr_impl_name(),
        "Sabnzbd"
    );
    assert_eq!(
        app.resolve_grab_client(None, "SABnzbd_nzo_legacy")
            .await
            .unwrap()
            .sonarr_impl_name(),
        "Sabnzbd"
    );
    let indexer = app
        .indexers
        .read()
        .await
        .iter()
        .find(|i| i.id() == 1)
        .unwrap()
        .clone();
    let hits = indexer
        .search(&indexers::SearchQuery {
            q: "Space Brothers".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let hit = &hits[0];
    let form = serde_json::from_value(json!({
        "url": hit.link,
        "source_guid": hit.guid,
        "indexer_id": hit.indexer_id,
        "title": hit.title
    }))
    .unwrap();
    let Json(response) = grab_release(State(app.clone()), Json(form)).await.unwrap();
    let hash = response["hash"].as_str().unwrap().to_owned();
    let pending = grabbed_torrents::get_all_pending(&app.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash, hash);
    assert_eq!(pending[0].episode_numbers, [7]);
    assert_eq!(pending[0].download_client_id, Some(2));
    Fixture {
        _server: server,
        tmp,
        daemon,
        foreign,
        db_url,
        app,
        release_url,
        hash,
    }
}

async fn exercise(failed: bool, remove: bool) {
    let Fixture {
        _server,
        tmp,
        daemon,
        foreign,
        db_url,
        app,
        release_url,
        hash,
    } = fixture(remove).await;
    let mount = tmp.path().join("completed");
    let media = tmp.path().join("library");
    let sibling = mount.join("Sonarr");
    let client = app.client_for_indexer(Some(1)).await.unwrap();
    let listed = client.list_scoped().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, "FETCHING");
    assert!(client.delete(&hash, false).await.is_err());
    client.pause(&hash).await.unwrap();
    client.resume(&hash).await.unwrap();
    assert!(client.delete(OTHER, true).await.is_err());
    assert!(client.pause("sonarr-key").await.is_err());
    assert!(client.resume(OTHER).await.is_err());
    // Restart recovery must also work from a listing without the release URL.
    daemon.lock().unwrap().queue.last_mut().unwrap()["URL"] = json!("");
    daemon.lock().unwrap().offline = true;
    tick(&app).await;
    assert_eq!(
        grabbed_torrents::get_all_pending(&app.db)
            .await
            .unwrap()
            .len(),
        1
    );
    app.db.close().await;
    drop(app);
    drop(client);

    daemon.lock().unwrap().offline = false;
    let app = boot(&db_url).await;
    let client = app.client_for_indexer(Some(1)).await.unwrap();
    let (outcome, recovered) = client
        .add_torrent_returning_id(&release_url, "")
        .await
        .unwrap();
    assert_eq!(outcome, download_client::AddOutcome::AlreadyPresent);
    assert_eq!(recovered, hash);
    let job = mount.join(TITLE);
    fs::create_dir(&job).unwrap();
    fs::write(job.join("Space Brothers - 07.mkv"), b"episode seven").unwrap();
    {
        let mut d = daemon.lock().unwrap();
        d.finish(
            &hash,
            "NZB",
            if failed {
                "FAILURE/PAR"
            } else {
                "SUCCESS/UNPACK"
            },
        );
        d.history.last_mut().unwrap()["NZBID"] = json!(11);
    }
    tick(&app).await;
    let (state, reason): (String, Option<String>) =
        sqlx::query_as("SELECT state,failure_reason FROM grabbed_torrents WHERE hash=?")
            .bind(&hash)
            .fetch_one(&app.db)
            .await
            .unwrap();
    let imported: Vec<_> = walkdir::WalkDir::new(&media)
        .into_iter()
        .flatten()
        .filter(|f| f.path().extension().is_some_and(|e| e == "mkv"))
        .collect();
    if failed {
        assert_eq!(state, "failed");
        assert_eq!(reason.as_deref(), Some("client_error"));
        assert!(imported.is_empty());
    } else {
        assert_eq!(state, "imported");
        assert_eq!(imported.len(), 1);
        assert!(imported[0].path().starts_with(media.join("Space Brothers")));
        assert!(imported[0].file_name().to_str().unwrap().contains("S01E07"));
        assert_eq!(fs::read(imported[0].path()).unwrap(), b"episode seven");
        assert_eq!(job.join("Space Brothers - 07.mkv").exists(), !remove);
    }
    assert_eq!(
        fs::read(sibling.join("Other - 07.mkv")).unwrap(),
        b"foreign"
    );
    let entry = series::get_by_anilist_id(&app.db, 42)
        .await
        .unwrap()
        .unwrap();
    assert!(!entry.allow_upgrades);
    let d = daemon.lock().unwrap();
    assert_eq!(d.adds, 1);
    assert_eq!(d.queue, foreign);
    let mut expected = vec![
        json!(["GroupPause", "", [10]]),
        json!(["GroupResume", "", [10]]),
    ];
    if remove {
        expected.push(json!(["HistoryFinalDelete", "", [11]]));
    }
    assert_eq!(d.edits, expected);
}

#[tokio::test]
async fn newznab_import_retries_after_restart_and_leaves_foreign_jobs_alone() {
    exercise(false, true).await;
}
#[tokio::test]
async fn retained_completed_job_keeps_its_source() {
    exercise(false, false).await;
}
#[tokio::test]
async fn failure_honors_remove_failed_on() {
    exercise(true, true).await;
}
#[tokio::test]
async fn failure_honors_remove_failed_off() {
    exercise(true, false).await;
}

#[tokio::test]
async fn manual_deletion_clears_pending_episode_without_a_replacement() {
    check_manual_deletion(true).await;
    check_manual_deletion(false).await;
}

async fn check_manual_deletion(import_enabled: bool) {
    let f = fixture(true).await;
    sqlx::query("UPDATE config SET auto_redownload_failed=1, post_processing_enabled=?")
        .bind(import_enabled)
        .execute(&f.app.db)
        .await
        .unwrap();
    // Another completed release for this episode keeps its history.
    sqlx::query("INSERT INTO episode_grab_history(series_id,episode_number,release_title,state)
                 SELECT series_id,episode_number,'Earlier release','completed' FROM episode_grab_history WHERE release_title=?")
        .bind(TITLE).execute(&f.app.db).await.unwrap();
    f.daemon
        .lock()
        .unwrap()
        .finish(&f.hash, "NZB", "DELETED/MANUAL");
    tick(&f.app).await;
    tick(&f.app).await;
    let grab: (String, Option<String>) =
        sqlx::query_as("SELECT state,failure_reason FROM grabbed_torrents WHERE hash=?")
            .bind(&f.hash)
            .fetch_one(&f.app.db)
            .await
            .unwrap();
    assert_eq!(grab.0, "removed");
    assert!(grab.1.unwrap_or_default().is_empty());
    let tags: i64 =
        sqlx::query_scalar("SELECT count(*) FROM episode_quality_tags WHERE episode_number=7")
            .fetch_one(&f.app.db)
            .await
            .unwrap();
    assert_eq!(tags, 0);
    let history: Vec<(String, String)> = sqlx::query_as(
        "SELECT release_title,state FROM episode_grab_history WHERE episode_number=7 ORDER BY id",
    )
    .fetch_all(&f.app.db)
    .await
    .unwrap();
    assert_eq!(
        history,
        [
            (TITLE.into(), "removed".into()),
            ("Earlier release".into(), "completed".into())
        ]
    );
    let d = f.daemon.lock().unwrap();
    assert_eq!(d.adds, 1);
    assert_eq!(d.queue, f.foreign);
    assert!(d.edits.is_empty());
}

#[tokio::test]
async fn skipped_url_fetch_allows_one_new_submission() {
    let f = fixture(false).await;
    f.daemon
        .lock()
        .unwrap()
        .finish(&f.hash, "URL", "WARNING/SKIPPED");
    tick(&f.app).await;
    let grab: (String, String) =
        sqlx::query_as("SELECT state,failure_reason FROM grabbed_torrents WHERE hash=?")
            .bind(&f.hash)
            .fetch_one(&f.app.db)
            .await
            .unwrap();
    assert_eq!(grab, ("failed".into(), "client_error".into()));

    let form = serde_json::from_value(json!({
        "url": f.release_url, "title": TITLE, "source_guid": "release-7", "indexer_id": 1
    }))
    .unwrap();
    let Json(response) = grab_release(State(f.app.clone()), Json(form))
        .await
        .unwrap();
    let new_key = response["hash"].as_str().unwrap();
    assert_ne!(new_key, f.hash);
    let pending = grabbed_torrents::get_all_pending(&f.app.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash, new_key);
    assert_eq!(pending[0].episode_numbers, [7]);
    let client = f.app.client_for_indexer(Some(1)).await.unwrap();
    let replay = client
        .add_torrent_returning_id(&f.release_url, "")
        .await
        .unwrap();
    assert_eq!(
        replay,
        (
            download_client::AddOutcome::AlreadyPresent,
            new_key.to_owned()
        )
    );
    let d = f.daemon.lock().unwrap();
    assert_eq!(d.adds, 2);
    assert_eq!(d.queue.len(), f.foreign.len() + 1);
    assert!(d.queue.starts_with(&f.foreign));
    assert!(d.edits.is_empty());
}

//! Unseen source numbering must be inferred before wanted-file selection.
//! These tests never seed a reviewed decision or invoke a review/picker route.
use super::*;
use ryokan::models::{episode_tags, grabbed_torrents};
use ryokan::services::download_client::DownloadItemState;
use ryokan::services::release_numbering::ReleaseMapping;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[path = "root_pack_discovery.rs"]
mod root_pack_discovery;

#[derive(Clone)]
struct Source {
    hash: String,
    view: u64,
    title: String,
    files: Vec<String>,
    entries: Vec<(i64, String, i32, i32)>,
    root: i64,
    description: &'static str,
}

fn dr_source() -> Source {
    let folder = "Dr. Stone Science Future Complete";
    Source {
        hash: "dddddddddddddddddddddddddddddddddddddddd".into(),
        view: 990037,
        title: "[Witness] Dr. Stone: Science Future (01-37) [1080p WEB-DL AAC] [Batch]".into(),
        files: (1..=37)
            .map(|n| {
                format!(
                    "{folder}/[Witness] Dr Stone - Science Future - {n:02} [1080p WEB-DL AAC].mkv"
                )
            })
            .collect(),
        entries: vec![
            (172019, "Dr. Stone: Science Future".into(), 12, 22),
            (189117, "Dr. Stone: Science Future Cour 2".into(), 12, 12),
            (199221, "Dr. Stone: Science Future Cour 3".into(), 13, 24),
        ],
        root: 172019,
        description: "Source: CR WEB-DL; full English subtitles",
    }
}

fn lagoon_source() -> Source {
    let folder = "[Legion] Black Lagoon Season 1+2+OVA [BD x264 1080p 10bit FLAC][Dual Audio]";
    Source {
        hash: "91a564c6d0f605344b67cef81eeab3199d61872b".into(),
        view: 1428087,
        title: "[Legion] Black Lagoon Seasons 1-2 + OVA (BD 1080p x264 Hi10P FLAC Dual Audio)".into(),
        files: (1..=29).map(|n| format!("{folder}/[Legion] Black Lagoon - {n:02} [BD x264 1080p 10bit FLAC][Dual Audio].mkv")).collect(),
        entries: vec![(889, "Black Lagoon".into(), 12, 0), (1519, "Black Lagoon: The Second Barrage".into(), 12, 12)],
        root: 889,
        description: "Source: Blu-ray; full English subtitles",
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn published_page(source: &Source, hash: &str, names: &[String]) -> String {
    let folder = names
        .first()
        .and_then(|name| name.rsplit_once('/'))
        .map(|(folder, _)| folder);
    let leaves: String = names
        .iter()
        .map(|name| {
            let name = if folder.is_some() {
                name.rsplit('/').next().unwrap()
            } else {
                name
            };
            format!(
                "<li><i class=\"fa fa-file\"></i>{}<span class=\"file-size\">(1 GiB)</span></li>",
                escape(name)
            )
        })
        .collect();
    let tree = if let Some(folder) = folder {
        format!(
            "<li><a class=\"folder\"><i class=\"fa fa-folder\"></i>{}</a><ul>{leaves}</ul></li>",
            escape(folder)
        )
    } else {
        leaves
    };
    format!(
        "<div class=\"panel\"><div class=\"panel-heading\"><h3 class=\"panel-title\">{}</h3></div><div class=\"panel-footer\"><a class=\"card-footer-item\" href=\"magnet:?xt=urn:btih:{hash}\">Magnet</a></div></div><div id=\"torrent-description\">{}</div><div class=\"panel\"><div class=\"panel-heading\"><h3 class=\"panel-title\">File list</h3></div><div class=\"torrent-file-list panel-body\"><ul>{tree}</ul></div></div>",
        escape(&source.title),
        source.description
    )
}

async fn serve(server: &MockServer, source: &Source, page: Option<String>) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                &source.hash,
                source.view,
                &source.title,
                "40 GiB",
                30,
            ))),
        )
        .mount(server)
        .await;
    if let Some(page) = page {
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", source.view)))
            .respond_with(ResponseTemplate::new(200).set_body_string(page))
            .mount(server)
            .await;
    }
    set_nyaa_base(&server.uri());
}

struct StrictAutomaticClient {
    db: sqlx::SqlitePool,
    hash: String,
    expected_series: i64,
    expected_episodes: Vec<i32>,
    expected_names: Vec<String>,
    view: u64,
    item: Mutex<DownloadItem>,
    files: Mutex<Vec<DownloadFile>>,
    added: AtomicBool,
    paused: AtomicBool,
    adds: AtomicUsize,
    resumes: AtomicUsize,
}

#[async_trait]
impl DownloadClient for StrictAutomaticClient {
    async fn test(&self) -> Result<String, String> {
        Ok("strict automatic fixture".into())
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("automatic bound source cannot use an unpaused whole add")
    }
    async fn add_torrent_paused(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("automatic bound source cannot use legacy paused preparation")
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("automatic bound source cannot fall back to optional selection")
    }
    async fn add_torrent_metadata_only(&self, hash: &str) -> Result<AddOutcome, String> {
        assert_eq!(hash, self.hash);
        assert!(
            !self.added.swap(true, Ordering::SeqCst),
            "one source must create only one client job"
        );
        self.adds.fetch_add(1, Ordering::SeqCst);
        self.paused.store(true, Ordering::SeqCst);
        Ok(AddOutcome::Added)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(if self.added.load(Ordering::SeqCst) {
            vec![self.item.lock().unwrap().clone()]
        } else {
            vec![]
        })
    }
    async fn get_files(&self, hash: &str) -> Result<Vec<DownloadFile>, String> {
        assert_eq!(hash, self.hash);
        Ok(self.files.lock().unwrap().clone())
    }
    async fn pause(&self, hash: &str) -> Result<(), String> {
        assert_eq!(hash, self.hash);
        self.paused.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn resume(&self, hash: &str) -> Result<(), String> {
        assert_eq!(hash, self.hash);
        assert!(self.paused.load(Ordering::SeqCst));
        let grabs = grabbed_torrents::get_all_pending(&self.db).await.unwrap();
        let grab = grabs
            .iter()
            .find(|grab| grab.hash == hash)
            .expect("payload cannot resume before durable ownership");
        assert_eq!(grab.series_id, self.expected_series);
        assert_eq!(grab.episode_numbers, self.expected_episodes);
        let mapping = grabbed_torrents::get_release_mapping(&self.db, grab.id)
            .await
            .unwrap()
            .expect("durable mapping before payload");
        assert_automatic_provenance(mapping.bound_source().unwrap(), self.view);
        let files = self.files.lock().unwrap().clone();
        let wanted: Vec<_> = files
            .iter()
            .map(|file| (file.name.as_str(), file.wanted))
            .collect();
        mapping.validate_bound_files(hash, &wanted).unwrap();
        let mut actual: Vec<_> = files
            .iter()
            .filter(|file| file.wanted)
            .map(|file| file.name.clone())
            .collect();
        let mut expected = self.expected_names.clone();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "actual wanted names must be read back before payload"
        );
        self.resumes.fetch_add(1, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("a proof hold must not delete data")
    }
    async fn set_file_wanted(
        &self,
        hash: &str,
        indices: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        assert_eq!(hash, self.hash);
        assert!(self.paused.load(Ordering::SeqCst));
        let mut files = self.files.lock().unwrap();
        for index in indices {
            files[*index].wanted = wanted;
        }
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

async fn fixture(
    source: &Source,
    target: i64,
    episodes: &[i32],
    raw_numbers: &[usize],
) -> (tempfile::TempDir, AppState, i64, Arc<StrictAutomaticClient>) {
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    let mut cfg = default_config();
    cfg.media_root = temp.path().join("library").to_string_lossy().into();
    cfg.post_processing_enabled = true;
    cfg.post_processing_mode = "copy".into();
    // This module tests numbering/dispatch/replay; actual language acceptance
    // has independent media-requirement tests and production pilot receipts.
    cfg.media_requirements_enabled = false;
    cfg.required_subtitle_languages.clear();
    cfg.required_audio_languages.clear();
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let mut target_sid = None;
    for (id, title, count, offset) in &source.entries {
        let sid = seed_series_with_cache(&state, *id, title).await;
        sqlx::query("UPDATE series SET format='TV',status='FINISHED',episodes=?,monitor_mode='all',allow_upgrades=0,cumulative_prior_episodes=?,cumulative_offset_known=1 WHERE id=?")
            .bind(count).bind(offset).bind(sid).execute(&state.db).await.unwrap();
        let mut detail = detail_for(*id, title);
        detail.episodes = Some(*count);
        ryokan::models::metadata_cache::upsert(&state.db, sid, *id, None, &detail)
            .await
            .unwrap();
        if *id == target {
            target_sid = Some(sid);
            for episode in episodes {
                ryokan::models::monitoring::set_episode_monitored(&state.db, sid, *episode, true)
                    .await
                    .unwrap();
            }
        }
    }
    let sid = target_sid.unwrap();
    let path = temp.path().join("downloads");
    let client = Arc::new(StrictAutomaticClient {
        db: state.db.clone(),
        hash: source.hash.clone(),
        expected_series: sid,
        expected_episodes: episodes.to_vec(),
        expected_names: raw_numbers
            .iter()
            .map(|n| source.files[n - 1].clone())
            .collect(),
        view: source.view,
        item: Mutex::new(DownloadItem {
            hash: source.hash.clone(),
            name: source.title.clone(),
            size: 100,
            progress: 0.0,
            dlspeed: 0,
            state: "paused".into(),
            category: "anime".into(),
            eta: 0,
            save_path: path.to_string_lossy().into(),
            content_path: path.to_string_lossy().into(),
            state_kind: DownloadItemState::Paused,
            seeding_done: false,
        }),
        files: Mutex::new(
            source
                .files
                .iter()
                .map(|name| DownloadFile {
                    name: name.clone(),
                    size: 100,
                    progress: 0.0,
                    wanted: true,
                })
                .collect(),
        ),
        added: AtomicBool::new(false),
        paused: AtomicBool::new(true),
        adds: AtomicUsize::new(0),
        resumes: AtomicUsize::new(0),
    });
    let mut clients = std::collections::HashMap::new();
    clients.insert(1, client.clone() as Arc<dyn DownloadClient>);
    *state.download_clients.write().await = Arc::new(DownloadClientPool {
        clients,
        default_torrent_id: Some(1),
        default_usenet_id: None,
    });
    let reviews: i64 = sqlx::query_scalar("SELECT count(*) FROM source_numbering_decisions")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        reviews, 0,
        "unseen-source fixture cannot preload review decisions"
    );
    (temp, state, sid, client)
}

async fn search(state: &AppState, sid: i64, episode: i32) -> usize {
    auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, episode)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0
    .grabbed
    .len()
}

fn assert_automatic_provenance(
    proof: &ryokan::services::source_numbering::BoundSourceProof,
    view: u64,
) {
    assert_eq!(
        proof.version, 2,
        "unseen automatic proof must carry its own provenance"
    );
    let value = serde_json::to_value(proof).unwrap();
    let provenance = &value["provenance"];
    assert_eq!(provenance["origin"], "automatic_unique");
    assert_eq!(provenance["evidence"]["verifier_version"], 1);
    let scope = provenance["evidence"]["scope_fingerprint"]
        .as_str()
        .expect("automatic scope fingerprint");
    assert_eq!(scope.len(), 64);
    assert!(
        scope
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(
        provenance["evidence"]["publication_url"],
        format!(
            "{}/view/{view}",
            std::env::var("RYOKAN_NYAA_API_BASE").unwrap()
        )
    );
}

async fn saved(state: &AppState, source: &Source, episodes: &[i32]) -> ReleaseMapping {
    let rows = grabbed_torrents::get_all_pending(&state.db).await.unwrap();
    assert_eq!(rows.len(), 1, "{}", dump_logs(&state.db).await);
    assert_eq!(rows[0].episode_numbers, episodes);
    let mapping = grabbed_torrents::get_release_mapping(&state.db, rows[0].id)
        .await
        .unwrap()
        .unwrap();
    let proof = mapping.bound_source().unwrap();
    assert_automatic_provenance(proof, source.view);
    assert_eq!(proof.info_hash, source.hash);
    assert_eq!(proof.domain.root_anilist_id, source.root);
    assert_eq!(proof.published_files.len(), source.files.len());
    mapping
}

async fn finish_after_restart(state: &AppState, client: &StrictAutomaticClient) {
    {
        let mut item = client.item.lock().unwrap();
        item.state_kind = DownloadItemState::Seeding;
        item.state = "seeding".into();
        item.progress = 1.0;
        let mut files = client.files.lock().unwrap();
        for file in files.iter_mut().filter(|file| file.wanted) {
            let path = std::path::Path::new(&item.save_path).join(&file.name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("exact automatic source {}", file.name)).unwrap();
            file.progress = 1.0;
        }
    }
    let restarted = build_test_app_state(state.db.clone(), None);
    *restarted.download_clients.write().await = state.download_clients.read().await.clone();
    ryokan::services::post_processing::run_once(&restarted).await;
}

#[tokio::test]
async fn automatic_source_numbering_dr_continuity_pack_selects_twelve_without_review() {
    let _gate = ENV_LOCK.lock().await;
    let source = dr_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let (temp, state, sid, client) = fixture(&source, 172019, &[12], &[12]).await;
    assert_eq!(
        search(&state, sid, 12).await,
        1,
        "{}",
        dump_logs(&state.db).await
    );
    let mapping = saved(&state, &source, &[12]).await;
    assert_eq!(mapping.bound_source().unwrap().selected_files.len(), 1);
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
    finish_after_restart(&state, &client).await;
    let videos = structural_season_numbering::library_videos(&temp.path().join("library"));
    assert_eq!(videos.len(), 1, "{}", dump_logs(&state.db).await);
    assert!(videos[0].to_string_lossy().contains("S01E12"));
    assert_eq!(
        episode_tags::get_for_series(&state.db, sid).await.unwrap()[&12].state,
        "completed"
    );
    let offset: i32 = sqlx::query_scalar("SELECT cumulative_prior_episodes FROM series WHERE id=?")
        .bind(sid)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(offset, 22);
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_legion_episode_selects_only_fifteen_as_three() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let (temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
    client.files.lock().unwrap().reverse();
    assert_eq!(
        search(&state, sid, 3).await,
        1,
        "{}",
        dump_logs(&state.db).await
    );
    let mapping = saved(&state, &source, &[3]).await;
    let proof = mapping.bound_source().unwrap();
    assert_eq!(proof.selected_files.len(), 1);
    assert_eq!(proof.selected_files[0].source_first, 15);
    assert_eq!(proof.selected_files[0].target_first, 3);
    finish_after_restart(&state, &client).await;
    let videos = structural_season_numbering::library_videos(&temp.path().join("library"));
    assert_eq!(videos.len(), 1, "{}", dump_logs(&state.db).await);
    assert!(videos[0].to_string_lossy().contains("S01E03"));
    let history: Vec<i32> =
        sqlx::query_scalar("SELECT episode_number FROM episode_grab_history WHERE series_id=?")
            .bind(sid)
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert!(!history.is_empty());
    assert!(history.iter().all(|n| *n == 3));
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_legion_batch_owns_only_second_barrage() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let episodes: Vec<_> = (1..=12).collect();
    let raw: Vec<_> = (13..=24).collect();
    let (temp, state, sid, client) = fixture(&source, 1519, &episodes, &raw).await;
    // The normal automatic batch action selects a complete entry. The
    // series action intentionally iterates individual wanted episodes.
    let report = ryokan::handlers::library::search::search_batch_releases(
        axum::extract::State(state.clone()),
        axum::extract::Path(sid),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(report.grabbed.len(), 1, "{}", dump_logs(&state.db).await);
    saved(&state, &source, &episodes).await;
    finish_after_restart(&state, &client).await;
    let videos = structural_season_numbering::library_videos(&temp.path().join("library"));
    assert_eq!(videos.len(), 12, "{}", dump_logs(&state.db).await);
    assert_eq!(
        episode_tags::get_for_series(&state.db, sid)
            .await
            .unwrap()
            .len(),
        12
    );
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_requested_entry_only_narrows_a_global_domain() {
    let _gate = ENV_LOCK.lock().await;
    for (source, requests) in [
        (dr_source(), vec![(172019, 12, 12), (189117, 12, 24)]),
        (lagoon_source(), vec![(889, 3, 3), (1519, 3, 15)]),
    ] {
        let server = MockServer::start().await;
        serve(
            &server,
            &source,
            Some(published_page(&source, &source.hash, &source.files)),
        )
        .await;
        let mut domains = Vec::new();
        for (target, episode, raw) in requests {
            let (_temp, state, sid, client) = fixture(&source, target, &[episode], &[raw]).await;
            assert_eq!(
                search(&state, sid, episode).await,
                1,
                "target={target}; {}",
                dump_logs(&state.db).await
            );
            let mapping = saved(&state, &source, &[episode]).await;
            let proof = mapping.bound_source().unwrap();
            domains.push(proof.domain.clone());
            assert_eq!(proof.selected_files[0].name, source.files[raw - 1]);
            assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            domains[0], domains[1],
            "requested ownership cannot choose a different global source namespace"
        );
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_solo_twelve_cannot_use_the_requested_entry_as_evidence() {
    let _gate = ENV_LOCK.lock().await;
    let mut source = dr_source();
    source.hash = "ba76bb6884e0da99f8cfbcbdf098781f385983e8".into();
    source.view = 1953036;
    source.title="[Erai-raws] Dr. Stone: Science Future - 12 [1080p CR WEBRip HEVC EAC3][MultiSub][9D591A30]".into();
    source.files=vec!["[Erai-raws] Dr Stone - Science Future - 12 [1080p CR WEBRip HEVC EAC3][MultiSub][9D591A30].mkv".into()];
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    for target in [172019, 189117] {
        let (_temp, state, sid, client) = fixture(&source, target, &[12], &[1]).await;
        assert_eq!(
            search(&state, sid, 12).await,
            0,
            "target={target}; {}",
            dump_logs(&state.db).await
        );
        assert_eq!(client.adds.load(Ordering::SeqCst), 0);
        assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
        assert!(
            grabbed_torrents::get_all_pending(&state.db)
                .await
                .unwrap()
                .is_empty()
        );
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_unavailable_or_mismatched_publication_never_adds() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    for page in [
        None,
        Some(published_page(&source, &"f".repeat(40), &source.files)),
    ] {
        let server = MockServer::start().await;
        serve(&server, &source, page).await;
        let (_temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
        assert_eq!(
            search(&state, sid, 3).await,
            0,
            "{}",
            dump_logs(&state.db).await
        );
        assert_eq!(client.adds.load(Ordering::SeqCst), 0);
        assert!(
            grabbed_torrents::get_all_pending(&state.db)
                .await
                .unwrap()
                .is_empty()
        );
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_changed_selected_or_unselected_manifest_never_resumes() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    for index in [0, 14] {
        let (_temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
        client.files.lock().unwrap()[index].name = "Foreign Show - 15.mkv".into();
        let result = auto_search_episode(
            axum::extract::State(state.clone()),
            axum::extract::Path((sid, 3)),
            axum::extract::Query(AutoSearchQuery::default()),
        )
        .await;
        match result {
            Err((status, reason)) => {
                assert_eq!(
                    status,
                    axum::http::StatusCode::BAD_GATEWAY,
                    "index={index}; {reason}"
                );
                assert!(!reason.is_empty());
            }
            Ok(report) => panic!(
                "changed actual manifest must return a held error, index={index}, grabbed={}; {}",
                report.0.grabbed.len(),
                dump_logs(&state.db).await
            ),
        }
        assert_eq!(
            client.adds.load(Ordering::SeqCst),
            1,
            "this control must reach strict client readback"
        );
        assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
        assert!(client.paused.load(Ordering::SeqCst));
        assert!(
            grabbed_torrents::get_all_pending(&state.db)
                .await
                .unwrap()
                .is_empty()
        );
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_failed_record_keeps_new_metadata_job_paused() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let (_temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
    sqlx::query("CREATE TRIGGER reject_automatic_source BEFORE INSERT ON grabbed_torrents BEGIN SELECT RAISE(ABORT,'test durable automatic record failure'); END").execute(&state.db).await.unwrap();
    assert_eq!(
        search(&state, sid, 3).await,
        0,
        "{}",
        dump_logs(&state.db).await
    );
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    assert!(client.paused.load(Ordering::SeqCst));
    assert!(
        grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap()
            .is_empty()
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_changed_completed_manifest_preserves_existing_media() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let (temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
    assert_eq!(
        search(&state, sid, 3).await,
        1,
        "{}",
        dump_logs(&state.db).await
    );
    let mapping = saved(&state, &source, &[3]).await;
    let folder: String = sqlx::query_scalar("SELECT folder_name FROM series WHERE id=?")
        .bind(sid)
        .fetch_one(&state.db)
        .await
        .unwrap();
    let old = temp
        .path()
        .join("library")
        .join(folder)
        .join("Season 01/Black Lagoon - S01E03 [720p WEB] [Old].mkv");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"protected original").unwrap();
    client.files.lock().unwrap()[14].name = "Foreign Show - 15.mkv".into();
    finish_after_restart(&state, &client).await;
    assert_eq!(std::fs::read(&old).unwrap(), b"protected original");
    assert_eq!(
        structural_season_numbering::library_videos(&temp.path().join("library")).len(),
        1
    );
    let grab_id: i64 = sqlx::query_scalar("SELECT id FROM grabbed_torrents WHERE hash=?")
        .bind(&source.hash)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        grabbed_torrents::get_release_mapping(&state.db, grab_id)
            .await
            .unwrap()
            .unwrap(),
        mapping,
        "a held completed manifest cannot rewrite its durable proof"
    );
    let imported: i64 =
        sqlx::query_scalar("SELECT count(*) FROM grabbed_torrents WHERE state='imported'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(imported, 0);
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_daily_upgrade_narrows_the_pack_and_stops_at_target() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    for already_at_target in [false, true] {
        let (temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
        let mut cfg = ryokan::models::config::get_config(&state.db)
            .await
            .unwrap()
            .unwrap();
        cfg.upgrade_search_enabled = true;
        cfg.media_requirements_enabled = true;
        cfg.minimum_resolution = "720".into();
        cfg.target_resolution = "1080".into();
        cfg.required_subtitle_languages = "en".into();
        cfg.minimum_subtitle_check = true;
        cfg.target_subtitle_check = true;
        ryokan::models::config::save_config(&state.db, &cfg)
            .await
            .unwrap();
        sqlx::query("UPDATE series SET allow_upgrades=1,monitor_mode='future' WHERE id=?")
            .bind(sid)
            .execute(&state.db)
            .await
            .unwrap();
        let folder: String = sqlx::query_scalar("SELECT folder_name FROM series WHERE id=?")
            .bind(sid)
            .fetch_one(&state.db)
            .await
            .unwrap();
        let old = temp
            .path()
            .join("library")
            .join(folder)
            .join("Season 01/Black Lagoon - S01E03 [1080p BluRay] [Old].mkv");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"existing complete episode").unwrap();
        let canonical = std::fs::canonicalize(&old).unwrap();
        let identity = ryokan::services::media_info::FileIdentity::from_metadata(
            &std::fs::metadata(&canonical).unwrap(),
        )
        .unwrap();
        ryokan::models::media_probe_cache::upsert_identified(&state.db,canonical.to_str().unwrap(),&identity.cache_key(),identity.size as i64,&serde_json::json!({"format":{"duration":"1400"},"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080},{"index":1,"codec_type":"subtitle","codec_name":"ass","tags":{"language":if already_at_target {"eng"} else {"deu"},"title":"Full subtitles"},"disposition":{"forced":0}}]}).to_string()).await;
        let result = ryokan::services::upgrade::run_once(&state).await.unwrap();
        assert_eq!(
            result.upgrades_grabbed,
            usize::from(!already_at_target),
            "target_met={already_at_target}; {}",
            dump_logs(&state.db).await
        );
        assert_eq!(
            client.adds.load(Ordering::SeqCst),
            usize::from(!already_at_target)
        );
        assert_eq!(
            client.resumes.load(Ordering::SeqCst),
            usize::from(!already_at_target)
        );
        assert_eq!(std::fs::read(&old).unwrap(), b"existing complete episode");
        if !already_at_target {
            saved(&state, &source, &[3]).await;
        }
    }
    unset_nyaa_base();
}

async fn rss_source_setup(
    state: &AppState,
    server: &MockServer,
    source: &Source,
    sole_tracked_owner: Option<i64>,
) {
    // Metadata evidence survives independently of what is tracked. Removing
    // library rows must not make the requested owner choose the source root.
    for (id, title, count, _) in &source.entries {
        let mut detail = detail_for(*id, title);
        detail.episodes = Some(*count);
        ryokan::models::metadata_cache::upsert_provider(&state.db, *id, None, &detail)
            .await
            .unwrap();
    }
    if let Some(owner) = sole_tracked_owner {
        sqlx::query("DELETE FROM series WHERE anilist_id<>?")
            .bind(owner)
            .execute(&state.db)
            .await
            .unwrap();
        let owners: Vec<i64> = sqlx::query_scalar("SELECT anilist_id FROM series")
            .fetch_all(&state.db)
            .await
            .unwrap();
        assert_eq!(owners, [owner]);
    }
    let mut cfg = ryokan::models::config::get_config(&state.db)
        .await
        .unwrap()
        .unwrap();
    cfg.rss_master_enabled = true;
    cfg.rss_enabled = false; // Only the mocked explicit feed is polled.
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let feed = format!(
        "<rss version=\"2.0\" xmlns:nyaa=\"https://nyaa.si/xmlns/nyaa\"><channel><title>Automatic source proof</title><item><title>{}</title><guid>automatic-source-{}</guid><link>{}/view/{}</link><nyaa:magneturi>magnet:?xt=urn:btih:{}</nyaa:magneturi><nyaa:infohash>{}</nyaa:infohash><nyaa:seeders>30</nyaa:seeders></item></channel></rss>",
        escape(&source.title),
        source.hash,
        server.uri(),
        source.view,
        source.hash,
        source.hash
    );
    Mock::given(method("GET"))
        .and(path("/automatic-feed"))
        .respond_with(ResponseTemplate::new(200).set_body_string(feed))
        .mount(server)
        .await;
    let url = format!("{}/automatic-feed", server.uri());
    ryokan::models::direct_rss_feeds::insert(
        &state.db,
        ryokan::models::direct_rss_feeds::DirectRssFeedForm {
            name: "Automatic source proof",
            url: &url,
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn automatic_source_numbering_rss_selects_unique_tracked_owner_without_review() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let episodes: Vec<_> = (1..=12).collect();
    let raw: Vec<_> = (13..=24).collect();
    let (_temp, state, _sid, client) = fixture(&source, 1519, &episodes, &raw).await;
    rss_source_setup(&state, &server, &source, Some(1519)).await;
    client.files.lock().unwrap().reverse();
    let result = ryokan::services::rss::sync_once(&state, "scheduled")
        .await
        .unwrap();
    assert_eq!(result.items_seen, 1, "{}", dump_logs(&state.db).await);
    assert_eq!(result.grabbed, 1, "{}", dump_logs(&state.db).await);
    let mapping = saved(&state, &source, &episodes).await;
    assert_eq!(mapping.bound_source().unwrap().selected_files.len(), 12);
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_rss_solo_twelve_holds_even_with_one_tracked_owner() {
    let _gate = ENV_LOCK.lock().await;
    let mut source = dr_source();
    source.hash = "ba76bb6884e0da99f8cfbcbdf098781f385983e8".into();
    source.view = 1953036;
    source.title = "[Erai-raws] Dr. Stone: Science Future - 12 [1080p CR WEBRip HEVC EAC3][MultiSub][9D591A30]".into();
    source.files = vec!["[Erai-raws] Dr Stone - Science Future - 12 [1080p CR WEBRip HEVC EAC3][MultiSub][9D591A30].mkv".into()];
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let (_temp, state, _sid, client) = fixture(&source, 172019, &[12], &[1]).await;
    rss_source_setup(&state, &server, &source, Some(172019)).await;
    let result = ryokan::services::rss::sync_once(&state, "scheduled")
        .await
        .unwrap();
    assert_eq!(result.items_seen, 1, "{}", dump_logs(&state.db).await);
    assert_eq!(result.grabbed, 0, "{}", dump_logs(&state.db).await);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    assert!(
        grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap()
            .is_empty()
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_rss_multiple_tracked_owners_hold_without_arbitrary_choice() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let episodes: Vec<_> = (1..=12).collect();
    let raw: Vec<_> = (13..=24).collect();
    let (_temp, state, _sid, client) = fixture(&source, 1519, &episodes, &raw).await;
    rss_source_setup(&state, &server, &source, None).await;
    let owners: i64 = sqlx::query_scalar("SELECT count(*) FROM series")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(owners, 2);
    let result = ryokan::services::rss::sync_once(&state, "scheduled")
        .await
        .unwrap();
    assert_eq!(result.items_seen, 1, "{}", dump_logs(&state.db).await);
    assert_eq!(result.grabbed, 0, "{}", dump_logs(&state.db).await);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    assert!(
        grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap()
            .is_empty()
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_source_numbering_rss_narrows_unique_global_pack_to_monitored_episode_three() {
    let _gate = ENV_LOCK.lock().await;
    let source = lagoon_source();
    let server = MockServer::start().await;
    serve(
        &server,
        &source,
        Some(published_page(&source, &source.hash, &source.files)),
    )
    .await;
    let (_temp, state, sid, client) = fixture(&source, 1519, &[3], &[15]).await;
    rss_source_setup(&state, &server, &source, Some(1519)).await;
    for episode in 1..=12 {
        ryokan::models::monitoring::set_episode_monitored(&state.db, sid, episode, episode == 3)
            .await
            .unwrap();
    }
    let monitored = ryokan::models::monitoring::get_monitored_episode_numbers(&state.db, sid)
        .await
        .unwrap();
    assert_eq!(monitored, [3]);
    let result = ryokan::services::rss::sync_once(&state, "scheduled")
        .await
        .unwrap();
    assert_eq!(result.items_seen, 1, "{}", dump_logs(&state.db).await);
    assert_eq!(result.grabbed, 1, "{}", dump_logs(&state.db).await);
    let mapping = saved(&state, &source, &[3]).await;
    let proof = mapping.bound_source().unwrap();
    assert_eq!(proof.selected_files.len(), 1);
    assert_eq!(proof.selected_files[0].name, source.files[14]);
    assert_eq!(proof.selected_files[0].source_first, 15);
    assert_eq!(proof.selected_files[0].target_first, 3);
    let files = client.files.lock().unwrap();
    assert_eq!(files.len(), 29);
    assert_eq!(
        files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| file.wanted.then_some(index))
            .collect::<Vec<_>>(),
        [14],
        "automatic source inference cannot authorize unmonitored files"
    );
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
    unset_nyaa_base();
}

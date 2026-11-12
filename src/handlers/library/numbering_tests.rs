use super::*;
use crate::models::{config, metadata_cache};
use crate::services::anilist::AnimeDetail;
use crate::services::source_numbering::tests::lagoon_files;
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "91a564c6d0f605344b67cef81eeab3199d61872b";
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct NyaaOrigin(Option<std::ffi::OsString>);

impl NyaaOrigin {
    fn set(origin: &str) -> Self {
        let previous = std::env::var_os("RYOKAN_NYAA_API_BASE");
        // nextest isolates tests; this guard also serializes this module's
        // origin changes when running it through cargo test.
        unsafe { std::env::set_var("RYOKAN_NYAA_API_BASE", origin) };
        Self(previous)
    }
}

impl Drop for NyaaOrigin {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(origin) => std::env::set_var("RYOKAN_NYAA_API_BASE", origin),
                None => std::env::remove_var("RYOKAN_NYAA_API_BASE"),
            }
        }
    }
}

fn detail(id: i64, title: &str) -> AnimeDetail {
    AnimeDetail {
        release_date: None,
        id,
        // A missing MAL identity deliberately prevents the provider-plan
        // fallback from making any external request.
        id_mal: None,
        title_romaji: title.into(),
        title_english: title.into(),
        title_native: String::new(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(12),
        duration: Some(24),
        season: String::new(),
        season_year: Some(2006),
        end_year: Some(2006),
        is_adult: false,
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

async fn state() -> (AppState, i64, i64) {
    seed_state(in_memory_pool().await).await
}

async fn seed_state(db: sqlx::SqlitePool) -> (AppState, i64, i64) {
    let root = seed_series(&db, 889, "Black Lagoon").await;
    let target = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    for detail in [
        detail(889, "Black Lagoon"),
        detail(1519, "Black Lagoon: The Second Barrage"),
    ] {
        metadata_cache::upsert_provider(&db, detail.id, None, &detail)
            .await
            .unwrap();
    }
    (build_test_app_state(db, None), root, target)
}

fn listing(files: &[String]) -> String {
    let leaves = files
        .iter()
        .map(|name| format!(r#"<li><i class="fa fa-file"></i>{name}</li>"#))
        .collect::<String>();
    format!(
        r#"<div class="panel"><div class="panel-heading"><h3 class="panel-title">Black Lagoon complete source</h3></div><div class="panel-footer"><a class="card-footer-item" href="magnet:?xt=urn:btih:{HASH}">Magnet</a></div></div><div class="panel"><div class="panel-heading"><h3 class="panel-title">File list</h3></div><div class="torrent-file-list panel-body"><ul>{leaves}</ul></div></div>"#
    )
}

fn query(server: &MockServer) -> ReviewQuery {
    ReviewQuery {
        link: format!("{}/view/123", server.uri()),
        hash: HASH.into(),
        root: Some(889),
    }
}

fn form(server: &MockServer, fingerprint: &str) -> ReviewForm {
    let query = query(server);
    ReviewForm {
        link: query.link,
        hash: query.hash,
        root: 889,
        fingerprint: fingerprint.into(),
    }
}

async fn html(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn assert_metadata_only(state: &AppState, expected_decisions: i64) {
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM source_numbering_decisions")
            .fetch_one(&state.db)
            .await
            .unwrap(),
        expected_decisions
    );
    for sql in [
        "SELECT COUNT(*) FROM grabbed_torrents",
        "SELECT COUNT(*) FROM pending_grabs",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&state.db)
                .await
                .unwrap(),
            0,
            "a review must not create a download job"
        );
    }
    assert!(state.download_clients.read().await.clients.is_empty());
}

#[tokio::test]
async fn review_source_is_limited_to_same_origin_nyaa_listing_and_complete_hash() {
    let _lock = ENV_LOCK.lock().await;
    let _origin = NyaaOrigin::set("https://nyaa.si");
    let hash = "a".repeat(40);
    assert!(validate_source("https://nyaa.si/view/1952833", &hash).is_ok());
    assert!(validate_source("https://nyaa.si/download/1952833.torrent", &hash).is_ok());
    for link in [
        "https://example.com/view/1952833",
        "https://nyaa.si/download/1952833.exe",
        "http://127.0.0.1/admin",
        "file:///etc/passwd",
    ] {
        assert!(validate_source(link, &hash).is_err());
    }
    assert!(validate_source("https://nyaa.si/view/1", "short").is_err());
    assert!(validate_source("https://nyaa.si/view/1", &"z".repeat(40)).is_err());
}

#[tokio::test]
async fn get_numbering_review_previews_verified_source_without_writes() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let _origin = NyaaOrigin::set(&server.uri());
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(listing(&lagoon_files())))
        .expect(1)
        .mount(&server)
        .await;
    let export_path = std::env::var_os("RYOKAN_PRIVATE_PREVIEW_DB");
    // sqlx's shared in-memory database cannot produce a VACUUM INTO copy.
    // Only the opt-in export uses a private temporary file-backed fixture.
    let fixture_dir = export_path.as_ref().map(|_| tempfile::tempdir().unwrap());
    let (state, _, target) = if let Some(directory) = &fixture_dir {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("numbering-preview.db"))
            .create_if_missing(true);
        let db = sqlx::SqlitePool::connect_with(options).await.unwrap();
        crate::models::migrations::migrate(&db).await.unwrap();
        seed_state(db).await
    } else {
        state().await
    };
    let response = review(State(state.clone()), Path(target), Query(query(&server)))
        .await
        .unwrap();
    let rendered = html(response).await;
    assert!(rendered.contains("12 files belong to Black Lagoon: The Second Barrage"));
    assert!(rendered.contains("Use this numbering"));
    assert!(rendered.contains("[Legion] Black Lagoon - 29"));
    assert_metadata_only(&state, 0).await;
    // Private, opt-in visual verification of the actual handler output.
    // Without the test-only variable, this test writes no files.
    if let Some(output) = std::env::var_os("RYOKAN_NUMBERING_PREVIEW_HTML") {
        std::fs::write(output, &rendered).expect("write private numbering preview");
    }
    if let Some(output) = export_path {
        config::save_config(
            &state.db,
            &config::Config {
                active_client: String::new(),
                qbit_url: String::new(),
                deluge_url: String::new(),
                transmission_url: String::new(),
                rtorrent_url: String::new(),
                jellyfin_url: String::new(),
                rss_enabled: false,
                rss_master_enabled: false,
                post_processing_enabled: false,
                upgrade_search_enabled: false,
                default_allow_upgrades: false,
                auto_grab_on_add: false,
                search_on_monitoring_change: false,
                auto_redownload_failed: false,
                nyaa_enabled: false,
                seadex_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE series SET allow_upgrades=0")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM download_clients")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
                .fetch_one(&state.db)
                .await
                .unwrap(),
            0
        );
        let output = std::path::PathBuf::from(output);
        let literal = output
            .to_str()
            .expect("UTF-8 preview path")
            .replace('\'', "''");
        // SQLite does not bind VACUUM's output filename. Escape it as a SQL
        // literal rather than interpolating raw test input or binding it.
        sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{literal}'")))
            .execute(&state.db)
            .await
            .expect("export private numbering preview database");
        assert!(std::fs::metadata(output).unwrap().len() > 0);
    }
}

#[tokio::test]
async fn save_refetches_manifest_and_refuses_stale_preview_without_writes() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let _origin = NyaaOrigin::set(&server.uri());
    let files = lagoon_files();
    let before = listing(&files);
    let mut changed = files;
    // Even a non-video addition changes the exact-source evidence. The
    // eligible episode set remains the same, so it cannot mask a stale save.
    changed.push("checksum.sha256".into());
    let after = listing(&changed);
    let calls = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_string(
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    before.clone()
                } else {
                    after.clone()
                },
            )
        })
        .expect(2)
        .mount(&server)
        .await;
    let (state, _, target) = state().await;
    let initial = page(&state, target, &query(&server)).await.unwrap();
    assert_eq!(initial.eligible_count, 12);
    let fingerprint = initial.preview.unwrap().fingerprint;
    let response = save_review(
        State(state.clone()),
        Path(target),
        Form(form(&server, &fingerprint)),
    )
    .await
    .unwrap();
    let rendered = html(response).await;
    assert!(rendered.contains("The source or numbering rules changed."));
    assert!(rendered.contains("checksum.sha256"));
    assert!(!rendered.contains("Numbering saved for this exact source."));
    assert_metadata_only(&state, 0).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn save_rejects_missing_or_edited_preview_fingerprints() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let _origin = NyaaOrigin::set(&server.uri());
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(listing(&lagoon_files())))
        .expect(2)
        .mount(&server)
        .await;
    let (state, _, target) = state().await;
    for fingerprint in ["".to_string(), "0".repeat(64)] {
        let response = save_review(
            State(state.clone()),
            Path(target),
            Form(form(&server, &fingerprint)),
        )
        .await
        .unwrap();
        assert!(
            html(response)
                .await
                .contains("The source or numbering rules changed.")
        );
        assert_metadata_only(&state, 0).await;
    }
}

#[tokio::test]
async fn save_binds_preview_to_tracked_target_not_anilist_number_or_sibling() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let _origin = NyaaOrigin::set(&server.uri());
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(listing(&lagoon_files())))
        .expect(2)
        .mount(&server)
        .await;
    let (state, root, target) = state().await;
    let initial = page(&state, target, &query(&server)).await.unwrap();
    let fingerprint = initial.preview.unwrap().fingerprint;
    let response = save_review(
        State(state.clone()),
        Path(root),
        Form(form(&server, &fingerprint)),
    )
    .await
    .unwrap();
    assert!(
        html(response)
            .await
            .contains("The source or numbering rules changed.")
    );
    let error = save_review(
        State(state.clone()),
        Path(1519),
        Form(form(&server, &fingerprint)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_metadata_only(&state, 0).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn save_identical_review_is_idempotent_and_only_owns_target_files() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let _origin = NyaaOrigin::set(&server.uri());
    let files = lagoon_files();
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(listing(&files)))
        .expect(3)
        .mount(&server)
        .await;
    let (state, _, target) = state().await;
    let initial = page(&state, target, &query(&server)).await.unwrap();
    assert_eq!(initial.eligible_count, 12);
    let preview = initial.preview.unwrap();
    let expected = preview.confirm(&files[12..24]).unwrap();
    let mut first_id = None;
    for _ in 0..2 {
        let response = save_review(
            State(state.clone()),
            Path(target),
            Form(form(&server, &preview.fingerprint)),
        )
        .await
        .unwrap();
        assert!(
            html(response)
                .await
                .contains("Numbering saved for this exact source.")
        );
        let saved = decisions::lookup_exact(&state.db, HASH, 1519)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.proof, expected);
        assert_eq!(saved.proof.published_files.len(), 29);
        assert_eq!(saved.proof.episodes(), (1..=12).collect::<Vec<_>>());
        assert!(saved.proof.selected_files.iter().all(|file| {
            (13..=24).contains(&file.source_first)
                && file.source_first == file.target_first + 12
                && file.source_last == file.target_last + 12
        }));
        if let Some(id) = first_id {
            assert_eq!(saved.id, id);
        } else {
            first_id = Some(saved.id);
        }
        assert_metadata_only(&state, 1).await;
        assert!(
            decisions::lookup_exact(&state.db, HASH, 889).await.is_err(),
            "a reviewed source cannot be reused by an unreviewed sibling"
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

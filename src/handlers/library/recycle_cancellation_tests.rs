use super::{episodes, recycle};
use crate::models::{config, episode_tags, grabbed_torrents, metadata_cache};
use crate::services::anilist::AnimeDetail;
use crate::services::recycle::{
    self as bin, RecycleKind, RecycleOutcome, test_support::PausedCopy,
};
use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series,
};
use axum::extract::{Path, State};
use axum_htmx::HxRequest;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
const VIDEO: &[u8] = b"small video fixture";

struct Fixture {
    _tmp: tempfile::TempDir,
    db: SqlitePool,
    series_id: i64,
    grab_id: i64,
    video: PathBuf,
    nfo: PathBuf,
    subtitle: PathBuf,
    neighbor: PathBuf,
    bin: String,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let season = root.join("media/Show/Season 01");
        std::fs::create_dir_all(&season).unwrap();
        let video = season.join("Show - S01E06-E07 [1080p WEB].mkv");
        let nfo = video.with_extension("nfo");
        let subtitle = video.with_extension("en.srt");
        let neighbor = season.join("Show - S01E70.mkv");
        for (path, bytes) in [
            (&video, VIDEO),
            (&nfo, b"nfo".as_slice()),
            (&subtitle, b"English captions".as_slice()),
            (&neighbor, b"keep neighbor".as_slice()),
        ] {
            std::fs::write(path, bytes).unwrap();
        }
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1519, "Show").await;
        config::save_config(
            &db,
            &config::Config {
                media_root: root.join("media").display().to_string(),
                recycle_bin_path: root.join("bin").display().to_string(),
                recycle_bin_age_days: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Keep the handler entirely offline, including its metadata lookup.
        let detail: AnimeDetail = serde_json::from_value(serde_json::json!({
            "id": 1519, "title_romaji": "Show", "title_english": "Show",
            "title_native": "Show", "cover_url": "", "banner_url": "",
            "format": "TV", "status": "FINISHED", "status_display": "Finished",
            "episodes": 12, "duration": 24, "season": "", "description": "",
            "genres": [], "score_is_ten_point": false, "score_class": "",
            "synonyms": [], "streaming_episodes": [], "relations": []
        }))
        .unwrap();
        metadata_cache::upsert(&db, series_id, 1519, None, &detail)
            .await
            .unwrap();
        for ep in [6, 7] {
            sqlx::query("INSERT INTO episode_quality_tags (series_id, episode_number, quality_tag, state) VALUES (?, ?, 'WEBDL-1080p', 'completed')")
                .bind(series_id).bind(ep).execute(&db).await.unwrap();
            sqlx::query("INSERT INTO episode_grab_history (series_id, episode_number, quality_tag, release_title, state) VALUES (?, ?, 'WEBDL-1080p', 'Show', 'completed')")
                .bind(series_id).bind(ep).execute(&db).await.unwrap();
        }
        let grab_id = seed_grabbed_torrent(&db, series_id, HASH, "Show", &[6, 7]).await;
        grabbed_torrents::mark_imported(&db, grab_id).await.unwrap();
        Self {
            _tmp: tmp,
            db,
            series_id,
            grab_id,
            video,
            nfo,
            subtitle,
            neighbor,
            bin: root.join("bin").display().to_string(),
        }
    }

    async fn wait_for_log(&self, message: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let finished: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM logs WHERE message = ?)")
                        .bind(message)
                        .fetch_one(&self.db)
                        .await
                        .unwrap();
                if finished {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("owned operation must finish after request cancellation");
    }

    fn assert_library_bytes(&self) {
        assert_eq!(std::fs::read(&self.video).unwrap(), VIDEO);
        assert_eq!(std::fs::read(&self.nfo).unwrap(), b"nfo");
        assert_eq!(std::fs::read(&self.subtitle).unwrap(), b"English captions");
        assert_eq!(std::fs::read(&self.neighbor).unwrap(), b"keep neighbor");
    }
}

#[tokio::test]
async fn delete_finishes_catalog_and_client_cleanup_after_request_disconnect() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let f = Fixture::new().await;
    let server = MockServer::start().await;
    Mock::given(path("/api/v2/auth/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("Ok.")
                .insert_header("set-cookie", "SID=test; HttpOnly; path=/"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .and(body_string_contains(format!("hashes={HASH}")))
        .and(body_string_contains("deleteFiles=true"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = crate::services::download_client::qbittorrent::QbitClient::new(
        &server.uri(),
        "test",
        "test",
        "ryokan",
    );
    let state = build_test_app_state(f.db.clone(), Some(Arc::new(client)));
    let copy = PausedCopy::at(&f.video);
    let request = tokio::spawn(episodes::delete_episode_file(
        State(state),
        HxRequest(true),
        Path((1519, 6)),
    ));
    tokio::time::timeout(Duration::from_secs(10), copy.wait())
        .await
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    // The real copy path has run; the move and all catalog work still
    // need to finish after the HTTP future has been dropped.
    assert!(f.video.is_file());
    copy.release();
    f.wait_for_log(&format!(
        "Deleted episode 6 file: Season 01/{}",
        f.video.file_name().unwrap().to_str().unwrap()
    ))
    .await;

    assert!(!f.video.exists());
    assert!(!f.nfo.exists());
    assert!(!f.subtitle.exists());
    assert_eq!(std::fs::read(&f.neighbor).unwrap(), b"keep neighbor");
    assert!(
        episode_tags::get_for_series(&f.db, f.series_id)
            .await
            .unwrap()
            .is_empty()
    );
    for ep in [6, 7] {
        let history = episode_tags::get_grab_history(&f.db, f.series_id, ep)
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].state, "removed");
    }
    let state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(f.grab_id)
        .fetch_one(&f.db)
        .await
        .unwrap();
    assert_eq!(state, "removed");
    let entries = bin::list_entries(&f.bin).await.unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.manifest.files.len(), 3);
    for (original, expected) in [
        (&f.video, VIDEO),
        (&f.nfo, b"nfo".as_slice()),
        (&f.subtitle, b"English captions".as_slice()),
    ] {
        assert_eq!(
            std::fs::read(entry.dir.join(original.file_name().unwrap())).unwrap(),
            expected
        );
    }
    assert_eq!(entry.manifest.original_path, f.video.to_str().unwrap());
    let events = crate::models::library_activity::recent(&f.db, &[f.series_id], None)
        .await
        .unwrap();
    assert_eq!(events.iter().filter(|e| e.action == "Recycled").count(), 1);
    assert!(events.iter().any(|e| e.reason.contains("Manually")));

    server.verify().await;
}

#[tokio::test]
async fn restore_retains_import_locks_and_retags_after_request_disconnect() {
    let f = Fixture::new().await;
    let outcome = bin::recycle(
        &f.db,
        &f.bin,
        RecycleKind::Episode,
        Some(f.series_id),
        "Show",
        &f.video,
    )
    .await
    .unwrap();
    let RecycleOutcome::Recycled { entry_id } = outcome else {
        panic!("expected recoverable entry");
    };
    for ep in [6, 7] {
        episode_tags::clear_episode_tag(&f.db, f.series_id, ep)
            .await
            .unwrap();
        episode_tags::mark_grab_history_removed(&f.db, f.series_id, ep)
            .await
            .unwrap();
    }
    let entry = bin::find_entry(&f.bin, &entry_id).await.unwrap().unwrap();
    let copy = PausedCopy::at(&entry.dir.join(f.video.file_name().unwrap()));
    let state = build_test_app_state(f.db.clone(), None);
    let request = tokio::spawn(recycle::restore(State(state), Path(entry_id.clone())));
    tokio::time::timeout(Duration::from_secs(10), copy.wait())
        .await
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(
        crate::services::post_processing::POST_PROC_LOCK
            .try_lock()
            .is_err(),
        "restore must retain post-processing lock after disconnect"
    );
    assert!(
        crate::services::manual_import::import::IMPORT_LOCK
            .try_lock()
            .is_err(),
        "restore must retain manual-import lock after disconnect"
    );
    copy.release();
    f.wait_for_log("Restored from recycle bin").await;

    // Reacquiring both locks also waits for the complete owned task.
    let _post_proc = crate::services::post_processing::POST_PROC_LOCK
        .lock()
        .await;
    let _manual_import = crate::services::manual_import::import::IMPORT_LOCK
        .lock()
        .await;
    f.assert_library_bytes();
    let events = crate::models::library_activity::recent(&f.db, &[f.series_id], None)
        .await
        .unwrap();
    assert_eq!(events.iter().filter(|e| e.action == "Restored").count(), 1);
    assert!(bin::find_entry(&f.bin, &entry_id).await.unwrap().is_none());
    let tags = episode_tags::get_for_series(&f.db, f.series_id)
        .await
        .unwrap();
    for ep in [6, 7] {
        assert_eq!(tags[&ep].state, "completed");
        let history = episode_tags::get_grab_history(&f.db, f.series_id, ep)
            .await
            .unwrap();
        assert!(history.iter().any(|h| h.state == "completed"));
        assert!(history.iter().any(|h| h.state == "removed"));
    }
}

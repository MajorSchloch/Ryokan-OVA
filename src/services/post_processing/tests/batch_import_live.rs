//! Live end-to-end batch-import proof for the PR #198 guards. Unlike
//! the `run_once.rs` fan-out tests (which stop short of
//! `import_torrent`), these drive the FULL import path — readiness
//! gate → batch preflight → import loop → real `do_file_op` calls on
//! a real temp filesystem — with only the download client mocked.
//!
//! Guards under proof:
//!   1. Unparseable extras (NCOP/NCED) are skipped while the
//!      parseable episodes import, including a dot-delimited name
//!      whose `H.264` token must not mis-parse as episode 264.
//!   2. Two files resolving to one destination slot fail the whole
//!      grab before any mutation (nothing lands in the library,
//!      sources untouched).
//!   3. An incomplete wanted video holds the entire batch in
//!      `pending` (readiness gate) rather than importing a subset.
//!   4. `E05` + `E05v2` in one pack imports the v2 and skips the v1
//!      (issue #204).
//!   5. PV / bare-SP / recap extras from the pinned corpus packs are
//!      skipped instead of colliding with real slots (issue #203).
//!   6. An upgrade lands the new file before the old one is retired,
//!      so a failed placement leaves the library and the old grab
//!      untouched (issue #202).

use crate::models::grabbed_torrents;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use crate::services::post_processing;
use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::POST_PROC_TEST_SERIALIZER;

#[tokio::test]
async fn verified_migration_replay_uses_file_identities_and_preserves_existing_episodes() {
    use grabbed_torrents::migration_replay::{ReplayFile, ReplayPlan};
    let _guard = POST_PROC_TEST_SERIALIZER.lock().await;
    for mode in ["copy", "hardlink", "move"] {
        let temp = tempfile::tempdir().unwrap();
        let media = temp.path().join("media");
        let downloads = temp.path().join("downloads");
        std::fs::create_dir_all(&media).unwrap();
        let names = [
            "Special/Show - S00E00.mkv",
            "Season 1/Show - 01.mkv",
            "Season 1/Show - 13.mkv",
            "Special/Show - S00E02.mkv",
        ];
        for (i, name) in names.iter().enumerate() {
            let path = downloads.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("source {i}")).unwrap();
        }
        let db = in_memory_pool().await;
        seed_config(&db, &media).await;
        sqlx::query("UPDATE config SET post_processing_mode=?, episode_file_format='{series.title} - S{season.number:00}E{episode.number:00}{ext}'")
            .bind(mode).execute(&db).await.unwrap();
        let parent = seed_series(&db, 9410, "Parent").await;
        let sibling = seed_series(&db, 9411, "Sibling").await;
        sqlx::query(
            "UPDATE series SET episodes=2, allow_upgrades=0, cumulative_prior_episodes=999",
        )
        .execute(&db)
        .await
        .unwrap();
        let gid = grabbed_torrents::record_grab(
            &db,
            "replay-pack",
            "Original pack",
            parent,
            &[91, 92],
            true,
        )
        .await
        .unwrap()
        .unwrap();
        grabbed_torrents::set_source_url(&db, gid, "https://indexer.example/release/123")
            .await
            .unwrap();
        // Deliberately stale offsets. The execution path must never use them.
        sqlx::query("INSERT INTO grabbed_torrent_series (grab_id,series_id,file_indices,episode_numbers,episode_offset) VALUES (?,?,'[0,1,2,3]','[91,92]',900)")
            .bind(gid).bind(parent).execute(&db).await.unwrap();
        let files = names
            .iter()
            .enumerate()
            .map(|(i, name)| ReplayFile {
                name: (*name).into(),
                series_id: if i < 2 { parent } else { sibling },
                anilist_id: if i < 2 { 9410 } else { 9411 },
                season: if i == 3 { 0 } else { 1 },
                first: if i == 1 { 2 } else { 1 },
                last: if i == 1 { 2 } else { 1 },
                title: if i == 3 {
                    "Sunny Day".into()
                } else {
                    String::new()
                },
                resolution: "1080p".into(),
                source: "BluRay".into(),
                release_group: "Group".into(),
            })
            .collect();
        let plan = ReplayPlan { version: 1, files };
        let bridge =
            serde_json::json!({"version":2,"replay_policy":"verified_files","replay":plan})
                .to_string();
        sqlx::query("UPDATE grabbed_torrents SET library_migration_bridge=? WHERE id=?")
            .bind(&bridge)
            .bind(gid)
            .execute(&db)
            .await
            .unwrap();
        let client = Arc::new(BatchClient::new(
            complete_torrent("replay-pack", &downloads),
            names.iter().rev().map(|n| complete_file(n)).collect(),
        ));
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, gid).await, "imported", "{mode}");
        for (relative, expected) in [
            ("Parent/Season 01/Parent - S01E01.mkv", "source 0"),
            ("Parent/Season 01/Parent - S01E02.mkv", "source 1"),
            ("Sibling/Season 01/Sibling - S01E01.mkv", "source 2"),
            ("Sibling/Specials/Sibling - S00E01.mkv", "source 3"),
        ] {
            assert_eq!(
                std::fs::read_to_string(media.join(relative)).unwrap(),
                expected
            );
        }
        let canonical = media.join("Parent/Season 01/Parent - S01E01.mkv");
        canonical
            .with_extension("nfo")
            .is_file()
            .then_some(())
            .expect("canonical NFO");
        std::fs::remove_file(&canonical).unwrap();
        if mode == "move" {
            for (i, name) in names.iter().enumerate() {
                std::fs::write(downloads.join(name), format!("source {i}")).unwrap();
            }
        }
        assert_eq!(
            grabbed_torrents::record_grab(
                &db,
                "replay-pack",
                "Changed display name",
                parent,
                &[1],
                false
            )
            .await
            .unwrap(),
            Some(gid)
        );
        let restarted = build_test_app_state(db.clone(), Some(client));
        post_processing::run_once(&restarted).await;
        assert_eq!(grab_state(&db, gid).await, "imported");
        assert_eq!(std::fs::read_to_string(&canonical).unwrap(), "source 0");
        assert_eq!(
            std::fs::read_to_string(media.join("Parent/Season 01/Parent - S01E02.mkv")).unwrap(),
            "source 1"
        );
        let claims: String =
            sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents WHERE id=?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(claims, "[91,92]");
        let offset: i32 =
            sqlx::query_scalar("SELECT episode_offset FROM grabbed_torrent_series WHERE grab_id=?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(offset, 900);
        assert_eq!(
            grabbed_torrents::find_imported_for_episode(&db, parent, 2)
                .await
                .unwrap()[0]
                .id,
            gid,
            "kept source ownership"
        );
        std::fs::remove_file(&canonical).unwrap();
        if mode == "move" {
            for (i, name) in names.iter().enumerate() {
                std::fs::write(downloads.join(name), format!("source {i}")).unwrap();
            }
        }
        let new_job = grabbed_torrents::source_replay::record(
            &db,
            "SABnzbd_nzo_new_job",
            "Changed title",
            parent,
            &[99],
            true,
            None,
            None,
            grabbed_torrents::source_replay::SourceRef {
                url: "https://indexer.example/release/123",
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(new_job.replayed);
        assert_ne!(new_job.id, gid);
        let new_client = Arc::new(BatchClient::new(
            complete_torrent("SABnzbd_nzo_new_job", &downloads),
            names.iter().map(|n| complete_file(n)).collect(),
        ));
        // The test app's torrent fallback can serve the SAB-shaped job id.
        let fresh = build_test_app_state(db.clone(), Some(new_client));
        post_processing::run_once(&fresh).await;
        assert_eq!(grab_state(&db, new_job.id).await, "imported", "{mode}");
        assert_eq!(std::fs::read_to_string(&canonical).unwrap(), "source 0");
        assert_eq!(
            std::fs::read_to_string(media.join("Parent/Season 01/Parent - S01E02.mkv")).unwrap(),
            "source 1"
        );
        assert_eq!(grab_state(&db, gid).await, "imported");
        let original_hash: String =
            sqlx::query_scalar("SELECT hash FROM grabbed_torrents WHERE id=?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(original_hash, "replay-pack");
    }
}

#[tokio::test]
async fn replay_with_unknown_file_or_changed_owner_is_held_without_library_writes() {
    use grabbed_torrents::migration_replay::{ReplayFile, ReplayPlan};
    let _guard = POST_PROC_TEST_SERIALIZER.lock().await;
    for changed_owner in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let media = temp.path().join("media");
        let downloads = temp.path().join("downloads");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(downloads.join("Show - 00.mkv"), b"original").unwrap();
        let db = in_memory_pool().await;
        seed_config(&db, &media).await;
        let sid = seed_series(&db, 9412, "Show").await;
        sqlx::query("UPDATE series SET episodes=1")
            .execute(&db)
            .await
            .unwrap();
        let gid =
            grabbed_torrents::record_grab(&db, "unknown-replay", "Show pack", sid, &[0], true)
                .await
                .unwrap()
                .unwrap();
        let plan = ReplayPlan {
            version: 1,
            files: vec![ReplayFile {
                name: "Show - 00.mkv".into(),
                series_id: sid,
                anilist_id: 9412,
                season: 1,
                first: 1,
                last: 1,
                title: String::new(),
                resolution: String::new(),
                source: String::new(),
                release_group: String::new(),
            }],
        };
        sqlx::query("UPDATE grabbed_torrents SET library_migration_bridge=? WHERE id=?")
            .bind(
                serde_json::json!({"version":2,"replay_policy":"verified_files","replay":plan})
                    .to_string(),
            )
            .bind(gid)
            .execute(&db)
            .await
            .unwrap();
        let mut files = vec![complete_file("Show - 00.mkv")];
        if changed_owner {
            sqlx::query("UPDATE series SET anilist_id=9413")
                .execute(&db)
                .await
                .unwrap();
        } else {
            files.push(complete_file("Show - 99.mkv"));
            std::fs::write(downloads.join("Show - 99.mkv"), b"new file").unwrap();
        }
        let client = Arc::new(BatchClient::new(
            complete_torrent("unknown-replay", &downloads),
            files,
        ));
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, gid).await, "pending");
        assert_eq!(std::fs::read_dir(media).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(downloads.join("Show - 00.mkv")).unwrap(),
            b"original"
        );
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
        sqlx::query("UPDATE config SET post_processing_enabled=0")
            .execute(&db)
            .await
            .unwrap();
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, gid).await, "pending");
    }
}

#[tokio::test]
async fn migration_replay_guard_keeps_source_and_library_unchanged_across_ticks() {
    let _guard = POST_PROC_TEST_SERIALIZER.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    let downloads = temp.path().join("downloads");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("Show - 01.mkv"), b"old source numbering").unwrap();
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let sid = seed_series(&db, 9407, "Show").await;
    let gid = grabbed_torrents::record_grab(&db, "migration-held", "Show 01", sid, &[1], false)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET library_migration_bridge=? WHERE id=?")
        .bind(r#"{"version":1,"replay_policy":"review_required"}"#)
        .bind(gid)
        .execute(&db)
        .await
        .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("migration-held", &downloads),
        vec![complete_file("Show - 01.mkv")],
    ));
    for _ in 0..2 {
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, gid).await, "pending");
        assert_eq!(
            std::fs::read(downloads.join("Show - 01.mkv")).unwrap(),
            b"old source numbering"
        );
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 0);
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
    }
}

/// Minimal mock: one canned complete torrent plus a canned file list.
/// Everything else is inert. `save_path` points at a real temp dir the
/// test populated, so `import_torrent` walks and moves real files.
struct BatchClient {
    torrent: DownloadItem,
    files: Vec<DownloadFile>,
    /// `delete` calls seen, so an upgrade test can assert the old
    /// torrent left the client (or, on a failed placement, did not).
    deleted: AtomicUsize,
}

impl BatchClient {
    fn new(torrent: DownloadItem, files: Vec<DownloadFile>) -> Self {
        Self {
            torrent,
            files,
            deleted: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl DownloadClient for BatchClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
        Ok(AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        Ok(SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![self.torrent.clone()])
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(self.files.clone())
    }
    async fn pause(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
        self.deleted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn set_file_wanted(
        &self,
        _hash: &str,
        _files: &[usize],
        _wanted: bool,
    ) -> Result<(), String> {
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

fn complete_torrent(hash: &str, save_path: &Path) -> DownloadItem {
    DownloadItem {
        hash: hash.to_string(),
        name: format!("torrent-{hash}"),
        size: 1000,
        progress: 1.0,
        dlspeed: 0,
        state: "seeding".to_string(),
        category: "anime".to_string(),
        eta: 0,
        save_path: save_path.to_string_lossy().into_owned(),
        content_path: String::new(),
        state_kind: DownloadItemState::Seeding,
        seeding_done: false,
    }
}

fn complete_file(name: &str) -> DownloadFile {
    DownloadFile {
        name: name.to_string(),
        size: 100,
        progress: 1.0,
        wanted: true,
    }
}

/// Fresh per-test temp tree: `<tmp>/ryokan-live-<tag>-<pid>/{downloads,media}`.
/// Any stale tree from a crashed prior run is cleared first.
fn temp_dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("ryokan-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let downloads = root.join("downloads");
    let media = root.join("media");
    std::fs::create_dir_all(&downloads).expect("create downloads dir");
    std::fs::create_dir_all(&media).expect("create media dir");
    (root, downloads, media)
}

async fn seed_config(db: &sqlx::SqlitePool, media_root: &Path) {
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root) \
         VALUES (1, 1, ?) \
         ON CONFLICT(id) DO UPDATE SET post_processing_enabled = 1, media_root = excluded.media_root",
    )
    .bind(media_root.to_string_lossy().as_ref())
    .execute(db)
    .await
    .expect("seed config row");
}

/// Recursively collect basenames of files with the given extension.
fn collect_files(root: &Path, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

async fn grab_state(db: &sqlx::SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("fetch grab state")
}

/// A pack fills E02 but must leave the Tdarr result and its companions at E01.
/// Exercise all file-operation modes, including a prior folder template.
#[tokio::test]
async fn protected_pack_keeps_files_and_records_in_every_mode() {
    use crate::models::{episode_tags, series};
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    for mode in ["hardlink", "copy", "move"] {
        let (root, downloads, media) = temp_dirs(&format!("protected-{mode}"));
        let old_dir = media.join("Show/Old folder");
        std::fs::create_dir_all(&old_dir).unwrap();
        for (name, bytes) in [
            ("Show - S01E01.mkv", "Tdarr video"),
            ("Show - S01E01.nfo", "original metadata"),
            ("Show - S01E01.en.srt", "original subtitles"),
        ] {
            std::fs::write(old_dir.join(name), bytes).unwrap();
        }
        for ep in [1, 2] {
            std::fs::write(
                downloads.join(format!("Show - {ep:02}.mkv")),
                format!("incoming {ep}"),
            )
            .unwrap();
        }
        let db = in_memory_pool().await;
        seed_config(&db, &media).await;
        sqlx::query("UPDATE config SET post_processing_mode = ?")
            .bind(mode)
            .execute(&db)
            .await
            .unwrap();
        let sid = seed_series(&db, 9301, "Show").await;
        series::update_allow_upgrades(&db, sid, false)
            .await
            .unwrap();
        let old = grabbed_torrents::record_grab(&db, "old-protected", "Old WEB", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        grabbed_torrents::mark_imported(&db, old).await.unwrap();
        let quality =
            crate::services::source::classify_release_sync("[Old] Show - 01 WEB 1080p", None);
        let old_history =
            episode_tags::record_grab(&db, sid, 1, &quality, "Old WEB", "Old", 10, false)
                .await
                .unwrap();
        episode_tags::mark_completed(&db, sid, &[1]).await.unwrap();
        episode_tags::mark_grab_history_completed(&db, sid, 1, "Show - S01E01.mkv", 10)
            .await
            .unwrap();
        let before = episode_tags::get_for_series(&db, sid).await.unwrap()[&1].clone();
        let gid =
            grabbed_torrents::record_grab(&db, "protected-pack", "Show 01-02", sid, &[1, 2], true)
                .await
                .unwrap()
                .unwrap();
        let incoming = crate::services::source::classify_release_sync("[New] Show BD 2160p", None);
        for ep in [1, 2] {
            episode_tags::record_grab(&db, sid, ep, &incoming, "Show 01-02", "New", 200, true)
                .await
                .unwrap();
        }
        assert_eq!(
            episode_tags::get_for_series(&db, sid).await.unwrap()[&1].release_title,
            "Old WEB",
            "acquisition preserves the completed record"
        );
        let client = Arc::new(BatchClient::new(
            complete_torrent("protected-pack", &downloads),
            vec![
                complete_file("Show - 01.mkv"),
                complete_file("Show - 02.mkv"),
            ],
        ));
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, gid).await, "imported");
        let import_mode: String =
            sqlx::query_scalar("SELECT import_mode FROM grabbed_torrents WHERE id = ?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(import_mode, mode);
        for (name, bytes) in [
            ("Show - S01E01.mkv", "Tdarr video"),
            ("Show - S01E01.nfo", "original metadata"),
            ("Show - S01E01.en.srt", "original subtitles"),
        ] {
            assert_eq!(std::fs::read_to_string(old_dir.join(name)).unwrap(), bytes);
        }
        assert_eq!(
            collect_files(&media, "mkv").len(),
            2,
            "no duplicate E01 under the current template"
        );
        assert_eq!(
            std::fs::read_to_string(media.join("Show/Season 01/Show - S01E02.mkv")).unwrap(),
            "incoming 2"
        );
        let tags = episode_tags::get_for_series(&db, sid).await.unwrap();
        assert_eq!(tags[&1].release_title, before.release_title);
        assert_eq!(tags[&1].quality_tag, before.quality_tag);
        assert_eq!(
            tags[&1].classification_evidence,
            before.classification_evidence
        );
        assert_eq!(tags[&1].state, "completed");
        assert_eq!(tags[&2].state, "completed");
        let history_state: String =
            sqlx::query_scalar("SELECT state FROM episode_grab_history WHERE id = ?")
                .bind(old_history)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(history_state, "completed");
        let skipped: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM episode_grab_history WHERE series_id = ? AND episode_number = 1 AND release_title = 'Show 01-02' AND state = 'skipped'").bind(sid).fetch_one(&db).await.unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(
            grabbed_torrents::find_imported_for_episode(&db, sid, 1)
                .await
                .unwrap()
                .iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            vec![old]
        );
        assert_eq!(
            grabbed_torrents::find_imported_for_episode(&db, sid, 2)
                .await
                .unwrap()[0]
                .id,
            gid
        );
        assert!(
            grabbed_torrents::list_imported_in_client(&db)
                .await
                .unwrap()
                .iter()
                .any(|g| g.id == gid),
            "all episodes are present, so normal cleanup can finish"
        );
        assert_eq!(
            grabbed_torrents::get_imported_source_paths(&db, gid)
                .await
                .len(),
            1
        );
        assert!(
            downloads.join("Show - 01.mkv").exists(),
            "the import does not move a protected file; client removal is disabled in this fixture"
        );
        assert_eq!(grab_state(&db, old).await, "imported");
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
        // A new app state and another tick do not repeat a handled import.
        let restarted = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&restarted).await;
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read_to_string(old_dir.join("Show - S01E01.mkv")).unwrap(),
            "Tdarr video"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn protected_overlap_keeps_the_whole_video_and_does_not_claim_missing_slots() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    for (existing, incoming) in [
        ("Show - S01E01.mkv", "Show - 01-02.mkv"),
        ("Show - S01E01-E02.mkv", "Show - 01.mkv"),
        ("Show - S01E01.strm", "Show - 01.mkv"),
    ] {
        let (root, downloads, media) = temp_dirs("protected-range");
        let season = media.join("Show/Season 01");
        std::fs::create_dir_all(&season).unwrap();
        std::fs::write(season.join(existing), "keep existing").unwrap();
        std::fs::write(downloads.join(incoming), "keep source").unwrap();
        let db = in_memory_pool().await;
        seed_config(&db, &media).await;
        let sid = seed_series(&db, 9302, "Show").await;
        crate::models::series::update_allow_upgrades(&db, sid, false)
            .await
            .unwrap();
        let gid =
            grabbed_torrents::record_grab(&db, "protected-range", "Show 01-02", sid, &[1, 2], true)
                .await
                .unwrap()
                .unwrap();
        let quality = crate::services::source::ClassificationResult::unknown();
        for ep in [1, 2] {
            crate::models::episode_tags::record_grab(
                &db,
                sid,
                ep,
                &quality,
                "Show 01-02",
                "",
                0,
                true,
            )
            .await
            .unwrap();
        }
        let client = Arc::new(BatchClient::new(
            complete_torrent("protected-range", &downloads),
            vec![complete_file(incoming)],
        ));
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&state).await;
        assert_eq!(
            grab_state(&db, gid).await,
            "imported",
            "an all-protected pack is handled, not stalled or blocklisted"
        );
        assert_eq!(
            std::fs::read_to_string(season.join(existing)).unwrap(),
            "keep existing"
        );
        assert_eq!(
            std::fs::read_to_string(downloads.join(incoming)).unwrap(),
            "keep source"
        );
        assert!(
            grabbed_torrents::find_imported_for_episode(&db, sid, 1)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            grabbed_torrents::find_imported_for_episode(&db, sid, 2)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            grabbed_torrents::list_imported_in_client(&db)
                .await
                .unwrap()
                .is_empty(),
            incoming == "Show - 01-02.mkv"
        );
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn explicit_episode_request_replaces_only_the_requested_episode() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    for (explicit, incoming) in [
        (false, "Show - 01.mkv"),
        (true, "Show - 01.mkv"),
        (true, "Show - 01-02.mkv"),
    ] {
        let (root, downloads, media) = temp_dirs("explicit-replacement");
        let season = media.join("Show/Old folder");
        std::fs::create_dir_all(&season).unwrap();
        for ep in [1, 2] {
            std::fs::write(
                season.join(format!("Show - S01E{ep:02}.mkv")),
                format!("old {ep}"),
            )
            .unwrap();
        }
        std::fs::write(downloads.join(incoming), "replacement").unwrap();
        let db = in_memory_pool().await;
        seed_config(&db, &media).await;
        let sid = seed_series(&db, 9303, "Show").await;
        crate::models::series::update_allow_upgrades(&db, sid, false)
            .await
            .unwrap();
        let old_quality =
            crate::services::source::classify_release_sync("[Old] Show WEB 720p", None);
        let old_history = crate::models::episode_tags::record_grab(
            &db,
            sid,
            1,
            &old_quality,
            "Old release",
            "Old",
            10,
            false,
        )
        .await
        .unwrap();
        crate::models::episode_tags::mark_completed(&db, sid, &[1])
            .await
            .unwrap();
        crate::models::episode_tags::mark_grab_history_completed(
            &db,
            sid,
            1,
            "Show - S01E01.mkv",
            10,
        )
        .await
        .unwrap();
        let gid = if explicit {
            grabbed_torrents::record_manual_episode_grab(&db, "explicit", "Show - 01", sid, &[1], 1)
                .await
        } else {
            grabbed_torrents::record_grab(&db, "explicit", "Show - 01", sid, &[1], false).await
        }
        .unwrap()
        .unwrap();
        let incoming_quality =
            crate::services::source::classify_release_sync("[New] Show BD 1080p", None);
        crate::models::episode_tags::record_grab(
            &db,
            sid,
            1,
            &incoming_quality,
            "Show - 01",
            "New",
            20,
            false,
        )
        .await
        .unwrap();
        let client = Arc::new(BatchClient::new(
            complete_torrent("explicit", &downloads),
            vec![complete_file(incoming)],
        ));
        let state = build_test_app_state(db.clone(), Some(client));
        post_processing::run_once(&state).await;
        let replaces = explicit && incoming == "Show - 01.mkv";
        assert_eq!(season.join("Show - S01E01.mkv").exists(), !replaces);
        assert_eq!(
            std::fs::read_to_string(season.join("Show - S01E02.mkv")).unwrap(),
            "old 2"
        );
        if replaces {
            assert_eq!(
                std::fs::read_to_string(media.join("Show/Season 01/Show - S01E01.mkv")).unwrap(),
                "replacement"
            );
            let tags = crate::models::episode_tags::get_for_series(&db, sid)
                .await
                .unwrap();
            assert_eq!(tags[&1].release_title, "Show - 01");
            assert_eq!(tags[&1].release_group, "New");
            assert_eq!(tags[&1].state, "completed");
        }
        let old_history_state: String =
            sqlx::query_scalar("SELECT state FROM episode_grab_history WHERE id = ?")
                .bind(old_history)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            old_history_state,
            if replaces { "replaced" } else { "completed" }
        );
        assert_eq!(grab_state(&db, gid).await, "imported");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn replacement_permission_does_not_spread_to_a_routed_sibling() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("protected-sibling");
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let parent = seed_series(&db, 9305, "Parent").await;
    let sibling = seed_series(&db, 9306, "Sibling").await;
    let names = vec!["Parent - 01.mkv".to_owned(), "Sibling - 01.mkv".to_owned()];
    for (sid, title, name) in [
        (parent, "Parent", &names[0]),
        (sibling, "Sibling", &names[1]),
    ] {
        crate::models::series::update_allow_upgrades(&db, sid, false)
            .await
            .unwrap();
        let dir = media.join(title).join("Season 01");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{title} - S01E01.mkv")), "original").unwrap();
        std::fs::write(downloads.join(name), "incoming").unwrap();
    }
    let gid = grabbed_torrents::record_manual_episode_grab(
        &db,
        "routed-protection",
        "Parent complete",
        parent,
        &[1],
        1,
    )
    .await
    .unwrap()
    .unwrap();
    let routes = [parent, sibling]
        .iter()
        .enumerate()
        .map(|(index, sid)| grabbed_torrents::GrabSeriesRoute {
            grab_id: gid,
            series_id: *sid,
            file_indices: vec![index],
            episode_numbers: vec![1],
            matched_subtitle: "fixture".into(),
            episode_offset: 0,
        })
        .collect::<Vec<_>>();
    grabbed_torrents::record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("routed-protection", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));
    post_processing::run_once(&state).await;
    assert_eq!(grab_state(&db, gid).await, "imported");
    assert_eq!(
        std::fs::read_to_string(media.join("Parent/Season 01/Parent - S01E01.mkv")).unwrap(),
        "incoming"
    );
    assert_eq!(
        std::fs::read_to_string(media.join("Sibling/Season 01/Sibling - S01E01.mkv")).unwrap(),
        "original"
    );
    assert_eq!(
        grabbed_torrents::find_imported_for_episode(&db, parent, 1)
            .await
            .unwrap()[0]
            .id,
        gid
    );
    assert!(
        grabbed_torrents::find_imported_for_episode(&db, sibling, 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(downloads.join(&names[1]).exists());
    assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn unreadable_library_scan_holds_import_without_removing_the_download() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("protection-scan-error");
    let season = media.join("Show/Season 01");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("Show - S01E01.mkv"), "old").unwrap();
    // A dangling link models an incomplete mount even when tests run as root.
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("unavailable"), media.join("Show/unavailable")).unwrap();
    std::fs::write(downloads.join("Show - 01.mkv"), "new").unwrap();
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let sid = seed_series(&db, 9304, "Show").await;
    let gid = grabbed_torrents::record_grab(&db, "scan-error", "Show - 01", sid, &[1], false)
        .await
        .unwrap()
        .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("scan-error", &downloads),
        vec![complete_file("Show - 01.mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));
    post_processing::run_once(&state).await;
    #[cfg(unix)]
    {
        assert_eq!(grab_state(&db, gid).await, "pending");
        assert_eq!(
            std::fs::read_to_string(season.join("Show - S01E01.mkv")).unwrap(),
            "old"
        );
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn saved_zero_based_route_preserves_video_title_nfo_and_completed_slots() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("canonical-zero");
    let names = [
        "Mapping Test - 00.mkv",
        "Mapping Test - 01.mkv",
        "Mapping Test - 02.mkv",
    ];
    let titles = ["Prologue", "First Regular", "Final Regular"];
    for (name, title) in names.iter().zip(titles) {
        std::fs::write(downloads.join(name), title.as_bytes()).unwrap();
    }
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    sqlx::query("UPDATE config SET episode_file_format = '{series.title} - S{season.number:00}E{episode.number:00} - {episode.title}{ext}' WHERE id = 1")
        .execute(&db).await.unwrap();
    let series_id = seed_series(&db, 9090, "Mapping Test").await;
    sqlx::query("UPDATE series SET episodes = 3, cumulative_prior_episodes = 99 WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    for (index, title) in titles.iter().enumerate() {
        sqlx::query("INSERT INTO series_episode_metadata (series_id, episode_number, title, title_english, aired) VALUES (?, ?, ?, ?, '2020-01-01')")
            .bind(series_id).bind(index as i32 + 1).bind(title).bind(title).execute(&db).await.unwrap();
    }
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "mapped-zero",
        "Mapping Test Complete",
        series_id,
        &[1, 2, 3],
        true,
    )
    .await
    .unwrap()
    .unwrap();
    // This fixture supplies a verified numbering choice. It does not infer
    // that choice from the desired slots or from the presence of episode 0.
    grabbed_torrents::record_grab_series_routes_with_files(
        &db,
        &[grabbed_torrents::GrabSeriesRoute {
            grab_id,
            series_id,
            file_indices: vec![0, 1, 2],
            episode_numbers: vec![1, 2, 3],
            matched_subtitle: "fixture: verified zero-based source".into(),
            episode_offset: -1,
        }],
        &names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("mapped-zero", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));
    post_processing::run_once(&state).await;
    assert_eq!(grab_state(&db, grab_id).await, "imported");
    let season_dir = media.join("Mapping Test").join("Season 01");
    for (index, title) in titles.iter().enumerate() {
        let stem = format!("Mapping Test - S01E{:02} - {title}", index + 1);
        assert_eq!(
            std::fs::read(season_dir.join(format!("{stem}.mkv"))).unwrap(),
            title.as_bytes()
        );
        let nfo = std::fs::read_to_string(season_dir.join(format!("{stem}.nfo"))).unwrap();
        assert!(
            nfo.contains(&format!("<episode>{}</episode>", index + 1)),
            "{nfo}"
        );
        assert!(nfo.contains(&format!("<title>{title}</title>")), "{nfo}");
    }
    let tags = crate::models::episode_tags::get_for_series(&db, series_id)
        .await
        .unwrap();
    assert_eq!(tags.len(), 3);
    for number in 1..=3 {
        assert_eq!(tags[&number].state, "completed");
    }
    assert_eq!(
        collect_files(&downloads, "mkv").len(),
        3,
        "source files stay intact"
    );
    // Recreate application state and reactivate the same grab. The saved
    // route, not a changed cumulative offset, must still determine identity.
    grabbed_torrents::record_grab(
        &db,
        "mapped-zero",
        "Mapping Test Complete",
        series_id,
        &[1, 2, 3],
        true,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE series SET cumulative_prior_episodes = 500 WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    let restarted_client = Arc::new(BatchClient::new(
        complete_torrent("mapped-zero", &downloads),
        names.iter().rev().map(|n| complete_file(n)).collect(),
    ));
    let restarted = build_test_app_state(db.clone(), Some(restarted_client));
    post_processing::run_once(&restarted).await;
    assert_eq!(grab_state(&db, grab_id).await, "imported");
    assert_eq!(collect_files(&media, "mkv").len(), 3);
    for (index, title) in titles.iter().enumerate() {
        assert_eq!(
            std::fs::read(
                season_dir.join(format!("Mapping Test - S01E{:02} - {title}.mkv", index + 1))
            )
            .unwrap(),
            title.as_bytes()
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn saved_pack_mapping_survives_file_reordering_between_entries() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("saved-pack-reorder");
    let names = ["Mapping Test - 00.mkv", "Mapping Test - 13.mkv"];
    let contents = ["the prologue", "the second entry"];
    for (name, content) in names.iter().zip(contents) {
        std::fs::write(downloads.join(name), content).unwrap();
    }
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let first = seed_series(&db, 9090, "Mapping Test").await;
    let second = seed_series(&db, 9091, "Mapping Test Second Part").await;
    sqlx::query("UPDATE series SET episodes = 13, cumulative_prior_episodes = 99")
        .execute(&db)
        .await
        .unwrap();
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "mapped-pack",
        "Mapping Test Complete",
        first,
        &[1],
        true,
    )
    .await
    .unwrap()
    .unwrap();
    let routes: Vec<_> = [(first, -1), (second, 12)]
        .into_iter()
        .enumerate()
        .map(
            |(index, (series_id, episode_offset))| grabbed_torrents::GrabSeriesRoute {
                grab_id,
                series_id,
                file_indices: vec![index],
                episode_numbers: vec![1],
                episode_offset,
                matched_subtitle: "fixture: selected source scheme".into(),
            },
        )
        .collect();
    grabbed_torrents::record_grab_series_routes_with_files(
        &db,
        &routes,
        &names.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("mapped-pack", &downloads),
        names.iter().rev().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));
    post_processing::run_once(&state).await;
    assert_eq!(grab_state(&db, grab_id).await, "imported");
    for (title, content, sid) in [
        ("Mapping Test", contents[0], first),
        ("Mapping Test Second Part", contents[1], second),
    ] {
        let folder = media.join(title).join("Season 01");
        let files = collect_files(&folder, "mkv");
        assert_eq!(files.len(), 1, "{title}: {files:?}");
        assert!(files[0].contains("S01E01"), "{}", files[0]);
        assert_eq!(
            std::fs::read_to_string(folder.join(&files[0])).unwrap(),
            content
        );
        let nfo = std::fs::read_to_string(folder.join(&files[0]).with_extension("nfo")).unwrap();
        assert!(nfo.contains("<episode>1</episode>"), "{nfo}");
        let tags = crate::models::episode_tags::get_for_series(&db, sid)
            .await
            .unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[&1].state, "completed");
    }
    assert_eq!(collect_files(&downloads, "mkv").len(), 2);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn release_choice_survives_restart_and_refuses_a_different_source_episode() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("release-choice");
    let name = "Mapping Test S02E06.mkv";
    std::fs::write(downloads.join(name), "the actual sixth episode").unwrap();
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9090, "Mapping Test").await;
    sqlx::query("UPDATE series SET format = 'TV', episodes = 12, cumulative_prior_episodes = 1 WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO series_episode_metadata (series_id, episode_number, title, title_english, aired) VALUES (?, 6, 'Chosen Six', 'Chosen Six', '')")
        .bind(series_id).execute(&db).await.unwrap();
    let mapping = crate::services::release_numbering::NumberingContext {
        anilist_id: 9090,
        tv_series: true,
        episode_count: Some(12),
        cumulative_offset: 1,
        expected_season: 2,
        provider_plan: None,
    }
    .choose("Mapping Test S02E06", false)
    .unwrap();
    let id = grabbed_torrents::record_grab_with_release_mapping(
        &db,
        "chosen-release",
        "Mapping Test S02E06",
        series_id,
        &[5],
        false,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    for offset in [99, 999] {
        sqlx::query("UPDATE series SET cumulative_prior_episodes = ? WHERE id = ?")
            .bind(offset)
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let client = Arc::new(BatchClient::new(
            complete_torrent("chosen-release", &downloads),
            vec![complete_file(name)],
        ));
        let state = build_test_app_state(db.clone(), Some(client));
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, id).await, "imported");
        let videos = collect_files(&media, "mkv");
        assert_eq!(videos.len(), 1);
        assert!(videos[0].contains("E06"), "{:?}", videos);
        let tags = crate::models::episode_tags::get_for_series(&db, series_id)
            .await
            .unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[&6].state, "completed");
        // A repeat grab uses the original decision even when the caller's
        // requested slot and the current series offset disagree with it.
        grabbed_torrents::record_grab(
            &db,
            "chosen-release",
            "Mapping Test S02E06",
            series_id,
            &[5],
            false,
        )
        .await
        .unwrap();
    }
    let wrong_name = "Mapping Test S02E07.mkv";
    std::fs::write(downloads.join(wrong_name), "a different video").unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("chosen-release", &downloads),
        vec![complete_file(wrong_name)],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));
    post_processing::run_once(&state).await;
    assert_eq!(grab_state(&db, id).await, "pending");
    assert_eq!(client.deleted.load(Ordering::SeqCst), 0);
    let videos = collect_files(&media, "mkv");
    assert_eq!(videos.len(), 1);
    assert!(videos[0].contains("E06"));
    // Locate the actual path independently of the configured season folder.
    let mut dirs = vec![media.clone()];
    let mut verified = false;
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "mkv") {
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "the actual sixth episode"
                );
                let nfo = std::fs::read_to_string(path.with_extension("nfo")).unwrap();
                assert!(nfo.contains("<episode>6</episode>"), "{nfo}");
                assert!(nfo.contains("<title>Chosen Six</title>"), "{nfo}");
                verified = true;
            }
        }
    }
    assert!(verified);
    assert_eq!(
        std::fs::read_to_string(downloads.join(wrong_name)).unwrap(),
        "a different video"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn invalid_saved_mapping_stops_the_whole_import_before_file_changes() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    for case in [
        "corrupt",
        "new-file",
        "missing-file",
        "new-entry",
        "shorter-entry",
    ] {
        let (root, downloads, media) = temp_dirs(&format!("saved-map-{case}"));
        let names = ["Mapping Test - 00.mkv", "Mapping Test - 01.mkv"];
        for (index, name) in names.iter().enumerate() {
            std::fs::write(downloads.join(name), format!("source-{index}")).unwrap();
        }
        let db = in_memory_pool().await;
        seed_config(&db, &media).await;
        let series_id = seed_series(&db, 9090, "Mapping Test").await;
        sqlx::query("UPDATE series SET episodes = 2 WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let grab_id = grabbed_torrents::record_grab(
            &db,
            "invalid-map",
            "Mapping Test Complete",
            series_id,
            &[1, 2],
            true,
        )
        .await
        .unwrap()
        .unwrap();
        grabbed_torrents::record_grab_series_routes_with_files(
            &db,
            &[grabbed_torrents::GrabSeriesRoute {
                grab_id,
                series_id,
                file_indices: vec![0, 1],
                episode_numbers: vec![1, 2],
                episode_offset: -1,
                matched_subtitle: "fixture: selected source scheme".into(),
            }],
            &names.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        let mut files: Vec<_> = names.iter().map(|n| complete_file(n)).collect();
        match case {
            "corrupt" => {
                sqlx::query(
                    "UPDATE grabbed_torrent_numbering SET snapshot = '{' WHERE grab_id = ?",
                )
                .bind(grab_id)
                .execute(&db)
                .await
                .unwrap();
            }
            "new-file" => {
                let extra = "Mapping Test - 02.mkv";
                std::fs::write(downloads.join(extra), "unmapped source").unwrap();
                files.push(complete_file(extra));
            }
            "missing-file" => {
                files.pop();
            }
            "new-entry" => {
                sqlx::query("UPDATE series SET anilist_id = 9092 WHERE id = ?")
                    .bind(series_id)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            "shorter-entry" => {
                sqlx::query("UPDATE series SET episodes = 1 WHERE id = ?")
                    .bind(series_id)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let client = Arc::new(BatchClient::new(
            complete_torrent("invalid-map", &downloads),
            files,
        ));
        sqlx::query("UPDATE config SET import_stall_hours = 1 WHERE id = 1")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE grabbed_torrents SET completed_seen_at = datetime('now', '-25 hours') WHERE id = ?")
            .bind(grab_id).execute(&db).await.unwrap();
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        post_processing::run_once(&state).await;
        assert_eq!(grab_state(&db, grab_id).await, "pending", "{case}");
        assert_eq!(client.deleted.load(Ordering::SeqCst), 0, "{case}");
        assert!(collect_files(&media, "mkv").is_empty(), "{case}");
        assert!(collect_files(&media, "nfo").is_empty(), "{case}");
        assert!(
            crate::models::episode_tags::get_for_series(&db, series_id)
                .await
                .unwrap()
                .is_empty(),
            "{case}"
        );
        for (index, name) in names.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(downloads.join(name)).unwrap(),
                format!("source-{index}")
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn live_batch_import_skips_extras_and_imports_episodes() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("extras");

    // Real files on disk, shapes lifted from the scraped Nyaa corpus:
    // three dash-form episodes, one dot-form episode whose `H.264`
    // token must not mis-parse as E264, and two NC extras that parse
    // to `None` by design.
    let names = [
        "[Moozzi2] Anne of Green Gables - 01 (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables - 02 (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables - 03 (BD 1440x1080 x.265 Flac).mkv",
        "Anne.of.Green.Gables.04.BD.1080p.H.264.mkv",
        "[Moozzi2] Anne of Green Gables [SP01] NCOP (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables [SP02] NCED (BD 1440x1080 x.265 Flac).mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9001, "Anne of Green Gables").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-extras",
        "[Moozzi2] Anne of Green Gables (TV + SP)",
        series_id,
        &[1, 2, 3, 4],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-extras", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    let imported = collect_files(&media, "mkv");
    assert_eq!(
        imported,
        vec![
            "Anne of Green Gables - S01E01.mkv".to_string(),
            "Anne of Green Gables - S01E02.mkv".to_string(),
            "Anne of Green Gables - S01E03.mkv".to_string(),
            "Anne of Green Gables - S01E04.mkv".to_string(),
        ],
        "episodes 1-4 import (dot-form E04 must not become E264); NC extras stay out"
    );
    // Hardlink mode: sources remain for continued seeding.
    assert_eq!(collect_files(&downloads, "mkv").len(), 6);
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_prefers_highest_version_in_slot() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("version");

    // `05` and `05v2` both resolve to episode 5 (issue #204): the v2
    // lands, the v1 is skipped, and the unambiguous E06 imports.
    let names = [
        ("Show - 05 (720p).mkv", b"v1 bytes".as_slice()),
        ("Show - 05v2 (1080p).mkv", b"v2 bytes".as_slice()),
        ("Show - 06 (1080p).mkv", b"e6 bytes".as_slice()),
    ];
    for (name, bytes) in &names {
        std::fs::write(downloads.join(name), bytes).expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9002, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-version",
        "Show 05-06 pack",
        series_id,
        &[5, 6],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-version", &downloads),
        names.iter().map(|(n, _)| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec![
            "Show - S01E05.mkv".to_string(),
            "Show - S01E06.mkv".to_string()
        ],
        "v2 wins the E05 slot; E06 imports alongside"
    );
    let e05 = std::fs::read(
        media
            .join("Show")
            .join("Season 01")
            .join("Show - S01E05.mkv"),
    )
    .expect("read E05");
    assert_eq!(
        e05, b"v2 bytes",
        "the higher version is the one that landed"
    );
    assert_eq!(
        collect_files(&downloads, "mkv").len(),
        3,
        "sources untouched (hardlink mode); the skipped v1 stays put"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_fails_closed_on_slot_collision() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("collision");

    // Two unversioned files resolve to episode 5 — a true duplicate
    // (or a mis-parse), so the whole grab must fail before ANY
    // mutation, including the unambiguous E06.
    let names = [
        "Show - 05 (720p).mkv",
        "Show - 05 (1080p).mkv",
        "Show - 06 (1080p).mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9005, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-collision",
        "Show 05-06 pack",
        series_id,
        &[5, 6],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-collision", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        Vec::<String>::new(),
        "duplicate destination must abort before any file lands"
    );
    assert_eq!(
        collect_files(&downloads, "mkv").len(),
        3,
        "sources untouched"
    );
    assert_eq!(grab_state(&db, grab_id).await, "failed");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_skips_corpus_pack_extras() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("corpus-extras");

    // Issue #203: the Erai-raws 86 pack's `SP` and `07_5` recap and
    // Moozzi2's `PV` files used to collide with E02 / E07 / E01 and
    // fail the whole pack closed. Now they are skipped and logged.
    let names = [
        "[Erai-raws] 86 Eighty-Six Part 2 - 02 [480p][Multiple Subtitle][2E7DEB0E].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - 07 [480p][Multiple Subtitle][E9DFD7E8].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - 07_5 [480p][Multiple Subtitle][3CC7C577].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - SP [480p][Multiple Subtitle][FDBE49E5].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - Character PV 1 [480p][ABCDEF01].mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9006, "86 Eighty-Six Part 2").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-corpus-extras",
        "[Erai-raws] 86 Eighty-Six Part 2 - 01 ~ 12 [480p][BATCH]",
        series_id,
        &[2, 7],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-corpus-extras", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec![
            "86 Eighty-Six Part 2 - S01E02.mkv".to_string(),
            "86 Eighty-Six Part 2 - S01E07.mkv".to_string(),
        ],
        "episodes import; SP / recap / PV extras stay out"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

/// Library state for an upgrade test: `<media>/Show/Season 01/Show -
/// S01E05.mkv` holding `old bytes`, backed by an imported grab row for
/// episode 5 so `find_imported_for_episode` sees a real predecessor.
async fn seed_imported_e05(db: &sqlx::SqlitePool, media: &Path, series_id: i64) -> (PathBuf, i64) {
    let season_dir = media.join("Show").join("Season 01");
    std::fs::create_dir_all(&season_dir).expect("create season dir");
    let dest = season_dir.join("Show - S01E05.mkv");
    std::fs::write(&dest, b"old bytes").expect("write old file");
    seed_grabbed_torrent(db, series_id, "oldhash-e05", "Show - 05 (720p)", &[5]).await;
    // Looked up by hash: `last_insert_rowid()` is per-connection and the
    // pool may answer the follow-up SELECT from a different one.
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE hash = 'oldhash-e05'")
        .execute(db)
        .await
        .expect("mark old grab imported");
    let old_grab_id: i64 =
        sqlx::query_scalar("SELECT id FROM grabbed_torrents WHERE hash = 'oldhash-e05'")
            .fetch_one(db)
            .await
            .expect("old grab id");
    (dest, old_grab_id)
}

#[tokio::test]
async fn live_upgrade_keeps_old_file_when_placement_fails() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("upgrade-fail");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9007, "Show").await;
    let (dest, old_grab_id) = seed_imported_e05(&db, &media, series_id).await;

    // Block the staging path with a directory so the placement fails
    // (hardlink returns EEXIST, the copy fallback EISDIR) before the
    // old file is touched. Issue #202: the pre-fix order had already
    // deleted the old file and the old torrent by this point.
    std::fs::create_dir_all(post_processing::staging_path(&dest)).expect("block staging path");

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("write source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-upgrade-fail",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-upgrade-fail", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        std::fs::read(&dest).expect("old file still readable"),
        b"old bytes",
        "a failed placement must leave the old file exactly as it was"
    );
    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["Show - S01E05.mkv".to_string()]
    );
    assert_eq!(
        grab_state(&db, old_grab_id).await,
        "imported",
        "the old grab is not marked replaced"
    );
    assert_eq!(
        client.deleted.load(Ordering::SeqCst),
        0,
        "the old torrent stays in the client"
    );
    assert_eq!(grab_state(&db, grab_id).await, "failed");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_upgrade_swaps_new_file_in_after_placement() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("upgrade-ok");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9008, "Show").await;
    let (dest, old_grab_id) = seed_imported_e05(&db, &media, series_id).await;

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("write source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-upgrade-ok",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-upgrade-ok", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        std::fs::read(&dest).expect("new file readable"),
        b"new bytes",
        "the upgrade replaced the old file in place"
    );
    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["Show - S01E05.mkv".to_string()]
    );
    assert!(
        !post_processing::staging_path(&dest).exists(),
        "no staged file is left behind after the swap"
    );
    assert_eq!(grab_state(&db, old_grab_id).await, "replaced");
    assert_eq!(
        client.deleted.load(Ordering::SeqCst),
        1,
        "the old torrent leaves the client after the swap"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_upgrade_retires_a_strm_stub() {
    // Issue #287: the scan counts a `.strm` as the episode's file, so an
    // upgrade retires it like a video rather than leaving two files on
    // one episode.
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("upgrade-strm");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9010, "Show").await;
    let (old_video, old_grab_id) = seed_imported_e05(&db, &media, series_id).await;
    let stub = old_video.with_extension("strm");
    std::fs::rename(&old_video, &stub).expect("swap the video for a stub");
    std::fs::write(&stub, b"https://example.com/show/5").expect("write stub");

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("write source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-upgrade-strm",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-upgrade-strm", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        std::fs::read(&old_video).expect("new file readable"),
        b"new bytes"
    );
    assert!(
        collect_files(&media, "strm").is_empty(),
        "the stub was retired by the upgrade"
    );
    assert_eq!(grab_state(&db, old_grab_id).await, "replaced");
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_waits_for_incomplete_wanted_video() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("notready");

    let names = ["Show - 01 (1080p).mkv", "Show - 02 (1080p).mkv"];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9003, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-notready",
        "Show pack",
        series_id,
        &[1, 2],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    // E02 is wanted but only half done — the whole batch must wait.
    let mut files: Vec<DownloadFile> = names.iter().map(|n| complete_file(n)).collect();
    files[1].progress = 0.5;

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-notready", &downloads),
        files,
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        Vec::<String>::new(),
        "no partial import while a wanted video is incomplete"
    );
    assert_eq!(grab_state(&db, grab_id).await, "pending");

    let _ = std::fs::remove_dir_all(&root);
}

// ── Naming templates (issue #124) ───────────────────────────────────

#[tokio::test]
async fn live_import_names_files_from_the_templates() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("naming");

    let name = "[SubsPlease] Show - 05 (1080p WEB-DL) [ABCD1234].mkv";
    std::fs::write(downloads.join(name), b"bytes").expect("write source");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    sqlx::query(
        "UPDATE config SET season_folder_format = 'S{season.number:00}', \
         episode_file_format = '[{group}] {series.title} - {episode.number:00} [{quality.full}]{ext}' \
         WHERE id = 1",
    )
    .execute(&db)
    .await
    .expect("set templates");
    let series_id = seed_series(&db, 9009, "Show").await;
    let grab_id =
        grabbed_torrents::record_grab(&db, "livehash-naming", name, series_id, &[5], false)
            .await
            .unwrap()
            .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-naming", &downloads),
        vec![complete_file(name)],
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    let season_dir = media.join("Show").join("S01");
    assert!(season_dir.is_dir(), "season folder follows its template");
    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["[SubsPlease] Show - 05 [1080p WEB-DL].mkv".to_string()],
        "group and quality tokens read the filename when no tag row exists"
    );
    assert!(
        season_dir
            .join("[SubsPlease] Show - 05 [1080p WEB-DL].nfo")
            .is_file(),
        "the NFO shares the rendered stem"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_upgrade_finds_the_old_file_named_under_another_template() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("naming-upgrade");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9010, "Show").await;
    // The library holds E05 under an older, dash-style template. The
    // default template now names it `Show - S01E05.mkv`; the existing
    // file must still be recognized as the slot's occupant.
    let season_dir = media.join("Show").join("Season 01");
    std::fs::create_dir_all(&season_dir).expect("season dir");
    let old = season_dir.join("[Old] Show - 05 [720p].mkv");
    std::fs::write(&old, b"old bytes").expect("old file");
    seed_grabbed_torrent(
        &db,
        series_id,
        "oldhash-naming",
        "[Old] Show - 05 [720p]",
        &[5],
    )
    .await;
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE hash = 'oldhash-naming'")
        .execute(&db)
        .await
        .unwrap();

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-naming-upgrade",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-naming-upgrade", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["Show - S01E05.mkv".to_string()],
        "the old file is retired and the new one lands under the current template"
    );
    assert!(!old.exists());
    assert_eq!(client.deleted.load(Ordering::SeqCst), 1);
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

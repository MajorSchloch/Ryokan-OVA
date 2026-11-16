use super::*;
use crate::{
    models::{
        config, media_probe_cache,
        series_language_preferences::{self, LanguagePreferences},
    },
    services::download_client::{AddOutcome, SelectiveOutcome},
    test_support::{build_test_app_state, in_memory_pool, seed_series},
};
use async_trait::async_trait;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
const HASH: &str = "1234567890123456789012345678901234567890";
const VIDEO: &str = "[Group] Show - 01.mkv";

struct Client {
    db: SqlitePool,
    disable_policy_on_files: AtomicBool,
    restart_download_on_files: AtomicBool,
    fail_list: AtomicBool,
    item: Mutex<DownloadItem>,
    files: Mutex<Vec<DownloadFile>>,
    stops: AtomicUsize,
    fail_files: AtomicBool,
    fail_stop: AtomicBool,
    change_manifest: AtomicBool,
    reads: AtomicUsize,
}
#[async_trait]
impl DownloadClient for Client {
    async fn test(&self) -> Result<String, String> {
        Ok("test".into())
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("must not acquire")
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("must not acquire")
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        if self.fail_list.load(Ordering::SeqCst) {
            return Err("listing unavailable".into());
        }
        Ok(vec![self.item.lock().unwrap().clone()])
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        if self.disable_policy_on_files.swap(false, Ordering::SeqCst) {
            let mut cfg = config::get_config(&self.db).await.unwrap().unwrap();
            cfg.accepted_only_seeding = false;
            config::save_config(&self.db, &cfg).await.unwrap();
        }
        if self.restart_download_on_files.swap(false, Ordering::SeqCst) {
            self.item.lock().unwrap().state_kind = DownloadItemState::Downloading;
        }
        if self.fail_files.load(Ordering::SeqCst) {
            return Err("manifest unavailable".into());
        }
        let mut files = self.files.lock().unwrap().clone();
        if self.change_manifest.load(Ordering::SeqCst)
            && self.reads.fetch_add(1, Ordering::SeqCst) > 0
        {
            files[0].wanted = false;
        }
        Ok(files)
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err("stop failed".into());
        }
        self.item.lock().unwrap().state_kind = DownloadItemState::PausedComplete;
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        panic!("must never resume")
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("must never delete")
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        panic!("must preserve selection")
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}
struct Fixture {
    state: AppState,
    cfg: Config,
    client: Arc<Client>,
    dir: tempfile::TempDir,
    path: PathBuf,
    sid: i64,
    gid: i64,
}
async fn fixture() -> Fixture {
    let db = in_memory_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let downloads = base.join("downloads");
    let library = base.join("library");
    tokio::fs::create_dir(&downloads).await.unwrap();
    tokio::fs::create_dir(&library).await.unwrap();
    let path = downloads.join(VIDEO);
    tokio::fs::write(&path, b"media bytes").await.unwrap();
    let cfg = Config {
        accepted_only_seeding: true,
        post_processing_enabled: true,
        media_root: library.to_string_lossy().into(),
        media_requirements_enabled: true,
        required_audio_languages: "ja".into(),
        required_subtitle_languages: "en".into(),
        ..Config::default()
    };
    config::save_config(&db, &cfg).await.unwrap();
    let dc = download_clients::insert(
        &db,
        download_clients::DownloadClientForm {
            name: "test",
            kind: "qbittorrent",
            url: "http://test",
            username: "",
            password: "",
            label: "anime",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(dc, 1);
    let sid = seed_series(&db, 101, "Show").await;
    let mapping:crate::services::release_numbering::ReleaseMapping=serde_json::from_value(serde_json::json!({"version":1,"anilist_id":101,"tv_series":true,"source_first":1,"source_last":1,"target_first":1,"source_season":1,"basis":"entry_relative"})).unwrap();
    let gid = grabbed_torrents::record_grab_with_release_mapping(
        &db,
        HASH,
        "[Group] Show - 01",
        sid,
        &[1],
        false,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET download_client_id=1, state='imported', imported_at=CURRENT_TIMESTAMP, import_mode='copy', verification='verified' WHERE id=?").bind(gid).execute(&db).await.unwrap();
    grabbed_torrents::stamp_imported_episode_coverage(&db, gid, &BTreeMap::from([(sid, vec![1])]))
        .await
        .unwrap();
    grabbed_torrents::stamp_imported_source_paths(&db, gid, &[path.to_string_lossy().into()])
        .await
        .unwrap();
    let client = Arc::new(Client {
        db: db.clone(),
        disable_policy_on_files: AtomicBool::new(false),
        restart_download_on_files: AtomicBool::new(false),
        fail_list: AtomicBool::new(false),
        item: Mutex::new(DownloadItem {
            hash: HASH.into(),
            name: "Show 01".into(),
            size: 11,
            progress: 1.0,
            dlspeed: 0,
            state: "uploading".into(),
            category: "anime".into(),
            eta: 0,
            save_path: downloads.to_string_lossy().into(),
            content_path: path.to_string_lossy().into(),
            state_kind: DownloadItemState::Seeding,
            seeding_done: false,
        }),
        files: Mutex::new(vec![DownloadFile {
            name: VIDEO.into(),
            size: 11,
            progress: 1.0,
            wanted: true,
        }]),
        stops: AtomicUsize::new(0),
        fail_files: AtomicBool::new(false),
        fail_stop: AtomicBool::new(false),
        change_manifest: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
    });
    let state = build_test_app_state(db, Some(client.clone()));
    let f = Fixture {
        state,
        cfg,
        client,
        dir,
        path,
        sid,
        gid,
    };
    cache(&f, 720, "jpn", Some("eng")).await;
    f
}
async fn cache(f: &Fixture, height: u32, audio: &str, subs: Option<&str>) {
    let mut streams = vec![
        serde_json::json!({"index":0,"codec_type":"video","codec_name":"h264","width":height*16/9,"height":height}),
        serde_json::json!({"index":1,"codec_type":"audio","codec_name":"aac","tags":{"language":audio}}),
    ];
    if let Some(subs) = subs {
        streams.push(serde_json::json!({"index":2,"codec_type":"subtitle","codec_name":"ass","tags":{"language":subs}}));
    }
    let json = serde_json::json!({"streams":streams,"format":{"duration":"1440"}}).to_string();
    let id = media_info::FileIdentity::from_metadata(&tokio::fs::metadata(&f.path).await.unwrap())
        .unwrap();
    media_probe_cache::upsert_identified(
        &f.state.db,
        &f.path.to_string_lossy(),
        &id.cache_key(),
        id.size as i64,
        &json,
    )
    .await;
}
async fn decision(f: &Fixture) -> Decision {
    let client: Arc<dyn DownloadClient> = f.client.clone();
    let item = f.client.item.lock().unwrap().clone();
    evaluate(&f.state, &f.cfg, 1, &item, &client, false).await
}
async fn set(f: &Fixture, column: &str, value: &str) {
    assert!(matches!(column, "state" | "import_mode" | "verification"));
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE grabbed_torrents SET {column}=? WHERE id=?"
    )))
    .bind(value)
    .bind(f.gid)
    .execute(&f.state.db)
    .await
    .unwrap();
}
#[tokio::test]
async fn accepts_minimum_not_target_and_never_resumes_native_or_user_stops() {
    let f = fixture().await;
    assert_eq!(decision(&f).await, Decision::Accepted);
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    f.client.item.lock().unwrap().state_kind = DownloadItemState::PausedComplete;
    f.client.item.lock().unwrap().seeding_done = true;
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        crate::models::seed_policy::list(&f.state.db).await.unwrap()[0].verdict,
        "Accepted"
    );
}
#[tokio::test]
async fn rejected_source_stops_keeps_files_and_does_not_rewrite_history() {
    let f = fixture().await;
    cache(&f, 1080, "hin", Some("eng")).await;
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
    check_all(&f.state, false).await.unwrap();
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 1);
    assert!(f.path.exists());
    let review = crate::models::seed_policy::list(&f.state.db)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(review.verdict, "Rejected");
    assert!(review.control.contains("Already paused"));
    let state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id=?")
        .bind(f.gid)
        .fetch_one(&f.state.db)
        .await
        .unwrap();
    assert_eq!(state, "imported");
}
#[tokio::test]
async fn honors_german_no_subtitles_release_exception_and_policy_changes() {
    let f = fixture().await;
    cache(&f, 1080, "ger", None).await;
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
    series_language_preferences::save(
        &f.state.db,
        f.sid,
        &LanguagePreferences {
            audio_languages: Some("de".into()),
            subtitle_languages: Some(String::new()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(decision(&f).await, Decision::Accepted);
    series_language_preferences::save(&f.state.db, f.sid, &LanguagePreferences::default())
        .await
        .unwrap();
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
}
#[tokio::test]
async fn disabled_policy_noops_and_preview_does_not_control() {
    let mut f = fixture().await;
    f.cfg.accepted_only_seeding = false;
    config::save_config(&f.state.db, &f.cfg).await.unwrap();
    set(&f, "state", "failed").await;
    check_all(&f.state, false).await.unwrap();
    assert!(
        crate::models::seed_policy::list(&f.state.db)
            .await
            .unwrap()
            .is_empty()
    );
    check_all(&f.state, true).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn stale_source_and_missing_cache_hold_without_using_library_facts() {
    let f = fixture().await;
    tokio::fs::write(&f.path, b"other bytes").await.unwrap();
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn legacy_partial_protected_and_edition_rejections_never_certify() {
    let f = fixture().await;
    for mode in ["partial", "protected", "move", ""] {
        set(&f, "import_mode", mode).await;
        assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    }
    set(&f, "import_mode", "copy").await;
    set(&f, "state", "failed").await;
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
    set(&f, "state", "imported").await;
    sqlx::query("UPDATE grabbed_torrents SET imported_episode_coverage=NULL WHERE id=?")
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
}
#[tokio::test]
async fn all_available_media_must_have_imported_coverage_even_if_unwanted() {
    let f = fixture().await;
    let second = f.path.parent().unwrap().join("Show - 02.mkv");
    tokio::fs::write(second, b"other bytes").await.unwrap();
    f.client.files.lock().unwrap().push(DownloadFile {
        name: "Show - 02.mkv".into(),
        size: 11,
        progress: 1.0,
        wanted: false,
    });
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn missing_native_manifest_changed_selection_and_stop_failure_hold() {
    let f = fixture().await;
    f.client.fail_files.store(true, Ordering::SeqCst);
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    f.client.fail_files.store(false, Ordering::SeqCst);
    f.client.change_manifest.store(true, Ordering::SeqCst);
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    f.client.fail_files.store(true, Ordering::SeqCst);
    f.client.fail_stop.store(true, Ordering::SeqCst);
    check_all(&f.state, false).await.unwrap();
    let r = crate::models::seed_policy::list(&f.state.db)
        .await
        .unwrap()
        .remove(0);
    assert!(r.control.contains("Stop failed"));
    assert!(f.path.exists());
}
#[tokio::test]
async fn null_owners_disabled_clients_and_incomplete_jobs_are_not_adopted() {
    let f = fixture().await;
    sqlx::query("UPDATE grabbed_torrents SET download_client_id=NULL WHERE id=?")
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    sqlx::query("UPDATE grabbed_torrents SET download_client_id=1 WHERE id=?")
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    f.client.item.lock().unwrap().state_kind = DownloadItemState::Downloading;
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    sqlx::query("UPDATE download_clients SET enabled=0")
        .execute(&f.state.db)
        .await
        .unwrap();
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
}
#[tokio::test]
async fn arbitrary_adjacent_subtitle_does_not_certify_source() {
    let f = fixture().await;
    cache(&f, 1080, "jpn", None).await;
    let sub = f.path.with_extension("en.srt");
    tokio::fs::write(&sub, "1\n00:00:01,000 --> 00:00:02,000\nHello\n")
        .await
        .unwrap();
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
    let size = tokio::fs::metadata(&sub).await.unwrap().len() as i64;
    f.client.files.lock().unwrap().push(DownloadFile {
        name: sub.file_name().unwrap().to_string_lossy().into(),
        size,
        progress: 1.0,
        wanted: true,
    });
    assert_eq!(decision(&f).await, Decision::Accepted);
}
#[tokio::test]
async fn finished_seed_cleanup_cannot_delete_held_original() {
    let f = fixture().await;
    cache(&f, 1080, "hin", None).await;
    f.client.item.lock().unwrap().seeding_done = true;
    download_clients::set_remove_completed(&f.state.db, 1, true)
        .await
        .unwrap();
    let report = crate::services::post_processing::sweep_finished_seeds_now(&f.state)
        .await
        .unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(report.waiting, 1);
    assert!(f.path.exists());
}
#[tokio::test]
async fn migrations_and_unrelated_config_saves_preserve_opt_in() {
    let f = fixture().await;
    crate::models::migrate(&f.state.db).await.unwrap();
    let mut cfg = config::get_config(&f.state.db).await.unwrap().unwrap();
    assert!(cfg.accepted_only_seeding);
    cfg.title_language = "romaji".into();
    config::save_config(&f.state.db, &cfg).await.unwrap();
    assert!(
        config::get_config(&f.state.db)
            .await
            .unwrap()
            .unwrap()
            .accepted_only_seeding
    );
    assert!(f.dir.path().exists());
}

#[tokio::test]
async fn partial_selection_cannot_borrow_single_video_subtitle_fallback() {
    let f = fixture().await;
    cache(&f, 1080, "jpn", None).await;
    let sub = f.path.parent().unwrap().join("English.srt");
    tokio::fs::write(&sub, "1\n00:00:01,000 --> 00:00:02,000\nHello\n")
        .await
        .unwrap();
    let sub_size = tokio::fs::metadata(&sub).await.unwrap().len() as i64;
    f.client.files.lock().unwrap().extend([
        DownloadFile {
            name: "Show - 02.mkv".into(),
            size: 11,
            progress: 0.0,
            wanted: false,
        },
        DownloadFile {
            name: "English.srt".into(),
            size: sub_size,
            progress: 1.0,
            wanted: true,
        },
    ]);
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
}

#[tokio::test]
async fn bound_proof_uses_full_native_manifest_including_non_video_files() {
    use crate::services::{
        release_numbering::ReleaseMapping,
        source_numbering::{
            prepare_review,
            tests::{lagoon_domain, lagoon_files},
        },
    };
    let f = fixture().await;
    let mut files = lagoon_files();
    files.push("release.nfo".into());
    let proof = prepare_review(lagoon_domain(), HASH, &files, 1519)
        .unwrap()
        .confirm(&files[12..24])
        .unwrap();
    let mapping = ReleaseMapping::from_bound_source(&proof).unwrap();
    sqlx::query("UPDATE grabbed_torrents SET release_mapping=? WHERE id=?")
        .bind(serde_json::to_string(&mapping).unwrap())
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    let owner = owners(&f.state.db, HASH).await.unwrap().remove(0);
    let manifest: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, n)| (i, n.as_str(), (12..24).contains(&i)))
        .collect();
    assert_eq!(
        bind_slots(&f.state.db, &owner, HASH, &manifest)
            .await
            .unwrap()
            .len(),
        12
    );
    assert!(
        bind_slots(&f.state.db, &owner, HASH, &manifest[..29])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn multi_episode_source_requires_actual_import_coverage_for_every_episode() {
    let mut f = fixture().await;
    let path = f.path.with_file_name("[Group] Show - 01-02.mkv");
    tokio::fs::rename(&f.path, &path).await.unwrap();
    f.path = path;
    f.client.files.lock().unwrap()[0].name = f.path.file_name().unwrap().to_string_lossy().into();
    let mapping = serde_json::json!({"version":1,"anilist_id":101,"tv_series":true,"source_first":1,"source_last":2,"target_first":1,"source_season":1,"basis":"entry_relative"});
    sqlx::query("UPDATE grabbed_torrents SET release_mapping=? WHERE id=?")
        .bind(mapping.to_string())
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    grabbed_torrents::stamp_imported_source_paths(
        &f.state.db,
        f.gid,
        &[f.path.to_string_lossy().into()],
    )
    .await
    .unwrap();
    cache(&f, 720, "jpn", Some("eng")).await;
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    grabbed_torrents::stamp_imported_episode_coverage(
        &f.state.db,
        f.gid,
        &BTreeMap::from([(f.sid, vec![1, 2])]),
    )
    .await
    .unwrap();
    assert_eq!(decision(&f).await, Decision::Accepted);
}

#[tokio::test]
async fn automatic_failed_and_misgrab_removal_keep_rejected_payload_and_retry_bookkeeping() {
    let f = fixture().await;
    download_clients::set_remove_failed(&f.state.db, 1, true)
        .await
        .unwrap();
    set(&f, "state", "failed").await;
    let grab = grabbed_torrents::get_by_id(&f.state.db, f.gid)
        .await
        .unwrap()
        .unwrap();
    crate::services::redownload::remove_failed_from_client(&f.state, &grab).await;
    assert!(f.path.exists());
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 1);
    let action = crate::services::misgrab::remove_and_blocklist(&f.state, &grab)
        .await
        .unwrap();
    assert!(matches!(
        action,
        crate::services::misgrab::MisgrabAction::RemovedNoDelete
    ));
    let grab = grabbed_torrents::get_by_id(&f.state.db, f.gid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(grab.state, "failed");
    assert!(f.path.exists());
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn source_check_owned_task_finishes_after_response_is_dropped() {
    let f = fixture().await;
    let response = crate::handlers::downloads::check_seed_sources(
        axum::extract::State(f.state.clone()),
        axum_htmx::HxRequest(false),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(g) = CHECK_LOCK.try_lock() {
                drop(g);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        crate::models::seed_policy::list(&f.state.db).await.unwrap()[0].verdict,
        "Accepted"
    );
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn legacy_quality_mode_still_enforces_explicit_subtitle_requirements() {
    let mut f = fixture().await;
    f.cfg.media_requirements_enabled = false;
    config::save_config(&f.state.db, &f.cfg).await.unwrap();
    cache(&f, 1080, "jpn", None).await;
    assert!(matches!(decision(&f).await, Decision::Rejected(_)));
}

#[tokio::test]
async fn disabling_policy_during_review_prevents_a_stale_stop() {
    let f = fixture().await;
    f.client
        .disable_policy_on_files
        .store(true, Ordering::SeqCst);
    f.client.fail_files.store(true, Ordering::SeqCst);
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    assert!(
        !config::get_config(&f.state.db)
            .await
            .unwrap()
            .unwrap()
            .accepted_only_seeding
    );
}
#[tokio::test]
async fn source_restarted_as_download_is_not_stopped_from_a_stale_listing() {
    let f = fixture().await;
    f.client
        .restart_download_on_files
        .store(true, Ordering::SeqCst);
    f.client.fail_files.store(true, Ordering::SeqCst);
    check_all(&f.state, false).await.unwrap();
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn failed_client_listing_does_not_skip_healthy_clients() {
    let f = fixture().await;
    cache(&f, 1080, "hin", None).await;
    let id = download_clients::insert(
        &f.state.db,
        download_clients::DownloadClientForm {
            name: "second",
            kind: "qbittorrent",
            url: "http://second",
            username: "",
            password: "",
            label: "anime",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();
    let second = Arc::new(Client {
        db: f.state.db.clone(),
        disable_policy_on_files: AtomicBool::new(false),
        restart_download_on_files: AtomicBool::new(false),
        fail_list: AtomicBool::new(false),
        item: Mutex::new(f.client.item.lock().unwrap().clone()),
        files: Mutex::new(f.client.files.lock().unwrap().clone()),
        stops: AtomicUsize::new(0),
        fail_files: AtomicBool::new(false),
        fail_stop: AtomicBool::new(false),
        change_manifest: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
    });
    let current_pool = f.state.download_clients.read().await.clone();
    let mut pool = crate::DownloadClientPool {
        clients: current_pool.clients.clone(),
        default_torrent_id: current_pool.default_torrent_id,
        default_usenet_id: current_pool.default_usenet_id,
    };
    pool.clients.insert(id, second.clone());
    let first = *pool.clients.keys().next().unwrap();
    let healthy = if first == 1 { id } else { 1 };
    if first == 1 {
        f.client.fail_list.store(true, Ordering::SeqCst);
    } else {
        second.fail_list.store(true, Ordering::SeqCst);
    }
    *f.state.download_clients.write().await = Arc::new(pool);
    sqlx::query("UPDATE grabbed_torrents SET download_client_id=? WHERE id=?")
        .bind(healthy)
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    check_all(&f.state, false).await.unwrap();
    let reviews = crate::models::seed_policy::list(&f.state.db).await.unwrap();
    assert!(
        reviews
            .iter()
            .any(|r| r.client_id == healthy && r.control.contains("Paused"))
    );
}

#[tokio::test]
async fn stale_failure_and_shared_hash_owners_retain_without_pausing() {
    let f = fixture().await;
    let grab = grabbed_torrents::get_by_id(&f.state.db, f.gid)
        .await
        .unwrap()
        .unwrap();
    let client: Arc<dyn DownloadClient> = f.client.clone();
    assert!(retain_rejected_source(&f.state, &grab, &client).await);
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    set(&f, "state", "failed").await;
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,state,download_client_id) VALUES(?, 'another owner', ?, 'pending', 1)").bind(HASH).bind(f.sid).execute(&f.state.db).await.unwrap();
    assert!(retain_rejected_source(&f.state, &grab, &client).await);
    assert_eq!(f.client.stops.load(Ordering::SeqCst), 0);
    assert!(f.path.exists());
}

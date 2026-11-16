use super::*;
use crate::models::seed_policy;

async fn enable(f: &Fixture) {
    let mut cfg = f.cfg.clone();
    cfg.retire_unaccepted_sources = true;
    config::save_config(&f.state.db, &cfg).await.unwrap();
    f.client.allow_delete.store(true, Ordering::SeqCst);
}
async fn retire(f: &Fixture) {
    check_all(&f.state, false).await.unwrap();
    super::super::retirement::run(&f.state).await.unwrap();
}
async fn blocked(f: &Fixture) -> bool {
    seed_policy::is_blocked(&f.state.db, HASH).await.unwrap()
}

#[tokio::test]
async fn explicit_opt_in_is_required_and_accepted_stops_are_preserved() {
    let f = fixture().await;
    cache(&f, 480, "jpn", Some("eng")).await;
    retire(&f).await;
    assert!(!blocked(&f).await);
    enable(&f).await;
    cache(&f, 1080, "jpn", Some("eng")).await;
    retire(&f).await;
    assert_eq!(
        f.client.item.lock().unwrap().state_kind,
        DownloadItemState::PausedComplete
    );
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 0);
    assert!(!blocked(&f).await);
}

#[tokio::test]
async fn retire_preserves_files_success_history_coverage_and_records_one_reason() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    let before = owners(&f.state.db, HASH).await.unwrap();
    retire(&f).await;
    retire(&f).await;
    assert!(
        blocked(&f).await,
        "reviews: {:?}",
        seed_policy::list(&f.state.db).await.unwrap()
    );
    assert_eq!(owners(&f.state.db, HASH).await.unwrap(), before);
    assert_eq!(tokio::fs::read(&f.path).await.unwrap(), b"media bytes");
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 1);
    let rows = seed_policy::retirements(&f.state.db).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].retired_at.is_some());
    assert!(rows[0].retirement_reason.contains("minimum"));
    assert!(rows[0].retirement_snapshot.contains(VIDEO));
    assert!(
        grabbed_torrents::client_removed_at(&f.state.db, f.gid)
            .await
            .is_some()
    );
    let activity = crate::models::library_activity::recent_all(&f.state.db)
        .await
        .unwrap();
    assert_eq!(
        activity
            .iter()
            .filter(|r| r.action == "Source retired")
            .count(),
        1
    );
    assert!(seed_policy::list(&f.state.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn legacy_missing_mapping_is_excluded_as_unverified_without_rewriting_import() {
    let f = fixture().await;
    enable(&f).await;
    sqlx::query("UPDATE grabbed_torrents SET release_mapping=NULL WHERE id=?")
        .bind(f.gid)
        .execute(&f.state.db)
        .await
        .unwrap();
    retire(&f).await;
    assert!(
        blocked(&f).await,
        "reviews: {:?}",
        seed_policy::list(&f.state.db).await.unwrap()
    );
    let rows = seed_policy::retirements(&f.state.db).await.unwrap();
    assert!(
        rows[0]
            .retirement_reason
            .contains("No saved source-file mapping")
    );
    assert_eq!(
        owners(&f.state.db, HASH).await.unwrap()[0].state,
        "imported"
    );
}

#[tokio::test]
async fn missing_mount_probe_errors_invalid_settings_and_changed_manifest_are_holds() {
    for case in [
        "missing_file",
        "bad_probe",
        "invalid_settings",
        "manifest_changed",
    ] {
        let f = fixture().await;
        enable(&f).await;
        match case {
            "missing_file" => tokio::fs::remove_file(&f.path).await.unwrap(),
            "bad_probe" => {
                tokio::fs::write(&f.path, b"not a media file")
                    .await
                    .unwrap();
            }
            "invalid_settings" => {
                let mut cfg = f.cfg.clone();
                cfg.retire_unaccepted_sources = true;
                cfg.required_subtitle_languages = "unknown_language".into();
                config::save_config(&f.state.db, &cfg).await.unwrap();
            }
            _ => {
                f.client.change_manifest.store(true, Ordering::SeqCst);
                cache(&f, 480, "jpn", Some("eng")).await;
            }
        }
        retire(&f).await;
        assert!(!blocked(&f).await, "{case}");
        assert_eq!(f.client.deletes.load(Ordering::SeqCst), 0, "{case}");
    }
}

#[tokio::test]
async fn incomplete_pending_and_shared_owners_never_retire() {
    for case in ["incomplete", "pending", "shared"] {
        let f = fixture().await;
        enable(&f).await;
        cache(&f, 480, "jpn", Some("eng")).await;
        match case {
            "incomplete" => {
                let mut item = f.client.item.lock().unwrap();
                item.progress = 0.5;
                item.state_kind = DownloadItemState::Downloading;
            }
            "pending" => set(&f, "state", "pending").await,
            _ => {
                sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,state,episode_numbers,download_client_id) VALUES(?, 'Older attempt',?,'failed','[1]',1)")
                .bind(HASH).bind(f.sid).execute(&f.state.db).await.unwrap();
            }
        }
        retire(&f).await;
        assert!(!blocked(&f).await, "{case}");
        assert_eq!(f.client.deletes.load(Ordering::SeqCst), 0, "{case}");
    }
}

#[tokio::test]
async fn open_picker_reservation_wins_even_with_terminal_owner() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    sqlx::query("INSERT INTO pending_grabs(preview_id,info_hash,client_kind,created_at,heartbeat_at,release_metadata_json) VALUES('open',?,'qbittorrent',0,0,'{}')")
        .bind(HASH).execute(&f.state.db).await.unwrap();
    retire(&f).await;
    assert!(!blocked(&f).await);
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_native_removal_keeps_block_and_retries_without_false_bookkeeping() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    f.client.fail_delete.store(true, Ordering::SeqCst);
    retire(&f).await;
    assert!(
        blocked(&f).await,
        "reviews: {:?}",
        seed_policy::list(&f.state.db).await.unwrap()
    );
    assert!(
        grabbed_torrents::client_removed_at(&f.state.db, f.gid)
            .await
            .is_none()
    );
    assert!(
        seed_policy::retirements(&f.state.db).await.unwrap()[0]
            .retired_at
            .is_none()
    );
    f.client.fail_delete.store(false, Ordering::SeqCst);
    f.client.item.lock().unwrap().eta = 99;
    f.client.item.lock().unwrap().seeding_done = true;
    set(&f, "state", "replaced").await;
    retire(&f).await;
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 2);
    assert!(
        seed_policy::retirements(&f.state.db).await.unwrap()[0]
            .retired_at
            .is_some()
    );
}

#[tokio::test]
async fn lost_bookkeeping_rolls_back_then_finishes_when_client_is_reachable() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    sqlx::query("DROP TABLE library_activity")
        .execute(&f.state.db)
        .await
        .unwrap();
    check_all(&f.state, false).await.unwrap();
    assert!(super::super::retirement::run(&f.state).await.is_err());
    assert!(f.client.deleted.load(Ordering::SeqCst));
    assert!(
        blocked(&f).await,
        "reviews: {:?}",
        seed_policy::list(&f.state.db).await.unwrap()
    );
    assert!(
        seed_policy::retirements(&f.state.db).await.unwrap()[0]
            .retired_at
            .is_none()
    );
    assert!(
        grabbed_torrents::client_removed_at(&f.state.db, f.gid)
            .await
            .is_none()
    );
    crate::models::library_activity::migrate(&f.state.db)
        .await
        .unwrap();
    f.client.fail_list.store(true, Ordering::SeqCst);
    super::super::retirement::run(&f.state).await.unwrap();
    assert!(
        seed_policy::retirements(&f.state.db).await.unwrap()[0]
            .retired_at
            .is_none()
    );
    f.client.fail_list.store(false, Ordering::SeqCst);
    retire(&f).await;
    assert!(
        seed_policy::retirements(&f.state.db).await.unwrap()[0]
            .retired_at
            .is_some()
    );
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn durable_hash_blocks_cover_collectors_and_late_recording_until_explicit_reconsideration() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    retire(&f).await;
    for hash in [HASH.to_owned(), HASH.to_uppercase()] {
        assert!(
            grabbed_torrents::is_blocklisted_for(&f.state.db, &hash, None)
                .await
                .unwrap()
        );
        assert!(
            grabbed_torrents::is_blocklisted_release(&f.state.db, f.sid, &hash, "renamed release")
                .await
        );
        assert!(
            grabbed_torrents::blocklist_snapshot(&f.state.db, 999)
                .await
                .rejects(&hash, "renamed")
        );
    }
    assert!(
        !grabbed_torrents::is_blocklisted_release(&f.state.db, f.sid, &"a".repeat(40), "Show 01")
            .await
    );
    let _ = grabbed_torrents::unblock_by_hash(&f.state.db, HASH, f.gid, Some(f.sid)).await;
    assert!(
        blocked(&f).await,
        "reviews: {:?}",
        seed_policy::list(&f.state.db).await.unwrap()
    );
    let result =
        grabbed_torrents::record_grab(&f.state.db, HASH, "Another title", f.sid, &[1], false).await;
    assert!(
        result.is_err(),
        "Late dispatch cannot revive a retired source"
    );
    assert_eq!(
        owners(&f.state.db, HASH).await.unwrap()[0].state,
        "imported"
    );
    seed_policy::unblock(&f.state.db, 1, HASH).await.unwrap();
    assert!(!blocked(&f).await);
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(tokio::fs::read(&f.path).await.unwrap(), b"media bytes");
}

#[tokio::test]
async fn routine_reviews_and_idempotent_migration_do_not_erase_blocks() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    retire(&f).await;
    let before = seed_policy::retirements(&f.state.db)
        .await
        .unwrap()
        .remove(0);
    seed_policy::save(
        &f.state.db,
        &Review {
            client_id: 1,
            hash: HASH.into(),
            name: "new observation".into(),
            verdict: "Accepted".into(),
            reason: "observation only".into(),
            control: "No action".into(),
            checked_at: String::new(),
        },
    )
    .await
    .unwrap();
    seed_policy::init_table(&f.state.db).await.unwrap();
    let after = seed_policy::retirements(&f.state.db)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(before.retirement_reason, after.retirement_reason);
    assert_eq!(before.retirement_snapshot, after.retirement_snapshot);
    assert!(
        blocked(&f).await,
        "reviews: {:?}",
        seed_policy::list(&f.state.db).await.unwrap()
    );
}

#[tokio::test]
async fn native_guard_denies_every_dispatch_and_resume_after_a_hash_block() {
    use crate::services::download_client::source_guard::SourceGuard;
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    retire(&f).await;
    let client = SourceGuard {
        db: f.state.db.clone(),
        inner: f.client.clone(),
    };
    let magnet = format!("magnet:?xt=urn:btih:{HASH}");
    assert!(
        client
            .add_torrent(&magnet, HASH)
            .await
            .unwrap_err()
            .contains("excluded")
    );
    assert!(
        client
            .add_torrent_returning_id(&magnet, "")
            .await
            .unwrap_err()
            .contains("excluded")
    );
    assert!(
        client
            .add_torrent_paused(&magnet, HASH)
            .await
            .unwrap_err()
            .contains("excluded")
    );
    assert!(
        client
            .add_torrent_metadata_only(HASH)
            .await
            .unwrap_err()
            .contains("excluded")
    );
    assert!(
        client
            .add_torrent_with_file_filter(&magnet, HASH, &mut |_| Some(vec![0]))
            .await
            .unwrap_err()
            .contains("excluded")
    );
    assert!(client.resume(HASH).await.unwrap_err().contains("excluded"));
    assert!(
        client
            .set_file_wanted(HASH, &[0], true)
            .await
            .unwrap_err()
            .contains("excluded")
    );
    assert!(
        client
            .add_torrent(&magnet, &"a".repeat(40))
            .await
            .unwrap_err()
            .contains("does not match")
    );
}

#[tokio::test]
async fn in_flight_native_call_waits_for_retirement_then_reads_the_new_block() {
    use crate::services::download_client::source_guard::{SOURCE_ACTION_LOCK, SourceGuard};
    let f = fixture().await;
    check_all(&f.state, false).await.unwrap();
    let client = SourceGuard {
        db: f.state.db.clone(),
        inner: f.client.clone(),
    };
    let writer = SOURCE_ACTION_LOCK.write().await;
    let call = client.add_torrent_metadata_only(HASH);
    tokio::pin!(call);
    assert!(futures_util::poll!(&mut call).is_pending());
    sqlx::query("UPDATE torrent_seed_reviews SET blocked_at=CURRENT_TIMESTAMP WHERE hash=?")
        .bind(HASH)
        .execute(&f.state.db)
        .await
        .unwrap();
    drop(writer);
    assert!(call.await.unwrap_err().contains("excluded"));
}

#[tokio::test]
async fn native_guard_fails_closed_on_database_error() {
    use crate::services::download_client::source_guard::SourceGuard;
    let f = fixture().await;
    let client = SourceGuard {
        db: f.state.db.clone(),
        inner: f.client.clone(),
    };
    sqlx::query("DROP TABLE torrent_seed_reviews")
        .execute(&f.state.db)
        .await
        .unwrap();
    assert!(
        client
            .resume(HASH)
            .await
            .unwrap_err()
            .contains("unavailable")
    );
    assert!(
        grabbed_torrents::blocklist_snapshot(&f.state.db, 101)
            .await
            .rejects(HASH, "Show")
    );
}

#[tokio::test]
async fn unknown_http_identity_never_reaches_the_download_client() {
    use crate::services::download_client::source_guard::SourceGuard;
    let f = fixture().await;
    let client = SourceGuard {
        db: f.state.db.clone(),
        inner: f.client.clone(),
    };
    check_all(&f.state, false).await.unwrap();
    sqlx::query("UPDATE torrent_seed_reviews SET blocked_at=CURRENT_TIMESTAMP WHERE hash=?")
        .bind(HASH)
        .execute(&f.state.db)
        .await
        .unwrap();
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not a torrent"))
        .mount(&mock)
        .await;
    assert!(
        client
            .add_torrent_returning_id(&mock.uri(), "")
            .await
            .unwrap_err()
            .contains("identity unavailable")
    );
}

#[tokio::test]
async fn successful_unknown_audio_probe_is_unverified_and_retired_not_an_io_hold() {
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 1080, "und", Some("eng")).await;
    assert!(matches!(decision(&f).await, Decision::Unverified(_)));
    retire(&f).await;
    assert!(blocked(&f).await);
    assert!(f.path.exists());
}

#[tokio::test]
async fn moved_sources_and_conflicting_magnets_require_review() {
    let f = fixture().await;
    enable(&f).await;
    set(&f, "import_mode", "move").await;
    set(&f, "state", "failed").await;
    retire(&f).await;
    assert!(!blocked(&f).await);
    use crate::services::download_client::source_guard::SourceGuard;
    let client = SourceGuard {
        db: f.state.db.clone(),
        inner: f.client.clone(),
    };
    let url = format!("magnet:?xt=urn:btih:{HASH}&xt=urn:btih:{}", "a".repeat(40));
    assert!(
        client
            .add_torrent(&url, HASH)
            .await
            .unwrap_err()
            .contains("Conflicting")
    );
    assert_eq!(f.client.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http_dispatch_reads_once_returns_real_hash_and_forwards_seed_limits() {
    use crate::services::download_client::{SeedRules, source_guard::SourceGuard};
    let f = fixture().await;
    enable(&f).await;
    cache(&f, 480, "jpn", Some("eng")).await;
    retire(&f).await;
    let tracker = "https://tracker.test/announce";
    let body = format!(
        "d8:announce{}:{}4:infod6:lengthi5e4:name5:a.mkvee",
        tracker.len(),
        tracker
    );
    let actual = crate::services::torrent_file::torrent_info_hash(body.as_bytes()).unwrap();
    assert_ne!(actual, HASH);
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&mock)
        .await;
    f.client.allow_add.store(true, Ordering::SeqCst);
    let client = SourceGuard {
        db: f.state.db.clone(),
        inner: f.client.clone(),
    };
    let (_, canonical) = client
        .add_torrent_returning_id(&mock.uri(), "")
        .await
        .unwrap();
    assert_eq!(canonical, actual);
    let calls = f.client.adds.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, actual);
    let url = reqwest::Url::parse(&calls[0].0).unwrap();
    assert_eq!(url.scheme(), "magnet");
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "tr" && v == "https://tracker.test/announce")
    );
    client
        .set_seed_rules(
            &actual,
            SeedRules {
                ratio: Some(2.0),
                time_minutes: Some(60),
            },
        )
        .await
        .unwrap();
    let limits = f.client.seed_rules.lock().unwrap();
    assert_eq!(limits[0].ratio, Some(2.0));
    assert_eq!(limits[0].time_minutes, Some(60));
}

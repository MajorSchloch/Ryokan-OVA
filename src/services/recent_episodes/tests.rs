use super::*;
use crate::test_support::{in_memory_pool, seed_series};

async fn fixture() -> (SqlitePool, config::Config, Target, tempfile::TempDir) {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 101, "Test Series").await;
    let root = tempfile::tempdir().unwrap();
    sqlx::query("UPDATE series SET format='TV',monitor_mode='all',folder_name='Test' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    let at = chrono::Utc::now().timestamp() - 3600;
    sqlx::query("INSERT INTO episode_airings(series_id,episode,airing_at,duration_minutes) VALUES(?,7,?,24)").bind(id).bind(at).execute(&db).await.unwrap();
    crate::models::monitoring::set_episode_monitored(&db, id, 7, true)
        .await
        .unwrap();
    let cfg = config::Config {
        media_root: root.path().to_string_lossy().into(),
        rss_master_enabled: true,
        recent_episode_search_enabled: true,
        post_processing_enabled: true,
        auto_redownload_failed: true,
        ..Default::default()
    };
    let target = Target {
        series_id: id,
        provider_id: 101,
        episode: 7,
        airing_at: at,
        folder_name: "Test".into(),
        title: "Test Series".into(),
    };
    (db, cfg, target, root)
}
#[tokio::test]
async fn missing_demand_requires_current_airing_monitoring_and_complete_file_absence() {
    let (db, mut cfg, t, root) = fixture().await;
    assert!(demand(&db, &cfg, &t).await.is_ok());
    crate::models::monitoring::set_episode_monitored(&db, t.series_id, 7, false)
        .await
        .unwrap();
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("monitoring")
    );
    crate::models::monitoring::set_episode_monitored(&db, t.series_id, 7, true)
        .await
        .unwrap();
    std::fs::create_dir(root.path().join("Test")).unwrap();
    std::fs::write(
        root.path().join("Test/Test Series - S01E06-E07.mkv"),
        b"file",
    )
    .unwrap();
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("already available")
    );
    cfg.media_root = root.path().join("unavailable").to_string_lossy().into();
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("completely")
    );
}
#[tokio::test]
async fn routed_ownership_replaces_parent_coverage_and_prevents_a_duplicate() {
    let (db, cfg, t, _root) = fixture().await;
    let other = seed_series(&db, 102, "Other").await;
    let gid:i64=sqlx::query_scalar("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state) VALUES('hash','pack',?,'[7]','pending') RETURNING id").bind(other).fetch_one(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO grabbed_torrent_series(grab_id,series_id,episode_numbers) VALUES(?,?,'[7]')",
    )
    .bind(gid)
    .bind(t.series_id)
    .execute(&db)
    .await
    .unwrap();
    assert!(has_pending(&db, t.series_id, 7).await.unwrap());
    assert!(!has_pending(&db, other, 7).await.unwrap());
    assert!(demand(&db, &cfg, &t).await.unwrap_err().contains("pending"));
}
#[tokio::test]
async fn retry_policy_preserves_manual_holds_and_failure_budget() {
    let (db, mut cfg, t, _root) = fixture().await;
    let gid:i64=sqlx::query_scalar("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason) VALUES('hash','release',?,'[7]','failed','requirements_rejected') RETURNING id").bind(t.series_id).fetch_one(&db).await.unwrap();
    assert!(demand(&db, &cfg, &t).await.is_ok());
    cfg.auto_redownload_failed = false;
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("disabled")
    );
    cfg.auto_redownload_failed = true;
    for state in ["cancelled", "removed", "imported"] {
        sqlx::query("UPDATE grabbed_torrents SET state=? WHERE id=?")
            .bind(state)
            .bind(gid)
            .execute(&db)
            .await
            .unwrap();
        assert!(demand(&db, &cfg, &t).await.unwrap_err().contains("review"));
    }
    sqlx::query("UPDATE grabbed_torrents SET state='failed',failure_reason='' WHERE id=?")
        .bind(gid)
        .execute(&db)
        .await
        .unwrap();
    assert!(demand(&db, &cfg, &t).await.unwrap_err().contains("review"));
    sqlx::query("UPDATE grabbed_torrents SET failure_reason='client_error' WHERE id=?")
        .bind(gid)
        .execute(&db)
        .await
        .unwrap();
    for i in 0..2 {
        sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason) VALUES(?,'failure',?,'[1]','failed','client_error')").bind(format!("other{i}")).bind(t.series_id).execute(&db).await.unwrap();
    }
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("Retry limit")
    );
}
#[tokio::test]
async fn fair_order_moves_checked_episode_behind_unchecked_and_ignores_old_identity() {
    let (db, cfg, t, _root) = fixture().await;
    sqlx::query("INSERT INTO episode_airings(series_id,episode,airing_at,duration_minutes) VALUES(?,8,?,24)").bind(t.series_id).bind(t.airing_at+100).execute(&db).await.unwrap();
    crate::models::monitoring::set_episode_monitored(&db, t.series_id, 8, true)
        .await
        .unwrap();
    assert_eq!(targets(&db, &cfg).await.unwrap()[0].episode, 8);
    let mut event =
        library_activity::Event::new("check", STARTED, Some(t.series_id), "Test", "checking");
    event.detail = "{\"episode\":8}".into();
    assert!(library_activity::try_append(&db, event).await.unwrap());
    assert_eq!(targets(&db, &cfg).await.unwrap()[0].episode, 7);
    sqlx::query("UPDATE library_activity SET provider_id=999")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(targets(&db, &cfg).await.unwrap()[0].episode, 8);
}

#[tokio::test]
async fn recent_priority_reserves_searches_for_older_gaps() {
    let (_db, _cfg, t, _root) = fixture().await;
    let mut recent: VecDeque<_> = (1..=8)
        .map(|episode| Target {
            episode,
            ..t.clone()
        })
        .collect();
    let mut older: VecDeque<_> = (9..=12)
        .map(|episode| Target {
            episode,
            ..t.clone()
        })
        .collect();
    let sequence: Vec<_> = (0..8)
        .map(|checked| {
            next_target(&mut recent, &mut older, checked)
                .unwrap()
                .episode
        })
        .collect();
    assert_eq!(sequence, [1, 2, 3, 9, 4, 5, 6, 10]);
}

#[tokio::test]
async fn wanted_timing_uses_current_identity_and_confirmed_airing() {
    let (db, mut cfg, t, _root) = fixture().await;
    let row =
        |statuses: &HashMap<_, RetryStatus>| statuses[&(t.series_id, t.episode)].timing.clone();
    let mut finished = library_activity::Event::new(
        "old worker",
        FINISHED,
        Some(t.series_id),
        &t.title,
        "Old identity hold",
    );
    finished.provider_id = Some(t.provider_id);
    finished.detail = serde_json::json!({"episode":t.episode,"airing_at":t.airing_at,"outcome":"held","retry_at":chrono::Utc::now().timestamp()+3600}).to_string();
    sqlx::query("UPDATE series SET anilist_id=102 WHERE id=?")
        .bind(t.series_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(library_activity::try_append(&db, finished).await.unwrap());
    assert_eq!(
        row(&retry_statuses(&db, &cfg, &[t.series_id]).await.unwrap()),
        RetryTiming::NextPass
    );

    sqlx::query("INSERT INTO scheduled_task_runs(task_key,last_status,last_started_at) VALUES(?,'running',CURRENT_TIMESTAMP)")
        .bind(TASK).execute(&db).await.unwrap();
    let mut started = library_activity::Event::new(
        "current worker",
        STARTED,
        Some(t.series_id),
        &t.title,
        "Checking",
    );
    started.detail = serde_json::json!({"episode":t.episode,"airing_at":t.airing_at}).to_string();
    assert!(library_activity::try_append(&db, started).await.unwrap());
    assert_eq!(
        row(&retry_statuses(&db, &cfg, &[t.series_id]).await.unwrap()),
        RetryTiming::Checking
    );
    sqlx::query("UPDATE episode_airings SET airing_at=airing_at-60 WHERE series_id=?")
        .bind(t.series_id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        row(&retry_statuses(&db, &cfg, &[t.series_id]).await.unwrap()),
        RetryTiming::NextPass
    );

    cfg.recent_episode_search_enabled = false;
    assert_eq!(
        row(&retry_statuses(&db, &cfg, &[t.series_id]).await.unwrap()),
        RetryTiming::Blocked
    );
    cfg.recent_episode_search_enabled = true;
    sqlx::query("UPDATE episode_airings SET airing_at=unixepoch('now')+3600 WHERE series_id=?")
        .bind(t.series_id)
        .execute(&db)
        .await
        .unwrap();
    let upcoming = retry_statuses(&db, &cfg, &[t.series_id]).await.unwrap();
    assert_eq!(row(&upcoming), RetryTiming::Blocked);
    assert!(
        upcoming[&(t.series_id, t.episode)]
            .reason
            .contains("not aired")
    );
}
#[tokio::test]
async fn old_monitored_gaps_never_expire_and_airing_changes_still_cancel_demand() {
    let (db, cfg, mut t, _root) = fixture().await;
    for age_days in [10, 400] {
        t.airing_at = chrono::Utc::now().timestamp() - age_days * 86400;
        sqlx::query("UPDATE episode_airings SET airing_at=? WHERE series_id=?")
            .bind(t.airing_at)
            .bind(t.series_id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(targets(&db, &cfg).await.unwrap()[0].airing_at, t.airing_at);
        assert!(demand(&db, &cfg, &t).await.is_ok());
    }
    sqlx::query("UPDATE episode_airings SET airing_at=strftime('%s','now')+3600 WHERE series_id=?")
        .bind(t.series_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(targets(&db, &cfg).await.unwrap().is_empty());
    assert!(demand(&db, &cfg, &t).await.unwrap_err().contains("airing"));
}

#[tokio::test]
async fn cached_dates_and_finished_episode_counts_supply_historical_airing_evidence() {
    let (db, cfg, _target, _root) = fixture().await;
    sqlx::query("DELETE FROM episode_airings")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        targets(&db, &cfg).await.unwrap().is_empty(),
        "unknown airing stays held"
    );
    let sid = seed_series(&db, 201, "Historical").await;
    sqlx::query("UPDATE series SET format='TV',status='RELEASING',monitor_mode='all' WHERE id=?")
        .bind(sid)
        .execute(&db)
        .await
        .unwrap();
    crate::models::monitoring::set_episode_monitored(&db, sid, 1, true)
        .await
        .unwrap();
    sqlx::query("INSERT INTO series_episode_metadata(series_id,episode_number,aired) VALUES(?,1,'2010-01-01')")
        .bind(sid).execute(&db).await.unwrap();
    let t = targets(&db, &cfg).await.unwrap().remove(0);
    assert_eq!(t.series_id, sid);
    assert!(demand(&db, &cfg, &t).await.is_ok());
    // A timestamp in the future wins over an older date-only cache row.
    sqlx::query("INSERT INTO episode_airings(series_id,episode,airing_at) VALUES(?,1,strftime('%s','now')+3600)")
        .bind(sid).execute(&db).await.unwrap();
    assert!(targets(&db, &cfg).await.unwrap().is_empty());
    assert!(demand(&db, &cfg, &t).await.is_err());
    sqlx::query("DELETE FROM episode_airings")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM series_episode_metadata")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE series SET format='MOVIE',status='FINISHED',episodes=1 WHERE id=?")
        .bind(sid)
        .execute(&db)
        .await
        .unwrap();
    crate::models::monitoring::set_episode_monitored(&db, sid, 2, true)
        .await
        .unwrap();
    let rows = targets(&db, &cfg).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "no inferred episode beyond the finished total"
    );
    assert_eq!(rows[0].airing_at, 0);
    assert!(demand(&db, &cfg, &rows[0]).await.is_ok());
    sqlx::query("UPDATE series SET status='FINISHED_AIRING',episodes=2 WHERE id=?")
        .bind(sid)
        .execute(&db)
        .await
        .unwrap();
    let fallback = targets(&db, &cfg).await.unwrap();
    assert_eq!(
        fallback.len(),
        1,
        "a movie's second metadata episode is not a second file slot"
    );
    assert!(demand(&db, &cfg, &fallback[0]).await.is_ok());
    sqlx::query("INSERT INTO series_episode_metadata(series_id,episode_number,aired) VALUES(?,1,'2010-01-01')")
        .bind(sid).execute(&db).await.unwrap();
    for status in [
        "NOT_YET_RELEASED",
        "NOT_YET_AIRED",
        "UNRELEASED",
        "UPCOMING",
    ] {
        sqlx::query("UPDATE series SET status=? WHERE id=?")
            .bind(status)
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        assert!(targets(&db, &cfg).await.unwrap().is_empty(), "{status}");
        assert!(demand(&db, &cfg, &rows[0]).await.is_err(), "{status}");
    }
}

#[tokio::test]
async fn settings_round_trip_and_master_hold_work_without_nyaa_rss() {
    let (db, mut cfg, t, _root) = fixture().await;
    cfg.rss_enabled = false;
    cfg.recent_episode_search_days = 14;
    config::save_config(&db, &cfg).await.unwrap();
    let read = config::get_config(&db).await.unwrap().unwrap();
    assert_eq!(read.recent_episode_search_days, 14);
    assert!(enabled(&read));
    cfg.rss_master_enabled = false;
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("disabled")
    );
}

#[tokio::test]
async fn freshly_reactivated_manual_stop_wins_over_a_higher_id_old_failure() {
    let (db, cfg, t, _root) = fixture().await;
    sqlx::query("INSERT INTO grabbed_torrents(id,hash,torrent_name,series_id,episode_numbers,state,grabbed_at) VALUES(1,'stop','stopped',?,'[7]','cancelled',datetime('now'))").bind(t.series_id).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO grabbed_torrents(id,hash,torrent_name,series_id,episode_numbers,state,failure_reason,grabbed_at) VALUES(2,'fail','failed',?,'[7]','failed','client_error',datetime('now','-1 day'))").bind(t.series_id).execute(&db).await.unwrap();
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("cancelled")
    );
}

#[tokio::test]
async fn manual_protection_and_orphan_grabbed_tags_are_holds() {
    let (db, cfg, t, _root) = fixture().await;
    sqlx::query("INSERT INTO episode_quality_tags(series_id,episode_number,state,manual_override) VALUES(?,7,'failed',1)").bind(t.series_id).execute(&db).await.unwrap();
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("manual protection")
    );
    sqlx::query("UPDATE episode_quality_tags SET manual_override=0,state='grabbed'")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        demand(&db, &cfg, &t)
            .await
            .unwrap_err()
            .contains("awaiting review")
    );
}

mod execution;

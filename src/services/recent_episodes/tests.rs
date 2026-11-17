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

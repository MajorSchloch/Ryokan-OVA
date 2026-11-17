use super::*;
use crate::models::{library_activity, monitoring};
use crate::test_support::{in_memory_pool, seed_series};

async fn fixture() -> (SqlitePool, Config, Vec<UpcomingEpisode>, tempfile::TempDir) {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 101, "Test").await;
    sqlx::query("UPDATE series SET monitor_mode='all',folder_name='Test' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let cfg = Config {
        media_root: root.path().to_string_lossy().into(),
        ..Default::default()
    };
    let episodes = (6..=8)
        .map(|episode| UpcomingEpisode {
            series_id: id,
            anilist_id: 101,
            series_title: "Test".into(),
            episode,
            airing_at: 100,
            duration_minutes: 24,
            monitored: true,
            cover_url: String::new(),
            search_haystack: "test".into(),
        })
        .collect();
    (db, cfg, episodes, root)
}
#[tokio::test]
async fn actual_multi_episode_file_wins_over_tags_failures_and_monitoring() {
    let (db, cfg, mut eps, root) = fixture().await;
    let id = eps[0].series_id;
    std::fs::create_dir(root.path().join("Test")).unwrap();
    std::fs::write(root.path().join("Test/Test - S01E06-E07.mkv"), b"file").unwrap();
    monitoring::set_episode_monitored(&db, id, 6, false)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason) VALUES('hash','old',?,'[6]','failed','requirements_rejected')").bind(id).execute(&db).await.unwrap();
    let out = load(&db, &cfg, &mut eps, 200).await.unwrap();
    assert_eq!(out[&(id, 6)].kind, "available");
    assert!(!eps[0].monitored);
    assert_eq!(out[&(id, 7)].kind, "available");
    assert_eq!(out[&(id, 8)].kind, "missing");
}
#[tokio::test]
async fn no_file_is_distinct_from_unavailable_library_and_future_or_unmonitored() {
    let (db, mut cfg, mut eps, _root) = fixture().await;
    let id = eps[0].series_id;
    monitoring::set_episode_monitored(&db, id, 6, false)
        .await
        .unwrap();
    eps[2].airing_at = 201;
    let out = load(&db, &cfg, &mut eps, 200).await.unwrap();
    assert_eq!(out[&(id, 6)].kind, "unmonitored");
    assert_eq!(out[&(id, 7)].kind, "missing");
    assert_eq!(out[&(id, 8)].kind, "upcoming");
    cfg.media_root.push_str("/missing-mount");
    let out = load(&db, &cfg, &mut eps, 200).await.unwrap();
    assert!(out.values().all(|s| s.kind == "unknown"));
}
#[tokio::test]
async fn routed_pending_has_no_parent_leak_and_displays_only_its_import_hold() {
    let (db, cfg, mut eps, _root) = fixture().await;
    let id = eps[0].series_id;
    let other = seed_series(&db, 102, "Other").await;
    let gid:i64=sqlx::query_scalar("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state) VALUES('hash','pack',?,'[6]','pending') RETURNING id").bind(other).fetch_one(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO grabbed_torrent_series(grab_id,series_id,episode_numbers) VALUES(?,?,'[7]')",
    )
    .bind(gid)
    .bind(id)
    .execute(&db)
    .await
    .unwrap();
    let mut event = library_activity::Event::new(
        "held",
        "Held",
        Some(id),
        "Test",
        "Episode mapping: review required",
    );
    event.grab_id = Some(gid);
    library_activity::append(&db, event).await;
    let out = load(&db, &cfg, &mut eps, 200).await.unwrap();
    assert_eq!(out[&(id, 6)].kind, "missing");
    assert_eq!(out[&(id, 7)].kind, "pending");
    assert!(out[&(id, 7)].explanation.contains("review required"));
    assert!(!out[&(id, 6)].explanation.contains("review required"));
}
#[tokio::test]
async fn last_check_is_bound_to_exact_episode_and_current_provider_identity() {
    let (db, cfg, mut eps, _root) = fixture().await;
    let id = eps[0].series_id;
    let mut event = library_activity::Event::new(
        "check",
        recent_episodes::FINISHED,
        Some(id),
        "Test",
        "No acceptable candidate",
    );
    event.detail = "{\"episode\":7,\"outcome\":\"held\"}".into();
    library_activity::append(&db, event).await;
    let out = load(&db, &cfg, &mut eps, 200).await.unwrap();
    assert!(out[&(id, 7)].last_check.contains("No acceptable candidate"));
    assert!(out[&(id, 6)].last_check.is_empty());
    sqlx::query("UPDATE library_activity SET provider_id=999")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        load(&db, &cfg, &mut eps, 200).await.unwrap()[&(id, 7)]
            .last_check
            .is_empty()
    );
}
#[test]
fn status_filter_rejects_unknown_values() {
    assert_eq!(filter(Some(&"missing".into())), "missing");
    assert_eq!(filter(Some(&"made-up".into())), "all");
}

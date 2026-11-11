use super::*;
use crate::test_support::{in_memory_pool, seed_series};

#[tokio::test]
async fn deselected_release_cleanup_preserves_completed_pins_and_other_releases() {
    let db = in_memory_pool().await;
    let sid = seed_series(&db, 999700, "Show").await;
    let classification = crate::services::source::ClassificationResult::unknown();
    for ep in 1..=4 {
        record_grab(
            &db,
            sid,
            ep,
            &classification,
            "Selected pack",
            "Group",
            1,
            true,
        )
        .await
        .unwrap();
    }
    mark_completed(&db, sid, &[2]).await.unwrap();
    mark_grab_history_completed(&db, sid, 2, "Show - S01E02.mkv", 1)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE episode_quality_tags SET manual_override=1 WHERE series_id=? AND episode_number=3",
    )
    .bind(sid)
    .execute(&db)
    .await
    .unwrap();
    record_grab(
        &db,
        sid,
        4,
        &classification,
        "Other release",
        "Other",
        1,
        false,
    )
    .await
    .unwrap();
    clear_unselected_for_release(&db, sid, &[1, 2, 3, 4], "Selected pack")
        .await
        .unwrap();
    let tags = get_for_series(&db, sid).await.unwrap();
    assert!(!tags.contains_key(&1));
    assert_eq!(tags[&2].state, "completed");
    assert!(tags[&3].manual_override);
    assert_eq!(tags[&3].state, "grabbed");
    assert_eq!(tags[&4].release_title, "Other release");
    assert_eq!(tags[&4].state, "grabbed");
    let history: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT episode_number,release_title,state FROM episode_grab_history WHERE series_id=? ORDER BY episode_number,id"
    ).bind(sid).fetch_all(&db).await.unwrap();
    assert_eq!(
        history,
        vec![
            (1, "Selected pack".into(), "removed".into()),
            (2, "Selected pack".into(), "completed".into()),
            (3, "Selected pack".into(), "grabbed".into()),
            (4, "Selected pack".into(), "removed".into()),
            (4, "Other release".into(), "grabbed".into()),
        ]
    );
}

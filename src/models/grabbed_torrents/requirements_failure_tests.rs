use super::*;

#[tokio::test]
async fn requirements_failure_only_updates_the_inspected_pending_attempt() {
    for change in ["same", "removed", "reactivated", "retitled", "failed"] {
        let db = crate::test_support::in_memory_pool().await;
        let sid = crate::test_support::seed_series(&db, 1, "Show").await;
        let id = record_grab(&db, "same-hash", "original title", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        stamp_completed_seen(&db, id).await.unwrap();
        let completed = completed_seen_at(&db, id).await.unwrap();
        let grab = get_by_id(&db, id).await.unwrap().unwrap();
        match change {
            "same" => {}
            "removed" => {
                mark_removed(&db, id).await.unwrap();
            }
            "reactivated" => {
                // Same-second generation: all other identity fields still
                // match, but reactivation has cleared completed_seen_at.
                sqlx::query("UPDATE grabbed_torrents SET completed_seen_at=NULL WHERE id=?")
                    .bind(id)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            "retitled" => {
                sqlx::query("UPDATE grabbed_torrents SET torrent_name='new attempt' WHERE id=?")
                    .bind(id)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            "failed" => {
                mark_failed_with_reason(&db, id, "some other failure")
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before: (String, String) = sqlx::query_as(
            "SELECT state,COALESCE(failure_reason,'') FROM grabbed_torrents WHERE id=?",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        let changed = fail_pending_requirements_attempt(&db, &grab, &completed)
            .await
            .unwrap();
        assert_eq!(changed, change == "same");
        let after: (String, String) = sqlx::query_as(
            "SELECT state,COALESCE(failure_reason,'') FROM grabbed_torrents WHERE id=?",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        if change == "same" {
            assert_eq!(
                after,
                (
                    "failed".into(),
                    crate::services::post_processing::REQUIREMENTS_REJECTED_REASON.into()
                )
            );
            assert!(
                !fail_pending_requirements_attempt(&db, &grab, &completed)
                    .await
                    .unwrap(),
                "a failed attempt is not queued twice"
            );
        } else {
            assert_eq!(
                before, after,
                "stale attempt must preserve current state: {change}"
            );
        }
    }
}

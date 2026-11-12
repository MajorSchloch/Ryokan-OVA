use super::*;
use crate::services::release_numbering::ReleaseMapping;
use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
use crate::test_support::{in_memory_pool, seed_series};

async fn fixture() -> (sqlx::SqlitePool, i64, i64, ReleaseMapping) {
    let db = in_memory_pool().await;
    let owner = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    let other = seed_series(&db, 889, "Black Lagoon").await;
    let proof = lagoon_proof();
    crate::models::source_numbering::save_reviewed(&db, &proof)
        .await
        .unwrap();
    (
        db,
        owner,
        other,
        ReleaseMapping::from_bound_source(&proof).unwrap(),
    )
}

#[tokio::test]
async fn source_proof_recording_cannot_downgrade_a_reviewed_hash_to_missing_or_legacy_mapping() {
    let (db, owner, _, _) = fixture().await;
    let hash = lagoon_proof().info_hash;
    assert!(
        record_grab(&db, &hash, "Black Lagoon - 15", owner, &[15], false)
            .await
            .is_err()
    );
    let legacy: ReleaseMapping = serde_json::from_value(serde_json::json!({
        "version":1,"anilist_id":1519,"tv_series":true,"source_first":3,"source_last":3,
        "target_first":3,"source_season":2,"basis":"season_relative"
    }))
    .unwrap();
    legacy.validate().unwrap();
    assert!(
        record_grab_with_release_mapping(
            &db,
            &hash,
            "Black Lagoon - 15",
            owner,
            &[3],
            false,
            Some(&legacy)
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn source_proof_recording_rejects_wrong_owner_and_hash_before_insert() {
    let (db, owner, other, mapping) = fixture().await;
    let hash = lagoon_proof().info_hash;
    assert!(
        record_grab_with_release_mapping(
            &db,
            &hash,
            "Black Lagoon",
            other,
            &[1],
            true,
            Some(&mapping)
        )
        .await
        .is_err()
    );
    assert!(
        record_grab_with_release_mapping(
            &db,
            &"f".repeat(40),
            "Black Lagoon",
            owner,
            &[1],
            true,
            Some(&mapping)
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn source_proof_rejected_reactivation_preserves_imported_decision_and_state() {
    let (db, owner, other, mapping) = fixture().await;
    let hash = lagoon_proof().info_hash;
    let id = record_grab_with_release_mapping(
        &db,
        &hash,
        "Reviewed source",
        owner,
        &mapping.episodes(),
        true,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET state='imported',client_content_path='kept/path',imported_at='2026-01-01 00:00:00' WHERE id=?")
        .bind(id).execute(&db).await.unwrap();
    let read = || {
        sqlx::query_as::<_, (i64, String, String, String, String, String)>(
        "SELECT series_id,state,episode_numbers,release_mapping,client_content_path,imported_at FROM grabbed_torrents WHERE id=?").bind(id)
    };
    let before = read().fetch_one(&db).await.unwrap();
    assert!(
        record_grab(&db, &hash, "Changed title", owner, &[3], false)
            .await
            .is_err()
    );
    assert!(
        record_grab_with_release_mapping(
            &db,
            &hash,
            "Wrong owner",
            other,
            &[3],
            false,
            Some(&mapping)
        )
        .await
        .is_err()
    );
    let subset = lagoon_proof()
        .narrow_to(&[lagoon_files()[14].clone()])
        .unwrap();
    let subset = ReleaseMapping::from_bound_source(&subset).unwrap();
    assert!(
        record_grab_with_release_mapping(
            &db,
            &hash,
            "Narrowed retry",
            owner,
            &[3],
            false,
            Some(&subset)
        )
        .await
        .is_err()
    );
    assert_eq!(read().fetch_one(&db).await.unwrap(), before);
    // A lost review cannot silently remove the historical bound decision.
    sqlx::query("DELETE FROM source_numbering_decisions")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        record_grab(&db, &hash, "Missing review", owner, &[3], false)
            .await
            .is_err()
    );
    assert_eq!(read().fetch_one(&db).await.unwrap(), before);
}

#[tokio::test]
async fn source_proof_pending_history_cannot_downgrade_after_the_review_is_lost() {
    let (db, owner, _, mapping) = fixture().await;
    let hash = lagoon_proof().info_hash;
    let id = record_grab_with_release_mapping(
        &db,
        &hash,
        "Reviewed pending source",
        owner,
        &mapping.episodes(),
        true,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    let before: (String, String, String) = sqlx::query_as(
        "SELECT state,episode_numbers,release_mapping FROM grabbed_torrents WHERE id=?",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("DELETE FROM source_numbering_decisions")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        record_grab(&db, &hash, "Unresolved retry", owner, &[3], false)
            .await
            .is_err()
    );
    let after: (String, String, String) = sqlx::query_as(
        "SELECT state,episode_numbers,release_mapping FROM grabbed_torrents WHERE id=?",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(after, before);
}

use super::*;
use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
use crate::services::source_numbering::{AutomaticEvidence, ProofProvenance};
use crate::test_support::{in_memory_pool, seed_series};

fn mapping(episode: usize) -> ReleaseMapping {
    let proof = lagoon_proof()
        .narrow_to(&lagoon_files()[episode + 11..episode + 12])
        .unwrap();
    ReleaseMapping::from_bound_source(&proof).unwrap()
}

async fn insert(db: &SqlitePool, owner: i64, state: &str, json: Option<&str>) {
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,release_mapping) VALUES(?,'Existing source',?,'[4]',?,?)")
        .bind(lagoon_proof().info_hash).bind(owner).bind(state).bind(json).execute(db).await.unwrap();
}

#[tokio::test]
async fn bound_source_compatibility_preserves_pending_and_imported_other_selection() {
    for state in ["pending", "imported"] {
        let db = in_memory_pool().await;
        let owner = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
        let old = serde_json::to_string(&mapping(4)).unwrap();
        insert(&db, owner, state, Some(&old)).await;
        let reason =
            ensure_automatic_compatibility(&db, &lagoon_proof().info_hash, 1519, Some(&mapping(5)))
                .await
                .unwrap_err();
        assert!(
            reason.contains("different or unresolved saved source assignment"),
            "{reason}"
        );
        let saved: (String, String, String) =
            sqlx::query_as("SELECT state,episode_numbers,release_mapping FROM grabbed_torrents")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(saved, (state.into(), "[4]".into(), old));
    }
}

#[tokio::test]
async fn bound_source_compatibility_matches_durable_receipt_and_formatting_predicate() {
    let db = in_memory_pool().await;
    let owner = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    let mapping = mapping(4);
    let old = serde_json::to_string(&mapping).unwrap();
    insert(&db, owner, "imported", Some(&old)).await;
    ensure_automatic_compatibility(
        &db,
        &lagoon_proof().info_hash.to_uppercase(),
        1519,
        Some(&mapping),
    )
    .await
    .unwrap();
    let mut proof = mapping.bound_source().unwrap().clone();
    proof.version = 2;
    proof.provenance = Some(ProofProvenance::AutomaticUnique {
        evidence: AutomaticEvidence {
            verifier_version: 1,
            scope_fingerprint: "a".repeat(64),
            publication_url: "https://nyaa.si/view/1428087".into(),
        },
    });
    let refreshed = ReleaseMapping::from_bound_source(&proof).unwrap();
    assert!(
        ensure_automatic_compatibility(&db, &proof.info_hash, 1519, Some(&refreshed))
            .await
            .is_err(),
        "a changed receipt must not pass the existing durable veto"
    );
    let after: String = sqlx::query_scalar("SELECT release_mapping FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(after, old, "read-only preflight cannot rewrite provenance");
    let pretty = serde_json::to_string_pretty(&mapping).unwrap();
    sqlx::query("UPDATE grabbed_torrents SET release_mapping=?")
        .bind(pretty)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        ensure_automatic_compatibility(&db, &proof.info_hash, 1519, Some(&mapping))
            .await
            .is_err(),
        "the current durable predicate also holds formatting changes"
    );
}

#[tokio::test]
async fn bound_source_compatibility_holds_foreign_or_unresolved_active_assignments() {
    for case in ["foreign", "missing", "malformed", "ordinary"] {
        let db = in_memory_pool().await;
        let owner = seed_series(
            &db,
            if case == "foreign" { 889 } else { 1519 },
            "Existing owner",
        )
        .await;
        let bound = serde_json::to_string(&mapping(4)).unwrap();
        let ordinary = r#"{"version":1,"anilist_id":1519,"tv_series":true,"source_first":4,"source_last":4,"target_first":4,"source_season":2,"basis":"season_relative"}"#;
        let json = match case {
            "missing" => None,
            "malformed" => Some("{"),
            "ordinary" => Some(ordinary),
            _ => Some(bound.as_str()),
        };
        insert(&db, owner, "imported", json).await;
        assert!(
            ensure_automatic_compatibility(&db, &lagoon_proof().info_hash, 1519, Some(&mapping(4)))
                .await
                .is_err(),
            "{case}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM grabbed_torrents WHERE state='imported'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1, "{case}");
    }
}

#[tokio::test]
async fn bound_source_compatibility_ignores_inactive_history_and_unbound_candidates() {
    let db = in_memory_pool().await;
    let owner = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    let old = serde_json::to_string(&mapping(4)).unwrap();
    for state in ["failed", "removed"] {
        insert(&db, owner, state, Some(&old)).await;
    }
    ensure_automatic_compatibility(&db, &lagoon_proof().info_hash, 1519, Some(&mapping(5)))
        .await
        .unwrap();
    insert(&db, owner, "pending", None).await;
    ensure_automatic_compatibility(&db, &lagoon_proof().info_hash, 1519, None)
        .await
        .unwrap();
    assert!(
        ensure_automatic_compatibility(&db, &lagoon_proof().info_hash, 1519, Some(&mapping(5)))
            .await
            .is_err(),
        "inactive history cannot conceal a conflicting active row"
    );
}

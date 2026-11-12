use super::*;
use crate::services::source_numbering::tests::{lagoon_domain, lagoon_files, lagoon_proof};
use crate::test_support::{in_memory_pool, seed_series};

#[tokio::test]
async fn exact_source_reviews_are_idempotent_and_conflicts_do_not_replace_them() {
    let db = in_memory_pool().await;
    let proof = lagoon_proof();
    let first = save_reviewed(&db, &proof).await.unwrap();
    let second = save_reviewed(&db, &proof).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.fingerprint, second.fingerprint);
    let saved = lookup_exact(&db, &proof.info_hash.to_ascii_uppercase(), 1519)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.proof, proof);
    assert!(
        lookup_exact(&db, &"f".repeat(40), 1519)
            .await
            .unwrap()
            .is_none()
    );
    assert!(lookup_exact(&db, &proof.info_hash, 889).await.is_err());
    assert!(lookup_exact(&db, "", 1519).await.unwrap().is_none());
    let subset = proof.narrow_to(&[lagoon_files()[14].clone()]).unwrap();
    assert!(subset.is_within_review(&saved.proof));
    assert!(
        save_reviewed(&db, &subset).await.is_err(),
        "an acquisition subset does not rewrite the full review"
    );
    assert_eq!(
        lookup_exact(&db, &proof.info_hash, 1519)
            .await
            .unwrap()
            .unwrap()
            .proof,
        proof
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_numbering_decisions")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    sqlx::query("UPDATE source_numbering_decisions SET fingerprint='changed'")
        .execute(&db)
        .await
        .unwrap();
    assert!(lookup_exact(&db, &proof.info_hash, 1519).await.is_err());
}

#[tokio::test]
async fn existing_migration_replay_prevents_a_new_source_review() {
    let db = in_memory_pool().await;
    let proof = lagoon_proof();
    let sid = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    let id = crate::models::grabbed_torrents::record_grab(
        &db,
        &proof.info_hash,
        "Existing reviewed source",
        sid,
        &[3],
        false,
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET library_migration_bridge='{}' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        save_reviewed(&db, &proof)
            .await
            .unwrap_err()
            .contains("library migration")
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_numbering_decisions")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

fn candidate(hash: &str) -> crate::services::nyaa::SearchResult {
    serde_json::from_value(serde_json::json!({
        "title":"[Legion] Black Lagoon - 15 [BD x264 1080p 10bit FLAC][Dual Audio]",
        "link":"https://nyaa.si/view/1428087","magnet":format!("magnet:?xt=urn:btih:{hash}"),"torrent":"",
        "size":"83.8 GiB","size_bytes":1,"seeders":1,"leechers":0,"downloads":0,
        "group":"Legion","resolution":"1080","quality_label":"","source":"","web_kind":"",
        "is_remux":false,"is_bdmv":false,"is_batch":false,"is_trusted":false,"score":0,"info_hash":hash
    })).unwrap()
}

#[tokio::test]
async fn early_source_lookup_requires_exact_saved_proof_and_never_reuses_a_title() {
    use crate::services::auto_search::SearchTarget;
    use crate::services::release_numbering::apply_reviewed_source;
    let db = in_memory_pool().await;
    let proof = lagoon_proof();
    let mut unbound = candidate(&proof.info_hash);
    assert!(
        !apply_reviewed_source(&db, 1519, &SearchTarget::Episode(3), &mut unbound)
            .await
            .unwrap()
    );
    save_reviewed(&db, &proof).await.unwrap();
    let mut bound = candidate(&proof.info_hash);
    assert!(
        apply_reviewed_source(&db, 1519, &SearchTarget::Episode(3), &mut bound)
            .await
            .unwrap()
    );
    assert!(
        bound.replay_episodes.is_none(),
        "new proof is not a library migration replay"
    );
    assert!(bound.is_batch);
    assert_eq!(bound.release_mapping.as_ref().unwrap().offset(), 12);
    let mut other = candidate(&"f".repeat(40));
    assert!(
        !apply_reviewed_source(&db, 1519, &SearchTarget::Episode(3), &mut other)
            .await
            .unwrap(),
        "an identical old title does not transfer source proof"
    );
    let mut wrong_slot = candidate(&proof.info_hash);
    assert!(
        apply_reviewed_source(&db, 1519, &SearchTarget::Episode(13), &mut wrong_slot)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM source_numbering_decisions")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        apply_reviewed_source(&db, 1519, &SearchTarget::Episode(3), &mut bound)
            .await
            .is_err(),
        "a required proof cannot disappear into heuristic fallback"
    );
}

async fn historical_bound_source(db: &SqlitePool, proof: &BoundSourceProof) -> i64 {
    let owner = seed_series(db, proof.target_anilist_id, "Historical source owner").await;
    let saved = save_reviewed(db, proof).await.unwrap();
    let mapping =
        crate::services::release_numbering::ReleaseMapping::from_bound_source(proof).unwrap();
    let id = crate::models::grabbed_torrents::record_grab_with_release_mapping(
        db,
        &proof.info_hash,
        "Historical source",
        owner,
        &proof.episodes(),
        proof.selected_files.len() > 1,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET state='imported' WHERE id=?")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM source_numbering_decisions WHERE id=?")
        .bind(saved.id)
        .execute(db)
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn lost_review_lookup_holds_historical_proofs_before_heuristic_fallback() {
    let db = in_memory_pool().await;
    let proof = lagoon_proof();
    historical_bound_source(&db, &proof).await;
    assert!(lookup_exact(&db, &proof.info_hash, 1519).await.is_err());
    assert!(lookup_exact(&db, &proof.info_hash, 889).await.is_err());
    assert!(lookup_hash(&db, &proof.info_hash).await.is_err());
    assert!(lookup_hash(&db, &"f".repeat(40)).await.unwrap().is_empty());
    let mut fresh_candidate = candidate(&proof.info_hash);
    assert!(
        crate::services::release_numbering::apply_reviewed_source(
            &db,
            1519,
            &crate::services::auto_search::SearchTarget::Episode(3),
            &mut fresh_candidate
        )
        .await
        .is_err(),
        "a fresh search result must not need an in-memory proof to hold"
    );
}

#[tokio::test]
async fn lost_review_recovery_preserves_historical_domain_manifest_and_selections() {
    let db = in_memory_pool().await;
    let full = lagoon_proof();
    let files = lagoon_files();
    let old = full.narrow_to(&[files[14].clone()]).unwrap();
    historical_bound_source(&db, &old).await;
    let excluding_old = full.narrow_to(&[files[15].clone()]).unwrap();
    assert!(save_reviewed(&db, &excluding_old).await.is_err());

    let mut different_manifest = files.clone();
    different_manifest.push("publisher-readme.txt".into());
    let changed_publication = crate::services::source_numbering::prepare_review(
        lagoon_domain(),
        &full.info_hash,
        &different_manifest,
        1519,
    )
    .unwrap()
    .confirm(&[files[14].clone()])
    .unwrap();
    assert!(save_reviewed(&db, &changed_publication).await.is_err());

    let mut changed_domain = lagoon_domain();
    changed_domain
        .entries
        .iter_mut()
        .find(|entry| entry.anilist_id == 1519)
        .unwrap()
        .provider_episodes = Some((1..=11).collect());
    // A valid, independently fingerprinted domain still cannot replace the
    // immutable domain of an older grab carrying the same exact torrent.
    changed_domain.rules_fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&(
            changed_domain.version,
            changed_domain.root_anilist_id,
            &changed_domain.root_title,
            &changed_domain.root_aliases,
            changed_domain.source_season,
            &changed_domain.rules,
            &changed_domain.entries,
        ))
        .unwrap(),
    ));
    let conflicting = crate::services::source_numbering::prepare_review(
        changed_domain,
        &full.info_hash,
        &files,
        1519,
    )
    .unwrap()
    .confirm(&[files[14].clone()])
    .unwrap();
    assert!(save_reviewed(&db, &conflicting).await.is_err());
    assert!(lookup_hash(&db, &full.info_hash).await.is_err());

    // A separate destination uses a target-specific publication digest, but
    // the exact same hash/domain/full listing permits its own finite range.
    let first_season = crate::services::source_numbering::prepare_review(
        lagoon_domain(),
        &full.info_hash,
        &files,
        889,
    )
    .unwrap()
    .confirm(&files[..12])
    .unwrap();
    save_reviewed(&db, &first_season).await.unwrap();
    let restored = save_reviewed(&db, &full).await.unwrap();
    assert!(old.is_within_review(&restored.proof));
    assert_eq!(lookup_hash(&db, &full.info_hash).await.unwrap().len(), 2);
    assert_eq!(
        lookup_exact(&db, &full.info_hash, 1519)
            .await
            .unwrap()
            .unwrap()
            .proof,
        full
    );
}

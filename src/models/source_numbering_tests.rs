use super::*;
use crate::services::source_numbering::tests::{lagoon_domain, lagoon_files, lagoon_proof};
use crate::services::source_numbering::{AutomaticEvidence, ProofProvenance};
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

fn automatic_proof() -> BoundSourceProof {
    let mut proof = lagoon_proof();
    proof.version = 2;
    proof.provenance = Some(ProofProvenance::AutomaticUnique {
        evidence: AutomaticEvidence {
            verifier_version: 1,
            scope_fingerprint: "e".repeat(64),
            publication_url: "https://nyaa.si/view/1428087".into(),
        },
    });
    proof.validate().unwrap();
    proof
}

async fn decisions_snapshot(db: &SqlitePool) -> Vec<serde_json::Value> {
    sqlx::query("SELECT * FROM source_numbering_decisions ORDER BY id")
        .fetch_all(db)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            serde_json::json!({
                "id":row.get::<i64,_>("id"),
                "info_hash":row.get::<String,_>("info_hash"),
                "target_anilist_id":row.get::<i64,_>("target_anilist_id"),
                "root_anilist_id":row.get::<i64,_>("root_anilist_id"),
                "fingerprint":row.get::<String,_>("fingerprint"),
                "proof_json":row.get::<String,_>("proof_json"),
                "created_at":row.get::<String,_>("created_at"),
            })
        })
        .collect()
}

#[tokio::test]
async fn automatic_save_is_idempotent_and_keeps_its_explicit_origin_in_grab_proofs() {
    let db = in_memory_pool().await;
    let proof = automatic_proof();
    let first = save_automatic(&db, &proof).await.unwrap();
    let before = decisions_snapshot(&db).await;
    let second = save_automatic(&db, &proof).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(before, decisions_snapshot(&db).await);
    let saved = lookup_exact(&db, &proof.info_hash, 1519)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.proof.origin().unwrap(), ProofOrigin::AutomaticUnique);
    let subset = proof.narrow_to(&[lagoon_files()[14].clone()]).unwrap();
    assert_eq!(subset.provenance, proof.provenance);
    assert!(subset.is_within_review(&proof));
    let mapping =
        crate::services::release_numbering::ReleaseMapping::from_bound_source(&subset).unwrap();
    let json = serde_json::to_string(&mapping).unwrap();
    let restored: crate::services::release_numbering::ReleaseMapping =
        serde_json::from_str(&json).unwrap();
    restored.validate().unwrap();
    assert_eq!(
        restored.bound_source().unwrap().origin().unwrap(),
        ProofOrigin::AutomaticUnique
    );
    let owner = seed_series(&db, 1519, "Automatic source owner").await;
    let id = crate::models::grabbed_torrents::record_grab_with_release_mapping(
        &db,
        &proof.info_hash,
        "Automatic source",
        owner,
        &[3],
        true,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT release_mapping FROM grabbed_torrents WHERE id=?")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(stored, json);
}

#[tokio::test]
async fn human_serialization_and_complete_row_stay_unchanged_when_automatic_evidence_arrives() {
    #[derive(serde::Serialize)]
    struct LegacyProof<'a> {
        version: u32,
        info_hash: &'a str,
        target_anilist_id: i64,
        domain: &'a crate::services::source_numbering::SourceDomain,
        publication_fingerprint: &'a str,
        published_files: &'a [String],
        selected_files: &'a [crate::services::source_numbering::BoundFile],
    }
    let db = in_memory_pool().await;
    let human = lagoon_proof();
    let legacy = LegacyProof {
        version: human.version,
        info_hash: &human.info_hash,
        target_anilist_id: human.target_anilist_id,
        domain: &human.domain,
        publication_fingerprint: &human.publication_fingerprint,
        published_files: &human.published_files,
        selected_files: &human.selected_files,
    };
    let legacy_json = serde_json::to_string(&legacy).unwrap();
    assert_eq!(serde_json::to_string(&human).unwrap(), legacy_json);
    let saved = save_reviewed(&db, &human).await.unwrap();
    assert_eq!(
        saved.fingerprint,
        hex::encode(Sha256::digest(legacy_json.as_bytes()))
    );
    let before = decisions_snapshot(&db).await;
    let automatic = save_automatic(&db, &automatic_proof()).await.unwrap();
    assert_eq!(automatic.id, saved.id);
    assert_eq!(automatic.fingerprint, saved.fingerprint);
    assert_eq!(automatic.proof, human);
    assert_eq!(
        automatic.proof.origin().unwrap(),
        ProofOrigin::HumanReviewed
    );
    assert_eq!(before, decisions_snapshot(&db).await);
}

#[tokio::test]
async fn compatible_human_precedence_is_applied_before_historical_origin_checks() {
    let db = in_memory_pool().await;
    let human = lagoon_proof();
    let saved = save_reviewed(&db, &human).await.unwrap();
    let owner = seed_series(&db, 1519, "Human source owner").await;
    let narrow = human.narrow_to(&[lagoon_files()[14].clone()]).unwrap();
    let mapping =
        crate::services::release_numbering::ReleaseMapping::from_bound_source(&narrow).unwrap();
    crate::models::grabbed_torrents::record_grab_with_release_mapping(
        &db,
        &human.info_hash,
        "Existing human source",
        owner,
        &[3],
        true,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    let before = decisions_snapshot(&db).await;
    let returned = save_automatic(&db, &automatic_proof()).await.unwrap();
    assert_eq!(returned.id, saved.id);
    assert_eq!(returned.proof.origin().unwrap(), ProofOrigin::HumanReviewed);
    assert_eq!(returned.proof, human);
    assert_eq!(before, decisions_snapshot(&db).await);
}

#[tokio::test]
async fn missing_human_decision_cannot_be_recreated_as_automatic_from_historical_evidence() {
    let db = in_memory_pool().await;
    let human = lagoon_proof();
    let grab = historical_bound_source(&db, &human).await;
    let before: String =
        sqlx::query_scalar("SELECT release_mapping FROM grabbed_torrents WHERE id=?")
            .bind(grab)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(save_automatic(&db, &automatic_proof()).await.is_err());
    assert!(decisions_snapshot(&db).await.is_empty());
    let after: String =
        sqlx::query_scalar("SELECT release_mapping FROM grabbed_torrents WHERE id=?")
            .bind(grab)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert!(lookup_exact(&db, &human.info_hash, 1519).await.is_err());
}

#[tokio::test]
async fn automatic_scope_url_selection_and_origin_conflicts_never_replace_saved_rows() {
    let db = in_memory_pool().await;
    let proof = automatic_proof();
    save_automatic(&db, &proof).await.unwrap();
    let before = decisions_snapshot(&db).await;
    let mut different_scope = proof.clone();
    let ProofProvenance::AutomaticUnique { evidence } =
        different_scope.provenance.as_mut().unwrap();
    evidence.scope_fingerprint = "d".repeat(64);
    let mut different_url = proof.clone();
    let ProofProvenance::AutomaticUnique { evidence } = different_url.provenance.as_mut().unwrap();
    evidence.publication_url = "https://nyaa.si/view/1428088".into();
    for conflicting in [
        different_scope,
        different_url,
        proof.narrow_to(&[lagoon_files()[14].clone()]).unwrap(),
    ] {
        assert!(save_automatic(&db, &conflicting).await.is_err());
        assert_eq!(before, decisions_snapshot(&db).await);
    }
    assert!(
        save_reviewed(&db, &lagoon_proof()).await.is_err(),
        "an explicit human endpoint must not relabel an automatic row"
    );
    assert_eq!(before, decisions_snapshot(&db).await);
}

#[tokio::test]
async fn save_entry_points_reject_wrong_or_future_proof_origins_before_any_write() {
    let db = in_memory_pool().await;
    assert!(save_automatic(&db, &lagoon_proof()).await.is_err());
    assert!(save_reviewed(&db, &automatic_proof()).await.is_err());
    let automatic = automatic_proof();
    let mut future_version = automatic.clone();
    future_version.version = 3;
    let mut missing_provenance = automatic.clone();
    missing_provenance.provenance = None;
    let mut future_verifier = automatic.clone();
    let ProofProvenance::AutomaticUnique { evidence } =
        future_verifier.provenance.as_mut().unwrap();
    evidence.verifier_version = 2;
    let mut invalid_scope = automatic.clone();
    let ProofProvenance::AutomaticUnique { evidence } = invalid_scope.provenance.as_mut().unwrap();
    evidence.scope_fingerprint = "E".repeat(64);
    let mut invalid_url = automatic;
    let ProofProvenance::AutomaticUnique { evidence } = invalid_url.provenance.as_mut().unwrap();
    evidence.publication_url = "file:///untrusted".into();
    for invalid in [
        future_version,
        missing_provenance,
        future_verifier,
        invalid_scope,
        invalid_url,
    ] {
        assert!(save_automatic(&db, &invalid).await.is_err());
    }
    assert!(decisions_snapshot(&db).await.is_empty());
}

#[tokio::test]
async fn concurrent_automatic_and_human_saves_preserve_an_existing_human_decision() {
    let db = in_memory_pool().await;
    let human = lagoon_proof();
    save_reviewed(&db, &human).await.unwrap();
    let before = decisions_snapshot(&db).await;
    let automatic = automatic_proof();
    let (manual, automatic) =
        tokio::join!(save_reviewed(&db, &human), save_automatic(&db, &automatic));
    assert_eq!(manual.unwrap().proof, human);
    assert_eq!(automatic.unwrap().proof, human);
    assert_eq!(before, decisions_snapshot(&db).await);
}

#[tokio::test]
async fn unknown_persisted_origin_holds_lookup_and_both_writers_without_legacy_fallback() {
    let db = in_memory_pool().await;
    let proof = automatic_proof();
    save_automatic(&db, &proof).await.unwrap();
    let mut value = serde_json::to_value(&proof).unwrap();
    value["provenance"]["origin"] = serde_json::json!("future_origin");
    sqlx::query("UPDATE source_numbering_decisions SET proof_json=?")
        .bind(value.to_string())
        .execute(&db)
        .await
        .unwrap();
    let before = decisions_snapshot(&db).await;
    assert!(has_bound_history(&db, &proof.info_hash).await.unwrap());
    assert!(lookup_exact(&db, &proof.info_hash, 1519).await.is_err());
    assert!(lookup_hash(&db, &proof.info_hash).await.is_err());
    assert!(save_automatic(&db, &proof).await.is_err());
    assert!(save_reviewed(&db, &lagoon_proof()).await.is_err());
    assert_eq!(before, decisions_snapshot(&db).await);
}

//! Immutable exact-torrent source-numbering decisions.

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::services::source_numbering::{BoundSourceProof, ProofOrigin};

#[cfg(test)]
#[path = "source_numbering_tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub struct SavedDecision {
    pub id: i64,
    pub fingerprint: String,
    pub proof: BoundSourceProof,
}

/// Human approval and automatic verification have distinct entry points.
/// Neither writer may silently relabel an existing source decision.
pub async fn save_reviewed(
    db: &SqlitePool,
    proof: &BoundSourceProof,
) -> Result<SavedDecision, String> {
    save_decision(db, proof, ProofOrigin::HumanReviewed).await
}

/// Persist only a uniquely verified automatic proof. An identical saved human
/// decision takes precedence, without changing its bytes, identity or origin.
pub async fn save_automatic(
    db: &SqlitePool,
    proof: &BoundSourceProof,
) -> Result<SavedDecision, String> {
    save_decision(db, proof, ProofOrigin::AutomaticUnique).await
}

fn decode_saved(row: &sqlx::sqlite::SqliteRow, hash: &str) -> Result<SavedDecision, String> {
    let proof: BoundSourceProof = serde_json::from_str(row.get("proof_json"))
        .map_err(|error| format!("Episode mapping: invalid saved source proof: {error}"))?;
    proof.validate()?;
    let json = serde_json::to_string(&proof).map_err(|error| error.to_string())?;
    let fingerprint = hex::encode(Sha256::digest(json.as_bytes()));
    if proof.info_hash != hash
        || row.get::<String, _>("info_hash") != hash
        || row.get::<i64, _>("target_anilist_id") != proof.target_anilist_id
        || row.get::<i64, _>("root_anilist_id") != proof.domain.root_anilist_id
        || row.get::<String, _>("fingerprint") != fingerprint
    {
        return Err("Episode mapping: saved source decision identity changed".into());
    }
    Ok(SavedDecision {
        id: row.get("id"),
        fingerprint,
        proof,
    })
}

fn source_basis_matches(left: &BoundSourceProof, right: &BoundSourceProof) -> Result<bool, String> {
    Ok(left.info_hash == right.info_hash
        && left.domain == right.domain
        && left.published_files == right.published_files
        && (!(left.origin()? == ProofOrigin::AutomaticUnique
            && right.origin()? == ProofOrigin::AutomaticUnique)
            || left.provenance == right.provenance))
}

async fn save_decision(
    db: &SqlitePool,
    proof: &BoundSourceProof,
    required_origin: ProofOrigin,
) -> Result<SavedDecision, String> {
    proof.validate()?;
    if proof.origin()? != required_origin {
        return Err(
            "Episode mapping: source proof origin does not match its save operation".into(),
        );
    }
    let json = serde_json::to_string(proof).map_err(|error| error.to_string())?;
    let fingerprint = hex::encode(Sha256::digest(json.as_bytes()));
    let mut tx = db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|error| error.to_string())?;
    let migration: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM grabbed_torrents WHERE hash=? COLLATE NOCASE AND library_migration_bridge IS NOT NULL)")
        .bind(&proof.info_hash).fetch_one(&mut *tx).await.map_err(|error| error.to_string())?;
    if migration {
        return Err(
            "Episode mapping: a reviewed library migration already owns this exact source".into(),
        );
    }
    let existing = sqlx::query("SELECT id,info_hash,target_anilist_id,root_anilist_id,proof_json,fingerprint FROM source_numbering_decisions WHERE info_hash=? COLLATE NOCASE ORDER BY id")
        .bind(&proof.info_hash).fetch_all(&mut *tx).await.map_err(|error| error.to_string())?;
    let mut same = None;
    for row in existing {
        let old = decode_saved(&row, &proof.info_hash)?;
        if !source_basis_matches(&old.proof, proof)? {
            return Err(
                "Episode mapping: this exact source has a conflicting numbering review".into(),
            );
        }
        if old.proof.target_anilist_id == proof.target_anilist_id {
            let human_wins = required_origin == ProofOrigin::AutomaticUnique
                && old.proof.origin()? == ProofOrigin::HumanReviewed
                && old.proof.same_geometry(proof);
            if old.proof != *proof && !human_wins {
                return Err(
                    "Episode mapping: this source entry has a conflicting reviewed selection"
                        .into(),
                );
            }
            same = Some(old);
        }
    }
    // Resolve compatible human precedence before inspecting historical grab
    // proofs. History must be compared with the unchanged human decision that
    // will actually be returned, not with a discarded automatic proposal.
    let effective = same.as_ref().map(|saved| &saved.proof).unwrap_or(proof);
    let historical: Vec<String> = sqlx::query_scalar(
        "SELECT release_mapping FROM grabbed_torrents WHERE hash=? COLLATE NOCASE AND release_mapping IS NOT NULL",
    ).bind(&proof.info_hash).fetch_all(&mut *tx).await.map_err(|error| error.to_string())?;
    for json in historical {
        let value: serde_json::Value = serde_json::from_str(&json).map_err(|error| {
            format!("Episode mapping: invalid historical source decision: {error}")
        })?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(2)
            && value
                .get("bound_source")
                .is_none_or(|source| source.is_null())
        {
            continue;
        }
        let mapping: crate::services::release_numbering::ReleaseMapping =
            serde_json::from_value(value).map_err(|error| {
                format!("Episode mapping: invalid historical source proof: {error}")
            })?;
        mapping.validate()?;
        let old = mapping
            .bound_source()
            .ok_or("Episode mapping: historical source decision has no exact source proof")?;
        if !source_basis_matches(old, effective)? {
            return Err(
                "Episode mapping: this exact source conflicts with its historical numbering evidence"
                    .into(),
            );
        }
        if old.target_anilist_id == effective.target_anilist_id && !old.is_within_review(effective)
        {
            return Err(
                "Episode mapping: a historical source selection is outside this review".into(),
            );
        }
    }
    let saved = match same {
        Some(saved) => saved,
        None => {
            let id = sqlx::query_scalar("INSERT INTO source_numbering_decisions(info_hash,target_anilist_id,root_anilist_id,fingerprint,proof_json) VALUES(?,?,?,?,?) RETURNING id")
                .bind(&proof.info_hash).bind(proof.target_anilist_id).bind(proof.domain.root_anilist_id)
                .bind(&fingerprint).bind(&json).fetch_one(&mut *tx).await.map_err(|error| error.to_string())?;
            SavedDecision {
                id,
                fingerprint,
                proof: proof.clone(),
            }
        }
    };
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(saved)
}

pub async fn lookup_exact(
    db: &SqlitePool,
    info_hash: &str,
    target_anilist_id: i64,
) -> Result<Option<SavedDecision>, String> {
    let Ok(hash) = crate::services::source_numbering::normalized_hash(info_hash) else {
        // Unidentified and non-BT candidates retain ordinary numbering holds.
        return Ok(None);
    };
    let row = sqlx::query("SELECT id,info_hash,target_anilist_id,root_anilist_id,fingerprint,proof_json FROM source_numbering_decisions WHERE info_hash=? COLLATE NOCASE ORDER BY (target_anilist_id=?) DESC,id DESC LIMIT 1")
        .bind(&hash).bind(target_anilist_id).fetch_optional(db).await.map_err(|error| error.to_string())?;
    let Some(row) = row else {
        if has_bound_history(db, &hash).await? {
            return Err(
                "Episode mapping: historical source proof requires its restored numbering review"
                    .into(),
            );
        }
        return Ok(None);
    };
    if row.get::<i64, _>("target_anilist_id") != target_anilist_id {
        return Err("Episode mapping: reviewed source has no decision for this release".into());
    }
    decode_saved(&row, &hash).map(Some)
}

pub(crate) async fn lookup_hash(
    db: &SqlitePool,
    info_hash: &str,
) -> Result<Vec<SavedDecision>, String> {
    let Ok(hash) = crate::services::source_numbering::normalized_hash(info_hash) else {
        return Ok(Vec::new());
    };
    let targets: Vec<i64> = sqlx::query_scalar("SELECT target_anilist_id FROM source_numbering_decisions WHERE info_hash=? COLLATE NOCASE ORDER BY target_anilist_id")
        .bind(&hash).fetch_all(db).await.map_err(|error| error.to_string())?;
    if targets.is_empty() && has_bound_history(db, &hash).await? {
        return Err(
            "Episode mapping: historical source proof requires its restored numbering review"
                .into(),
        );
    }
    let mut decisions = Vec::new();
    for target in targets {
        if let Some(saved) = lookup_exact(db, &hash, target).await? {
            decisions.push(saved);
        }
    }
    Ok(decisions)
}

/// A reviewed torrent must never return to a title/offset-only grab path,
/// including after its review row has been lost from a restored database.
pub async fn has_bound_history(db: &SqlitePool, info_hash: &str) -> Result<bool, String> {
    let mut connection = db.acquire().await.map_err(|error| error.to_string())?;
    has_bound_history_in_connection(&mut connection, info_hash)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn has_bound_history_in_connection(
    db: &mut sqlx::SqliteConnection,
    info_hash: &str,
) -> Result<bool, sqlx::Error> {
    let Ok(hash) = crate::services::source_numbering::normalized_hash(info_hash) else {
        return Ok(false);
    };
    let reviewed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM source_numbering_decisions WHERE info_hash=? COLLATE NOCASE)",
    )
    .bind(&hash)
    .fetch_one(&mut *db)
    .await?;
    if reviewed {
        return Ok(true);
    }
    let historical: Vec<String> = sqlx::query_scalar(
        "SELECT release_mapping FROM grabbed_torrents WHERE hash=? COLLATE NOCASE AND release_mapping IS NOT NULL",
    ).bind(&hash).fetch_all(&mut *db).await?;
    for json in historical {
        let old: serde_json::Value =
            serde_json::from_str(&json).map_err(|error| sqlx::Error::Decode(error.into()))?;
        if old.get("version").and_then(serde_json::Value::as_u64) == Some(2)
            || old
                .get("bound_source")
                .is_some_and(|proof| !proof.is_null())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

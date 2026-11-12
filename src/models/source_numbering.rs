//! Immutable exact-torrent source-numbering reviews.

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::services::source_numbering::BoundSourceProof;

#[cfg(test)]
#[path = "source_numbering_tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub struct SavedDecision {
    pub id: i64,
    pub fingerprint: String,
    pub proof: BoundSourceProof,
}

/// An identical review is idempotent. A conflicting existing review is held;
/// it is never replaced by a newer write or a changed source title.
pub async fn save_reviewed(
    db: &SqlitePool,
    proof: &BoundSourceProof,
) -> Result<SavedDecision, String> {
    proof.validate()?;
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
    // A restore can lose the review table while retaining a previously
    // recorded grab. Its exact evidence still constrains any new review.
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
        if old.info_hash != proof.info_hash
            || old.domain != proof.domain
            || old.published_files != proof.published_files
        {
            return Err(
                "Episode mapping: this exact source conflicts with its historical numbering evidence"
                    .into(),
            );
        }
        if old.target_anilist_id == proof.target_anilist_id && !old.is_within_review(proof) {
            return Err(
                "Episode mapping: a historical source selection is outside this review".into(),
            );
        }
    }
    let existing = sqlx::query("SELECT id,target_anilist_id,proof_json,fingerprint FROM source_numbering_decisions WHERE info_hash=? ORDER BY id")
        .bind(&proof.info_hash).fetch_all(&mut *tx).await.map_err(|error| error.to_string())?;
    let mut same = None;
    for row in existing {
        let old: BoundSourceProof = serde_json::from_str(row.get("proof_json"))
            .map_err(|error| format!("Episode mapping: invalid saved source proof: {error}"))?;
        old.validate()?;
        if old.domain != proof.domain || old.published_files != proof.published_files {
            return Err(
                "Episode mapping: this exact source has a conflicting numbering review".into(),
            );
        }
        if row.get::<i64, _>("target_anilist_id") == proof.target_anilist_id {
            if old != *proof || row.get::<String, _>("fingerprint") != fingerprint {
                return Err(
                    "Episode mapping: this source entry has a conflicting reviewed selection"
                        .into(),
                );
            }
            same = Some(row.get::<i64, _>("id"));
        }
    }
    let id = match same {
        Some(id) => id,
        None => sqlx::query_scalar("INSERT INTO source_numbering_decisions(info_hash,target_anilist_id,root_anilist_id,fingerprint,proof_json) VALUES(?,?,?,?,?) RETURNING id")
            .bind(&proof.info_hash).bind(proof.target_anilist_id).bind(proof.domain.root_anilist_id)
            .bind(&fingerprint).bind(&json).fetch_one(&mut *tx).await.map_err(|error| error.to_string())?,
    };
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(SavedDecision {
        id,
        fingerprint,
        proof: proof.clone(),
    })
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
    let row = sqlx::query("SELECT id,info_hash,target_anilist_id,root_anilist_id,fingerprint,proof_json FROM source_numbering_decisions WHERE info_hash=? ORDER BY (target_anilist_id=?) DESC,id DESC LIMIT 1")
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
    let proof: BoundSourceProof = serde_json::from_str(row.get("proof_json"))
        .map_err(|error| format!("Episode mapping: invalid saved source proof: {error}"))?;
    proof.validate()?;
    let json = serde_json::to_string(&proof).map_err(|error| error.to_string())?;
    let fingerprint = hex::encode(Sha256::digest(json.as_bytes()));
    if proof.info_hash != hash
        || proof.target_anilist_id != target_anilist_id
        || row.get::<String, _>("info_hash") != hash
        || row.get::<i64, _>("target_anilist_id") != target_anilist_id
        || row.get::<i64, _>("root_anilist_id") != proof.domain.root_anilist_id
        || row.get::<String, _>("fingerprint") != fingerprint
    {
        return Err("Episode mapping: saved source decision identity changed".into());
    }
    Ok(Some(SavedDecision {
        id: row.get("id"),
        fingerprint,
        proof,
    }))
}

pub(crate) async fn lookup_hash(
    db: &SqlitePool,
    info_hash: &str,
) -> Result<Vec<SavedDecision>, String> {
    let Ok(hash) = crate::services::source_numbering::normalized_hash(info_hash) else {
        return Ok(Vec::new());
    };
    let targets: Vec<i64> = sqlx::query_scalar("SELECT target_anilist_id FROM source_numbering_decisions WHERE info_hash=? ORDER BY target_anilist_id")
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

//! Read-only eligibility for sources that already have durable file ownership.
use crate::services::release_numbering::ReleaseMapping;
use sqlx::SqlitePool;

/// A new automatic winner must not require retargeting an existing bound
/// download. Client-only conflicts are still checked at dispatch time.
pub(crate) async fn ensure_automatic_compatibility(
    db: &SqlitePool,
    hash: &str,
    anilist_id: i64,
    mapping: Option<&ReleaseMapping>,
) -> Result<(), String> {
    let Some(mapping) = mapping.filter(|mapping| mapping.bound_source().is_some()) else {
        return Ok(());
    };
    let rows: Vec<(Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT s.anilist_id, g.release_mapping FROM grabbed_torrents g
         LEFT JOIN series s ON s.id=g.series_id
         WHERE g.hash=? COLLATE NOCASE AND g.state IN ('pending','imported')",
    )
    .bind(hash)
    .fetch_all(db)
    .await
    .map_err(|error| format!("Episode mapping: cannot read existing source assignment: {error}"))?;
    if rows.is_empty() {
        return Ok(());
    }
    let mapping = mapping.clone();
    let hash = hash.to_string();
    // Bound proofs include the complete publication manifest. Keep validation
    // and serialization off the async worker, as in source-proof projection.
    tokio::task::spawn_blocking(move || {
        mapping.validate()?;
        let proof = mapping.bound_source().expect("filtered to a bound mapping");
        if proof.target_anilist_id != anilist_id || !proof.info_hash.eq_ignore_ascii_case(&hash) {
            return Err("Episode mapping: candidate source assignment has another identity".into());
        }
        let incoming = serde_json::to_string(&mapping)
            .map_err(|error| format!("Episode mapping: cannot encode source assignment: {error}"))?;
        for (owner, saved) in rows {
            if owner != Some(anilist_id) {
                return Err("Episode mapping: existing download belongs to another saved release".into());
            }
            // record_grab_in_connection uses this exact serialized decision
            // predicate for pending/imported rows. Do not admit a candidate
            // that its durable commit would reject, even if only a receipt
            // differs. An unchanged refreshed receipt remains eligible.
            if saved.as_deref() != Some(incoming.as_str()) {
                return Err("Episode mapping: existing download has a different or unresolved saved source assignment".into());
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("Episode mapping: existing source check failed: {error}"))?
}

#[cfg(test)]
#[path = "source_compatibility_tests.rs"]
mod tests;

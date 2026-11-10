//! Cache of ffprobe output keyed by path and a strong file identity.
//!
//! Backs Layer 5 of the classification pipeline. ffprobe is a shell-out and
//! we don't want to re-run it on every library scan or re-classification —
//! the output is deterministic for a given file, so we cache the raw JSON
//! blob and invalidate by comparing nanosecond mtime, size and, on Unix,
//! device/inode. Legacy second-resolution rows are never used by the
//! identified reader. Sidecars are deliberately not stored in this cache.
//!
//! Writes are best-effort: a failed cache write never breaks the
//! classification path. On a cache hit the probe JSON is returned verbatim
//! and re-parsed by the Layer 5 scanner, so the cache layer knows nothing
//! about stream codecs or PGS tracks.
//!
//! Rows are keyed on file path, so deleted or renamed files leave orphaned
//! entries that will never be invalidated organically — `classify_ffprobe`
//! only sees live files, so a cached row for a path that no longer exists
//! just sits there. [`cleanup`] is the eviction path for those: the hourly
//! cleanup task calls it to drop rows that haven't been touched in a long
//! time, which naturally sweeps up orphans from library reorganizations,
//! deleted series, and moved media roots.

use sqlx::{Row, SqlitePool};

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS media_probe_cache (
            path        TEXT PRIMARY KEY,
            mtime       INTEGER NOT NULL,
            size        INTEGER NOT NULL,
            identity    TEXT,
            probe_json  TEXT NOT NULL,
            cached_at   DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;
    let columns = sqlx::query("PRAGMA table_info(media_probe_cache)")
        .fetch_all(db)
        .await?;
    if !columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "identity")
    {
        sqlx::query("ALTER TABLE media_probe_cache ADD COLUMN identity TEXT")
            .execute(db)
            .await?;
    }
    Ok(())
}

/// Return a probe only when the complete versioned identity matches.
/// NULL identities from older releases always miss, even if size/seconds
/// happen to match the current file.
pub async fn get_identified(db: &SqlitePool, path: &str, identity: &str) -> Option<String> {
    sqlx::query_scalar("SELECT probe_json FROM media_probe_cache WHERE path = ? AND identity = ?")
        .bind(path)
        .bind(identity)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
}

pub async fn upsert_identified(
    db: &SqlitePool,
    path: &str,
    identity: &str,
    size: i64,
    probe_json: &str,
) {
    let _ = sqlx::query(
        r#"INSERT INTO media_probe_cache (path, mtime, size, identity, probe_json)
           VALUES (?, 0, ?, ?, ?)
           ON CONFLICT(path) DO UPDATE SET
               mtime = 0, size = excluded.size, identity = excluded.identity,
               probe_json = excluded.probe_json, cached_at = CURRENT_TIMESTAMP"#,
    )
    .bind(path)
    .bind(size)
    .bind(identity)
    .bind(probe_json)
    .execute(db)
    .await;
}

/// Look up a cached probe JSON for `path`. Returns `None` when the row is
/// missing, when mtime/size no longer match (file was modified), or when
/// the DB lookup fails — Layer 5 falls back to a live shell-out in every
/// such case.
#[cfg(test)]
pub async fn get(db: &SqlitePool, path: &str, mtime: i64, size: i64) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let row = sqlx::query("SELECT mtime, size, probe_json FROM media_probe_cache WHERE path = ?")
        .bind(path)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;

    let cached_mtime: i64 = row.get("mtime");
    let cached_size: i64 = row.get("size");
    if cached_mtime != mtime || cached_size != size {
        return None;
    }
    Some(row.get::<String, _>("probe_json"))
}

/// Insert or replace a probe JSON blob. Errors are swallowed so a cache
/// write failure never bubbles up into the classifier. The next lookup
/// will simply miss and Layer 5 will re-probe.
#[cfg(test)]
pub async fn upsert(db: &SqlitePool, path: &str, mtime: i64, size: i64, probe_json: &str) {
    if path.is_empty() {
        return;
    }
    let _ = sqlx::query(
        r#"INSERT INTO media_probe_cache (path, mtime, size, probe_json)
           VALUES (?, ?, ?, ?)
           ON CONFLICT(path) DO UPDATE SET
               mtime = excluded.mtime,
               size = excluded.size,
               identity = NULL,
               probe_json = excluded.probe_json,
               cached_at = CURRENT_TIMESTAMP"#,
    )
    .bind(path)
    .bind(mtime)
    .bind(size)
    .bind(probe_json)
    .execute(db)
    .await;
}

/// Drop rows whose `cached_at` is older than `max_age_days`. Called from the
/// hourly cleanup task to evict orphaned entries — rows for files that have
/// been deleted, moved, or renamed will never get their `cached_at`
/// refreshed (nothing re-probes them), so they accumulate indefinitely
/// without this sweep.
///
/// Live files whose mtime/size never change also have their `cached_at`
/// frozen, so they get evicted at the same rate as orphans — but that's
/// fine, the cost of re-probing a still-live file once per TTL window is
/// one ffprobe shell-out, which is exactly what the cache was already
/// saving for more frequent accesses. Use a generous TTL (90 days) so the
/// common case is still a cache hit.
pub async fn cleanup(db: &SqlitePool, max_age_days: i32) -> Result<u64, sqlx::Error> {
    let cutoff = format!("-{} days", max_age_days);
    let res = sqlx::query("DELETE FROM media_probe_cache WHERE cached_at < datetime('now', ?)")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identified_cache_rejects_legacy_and_different_file_identity() {
        let db = crate::test_support::in_memory_pool().await;
        upsert(&db, "/episode.mkv", 123, 45, "legacy").await;
        assert_eq!(
            get(&db, "/episode.mkv", 123, 45).await.as_deref(),
            Some("legacy")
        );
        assert!(
            get_identified(&db, "/episode.mkv", "v2:45:123:1:2")
                .await
                .is_none()
        );
        upsert_identified(&db, "/episode.mkv", "v2:45:123000000001:1:2", 45, "current").await;
        assert_eq!(
            get_identified(&db, "/episode.mkv", "v2:45:123000000001:1:2")
                .await
                .as_deref(),
            Some("current")
        );
        // Same second and size, but different nanosecond or inode.
        assert!(
            get_identified(&db, "/episode.mkv", "v2:45:123000000002:1:2")
                .await
                .is_none()
        );
        assert!(
            get_identified(&db, "/episode.mkv", "v2:45:123000000001:1:3")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn migrate_adds_identity_to_an_existing_cache_idempotently() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE media_probe_cache (path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, size INTEGER NOT NULL, probe_json TEXT NOT NULL, cached_at DATETIME DEFAULT CURRENT_TIMESTAMP)")
            .execute(&db).await.unwrap();
        migrate(&db).await.unwrap();
        migrate(&db).await.unwrap();
        upsert_identified(&db, "/episode.mkv", "v2:new", 42, "{}").await;
        assert_eq!(
            get_identified(&db, "/episode.mkv", "v2:new")
                .await
                .as_deref(),
            Some("{}")
        );
    }
}

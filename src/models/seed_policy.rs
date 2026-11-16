//! Current source review, separate from historical import decisions and ownership.
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

pub(crate) const RETIREMENT_INSERT_TRIGGER: &str = "CREATE TRIGGER IF NOT EXISTS refuse_retired_source_insert BEFORE INSERT ON grabbed_torrents WHEN NEW.state='pending' AND EXISTS(SELECT 1 FROM torrent_seed_reviews r WHERE lower(r.hash)=lower(NEW.hash) AND r.blocked_at IS NOT NULL AND r.unblocked_at IS NULL) BEGIN SELECT RAISE(ABORT,'Source excluded by seeding policy; reconsider it explicitly before another grab'); END";
pub(crate) const RETIREMENT_UPDATE_TRIGGER: &str = "CREATE TRIGGER IF NOT EXISTS refuse_retired_source_update BEFORE UPDATE ON grabbed_torrents WHEN NEW.state='pending' AND EXISTS(SELECT 1 FROM torrent_seed_reviews r WHERE lower(r.hash)=lower(NEW.hash) AND r.blocked_at IS NOT NULL AND r.unblocked_at IS NULL) BEGIN SELECT RAISE(ABORT,'Source excluded by seeding policy; reconsider it explicitly before another grab'); END";

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Review {
    pub client_id: i64,
    pub hash: String,
    pub name: String,
    pub verdict: String,
    pub reason: String,
    pub control: String,
    pub checked_at: String,
}

pub async fn init_table(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS torrent_seed_reviews (
        client_id INTEGER NOT NULL, hash TEXT NOT NULL, name TEXT NOT NULL,
        verdict TEXT NOT NULL, reason TEXT NOT NULL, control TEXT NOT NULL,
        checked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        PRIMARY KEY(client_id, hash))",
    )
    .execute(db)
    .await?;
    for (name, definition) in [
        ("blocked_at", "TEXT"),
        ("unblocked_at", "TEXT"),
        ("retirement_reason", "TEXT"),
        ("retirement_owner", "INTEGER"),
        ("retirement_attempt", "TEXT"),
        ("retirement_snapshot", "TEXT"),
        ("retired_at", "TEXT"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE torrent_seed_reviews ADD COLUMN {name} {definition}"
        )))
        .execute(db)
        .await
        .ok();
    }
    // An add may have begun before retirement acquired the native-action lock.
    // It must not claim or reactivate an owner after the source was excluded.
    sqlx::query(RETIREMENT_INSERT_TRIGGER).execute(db).await?;
    sqlx::query(RETIREMENT_UPDATE_TRIGGER).execute(db).await?;
    Ok(())
}

pub async fn save(db: &SqlitePool, review: &Review) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO torrent_seed_reviews
        (client_id, hash, name, verdict, reason, control) VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(client_id, hash) DO UPDATE SET name=CASE WHEN torrent_seed_reviews.blocked_at IS NOT NULL AND torrent_seed_reviews.unblocked_at IS NULL THEN torrent_seed_reviews.name ELSE excluded.name END,
        verdict=excluded.verdict, reason=excluded.reason, control=excluded.control,
        checked_at=CURRENT_TIMESTAMP",
    )
    .bind(review.client_id)
    .bind(review.hash.to_ascii_lowercase())
    .bind(&review.name)
    .bind(&review.verdict)
    .bind(&review.reason)
    .bind(&review.control)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn list(db: &SqlitePool) -> Result<Vec<Review>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM torrent_seed_reviews WHERE retired_at IS NULL ORDER BY checked_at DESC, client_id, name")
        .fetch_all(db)
        .await
}

/// Source exclusions are separate from historical import success. Routine
/// observations must never clear a block or replace its recorded reason.
#[derive(Debug, Clone, FromRow)]
pub struct Retirement {
    pub client_id: i64,
    pub hash: String,
    pub name: String,
    pub blocked_at: String,
    pub retirement_reason: String,
    pub retirement_owner: i64,
    pub retirement_attempt: String,
    pub retirement_snapshot: String,
    pub retired_at: Option<String>,
}

pub async fn retirements(db: &SqlitePool) -> Result<Vec<Retirement>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM torrent_seed_reviews WHERE blocked_at IS NOT NULL AND unblocked_at IS NULL ORDER BY blocked_at DESC, client_id, hash")
        .fetch_all(db).await
}

pub async fn is_blocked(db: &SqlitePool, hash: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM torrent_seed_reviews WHERE lower(hash)=lower(?) AND blocked_at IS NOT NULL AND unblocked_at IS NULL)")
        .bind(hash).fetch_one(db).await
}

/// Capture intent before native removal. Repeated calls cannot adopt another
/// attempt. No episode state, import coverage or historical verdict is changed.
pub async fn request_retirement(
    db: &SqlitePool,
    client_id: i64,
    hash: &str,
    owner: i64,
    attempt: &str,
    reason: &str,
    snapshot: &str,
) -> Result<(), String> {
    let changed = sqlx::query("UPDATE torrent_seed_reviews SET blocked_at=CURRENT_TIMESTAMP, unblocked_at=NULL, retirement_reason=?1, retirement_owner=?2, retirement_attempt=?3, retirement_snapshot=?4, retired_at=NULL WHERE client_id=?5 AND lower(hash)=?6 AND (blocked_at IS NULL OR unblocked_at IS NOT NULL) AND EXISTS(SELECT 1 FROM grabbed_torrents g WHERE g.id=?2 AND g.grabbed_at=?3 AND g.download_client_id=?5 AND lower(g.hash)=?6 AND g.state IN('failed','removed','cancelled','imported','replaced') AND g.state=json_extract(?4,'$.owner.state') AND g.series_id=json_extract(?4,'$.owner.series_id') AND g.imported_episode_coverage IS json_extract(?4,'$.owner.imported_episode_coverage') AND g.imported_source_paths IS json_extract(?4,'$.owner.imported_source_paths') AND g.verification IS json_extract(?4,'$.owner.verification')) AND (SELECT COUNT(*) FROM grabbed_torrents WHERE lower(hash)=?6)=1 AND NOT EXISTS(SELECT 1 FROM pending_grabs WHERE lower(info_hash)=?6)")
        .bind(reason).bind(owner).bind(attempt).bind(snapshot).bind(client_id).bind(hash)
        .execute(db).await.map_err(|e|e.to_string())?.rows_affected();
    if changed == 1 {
        return Ok(());
    }
    let same: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM torrent_seed_reviews WHERE client_id=? AND lower(hash)=? AND blocked_at IS NOT NULL AND unblocked_at IS NULL AND retirement_owner=? AND retirement_attempt=? AND json_remove(retirement_snapshot,'$.owner.state','$.item.name')=json_remove(?,'$.owner.state','$.item.name') AND NOT EXISTS(SELECT 1 FROM pending_grabs WHERE lower(info_hash)=?))")
        .bind(client_id).bind(hash).bind(owner).bind(attempt).bind(snapshot).bind(hash)
        .fetch_one(db).await.map_err(|e|e.to_string())?;
    if same {
        Ok(())
    } else {
        Err("Source ownership or retirement request changed; no removal permitted".into())
    }
}

/// Finish bookkeeping in one transaction, including a durable activity reason.
/// Called only after the exact native job is confirmed absent.
pub async fn finish_retirement(db: &SqlitePool, row: &Retirement) -> Result<(), String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    let changed = sqlx::query("UPDATE torrent_seed_reviews SET retired_at=COALESCE(retired_at,CURRENT_TIMESTAMP), control='Removed from client and blocked; source files retained' WHERE client_id=? AND hash=? AND blocked_at=? AND retirement_owner=? AND retirement_attempt=? AND unblocked_at IS NULL")
        .bind(row.client_id).bind(&row.hash).bind(&row.blocked_at).bind(row.retirement_owner).bind(&row.retirement_attempt)
        .execute(&mut *tx).await.map_err(|e|e.to_string())?.rows_affected();
    if changed != 1 {
        return Err("Source retirement changed; bookkeeping deferred".into());
    }
    sqlx::query("UPDATE grabbed_torrents SET client_removed_at=COALESCE(client_removed_at,CURRENT_TIMESTAMP) WHERE id=? AND grabbed_at=? AND download_client_id=? AND lower(hash)=?")
        .bind(row.retirement_owner).bind(&row.retirement_attempt).bind(row.client_id).bind(&row.hash)
        .execute(&mut *tx).await.map_err(|e|e.to_string())?;
    sqlx::query("INSERT OR IGNORE INTO library_activity(operation_key,action,series_id,provider_id,franchise_id,title,reason,detail,note,grab_id,recycle_entry_id) SELECT ?, 'Source retired', g.series_id, s.anilist_id, NULL, ?, ?, ?, '', g.id, NULL FROM grabbed_torrents g LEFT JOIN series s ON s.id=g.series_id WHERE g.id=? AND g.grabbed_at=?")
        .bind(format!("source-retirement:{}:{}:{}:{}",row.client_id,row.hash,row.retirement_owner,row.retirement_attempt))
        .bind(&row.name).bind(&row.retirement_reason).bind(&row.retirement_snapshot)
        .bind(row.retirement_owner).bind(&row.retirement_attempt)
        .execute(&mut *tx).await.map_err(|e|e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// Explicit reconsideration does not acquire, resume, restore or change history.
pub async fn unblock(db: &SqlitePool, client_id: i64, hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE torrent_seed_reviews SET unblocked_at=CURRENT_TIMESTAMP WHERE client_id=? AND hash=?")
        .bind(client_id).bind(hash).execute(db).await?;
    Ok(())
}

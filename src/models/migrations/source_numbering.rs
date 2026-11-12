//! Exact-source numbering review storage; separate from library migration replay.

use sqlx::SqlitePool;

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS source_numbering_decisions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            info_hash TEXT NOT NULL,
            target_anilist_id INTEGER NOT NULL,
            root_anilist_id INTEGER NOT NULL,
            fingerprint TEXT NOT NULL,
            proof_json TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(info_hash, target_anilist_id)
        )",
    )
    .execute(db)
    .await?;
    Ok(())
}

//! Current source review, separate from historical import decisions and ownership.
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

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
    Ok(())
}

pub async fn save(db: &SqlitePool, review: &Review) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO torrent_seed_reviews
        (client_id, hash, name, verdict, reason, control) VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(client_id, hash) DO UPDATE SET name=excluded.name,
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
    sqlx::query_as("SELECT * FROM torrent_seed_reviews ORDER BY checked_at DESC, client_id, name")
        .fetch_all(db)
        .await
}

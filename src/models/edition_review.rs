//! Exact, local evidence for a deferred replacement of one episode.
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::services::media_info::FileIdentity;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Review {
    pub id: i64,
    pub grab_id: i64,
    pub grabbed_at: String,
    pub series_id: i64,
    pub provider_id: i64,
    pub episode: i32,
    pub source_root: String,
    pub source_path: String,
    pub source_identity: String,
    pub current_path: String,
    pub current_identity: String,
    pub reason: String,
}

#[derive(sqlx::FromRow)]
pub struct Summary {
    pub id: i64,
    pub series_id: i64,
    pub title: String,
    pub cover_url: String,
    pub episode: i32,
    pub candidates: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct OpenEpisode {
    pub id: i64,
    pub series_id: i64,
    pub episode: i32,
}

impl Review {
    pub(crate) async fn check_files(&self) -> Result<(), String> {
        self.checked_file(false).await?;
        self.checked_file(true).await?;
        Ok(())
    }

    pub(crate) async fn checked_file(&self, source: bool) -> Result<PathBuf, String> {
        let (path, identity) = if source {
            (&self.source_path, &self.source_identity)
        } else {
            (&self.current_path, &self.current_identity)
        };
        let path = Path::new(path);
        let resolved = tokio::fs::canonicalize(path)
            .await
            .map_err(|_| "The file is unavailable. Reload the episode.".to_string())?;
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| "The file is unavailable. Reload the episode.".to_string())?;
        if resolved != path
            || !meta.is_file()
            || FileIdentity::from_metadata(&meta)
                .map_err(|e| e.to_string())?
                .cache_key()
                != *identity
        {
            return Err("The file changed. Reload the episode.".into());
        }
        Ok(resolved)
    }
}

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS episode_edition_reviews (
            id INTEGER PRIMARY KEY,
            grab_id INTEGER NOT NULL REFERENCES grabbed_torrents(id) ON DELETE CASCADE,
            grabbed_at TEXT NOT NULL,
            series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
            provider_id INTEGER NOT NULL,
            episode INTEGER NOT NULL,
            source_root TEXT NOT NULL,
            source_path TEXT NOT NULL,
            source_identity TEXT NOT NULL,
            current_path TEXT NOT NULL,
            current_identity TEXT NOT NULL,
            reason TEXT NOT NULL,
            decision TEXT,
            UNIQUE(grab_id, grabbed_at)
        );
        CREATE INDEX IF NOT EXISTS episode_edition_slot
            ON episode_edition_reviews (series_id, episode, decision);",
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn save(db: &SqlitePool, review: &Review) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO episode_edition_reviews
         (grab_id, grabbed_at, series_id, provider_id, episode, source_root, source_path, source_identity, current_path, current_identity, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(review.grab_id)
    .bind(&review.grabbed_at)
    .bind(review.series_id)
    .bind(review.provider_id)
    .bind(review.episode)
    .bind(&review.source_root)
    .bind(&review.source_path)
    .bind(&review.source_identity)
    .bind(&review.current_path)
    .bind(&review.current_identity)
    .bind(&review.reason)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get_open(db: &SqlitePool, id: i64) -> Result<Option<Review>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.* FROM episode_edition_reviews r
         JOIN series s ON s.id = r.series_id AND s.anilist_id = r.provider_id
         JOIN grabbed_torrents g ON g.id = r.grab_id AND g.grabbed_at = r.grabbed_at
         WHERE r.id = ? AND r.decision IS NULL AND g.state = 'failed'",
    )
    .bind(id)
    .fetch_optional(db)
    .await
}

pub(crate) async fn holds_source(
    db: &SqlitePool,
    grab_id: i64,
    grabbed_at: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM episode_edition_reviews
                        WHERE grab_id = ? AND grabbed_at = ? AND decision IS NULL)",
    )
    .bind(grab_id)
    .bind(grabbed_at)
    .fetch_one(db)
    .await
}

pub async fn for_episode(
    db: &SqlitePool,
    series_id: i64,
    episode: i32,
) -> Result<Vec<Review>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.* FROM episode_edition_reviews r
         JOIN series s ON s.id = r.series_id AND s.anilist_id = r.provider_id
         JOIN grabbed_torrents g ON g.id = r.grab_id AND g.grabbed_at = r.grabbed_at
         WHERE r.series_id = ? AND r.episode = ? AND r.decision IS NULL AND g.state = 'failed'
         ORDER BY r.id DESC LIMIT 12",
    )
    .bind(series_id)
    .bind(episode)
    .fetch_all(db)
    .await
}

pub async fn summaries(db: &SqlitePool) -> Result<Vec<Summary>, sqlx::Error> {
    sqlx::query_as(
        "SELECT MAX(r.id) AS id, s.id AS series_id, s.title, COALESCE(s.cover_url, '') AS cover_url,
                r.episode, COUNT(*) AS candidates
         FROM episode_edition_reviews r
         JOIN series s ON s.id = r.series_id AND s.anilist_id = r.provider_id
         JOIN grabbed_torrents g ON g.id = r.grab_id AND g.grabbed_at = r.grabbed_at
         WHERE r.decision IS NULL AND g.state = 'failed'
         GROUP BY r.series_id, r.episode ORDER BY id DESC LIMIT 100",
    )
    .fetch_all(db)
    .await
}

/// The newest current review for each episode, newest first.
pub(crate) async fn open_episodes(db: &SqlitePool) -> Result<Vec<OpenEpisode>, sqlx::Error> {
    sqlx::query_as(
        "SELECT MAX(r.id) AS id, r.series_id, r.episode
         FROM episode_edition_reviews r
         JOIN series s ON s.id = r.series_id AND s.anilist_id = r.provider_id
         JOIN grabbed_torrents g ON g.id = r.grab_id AND g.grabbed_at = r.grabbed_at
         WHERE r.decision IS NULL AND g.state = 'failed'
         GROUP BY r.series_id, r.episode ORDER BY id DESC",
    )
    .fetch_all(db)
    .await
}

pub async fn dismiss(db: &SqlitePool, review: &Review, all: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE episode_edition_reviews SET decision = 'kept'
         WHERE decision IS NULL AND (id = ? OR (? AND series_id = ? AND episode = ?
                AND current_path = ? AND current_identity = ?))",
    )
    .bind(review.id)
    .bind(all)
    .bind(review.series_id)
    .bind(review.episode)
    .bind(&review.current_path)
    .bind(&review.current_identity)
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn finish(
    db: &SqlitePool,
    review: &Review,
    dismiss_others: bool,
) -> Result<(), String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    let changed = sqlx::query(
        "UPDATE grabbed_torrents SET state = 'imported', imported_at = CURRENT_TIMESTAMP,
                import_mode = 'protected', failure_reason = ''
         WHERE id = ? AND grabbed_at = ? AND state = 'failed'",
    )
    .bind(review.grab_id)
    .bind(&review.grabbed_at)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .rows_affected();
    if changed != 1 {
        return Err(
            "The source record changed while the library file was installed. Check Activity."
                .into(),
        );
    }
    sqlx::query(
        "UPDATE episode_edition_reviews SET decision = CASE WHEN id = ? THEN 'used' ELSE 'kept' END
         WHERE decision IS NULL AND (id = ? OR (? AND series_id = ? AND episode = ?
                AND current_path = ? AND current_identity = ?))",
    )
    .bind(review.id)
    .bind(review.id)
    .bind(dismiss_others)
    .bind(review.series_id)
    .bind(review.episode)
    .bind(&review.current_path)
    .bind(&review.current_identity)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

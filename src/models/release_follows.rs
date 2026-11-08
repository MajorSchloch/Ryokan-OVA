use sqlx::SqlitePool;

use super::{monitoring::MonitorMode, series};
use crate::services::{anilist::RelatedEntry, library_link::pick_title};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Follow {
    pub id: i64,
    pub anilist_id: i64,
    pub title: String,
    pub started_on: String,
    pub movies: bool,
    pub ovas: bool,
    pub alternatives: bool,
}

pub async fn list(db: &SqlitePool) -> Result<Vec<Follow>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM release_follows ORDER BY title COLLATE NOCASE, id")
        .fetch_all(db)
        .await
}

pub async fn start(
    db: &SqlitePool,
    root: i64,
    title: &str,
    movies: bool,
    ovas: bool,
    alternatives: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO release_follows (anilist_id, title, movies, ovas, alternatives)
        VALUES (?, ?, ?, ?, ?) ON CONFLICT(anilist_id) DO NOTHING",
    )
    .bind(root)
    .bind(title)
    .bind(movies)
    .bind(ovas)
    .bind(alternatives)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn update(
    db: &SqlitePool,
    id: i64,
    movies: bool,
    ovas: bool,
    alternatives: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE release_follows SET movies = ?, ovas = ?, alternatives = ? WHERE id = ?")
        .bind(movies)
        .bind(ovas)
        .bind(alternatives)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn remove(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM release_follows WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Record the outcome with the new series and its initial episode selection.
/// No network request, search, or file operation runs inside this transaction.
pub(crate) async fn accept(
    db: &SqlitePool,
    follow: &Follow,
    entry: &RelatedEntry,
) -> Result<bool, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    // Settings or the follow itself may have changed during the AniList request.
    let receipt = sqlx::query(
        "INSERT INTO followed_releases (follow_id, anilist_id)
        SELECT id, ? FROM release_follows WHERE id = ? AND movies = ? AND ovas = ? AND alternatives = ?
          AND NOT EXISTS (SELECT 1 FROM external_sync_exclusions WHERE anilist_id = ?
                          OR (anilist_id IS NULL AND mal_id = ?))
        ON CONFLICT DO NOTHING",
    )
    .bind(entry.id)
    .bind(follow.id)
    .bind(follow.movies)
    .bind(follow.ovas)
    .bind(follow.alternatives)
    .bind(entry.id)
    .bind(entry.id_mal)
    .execute(&mut *tx)
    .await?;
    if receipt.rows_affected() == 0 {
        return Ok(false);
    }
    // A positive AniList ID owns its own season, even when two share a MAL ID.
    // A MAL-only row for the same entry already belongs to the user.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
        SELECT 1 FROM series WHERE anilist_id = ? OR (anilist_id < 0 AND mal_id = ?)
    )",
    )
    .bind(entry.id)
    .bind(entry.id_mal)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        tx.commit().await?;
        return Ok(false);
    }

    let title = pick_title(
        "romaji",
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    );
    let core = series::SeriesCore {
        anilist_id: entry.id,
        mal_id: entry.id_mal,
        title,
        title_romaji: &entry.title_romaji,
        title_english: &entry.title_english,
        title_native: &entry.title_native,
        cover_url: &entry.cover_url,
        format: &entry.format,
        status: &entry.status,
        episodes: entry.episodes,
        season_year: entry.season_year,
        end_year: None,
    };
    let id = series::insert_new(&mut tx, core, MonitorMode::All).await?;
    // All episodes of a qualifying *new entry* are wanted, including ones
    // that aired during downtime. Normal metadata refresh fills unknown totals.
    if let Some(count) = entry.episodes.filter(|n| *n > 0) {
        sqlx::query("WITH RECURSIVE episodes(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM episodes WHERE n < ?)
            INSERT INTO episode_monitor_state(series_id, episode_number, monitored) SELECT ?, n, 1 FROM episodes")
            .bind(count).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn calendar_add_and_release_follow_share_one_entry_and_upgrade_default() {
        let db = in_memory_pool().await;
        start(&db, 10, "Original", true, true, false).await.unwrap();
        let follow = list(&db).await.unwrap().remove(0);
        let entry: RelatedEntry = serde_json::from_value(serde_json::json!({
            "id":11,"id_mal":80,"title_romaji":"Sequel","title_english":"","title_native":"",
            "cover_url":"","format":"TV","status":"NOT_YET_RELEASED","status_display":"Upcoming",
            "episodes":1,"relation_type":"SEQUEL","media_type":"ANIME"
        }))
        .unwrap();
        let core = series::SeriesCore {
            anilist_id: entry.id,
            mal_id: entry.id_mal,
            title: &entry.title_romaji,
            title_romaji: &entry.title_romaji,
            title_english: "",
            title_native: "",
            cover_url: "",
            format: &entry.format,
            status: &entry.status,
            episodes: entry.episodes,
            season_year: None,
            end_year: None,
        };
        let (manual, automatic) =
            tokio::join!(series::upsert(&db, core), accept(&db, &follow, &entry));
        let (id, created) = manual.unwrap();
        assert_ne!(created, automatic.unwrap());
        let rows = series::get_all(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert!(!rows[0].allow_upgrades);
        assert!(!accept(&db, &follow, &entry).await.unwrap());
    }

    #[tokio::test]
    async fn release_follows_retry_exclusions_and_interrupted_adds_without_overriding_a_stop() {
        let db = in_memory_pool().await;
        start(&db, 1, "Source", true, false, false).await.unwrap();
        let follow = list(&db).await.unwrap().remove(0);
        let movie: RelatedEntry = serde_json::from_value(serde_json::json!({
            "id":2, "title_romaji":"Movie", "title_english":"", "title_native":"",
            "cover_url":"", "format":"MOVIE", "status":"NOT_YET_RELEASED",
            "status_display":"Upcoming", "episodes":1, "relation_type":"SEQUEL", "media_type":"ANIME"
        })).unwrap();
        sqlx::query("INSERT INTO external_sync_exclusions(anilist_id) VALUES (2)")
            .execute(&db)
            .await
            .unwrap();
        assert!(!accept(&db, &follow, &movie).await.unwrap());
        sqlx::query("DELETE FROM external_sync_exclusions")
            .execute(&db)
            .await
            .unwrap();

        // The row, episode selection and receipt must all roll back together.
        sqlx::query(
            "CREATE TRIGGER fail_episode BEFORE INSERT ON episode_monitor_state
            BEGIN SELECT RAISE(ABORT, 'test interruption'); END",
        )
        .execute(&db)
        .await
        .unwrap();
        assert!(accept(&db, &follow, &movie).await.is_err());
        assert!(series::get_by_anilist_id(&db, 2).await.unwrap().is_none());
        sqlx::query("DROP TRIGGER fail_episode")
            .execute(&db)
            .await
            .unwrap();

        // A response that arrives after a settings change cannot add the movie.
        update(&db, follow.id, false, false, false).await.unwrap();
        assert!(!accept(&db, &follow, &movie).await.unwrap());
        update(&db, follow.id, true, false, false).await.unwrap();
        let (first, second) =
            tokio::join!(accept(&db, &follow, &movie), accept(&db, &follow, &movie));
        assert_ne!(first.unwrap(), second.unwrap());
        let added = series::get_by_anilist_id(&db, 2).await.unwrap().unwrap();
        series::remove(&db, added.id).await.unwrap();
        remove(&db, follow.id).await.unwrap();
        assert!(!accept(&db, &follow, &movie).await.unwrap());
    }
}

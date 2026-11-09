use sqlx::SqlitePool;

use super::{monitoring::MonitorMode, series};
use crate::services::{anilist::RelatedEntry, library_link::pick_title};

#[derive(Default, Debug, Clone, sqlx::FromRow)]
pub struct Follow {
    pub id: i64,
    pub anilist_id: i64,
    pub title: String,
    pub started_on: String,
    pub tv: bool,
    pub revision: i64,
    pub library_folder: String,
    pub movies: bool,
    pub ovas: bool,
    pub alternatives: bool,
    pub check_status: Option<String>,
    pub last_checked_at: Option<String>,
    pub last_success_at: Option<String>,
    #[sqlx(skip)]
    pub matches: Vec<MatchedRelease>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MatchedRelease {
    pub follow_id: i64,
    pub anilist_id: i64,
    pub title: String,
    pub series_id: Option<i64>,
}

pub async fn list<'e>(db: impl sqlx::SqliteExecutor<'e>) -> Result<Vec<Follow>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM release_follows ORDER BY title COLLATE NOCASE, id")
        .fetch_all(db)
        .await
}

/// Render from local records. Use the receipt title if the series was removed.
pub async fn list_with_matches(db: &SqlitePool) -> Result<Vec<Follow>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let mut follows = list(&mut *tx).await?;
    let releases: Vec<MatchedRelease> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} SELECT r.follow_id, r.anilist_id, s.id AS series_id,
            COALESCE(NULLIF(s.title,''), NULLIF(r.title,''),
                CASE WHEN json_valid(p.detail_json) THEN COALESCE(
                    NULLIF(json_extract(p.detail_json,'$.title_romaji'),''),
                    NULLIF(json_extract(p.detail_json,'$.title_english'),''),
                    NULLIF(json_extract(p.detail_json,'$.title_native'),'')) END,
                'AniList #' || r.anilist_id) AS title
         FROM followed_releases r
         LEFT JOIN local_anilist_ids l ON l.anilist_id=r.anilist_id
         LEFT JOIN series s ON s.id=l.series_id
         LEFT JOIN provider_metadata_cache p ON p.provider_id=r.anilist_id
         ORDER BY r.follow_id, title COLLATE NOCASE, r.anilist_id",
        series::LOCAL_ANILIST_IDS
    )))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut by_follow = std::collections::HashMap::<_, Vec<_>>::new();
    for release in releases {
        by_follow
            .entry(release.follow_id)
            .or_default()
            .push(release);
    }
    for follow in &mut follows {
        follow.matches = by_follow.remove(&follow.id).unwrap_or_default();
    }
    Ok(follows)
}

pub(crate) async fn record_check(
    db: &SqlitePool,
    follow: &Follow,
    succeeded: bool,
) -> Result<(), sqlx::Error> {
    // Ignore a result from a check that used settings the user has since changed.
    sqlx::query(
        "UPDATE release_follows SET
        check_status = CASE WHEN ?1 THEN 'success' ELSE 'failed' END,
        last_checked_at = CURRENT_TIMESTAMP,
        last_success_at = CASE WHEN ?1 THEN CURRENT_TIMESTAMP ELSE last_success_at END
        WHERE id=?2 AND movies=?3 AND ovas=?4 AND alternatives=?5 AND revision=?6",
    )
    .bind(succeeded)
    .bind(follow.id)
    .bind(follow.movies)
    .bind(follow.ovas)
    .bind(follow.alternatives)
    .bind(follow.revision)
    .execute(db)
    .await?;
    Ok(())
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
        "INSERT INTO release_follows (anilist_id, title, movies, ovas, alternatives, check_status)
        SELECT ?1, ?2, ?3, ?4, ?5, 'pending' WHERE NOT EXISTS (SELECT 1 FROM franchise_choices WHERE anilist_id=?1) ON CONFLICT(anilist_id) DO NOTHING",
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
    sqlx::query("UPDATE release_follows SET movies = ?, ovas = ?, alternatives = ? WHERE id = ? AND revision=0")
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
    let title = pick_title(
        "romaji",
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    );
    let receipt = sqlx::query(
        "INSERT INTO followed_releases (follow_id, anilist_id, title)
        SELECT id, ?, ? FROM release_follows WHERE id = ? AND movies = ? AND ovas = ? AND alternatives = ? AND revision=?
          AND NOT EXISTS (SELECT 1 FROM franchise_choices WHERE anilist_id=? AND (selected=0 OR follow_id != release_follows.id))
          AND NOT EXISTS (SELECT 1 FROM external_sync_exclusions WHERE anilist_id = ?
                          OR (anilist_id IS NULL AND mal_id = ?))
        ON CONFLICT DO NOTHING",
    )
    .bind(entry.id)
    .bind(title)
    .bind(follow.id)
    .bind(follow.movies)
    .bind(follow.ovas)
    .bind(follow.alternatives)
    .bind(follow.revision)
    .bind(entry.id)
    .bind(entry.id)
    .bind(entry.id_mal)
    .execute(&mut *tx)
    .await?;
    if receipt.rows_affected() == 0 {
        // Older receipts may outlive both the library entry and its cached metadata.
        sqlx::query("UPDATE followed_releases SET title=? WHERE follow_id=? AND anilist_id=? AND title IS NULL")
            .bind(title).bind(follow.id).bind(entry.id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(false);
    }
    let added = track_release(&mut tx, entry).await?;
    if follow.revision > 0 {
        sqlx::query("INSERT INTO franchise_choices(anilist_id,follow_id,title,selected) VALUES(?,?,?,1) ON CONFLICT DO NOTHING")
            .bind(entry.id).bind(follow.id).bind(title).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(added)
}

pub(crate) async fn track_release(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    entry: &RelatedEntry,
) -> Result<bool, sqlx::Error> {
    let title = pick_title(
        "romaji",
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    );
    // A positive AniList ID owns its own season, even when two share a MAL ID.
    // A MAL-only row for the same entry already belongs to the user.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
        SELECT 1 FROM series WHERE anilist_id = ? OR (anilist_id < 0 AND mal_id = ?)
    )",
    )
    .bind(entry.id)
    .bind(entry.id_mal)
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Ok(false);
    }

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
    let id = series::insert_new(tx, core, MonitorMode::All).await?;
    // All episodes of a qualifying *new entry* are wanted, including ones
    // that aired during downtime. Normal metadata refresh fills unknown totals.
    if let Some(count) = entry.episodes.filter(|n| *n > 0) {
        sqlx::query("WITH RECURSIVE episodes(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM episodes WHERE n < ?)
            INSERT INTO episode_monitor_state(series_id, episode_number, monitored) SELECT ?, n, 1 FROM episodes")
            .bind(count).bind(id).execute(&mut **tx).await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn migration_preserves_legacy_matches_without_inventing_check_dates() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql("
            CREATE TABLE release_follows (id INTEGER PRIMARY KEY AUTOINCREMENT,
                anilist_id INTEGER NOT NULL UNIQUE, title TEXT NOT NULL, started_on TEXT NOT NULL,
                movies BOOLEAN NOT NULL, ovas BOOLEAN NOT NULL, alternatives BOOLEAN NOT NULL);
            CREATE TABLE followed_releases (follow_id INTEGER NOT NULL REFERENCES release_follows(id)
                ON DELETE CASCADE, anilist_id INTEGER NOT NULL, PRIMARY KEY(follow_id, anilist_id));
            INSERT INTO release_follows VALUES (1,10,'Original','2024-01-02',1,0,0);
            INSERT INTO followed_releases VALUES (1,11);
        ").execute(&db).await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let id = crate::test_support::seed_series(&db, 11, "Existing sequel").await;
        let follow = list_with_matches(&db).await.unwrap().remove(0);
        assert!(follow.check_status.is_none());
        assert!(follow.last_success_at.is_none() && follow.last_checked_at.is_none());
        assert_eq!(follow.started_on, "2024-01-02");
        assert!(follow.movies);
        assert_eq!(follow.matches.len(), 1);
        assert_eq!(follow.matches[0].title, "Existing sequel");
        assert_eq!(follow.matches[0].series_id, Some(id));

        record_check(&db, &follow, false).await.unwrap();
        let failed = list_with_matches(&db).await.unwrap().remove(0);
        assert!(failed.last_success_at.is_none());
        assert!(failed.last_checked_at.is_some());
        assert_eq!(failed.check_status.as_deref(), Some("failed"));
        record_check(&db, &follow, true).await.unwrap();
        let recovered = list_with_matches(&db).await.unwrap().remove(0);
        assert_eq!(recovered.check_status.as_deref(), Some("success"));
        assert!(recovered.last_success_at.is_some());
        assert_eq!(recovered.matches.len(), 1);
    }

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

use sqlx::{Row, SqlitePool};

use crate::services::anilist::AnimeDetail;
use serde::{Deserialize, Serialize};

pub const METADATA_REFRESH_INTERVAL_HOURS: i64 = 12;

/// How long a row may go without a successful refresh before the series
/// page warns that its metadata may be out of date. Deliberately much
/// wider than `METADATA_REFRESH_INTERVAL_HOURS`: a row is past the
/// refresh window between any two sweeps, so a banner keyed on
/// `is_fresh` showed on nearly every page view. A week with no
/// successful refresh is the shape of a provider outage (or Ryokan
/// being off), which is what the banner is for.
pub const METADATA_STALE_WARNING_DAYS: i64 = 7;

#[derive(Debug, Clone)]
pub struct CachedSeriesMetadata {
    pub provider_id: i64,
    pub detail: AnimeDetail,
    pub cached_at: String,
    /// Within `METADATA_REFRESH_INTERVAL_HOURS`; the readers that decide
    /// whether to fetch again key on this.
    pub is_fresh: bool,
    /// Older than `METADATA_STALE_WARNING_DAYS`; the series page's
    /// warning banner keys on this. `!is_fresh` is not "stale".
    pub is_stale: bool,
}

/// Display fields from the exact provider entry, with independent freshness
/// while the authoritative episode snapshot is held.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerifiedTitleMetadata {
    title_romaji: String,
    title_english: String,
    title_native: String,
    synonyms: Vec<String>,
    description: String,
    genres: Vec<String>,
    country_of_origin: Option<String>,
    cover_url: String,
    banner_url: String,
    average_score: Option<i32>,
    average_score_display: Option<String>,
    score_is_ten_point: bool,
    score_class: String,
}

#[derive(Debug, Clone)]
pub struct MetadataMappingHold {
    title: VerifiedTitleMetadata,
    pub reason: String,
    pub refreshed_at: String,
    pub is_stale: bool,
}

impl MetadataMappingHold {
    pub(crate) fn apply_title(&self, detail: &mut AnimeDetail) {
        detail.title_romaji.clone_from(&self.title.title_romaji);
        detail.title_english.clone_from(&self.title.title_english);
        detail.title_native.clone_from(&self.title.title_native);
        detail.synonyms.clone_from(&self.title.synonyms);
        detail.description.clone_from(&self.title.description);
        detail.genres.clone_from(&self.title.genres);
        detail
            .country_of_origin
            .clone_from(&self.title.country_of_origin);
        detail.cover_url.clone_from(&self.title.cover_url);
        detail.banner_url.clone_from(&self.title.banner_url);
        detail.average_score = self.title.average_score;
        detail
            .average_score_display
            .clone_from(&self.title.average_score_display);
        detail.score_is_ten_point = self.title.score_is_ten_point;
        detail.score_class.clone_from(&self.title.score_class);
    }
}

pub(crate) async fn save_mapping_hold(
    db: &SqlitePool,
    series_id: i64,
    detail: &AnimeDetail,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    let title = VerifiedTitleMetadata {
        title_romaji: detail.title_romaji.clone(),
        title_english: detail.title_english.clone(),
        title_native: detail.title_native.clone(),
        synonyms: detail.synonyms.clone(),
        description: detail.description.clone(),
        genres: detail.genres.clone(),
        country_of_origin: detail.country_of_origin.clone(),
        cover_url: detail.cover_url.clone(),
        banner_url: detail.banner_url.clone(),
        average_score: detail.average_score,
        average_score_display: detail.average_score_display.clone(),
        score_is_ten_point: detail.score_is_ten_point,
        score_class: detail.score_class.clone(),
    };
    let title_json = serde_json::to_string(&title).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let result = sqlx::query(
        "INSERT INTO series_metadata_mapping_holds (series_id, provider_id, title_json, reason)
         SELECT id, anilist_id, ?3, ?4 FROM series WHERE id=?1 AND anilist_id=?2
         ON CONFLICT(series_id) DO UPDATE SET provider_id=excluded.provider_id,
            title_json=excluded.title_json, reason=excluded.reason, refreshed_at=CURRENT_TIMESTAMP",
    )
    .bind(series_id)
    .bind(detail.id)
    .bind(title_json)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn get_mapping_hold(
    db: &SqlitePool,
    series_id: i64,
    provider_id: i64,
) -> Result<Option<MetadataMappingHold>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, String, String, bool)>(
        "SELECT title_json, reason, refreshed_at, refreshed_at < datetime('now', ?3)
         FROM series_metadata_mapping_holds WHERE series_id=?1 AND provider_id=?2",
    )
    .bind(series_id)
    .bind(provider_id)
    .bind(format!("-{} days", METADATA_STALE_WARNING_DAYS))
    .fetch_optional(db)
    .await?;
    row.map(|(title_json, reason, refreshed_at, is_stale)| {
        let title =
            serde_json::from_str(&title_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        Ok(MetadataMappingHold {
            title,
            reason,
            refreshed_at,
            is_stale,
        })
    })
    .transpose()
}

pub(crate) async fn has_fresh_mapping_hold(
    db: &SqlitePool,
    series_id: i64,
    provider_id: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM series_metadata_mapping_holds
         WHERE series_id=?1 AND provider_id=?2 AND refreshed_at >= datetime('now', ?3))",
    )
    .bind(series_id)
    .bind(provider_id)
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .fetch_one(db)
    .await
}

pub(crate) async fn clear_mapping_hold(
    db: &SqlitePool,
    series_id: i64,
    provider_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM series_metadata_mapping_holds WHERE series_id=? AND provider_id=?")
        .bind(series_id)
        .bind(provider_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn get_by_series_id(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Option<CachedSeriesMetadata>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT provider_id, detail_json, cached_at,
               CASE
                   WHEN cached_at >= datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_fresh,
               CASE
                   WHEN cached_at < datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_stale
        FROM series_metadata_cache
        WHERE series_id = ?
        "#,
    )
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .bind(format!("-{} days", METADATA_STALE_WARNING_DAYS))
    .bind(series_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let detail_json: String = row.get("detail_json");
    let detail: AnimeDetail =
        serde_json::from_str(&detail_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Some(CachedSeriesMetadata {
        provider_id: row.get("provider_id"),
        detail,
        cached_at: row.get("cached_at"),
        is_fresh: row.get::<i64, _>("is_fresh") != 0,
        is_stale: row.get::<i64, _>("is_stale") != 0,
    }))
}

pub async fn upsert(
    db: &SqlitePool,
    series_id: i64,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    let detail_json =
        serde_json::to_string(detail).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        r#"
        INSERT INTO series_metadata_cache (series_id, provider_id, mal_id, detail_json, cached_at)
        VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(series_id) DO UPDATE SET
            provider_id = excluded.provider_id,
            mal_id = excluded.mal_id,
            detail_json = excluded.detail_json,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(series_id)
    .bind(provider_id)
    .bind(mal_id)
    .bind(detail_json)
    .execute(db)
    .await?;

    Ok(())
}

pub async fn get_by_provider_id(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<Option<CachedSeriesMetadata>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT provider_id, detail_json, cached_at,
               CASE
                   WHEN cached_at >= datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_fresh,
               CASE
                   WHEN cached_at < datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_stale
        FROM provider_metadata_cache
        WHERE provider_id = ?
        "#,
    )
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .bind(format!("-{} days", METADATA_STALE_WARNING_DAYS))
    .bind(provider_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let detail_json: String = row.get("detail_json");
    let detail: AnimeDetail =
        serde_json::from_str(&detail_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Some(CachedSeriesMetadata {
        provider_id: row.get("provider_id"),
        detail,
        cached_at: row.get("cached_at"),
        is_fresh: row.get::<i64, _>("is_fresh") != 0,
        is_stale: row.get::<i64, _>("is_stale") != 0,
    }))
}

pub async fn upsert_provider(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    let detail_json =
        serde_json::to_string(detail).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        r#"
        INSERT INTO provider_metadata_cache (provider_id, mal_id, detail_json, cached_at)
        VALUES (?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(provider_id) DO UPDATE SET
            mal_id = excluded.mal_id,
            detail_json = excluded.detail_json,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(provider_id)
    .bind(mal_id)
    .bind(detail_json)
    .execute(db)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};

    fn detail(id: i64) -> AnimeDetail {
        AnimeDetail {
            country_of_origin: None,
            release_date: None,
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: "Cache Test".to_string(),
            title_english: "Cache Test".to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(12),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2015),
            end_year: Some(2015),
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    #[tokio::test]
    async fn country_hint_round_trips_through_both_metadata_caches() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 123, "Country cache").await;
        let mut detail = detail(123);
        detail.country_of_origin = Some("CN".into());
        upsert(&db, series_id, 123, None, &detail).await.unwrap();
        upsert_provider(&db, 123, None, &detail).await.unwrap();
        for cached in [
            get_by_series_id(&db, series_id).await.unwrap().unwrap(),
            get_by_provider_id(&db, 123).await.unwrap().unwrap(),
        ] {
            assert_eq!(cached.detail.country_of_origin.as_deref(), Some("CN"));
            assert_eq!(
                cached.detail.original_language_hint(),
                Some(("Chinese", "China"))
            );
        }
    }

    async fn backdate(db: &SqlitePool, table: &str, key_col: &str, key: i64, modifier: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET cached_at = datetime('now', ?) WHERE {key_col} = ?"
        )))
        .bind(modifier)
        .bind(key)
        .execute(db)
        .await
        .expect("backdate cached_at");
    }

    // The two flags answer different questions: `is_fresh` is the
    // 12-hour refresh window, `is_stale` the week-long warning window.
    // A row between the two (past the refresh window, well short of a
    // week) is the normal state between sweeps and must warn nobody.
    #[tokio::test]
    async fn provider_row_is_stale_only_after_the_warning_window() {
        let db = in_memory_pool().await;
        upsert_provider(&db, 1535, None, &detail(1535))
            .await
            .expect("upsert");

        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(row.is_fresh, "just written: fresh");
        assert!(!row.is_stale, "just written: not stale");

        backdate(
            &db,
            "provider_metadata_cache",
            "provider_id",
            1535,
            "-13 hours",
        )
        .await;
        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(!row.is_fresh, "13 hours: past the refresh window");
        assert!(!row.is_stale, "13 hours: nowhere near the warning window");

        backdate(
            &db,
            "provider_metadata_cache",
            "provider_id",
            1535,
            "-6 days",
        )
        .await;
        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(!row.is_stale, "6 days: still short of the warning window");

        backdate(
            &db,
            "provider_metadata_cache",
            "provider_id",
            1535,
            "-8 days",
        )
        .await;
        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(!row.is_fresh, "8 days: past the refresh window");
        assert!(row.is_stale, "8 days: past the warning window");
    }

    #[tokio::test]
    async fn series_row_carries_the_same_flags() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1535, "Cache Test").await;
        upsert(&db, series_id, 1535, None, &detail(1535))
            .await
            .expect("upsert");

        let row = get_by_series_id(&db, series_id).await.unwrap().unwrap();
        assert!(row.is_fresh);
        assert!(!row.is_stale);

        backdate(
            &db,
            "series_metadata_cache",
            "series_id",
            series_id,
            "-8 days",
        )
        .await;
        let row = get_by_series_id(&db, series_id).await.unwrap().unwrap();
        assert!(!row.is_fresh);
        assert!(row.is_stale);
    }
}

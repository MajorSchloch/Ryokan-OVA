//! Nullable quality overrides. Language overrides retain their existing storage.
use sqlx::{Row, SqlitePool};

#[derive(Clone, Debug, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct QualityPreferences {
    pub minimum_resolution: Option<String>,
    pub target_resolution: Option<String>,
    pub minimum_subtitle_check: Option<bool>,
    pub target_subtitle_check: Option<bool>,
}

#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub struct FranchisePreferences {
    pub page_id: String,
    pub audio_languages: Option<String>,
    pub audio_match: Option<String>,
    pub subtitle_languages: Option<String>,
    pub subtitle_match: Option<String>,
    pub minimum_resolution: Option<String>,
    pub target_resolution: Option<String>,
    pub minimum_subtitle_check: Option<bool>,
    pub target_subtitle_check: Option<bool>,
}

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let columns = sqlx::query("PRAGMA table_info(series_language_preferences)")
        .fetch_all(db)
        .await?;
    for (name, kind) in [
        ("minimum_resolution", "TEXT"),
        ("target_resolution", "TEXT"),
        ("minimum_subtitle_check", "BOOLEAN"),
        ("target_subtitle_check", "BOOLEAN"),
    ] {
        if !columns.iter().any(|r| r.get::<String, _>("name") == name) {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE series_language_preferences ADD COLUMN {name} {kind}"
            )))
            .execute(db)
            .await?;
        }
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS franchise_requirement_preferences (
        page_id TEXT PRIMARY KEY, audio_languages TEXT, audio_match TEXT,
        subtitle_languages TEXT, subtitle_match TEXT, minimum_resolution TEXT,
        target_resolution TEXT, minimum_subtitle_check BOOLEAN, target_subtitle_check BOOLEAN)",
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn save_quality(
    db: &SqlitePool,
    series_id: i64,
    p: &QualityPreferences,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO series_language_preferences(series_id,minimum_resolution,target_resolution,minimum_subtitle_check,target_subtitle_check)
        VALUES(?,?,?,?,?) ON CONFLICT(series_id) DO UPDATE SET minimum_resolution=excluded.minimum_resolution,
        target_resolution=excluded.target_resolution,minimum_subtitle_check=excluded.minimum_subtitle_check,target_subtitle_check=excluded.target_subtitle_check")
        .bind(series_id).bind(&p.minimum_resolution).bind(&p.target_resolution).bind(p.minimum_subtitle_check).bind(p.target_subtitle_check).execute(db).await?;
    Ok(())
}

pub async fn save_franchise(db: &SqlitePool, p: &FranchisePreferences) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO franchise_requirement_preferences(page_id,audio_languages,audio_match,subtitle_languages,subtitle_match,minimum_resolution,target_resolution,minimum_subtitle_check,target_subtitle_check)
        VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(page_id) DO UPDATE SET audio_languages=excluded.audio_languages,audio_match=excluded.audio_match,
        subtitle_languages=excluded.subtitle_languages,subtitle_match=excluded.subtitle_match,minimum_resolution=excluded.minimum_resolution,
        target_resolution=excluded.target_resolution,minimum_subtitle_check=excluded.minimum_subtitle_check,target_subtitle_check=excluded.target_subtitle_check")
        .bind(&p.page_id).bind(&p.audio_languages).bind(&p.audio_match).bind(&p.subtitle_languages).bind(&p.subtitle_match)
        .bind(&p.minimum_resolution).bind(&p.target_resolution).bind(p.minimum_subtitle_check).bind(p.target_subtitle_check).execute(db).await?;
    Ok(())
}

pub async fn franchise(db: &SqlitePool, id: &str) -> Result<FranchisePreferences, sqlx::Error> {
    Ok(sqlx::query_as::<_, FranchisePreferences>(
        "SELECT * FROM franchise_requirement_preferences WHERE page_id=?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .unwrap_or_else(|| FranchisePreferences {
        page_id: id.into(),
        ..Default::default()
    }))
}

pub async fn quality(db: &SqlitePool, id: i64) -> Result<QualityPreferences, sqlx::Error> {
    Ok(sqlx::query_as("SELECT minimum_resolution,target_resolution,minimum_subtitle_check,target_subtitle_check FROM series_language_preferences WHERE series_id=?")
        .bind(id).fetch_optional(db).await?.unwrap_or_default())
}

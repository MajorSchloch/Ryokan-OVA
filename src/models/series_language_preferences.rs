//! Independent per-release language overrides. NULL inherits the library;
//! an empty list explicitly removes that language requirement.
use sqlx::SqlitePool;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct LanguagePreferences {
    pub audio_languages: Option<String>,
    pub audio_match: Option<String>,
    pub subtitle_languages: Option<String>,
    pub subtitle_match: Option<String>,
}

pub async fn get(db: &SqlitePool, series_id: i64) -> Result<LanguagePreferences, sqlx::Error> {
    Ok(sqlx::query_as::<_, LanguagePreferences>(
        "SELECT audio_languages, audio_match, subtitle_languages, subtitle_match
         FROM series_language_preferences WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_optional(db)
    .await?
    .unwrap_or_default())
}

pub async fn get_all(db: &SqlitePool) -> Result<HashMap<i64, LanguagePreferences>, sqlx::Error> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT series_id, audio_languages, audio_match, subtitle_languages, subtitle_match
         FROM series_language_preferences",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get("series_id"),
                LanguagePreferences {
                    audio_languages: row.get("audio_languages"),
                    audio_match: row.get("audio_match"),
                    subtitle_languages: row.get("subtitle_languages"),
                    subtitle_match: row.get("subtitle_match"),
                },
            )
        })
        .collect())
}

pub async fn save(
    db: &SqlitePool,
    series_id: i64,
    preferences: &LanguagePreferences,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO series_language_preferences
             (series_id, audio_languages, audio_match, subtitle_languages, subtitle_match)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(series_id) DO UPDATE SET
             audio_languages = excluded.audio_languages, audio_match = excluded.audio_match,
             subtitle_languages = excluded.subtitle_languages, subtitle_match = excluded.subtitle_match",
    )
    .bind(series_id)
    .bind(&preferences.audio_languages)
    .bind(&preferences.audio_match)
    .bind(&preferences.subtitle_languages)
    .bind(&preferences.subtitle_match)
    .execute(db)
    .await?;
    Ok(())
}

//! One effective language policy for acquisition, import, and file details.
use sqlx::SqlitePool;

use crate::models::{
    config::Config,
    series_language_preferences::{self, LanguagePreferences},
};

/// Normalize names/codes without silently discarding an invalid requirement.
pub fn normalize_languages(value: &str, kind: &str) -> Result<String, String> {
    let mut languages = Vec::new();
    for part in value
        .split([',', ';', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let language = super::media_info::normalize_language(part).ok_or_else(|| {
            format!("Unknown {kind} language: {part}. Choose a language or enter its name or code.")
        })?;
        if !languages.contains(&language) {
            languages.push(language);
        }
    }
    if languages.len() > 16 {
        return Err(format!("Choose at most 16 {kind} languages."));
    }
    Ok(languages.join(","))
}

fn apply_one(
    languages: &Option<String>,
    mode: &Option<String>,
    kind: &str,
) -> Result<Option<(String, String)>, String> {
    let Some(languages) = languages else {
        if mode.is_some() {
            return Err(format!(
                "Language requirement: invalid inherited {kind} rule."
            ));
        }
        return Ok(None);
    };
    let mode = mode.as_deref().unwrap_or("all");
    if !matches!(mode, "all" | "any") {
        return Err(format!(
            "Language requirement: choose all or any {kind} languages."
        ));
    }
    Ok(Some((
        normalize_languages(languages, kind)?,
        mode.to_string(),
    )))
}

pub fn apply(cfg: &Config, preferences: &LanguagePreferences) -> Result<Config, String> {
    let mut effective = cfg.clone();
    if let Some((languages, mode)) = apply_one(
        &preferences.audio_languages,
        &preferences.audio_match,
        "audio",
    )? {
        effective.required_audio_languages = languages;
        effective.audio_language_match = mode;
    }
    if let Some((languages, mode)) = apply_one(
        &preferences.subtitle_languages,
        &preferences.subtitle_match,
        "subtitle",
    )? {
        effective.required_subtitle_languages = languages;
        effective.subtitle_language_match = mode;
    }
    Ok(effective)
}

/// A failed lookup is an error, never permission to use another language rule.
pub async fn effective_config(
    db: &SqlitePool,
    cfg: &Config,
    series_id: i64,
) -> Result<Config, String> {
    let preferences = series_language_preferences::get(db, series_id)
        .await
        .map_err(|_| {
            "Language requirement: could not read this release's language settings.".to_string()
        })?;
    apply(cfg, &preferences)
}

/// Library pages read overrides once; one-release menus keep their scoped query.
pub async fn effective_configs(
    db: &SqlitePool,
    cfg: &Config,
    series_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Config>, String> {
    if let [series_id] = series_ids {
        return Ok(std::collections::HashMap::from([(
            *series_id,
            effective_config(db, cfg, *series_id).await?,
        )]));
    }
    let preferences = series_language_preferences::get_all(db)
        .await
        .map_err(|_| {
            "Language requirement: could not read release language settings.".to_string()
        })?;
    let inherited = LanguagePreferences::default();
    series_ids
        .iter()
        .map(|id| Ok((*id, apply(cfg, preferences.get(id).unwrap_or(&inherited))?)))
        .collect()
}

/// Search can also open an untracked provider entry, which uses library defaults.
pub async fn for_provider(
    db: &SqlitePool,
    cfg: &Config,
    provider_id: i64,
) -> Result<Config, String> {
    let release = crate::models::series::get_by_anilist_id(db, provider_id)
        .await
        .map_err(|_| {
            "Language requirement: could not resolve the release's saved language settings."
                .to_string()
        })?;
    match release {
        Some(release) => effective_config(db, cfg, release.id).await,
        None => Ok(cfg.clone()),
    }
}

/// A release label can prioritize a download, but cannot satisfy a file gate.
/// An adjacent subtitle marker must not be mistaken for a requested dub.
pub fn audio_hint_bonus(cfg: &Config, title: &str) -> i32 {
    let wanted: std::collections::HashSet<_> = cfg
        .required_audio_languages
        .split(',')
        .filter_map(super::media_info::normalize_language)
        .collect();
    if wanted.is_empty() {
        return 0;
    }
    let tokens: Vec<_> = title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut matched = std::collections::HashSet::new();
    for (index, token) in tokens.iter().enumerate() {
        let language_token = token
            .strip_suffix("dubbed")
            .or_else(|| token.strip_suffix("dub"))
            .unwrap_or(token);
        let Some(language) = super::media_info::normalize_language(language_token) else {
            continue;
        };
        let previous = index
            .checked_sub(1)
            .and_then(|i| tokens.get(i))
            .map(String::as_str)
            .unwrap_or_default();
        let next = tokens
            .get(index + 1)
            .map(String::as_str)
            .unwrap_or_default();
        if previous.starts_with("sub")
            || next.starts_with("sub")
            || previous == "commentary"
            || next == "commentary"
        {
            continue;
        }
        if wanted.contains(&language) {
            matched.insert(language);
        }
    }
    let count = if cfg.audio_language_match == "any" {
        usize::from(!matched.is_empty())
    } else {
        matched.len()
    };
    // Explicit CF floors are applied before this hint. Resolution target
    // ordering remains above scores, and actual audio is checked at import.
    2_000 * i32::try_from(count.min(16)).unwrap_or(16)
}

pub fn apply_audio_hint(cfg: &Config, result: &mut super::nyaa::SearchResult) {
    let bonus = audio_hint_bonus(cfg, &result.title);
    if bonus == 0 {
        return;
    }
    result.score = result.score.saturating_add(bonus);
    result.score_breakdown.push(super::scoring::ScoreComponent {
        label: "Requested audio language hint".into(),
        delta: bonus,
        detail: Some("Release label only. The downloaded audio must pass the file check.".into()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inheritance_empty_override_and_other_release_are_independent() {
        let db = crate::test_support::in_memory_pool().await;
        let first = crate::test_support::seed_series(&db, 1, "First").await;
        let second = crate::test_support::seed_series(&db, 2, "Second").await;
        let cfg = Config {
            required_subtitle_languages: "en".into(),
            ..Default::default()
        };
        let inherited = effective_config(&db, &cfg, first).await.unwrap();
        assert_eq!(inherited.required_subtitle_languages, "en");
        series_language_preferences::save(
            &db,
            first,
            &LanguagePreferences {
                audio_languages: Some("German".into()),
                audio_match: Some("all".into()),
                subtitle_languages: Some(String::new()),
                subtitle_match: Some("all".into()),
            },
        )
        .await
        .unwrap();
        let custom = effective_config(&db, &cfg, first).await.unwrap();
        assert_eq!(custom.required_audio_languages, "de");
        assert!(custom.required_subtitle_languages.is_empty());
        assert_eq!(custom.minimum_resolution, cfg.minimum_resolution);
        assert_eq!(custom.default_allow_upgrades, cfg.default_allow_upgrades);
        assert_eq!(
            effective_config(&db, &cfg, second)
                .await
                .unwrap()
                .required_subtitle_languages,
            "en"
        );
        series_language_preferences::save(&db, first, &LanguagePreferences::default())
            .await
            .unwrap();
        assert_eq!(
            effective_config(&db, &cfg, first)
                .await
                .unwrap()
                .required_subtitle_languages,
            "en"
        );
        sqlx::query("DROP TABLE series_language_preferences")
            .execute(&db)
            .await
            .unwrap();
        assert!(effective_config(&db, &cfg, first).await.is_err());
    }

    #[test]
    fn aliases_are_normalized_and_invalid_rules_fail_closed() {
        assert_eq!(
            normalize_languages("German, deu, Spanish", "audio").unwrap(),
            "de,es"
        );
        assert!(normalize_languages("und", "audio").is_err());
        assert!(
            apply(
                &Config::default(),
                &LanguagePreferences {
                    audio_languages: Some("de".into()),
                    audio_match: Some("invalid".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn requested_dub_hints_do_not_count_subtitle_or_commentary_labels() {
        let mut cfg = Config {
            required_audio_languages: "de".into(),
            ..Default::default()
        };
        assert!(
            audio_hint_bonus(&cfg, "Show German Dub [1080p]")
                > audio_hint_bonus(&cfg, "Show Japanese Audio [1080p]")
        );
        assert_eq!(audio_hint_bonus(&cfg, "Show German-Sub [1080p]"), 0);
        assert_eq!(audio_hint_bonus(&cfg, "Show German Commentary [1080p]"), 0);
        cfg.required_audio_languages = "en".into();
        assert_eq!(audio_hint_bonus(&cfg, "Show English-Sub [1080p]"), 0);
        cfg.required_audio_languages = "Spanish,deu".into();
        cfg.audio_language_match = "any".into();
        assert_eq!(
            audio_hint_bonus(&cfg, "Show German Spanish Dual Audio"),
            2_000
        );
        cfg.audio_language_match = "all".into();
        assert_eq!(
            audio_hint_bonus(&cfg, "Show German Spanish Dual Audio"),
            4_000
        );
    }
}

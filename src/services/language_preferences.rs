//! One effective language policy for acquisition, import, and file details.
use sqlx::Row;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};

use crate::models::{
    config::Config,
    requirement_preferences::{self, FranchisePreferences, QualityPreferences},
    series_language_preferences::{self, LanguagePreferences},
};

#[derive(Clone)]
pub struct EffectivePolicy {
    pub config: Config,
    pub origins: BTreeMap<&'static str, &'static str>,
    pub franchise_id: Option<String>,
}

pub fn overlay_quality(cfg: &Config, p: &QualityPreferences) -> Config {
    let mut cfg = cfg.clone();
    if let Some(v) = &p.minimum_resolution {
        cfg.minimum_resolution = v.clone();
    }
    if let Some(v) = &p.target_resolution {
        cfg.target_resolution = v.clone();
    }
    if let Some(v) = p.minimum_subtitle_check {
        cfg.minimum_subtitle_check = v;
    }
    if let Some(v) = p.target_subtitle_check {
        cfg.target_subtitle_check = v;
    }
    cfg
}

pub fn apply_quality(cfg: &Config, p: &QualityPreferences) -> Result<Config, String> {
    let cfg = overlay_quality(cfg, p);
    let minimum = super::source::Resolution::from_str(&cfg.minimum_resolution);
    let target = super::source::Resolution::from_str(&cfg.target_resolution);
    if minimum == super::source::Resolution::Unknown
        || target == super::source::Resolution::Unknown
        || target.rank() < minimum.rank()
    {
        return Err("Choose a valid target resolution at least as high as the minimum.".into());
    }
    Ok(cfg)
}

pub fn apply_franchise(cfg: &Config, p: &FranchisePreferences) -> Result<Config, String> {
    let cfg = apply(
        cfg,
        &LanguagePreferences {
            audio_languages: p.audio_languages.clone(),
            audio_match: p.audio_match.clone(),
            subtitle_languages: p.subtitle_languages.clone(),
            subtitle_match: p.subtitle_match.clone(),
        },
    )?;
    Ok(overlay_quality(
        &cfg,
        &QualityPreferences {
            minimum_resolution: p.minimum_resolution.clone(),
            target_resolution: p.target_resolution.clone(),
            minimum_subtitle_check: p.minimum_subtitle_check,
            target_subtitle_check: p.target_subtitle_check,
        },
    ))
}

pub async fn franchise_config(
    db: &SqlitePool,
    cfg: &Config,
    page_id: &str,
) -> Result<Config, String> {
    let id = crate::models::library_layout::resolve_page_id(db, page_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Franchise not found.")?;
    let p = requirement_preferences::franchise(db, &id)
        .await
        .map_err(|e| e.to_string())?;
    apply_franchise(cfg, &p)
}

/// Resolve stored membership through either a saved provider or its unique
/// cached AniList identity. Conflicting saved parents remain an error.
pub async fn franchise_for_release(db: &SqlitePool, id: i64) -> Result<Option<String>, String> {
    let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{} SELECT DISTINCT f.page_id FROM series s LEFT JOIN local_anilist_ids l ON l.series_id=s.id JOIN library_franchise_members m ON m.provider_id IN(l.anilist_id,s.anilist_id) JOIN library_franchises f ON f.folder=m.folder WHERE s.id=?",crate::models::series::LOCAL_ANILIST_IDS))).bind(id).fetch_all(db).await.map_err(|e|e.to_string())?;
    if ids.len() > 1 {
        return Err("This release has conflicting franchise settings.".into());
    }
    Ok(ids.into_iter().next())
}

/// Bulk snapshot shared by RSS, library summaries and single-release callers.
pub async fn resolve_snapshot(
    db: &SqlitePool,
    cfg: &Config,
    ids: &[i64],
) -> Result<HashMap<i64, Result<EffectivePolicy, String>>, String> {
    let languages = series_language_preferences::get_all(db)
        .await
        .map_err(|e| e.to_string())?;
    let qualities = sqlx::query("SELECT series_id,minimum_resolution,target_resolution,minimum_subtitle_check,target_subtitle_check FROM series_language_preferences").fetch_all(db).await.map_err(|e| e.to_string())?;
    let qualities: HashMap<i64, QualityPreferences> = qualities
        .into_iter()
        .map(|r| {
            (
                r.get("series_id"),
                QualityPreferences {
                    minimum_resolution: r.get("minimum_resolution"),
                    target_resolution: r.get("target_resolution"),
                    minimum_subtitle_check: r.get("minimum_subtitle_check"),
                    target_subtitle_check: r.get("target_subtitle_check"),
                },
            )
        })
        .collect();
    let parents = sqlx::query(sqlx::AssertSqlSafe(format!("{} SELECT s.id AS series_id,f.page_id FROM series s LEFT JOIN local_anilist_ids l ON l.series_id=s.id JOIN library_franchise_members m ON m.provider_id IN(l.anilist_id,s.anilist_id) JOIN library_franchises f ON f.folder=m.folder", crate::models::series::LOCAL_ANILIST_IDS))).fetch_all(db).await.map_err(|e| e.to_string())?;
    let mut parent_ids = HashMap::<i64, String>::new();
    let mut conflicts = std::collections::HashSet::new();
    for r in parents {
        let id = r.get("series_id");
        if !ids.contains(&id) {
            continue;
        }
        let parent: String = r.get("page_id");
        if let Some(old) = parent_ids.insert(id, parent.clone())
            && old != parent
        {
            conflicts.insert(id);
        }
    }
    let franchises: HashMap<String, FranchisePreferences> =
        sqlx::query_as::<_, FranchisePreferences>(
            "SELECT * FROM franchise_requirement_preferences",
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|p| (p.page_id.clone(), p))
        .collect();
    let inherited_languages = LanguagePreferences::default();
    let inherited_quality = QualityPreferences::default();
    let mut result = HashMap::new();
    for id in ids {
        let resolved = (|| -> Result<EffectivePolicy, String> {
            if conflicts.contains(id) {
                return Err("This release has conflicting franchise settings.".into());
            }
            let parent = parent_ids.get(id);
            let p = parent.and_then(|id| franchises.get(id));
            let mut effective = if let Some(p) = p {
                apply_franchise(cfg, p)?
            } else {
                cfg.clone()
            };
            let l = languages.get(id).unwrap_or(&inherited_languages);
            let q = qualities.get(id).unwrap_or(&inherited_quality);
            effective = apply_quality(&apply(&effective, l)?, q)?;
            let mut origins = BTreeMap::new();
            for (name, custom, parent_custom) in [
                (
                    "audio",
                    l.audio_languages.is_some(),
                    p.is_some_and(|p| p.audio_languages.is_some()),
                ),
                (
                    "subtitles",
                    l.subtitle_languages.is_some(),
                    p.is_some_and(|p| p.subtitle_languages.is_some()),
                ),
                (
                    "minimum",
                    q.minimum_resolution.is_some(),
                    p.is_some_and(|p| p.minimum_resolution.is_some()),
                ),
                (
                    "target",
                    q.target_resolution.is_some(),
                    p.is_some_and(|p| p.target_resolution.is_some()),
                ),
                (
                    "minimum_subtitles",
                    q.minimum_subtitle_check.is_some(),
                    p.is_some_and(|p| p.minimum_subtitle_check.is_some()),
                ),
                (
                    "target_subtitles",
                    q.target_subtitle_check.is_some(),
                    p.is_some_and(|p| p.target_subtitle_check.is_some()),
                ),
            ] {
                origins.insert(
                    name,
                    if custom {
                        "Release"
                    } else if parent_custom {
                        "Franchise"
                    } else {
                        "App default"
                    },
                );
            }
            Ok(EffectivePolicy {
                config: effective,
                origins,
                franchise_id: parent.cloned(),
            })
        })();
        result.insert(*id, resolved);
    }
    Ok(result)
}

pub async fn resolve_all(
    db: &SqlitePool,
    cfg: &Config,
    ids: &[i64],
) -> Result<HashMap<i64, EffectivePolicy>, String> {
    resolve_snapshot(db, cfg, ids)
        .await?
        .into_iter()
        .map(|(id, value)| value.map(|p| (id, p)))
        .collect()
}

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

/// Raw values are only for the repair editor; acquisition must use `apply`.
pub fn overlay_languages(cfg: &Config, p: &LanguagePreferences) -> Config {
    let mut value = cfg.clone();
    if let Some(v) = &p.audio_languages {
        value.required_audio_languages = v.clone();
    }
    if let Some(v) = &p.audio_match {
        value.audio_language_match = v.clone();
    }
    if let Some(v) = &p.subtitle_languages {
        value.required_subtitle_languages = v.clone();
    }
    if let Some(v) = &p.subtitle_match {
        value.subtitle_language_match = v.clone();
    }
    value
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
    Ok(resolve_all(db, cfg, &[series_id])
        .await?
        .remove(&series_id)
        .ok_or("Could not resolve release requirements.")?
        .config)
}

/// Library pages read overrides once; one-release menus keep their scoped query.
pub async fn effective_configs(
    db: &SqlitePool,
    cfg: &Config,
    series_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Config>, String> {
    Ok(resolve_all(db, cfg, series_ids)
        .await?
        .into_iter()
        .map(|(id, p)| (id, p.config))
        .collect())
}

/// Search can also open an untracked provider entry, which uses library defaults.
pub async fn for_provider(
    db: &SqlitePool,
    cfg: &Config,
    provider_id: i64,
) -> Result<Config, String> {
    let ids: Vec<i64>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{} SELECT DISTINCT s.id FROM series s LEFT JOIN local_anilist_ids l ON l.series_id=s.id WHERE l.anilist_id=? OR s.anilist_id=?",crate::models::series::LOCAL_ANILIST_IDS))).bind(provider_id).bind(provider_id).fetch_all(db).await.map_err(|e|e.to_string())?;
    if ids.len() > 1 {
        return Err("This provider identity has conflicting release settings.".into());
    }
    match ids.first() {
        Some(id) => effective_config(db, cfg, *id).await,
        None => {
            match crate::models::library_layout::page_for_provider(db, provider_id)
                .await
                .map_err(|e| e.to_string())?
            {
                Some((page, _)) => apply_quality(
                    &franchise_config(db, cfg, &page).await?,
                    &QualityPreferences::default(),
                ),
                None => Ok(cfg.clone()),
            }
        }
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

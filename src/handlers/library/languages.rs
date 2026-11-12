//! Release language choices. Saving never starts a search or enables upgrades.
use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::{
    AppState,
    models::log::LogCategory,
    models::{
        config, series,
        series_language_preferences::{self, LanguagePreferences},
    },
    services::{language_preferences, logger},
};

const CHOICES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("ja", "Japanese"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("zh", "Chinese"),
    ("ko", "Korean"),
    ("ru", "Russian"),
    ("ar", "Arabic"),
    ("pl", "Polish"),
];

#[derive(Debug, Clone)]
pub struct LanguageOption {
    pub code: &'static str,
    pub name: &'static str,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct LanguageRow {
    pub mode: String,
    pub match_mode: String,
    pub options: Vec<LanguageOption>,
    pub other: String,
    pub default_summary: String,
}

#[derive(Debug, Clone)]
pub struct LanguagePreferencesView {
    pub series_id: i64,
    pub audio: LanguageRow,
    pub subtitles: LanguageRow,
    pub summary: String,
    pub message: Option<String>,
    pub error: Option<String>,
    pub available: bool,
    pub title_language: String,
}

fn summary(languages: &str, match_mode: &str, empty: &str) -> String {
    let names: Vec<String> = languages
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|code| {
            CHOICES
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| code.to_uppercase())
        })
        .collect();
    if names.is_empty() {
        return empty.to_string();
    }
    let mut value = names.join(", ");
    if names.len() > 1 {
        value.push_str(if match_mode == "any" {
            " (any one)"
        } else {
            " (all)"
        });
    }
    value
}

impl LanguageRow {
    fn new(
        override_languages: &Option<String>,
        override_match: &Option<String>,
        default: &str,
        default_match: &str,
        empty: &str,
    ) -> Self {
        let languages = override_languages.as_deref().unwrap_or(default);
        let selected: Vec<_> = languages.split(',').collect();
        Self {
            mode: match override_languages {
                None => "inherit",
                Some(value) if value.is_empty() => "any",
                Some(_) => "require",
            }
            .into(),
            match_mode: override_match
                .as_deref()
                .unwrap_or(default_match)
                .to_string(),
            options: CHOICES
                .iter()
                .map(|(code, name)| LanguageOption {
                    code,
                    name,
                    selected: selected.contains(code),
                })
                .collect(),
            other: selected
                .into_iter()
                .filter(|code| !code.is_empty() && !CHOICES.iter().any(|(known, _)| known == code))
                .collect::<Vec<_>>()
                .join(", "),
            default_summary: summary(default, default_match, empty),
        }
    }
}

impl LanguagePreferencesView {
    pub fn from_preferences(
        series_id: i64,
        cfg: &config::Config,
        preferences: &LanguagePreferences,
    ) -> Self {
        let audio = LanguageRow::new(
            &preferences.audio_languages,
            &preferences.audio_match,
            &cfg.required_audio_languages,
            &cfg.audio_language_match,
            "Any audio language",
        );
        let subtitles = LanguageRow::new(
            &preferences.subtitle_languages,
            &preferences.subtitle_match,
            &cfg.required_subtitle_languages,
            &cfg.subtitle_language_match,
            "Not required",
        );
        let audio_summary = summary(
            preferences
                .audio_languages
                .as_deref()
                .unwrap_or(&cfg.required_audio_languages),
            preferences
                .audio_match
                .as_deref()
                .unwrap_or(&cfg.audio_language_match),
            "Any",
        );
        let subtitle_summary = summary(
            preferences
                .subtitle_languages
                .as_deref()
                .unwrap_or(&cfg.required_subtitle_languages),
            preferences
                .subtitle_match
                .as_deref()
                .unwrap_or(&cfg.subtitle_language_match),
            "Not required",
        );
        Self {
            series_id,
            audio,
            subtitles,
            summary: format!("Audio: {audio_summary} · Subtitles: {subtitle_summary}"),
            message: None,
            error: None,
            available: true,
            title_language: cfg.title_language.clone(),
        }
    }

    pub fn unavailable(series_id: i64, cfg: &config::Config) -> Self {
        let mut view = Self::from_preferences(series_id, cfg, &LanguagePreferences::default());
        view.summary = "Language settings are unavailable".into();
        view.available = false;
        view.error = Some("Could not read this release's language settings. Reload the page before you change them.".into());
        view
    }

    fn preserve_form(&mut self, form: &LanguageForm) {
        for (kind, row, mode, matching, other) in [
            (
                "audio",
                &mut self.audio,
                &form.audio_mode,
                &form.audio_match,
                &form.audio_other,
            ),
            (
                "subtitle",
                &mut self.subtitles,
                &form.subtitle_mode,
                &form.subtitle_match,
                &form.subtitle_other,
            ),
        ] {
            row.mode.clone_from(mode);
            row.match_mode.clone_from(matching);
            row.other.clone_from(other);
            for option in &mut row.options {
                option.selected = form
                    .choices
                    .contains_key(&format!("{kind}_lang_{}", option.code));
            }
        }
    }
}

pub async fn load_view(
    db: &sqlx::SqlitePool,
    cfg: &config::Config,
    series_id: i64,
) -> Result<LanguagePreferencesView, String> {
    let preferences = series_language_preferences::get(db, series_id)
        .await
        .map_err(|_| "Could not read release language settings.".to_string())?;
    language_preferences::apply(cfg, &preferences)?;
    Ok(LanguagePreferencesView::from_preferences(
        series_id,
        cfg,
        &preferences,
    ))
}

#[derive(Debug, Deserialize, Default)]
pub struct LanguageForm {
    #[serde(default)]
    pub series_id: i64,
    #[serde(default)]
    pub audio_mode: String,
    #[serde(default)]
    pub audio_match: String,
    #[serde(default)]
    pub audio_other: String,
    #[serde(default)]
    pub subtitle_mode: String,
    #[serde(default)]
    pub subtitle_match: String,
    #[serde(default)]
    pub subtitle_other: String,
    #[serde(flatten)]
    pub choices: HashMap<String, String>,
}

impl LanguageForm {
    fn selection(
        &self,
        kind: &str,
        mode: &str,
        matching: &str,
        other: &str,
    ) -> Result<(Option<String>, Option<String>), String> {
        if mode == "inherit" {
            return Ok((None, None));
        }
        if mode == "any" {
            return Ok((Some(String::new()), Some("all".into())));
        }
        if mode != "require" {
            return Err(format!("Choose how this release uses {kind} languages."));
        }
        if !matches!(matching, "all" | "any") {
            return Err(format!("Choose all or any selected {kind} languages."));
        }
        let prefix = format!("{kind}_lang_");
        let mut picked: Vec<_> = self
            .choices
            .keys()
            .filter_map(|name| name.strip_prefix(&prefix))
            .collect();
        picked.sort_unstable();
        let mut input = picked.join(",");
        if !other.trim().is_empty() {
            input.push(',');
            input.push_str(other);
        }
        let languages = language_preferences::normalize_languages(&input, kind)?;
        if languages.is_empty() {
            return Err(format!(
                "Choose at least one {kind} language, or select the option without a requirement."
            ));
        }
        Ok((Some(languages), Some(matching.to_string())))
    }

    fn preferences(&self) -> Result<LanguagePreferences, String> {
        let (audio_languages, audio_match) = self.selection(
            "audio",
            &self.audio_mode,
            &self.audio_match,
            &self.audio_other,
        )?;
        let (subtitle_languages, subtitle_match) = self.selection(
            "subtitle",
            &self.subtitle_mode,
            &self.subtitle_match,
            &self.subtitle_other,
        )?;
        Ok(LanguagePreferences {
            audio_languages,
            audio_match,
            subtitle_languages,
            subtitle_match,
        })
    }
}

#[derive(Template)]
#[template(path = "partials/library/languages.html")]
struct LanguagePartial {
    languages: LanguagePreferencesView,
}

#[derive(Template)]
#[template(path = "library_languages.html")]
struct LanguagePage {
    page: &'static str,
    title: String,
    title_language: String,
    languages: LanguagePreferencesView,
}

fn render(view: LanguagePreferencesView, title: String, is_htmx: bool) -> Response {
    if is_htmx {
        Html(
            LanguagePartial { languages: view }
                .render()
                .unwrap_or_default(),
        )
        .into_response()
    } else {
        Html(
            LanguagePage {
                page: "library",
                title,
                title_language: view.title_language.clone(),
                languages: view,
            }
            .render()
            .unwrap_or_default(),
        )
        .into_response()
    }
}

pub async fn save_languages(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<LanguageForm>,
) -> Response {
    let release = match series::get_by_id(&state.db, form.series_id).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                "This release is no longer in the library.",
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not read the library entry.",
            )
                .into_response();
        }
    };
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(value)) => value,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not read the library settings.",
            )
                .into_response();
        }
    };
    let mut view = match load_view(&state.db, &cfg, release.id).await {
        Ok(value) => value,
        Err(_) => {
            return render(
                LanguagePreferencesView::unavailable(release.id, &cfg),
                release.title,
                is_htmx,
            );
        }
    };
    view.preserve_form(&form);
    let preferences = match form.preferences() {
        Ok(value) => value,
        Err(error) => {
            view.error = Some(error);
            return render(view, release.title, is_htmx);
        }
    };
    if series_language_preferences::save(&state.db, release.id, &preferences)
        .await
        .is_err()
    {
        view.error = Some(
            "Could not save language settings. Your choices are still shown; please try again."
                .into(),
        );
        return render(view, release.title, is_htmx);
    }
    crate::services::interactive_search_cache::clear(&state.interactive_search_cache);
    logger::info(
        &state.db,
        LogCategory::Library,
        "Saved release language choices",
        &format!("series_id={}", release.id),
    )
    .await;
    if !is_htmx {
        return crate::handlers::responses::htmx_aware_redirect(
            false,
            &format!("/series/{}", release.id),
        );
    }
    let mut saved = LanguagePreferencesView::from_preferences(release.id, &cfg, &preferences);
    saved.message =
        Some("Language settings saved. Existing files and upgrade settings are unchanged.".into());
    render(saved, release.title, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::FromRequest;

    async fn decode_form(body: &str) -> LanguageForm {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/library/languages")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        Form::<LanguageForm>::from_request(request, &())
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn named_checkboxes_and_explicit_no_subtitles_do_not_inherit_english() {
        let form = decode_form("series_id=34&audio_mode=require&audio_match=all&audio_lang_de=on&subtitle_mode=any&subtitle_match=all").await;
        let preferences = form.preferences().unwrap();
        assert_eq!(preferences.audio_languages.as_deref(), Some("de"));
        assert_eq!(preferences.subtitle_languages.as_deref(), Some(""));
        let cfg = language_preferences::apply(
            &config::Config {
                required_subtitle_languages: "en".into(),
                ..Default::default()
            },
            &preferences,
        )
        .unwrap();
        assert!(cfg.required_subtitle_languages.is_empty());
        assert_eq!(cfg.required_audio_languages, "de");
    }

    #[tokio::test]
    async fn render_private_digimon_language_controls_from_saved_release_choices() {
        let db = crate::test_support::in_memory_pool().await;
        let cfg = config::Config {
            required_subtitle_languages: "en".into(),
            ..Default::default()
        };
        config::save_config(&db, &cfg).await.unwrap();
        let id = crate::test_support::seed_series(&db, 552, "Digimon Adventure").await;
        sqlx::query("UPDATE series SET allow_upgrades=0 WHERE id=?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        series_language_preferences::save(
            &db,
            id,
            &LanguagePreferences {
                audio_languages: Some("de".into()),
                audio_match: Some("all".into()),
                subtitle_languages: Some(String::new()),
                subtitle_match: Some("all".into()),
            },
        )
        .await
        .unwrap();
        let saved = series_language_preferences::get(&db, id).await.unwrap();
        let view = LanguagePreferencesView::from_preferences(id, &cfg, &saved);
        assert_eq!(view.summary, "Audio: German · Subtitles: Not required");
        assert_eq!(view.audio.mode, "require");
        assert_eq!(view.subtitles.mode, "any");
        let response = render(view, "Digimon Adventure".into(), false);
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let rendered = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(rendered.contains("Digimon Adventure"));
        assert!(rendered.contains("audio_lang_de"));
        assert!(
            !series::get_by_id(&db, id)
                .await
                .unwrap()
                .unwrap()
                .allow_upgrades
        );
        assert_eq!(
            config::get_config(&db)
                .await
                .unwrap()
                .unwrap()
                .required_subtitle_languages,
            "en"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        // Only an explicit private test run emits this rendered page.
        if let Some(output) = std::env::var_os("RYOKAN_LANGUAGE_PREVIEW_HTML") {
            std::fs::write(output, &rendered).expect("write private release language preview");
        }
    }

    #[tokio::test]
    async fn save_changes_only_this_releases_languages_and_invalidates_search_results() {
        let db = crate::test_support::in_memory_pool().await;
        config::save_config(
            &db,
            &config::Config {
                media_requirements_enabled: true,
                required_subtitle_languages: "en".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let id = crate::test_support::seed_series(&db, 10, "Release").await;
        let other = crate::test_support::seed_series(&db, 11, "Other").await;
        let original_upgrades = series::get_by_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .allow_upgrades;
        let state = crate::test_support::build_test_app_state(db.clone(), None);
        crate::services::interactive_search_cache::insert(
            &state.interactive_search_cache,
            crate::services::interactive_search_cache::Key::Episode(id, 1),
            Vec::new(),
        );
        let form = decode_form(&format!("series_id={id}&audio_mode=require&audio_match=all&audio_lang_de=on&subtitle_mode=any&subtitle_match=all")).await;
        let response = save_languages(State(state.clone()), HxRequest(true), Form(form)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = series_language_preferences::get(&db, id).await.unwrap();
        assert_eq!(saved.audio_languages.as_deref(), Some("de"));
        assert_eq!(saved.subtitle_languages.as_deref(), Some(""));
        assert_eq!(
            series_language_preferences::get(&db, other).await.unwrap(),
            LanguagePreferences::default()
        );
        assert_eq!(
            config::get_config(&db)
                .await
                .unwrap()
                .unwrap()
                .required_subtitle_languages,
            "en"
        );
        assert_eq!(
            series::get_by_id(&db, id)
                .await
                .unwrap()
                .unwrap()
                .allow_upgrades,
            original_upgrades
        );
        assert!(state.interactive_search_cache.lock().unwrap().is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn invalid_choices_leave_saved_languages_and_upgrade_flag_unchanged() {
        let db = crate::test_support::in_memory_pool().await;
        config::save_config(&db, &config::Config::default())
            .await
            .unwrap();
        let id = crate::test_support::seed_series(&db, 10, "Release").await;
        let original_upgrades = series::get_by_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .allow_upgrades;
        let response = save_languages(
            State(crate::test_support::build_test_app_state(db.clone(), None)),
            HxRequest(true),
            Form(LanguageForm {
                series_id: id,
                audio_mode: "require".into(),
                audio_match: "all".into(),
                audio_other: "und".into(),
                subtitle_mode: "inherit".into(),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("Unknown audio language"));
        assert_eq!(
            series_language_preferences::get(&db, id).await.unwrap(),
            LanguagePreferences::default()
        );
        assert_eq!(
            series::get_by_id(&db, id)
                .await
                .unwrap()
                .unwrap()
                .allow_upgrades,
            original_upgrades
        );
    }
}

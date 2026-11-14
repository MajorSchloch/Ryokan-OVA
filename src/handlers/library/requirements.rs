//! Authenticated, scoped requirement forms. Saving never starts acquisition.
use crate::{
    AppState,
    models::{
        config, library_layout,
        requirement_preferences::{self, FranchisePreferences, QualityPreferences},
        series,
        series_language_preferences::LanguagePreferences,
    },
    services::language_preferences,
};
use askama::Template;
use axum::{
    Form, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use axum_htmx::HxRequest;
use serde::Deserialize;

#[derive(Clone, Default)]
pub struct RequirementField {
    pub name: &'static str,
    pub label: &'static str,
    pub value: String,
    pub options: Vec<(String, String)>,
    pub effective: String,
    pub origin: String,
}
#[derive(Clone, Default)]
pub struct RequirementsView {
    pub scope: String,
    pub id: String,
    pub return_to: String,
    pub fields: Vec<RequirementField>,
    pub audio: String,
    pub subtitles: String,
    pub audio_match: String,
    pub subtitle_match: String,
    pub error: String,
    pub message: String,
}
#[derive(Template)]
#[template(path = "partials/library/requirements.html")]
struct Partial {
    requirements: RequirementsView,
}
#[derive(Template)]
#[template(path = "requirements.html")]
struct Page {
    page: &'static str,
    title_language: String,
    requirements: RequirementsView,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/library/requirements", post(save))
}

pub async fn release_view(
    db: &sqlx::SqlitePool,
    cfg: &config::Config,
    id: i64,
) -> Result<RequirementsView, String> {
    let row = series::get_by_id(db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Release not found.")?;
    let resolved = language_preferences::resolve_snapshot(db, cfg, &[id])
        .await?
        .remove(&id);
    let parent = language_preferences::franchise_for_release(db, id).await?;
    let base = if let Some(parent) = &parent {
        language_preferences::franchise_config(db, cfg, parent).await?
    } else {
        cfg.clone()
    };
    let l = crate::models::series_language_preferences::get(db, id)
        .await
        .map_err(|e| e.to_string())?;
    let q = requirement_preferences::quality(db, id)
        .await
        .map_err(|e| e.to_string())?;
    let raw = language_preferences::overlay_languages(&base, &l);
    let effective = language_preferences::overlay_quality(&raw, &q);
    let mut v = view("release", &id.to_string(), &base, &effective, &l, &q);
    v.return_to = super::detail::path(row.anilist_id);
    if let Some(Ok(resolved)) = resolved {
        for f in &mut v.fields {
            f.origin = resolved
                .origins
                .get(match f.name {
                    "audio_mode" => "audio",
                    "subtitle_mode" => "subtitles",
                    "minimum_resolution" => "minimum",
                    "target_resolution" => "target",
                    "minimum_subtitle_check" => "minimum_subtitles",
                    _ => "target_subtitles",
                })
                .unwrap_or(&"App default")
                .to_string();
        }
    } else {
        v.error="These saved requirements are invalid. Correct the fields below or choose inherited values.".into();
    }
    Ok(v)
}

pub async fn franchise_view(
    db: &sqlx::SqlitePool,
    cfg: &config::Config,
    id: &str,
) -> Result<RequirementsView, String> {
    let id = library_layout::resolve_page_id(db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Franchise not found.")?;
    let p = requirement_preferences::franchise(db, &id)
        .await
        .map_err(|e| e.to_string())?;
    let l = LanguagePreferences {
        audio_languages: p.audio_languages,
        audio_match: p.audio_match,
        subtitle_languages: p.subtitle_languages,
        subtitle_match: p.subtitle_match,
    };
    let q = QualityPreferences {
        minimum_resolution: p.minimum_resolution,
        target_resolution: p.target_resolution,
        minimum_subtitle_check: p.minimum_subtitle_check,
        target_subtitle_check: p.target_subtitle_check,
    };
    let effective = language_preferences::overlay_quality(
        &language_preferences::overlay_languages(cfg, &l),
        &q,
    );
    let mut v = view("franchise", &id, cfg, &effective, &l, &q);
    if let Err(error) = language_preferences::apply(cfg, &l)
        .and_then(|c| language_preferences::apply_quality(&c, &q))
    {
        v.error = error;
    }
    v.return_to = format!("/franchises/{id}");
    Ok(v)
}

pub fn brief(cfg: &config::Config) -> String {
    format!(
        "{} audio · {} subtitles · {}p minimum → {}p target",
        languages(&cfg.required_audio_languages),
        languages(&cfg.required_subtitle_languages),
        cfg.minimum_resolution,
        cfg.target_resolution
    )
}

fn languages(raw: &str) -> String {
    if raw.is_empty() {
        return "No language requirement".into();
    }
    raw.split(',')
        .map(|s| match s {
            "ja" => "Japanese",
            "en" => "English",
            "de" => "German",
            "es" => "Spanish",
            "fr" => "French",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(", ")
}
fn view(
    scope: &str,
    id: &str,
    base: &config::Config,
    effective: &config::Config,
    l: &LanguagePreferences,
    q: &QualityPreferences,
) -> RequirementsView {
    let mut fields = Vec::new();
    for (name, label, value, current, inherited) in [
        (
            "audio_mode",
            "Audio language requirement",
            l.audio_languages.as_deref(),
            languages(&effective.required_audio_languages),
            languages(&base.required_audio_languages),
        ),
        (
            "subtitle_mode",
            "Subtitle language requirement",
            l.subtitle_languages.as_deref(),
            languages(&effective.required_subtitle_languages),
            languages(&base.required_subtitle_languages),
        ),
    ] {
        fields.push(RequirementField {
            name,
            label,
            value: match value {
                None => "inherit",
                Some("") => "none",
                Some(_) => "require",
            }
            .into(),
            options: vec![
                ("inherit".into(), format!("Use inherited: {inherited}")),
                ("none".into(), "No language requirement".into()),
                ("require".into(), "Choose required languages".into()),
            ],
            effective: current,
            origin: if value.is_some() {
                if scope == "franchise" {
                    "Franchise"
                } else {
                    "Release"
                }
            } else {
                "App default"
            }
            .into(),
        });
    }
    for (name, label, value, current, inherited) in [
        (
            "minimum_resolution",
            "Minimum resolution",
            &q.minimum_resolution,
            &effective.minimum_resolution,
            &base.minimum_resolution,
        ),
        (
            "target_resolution",
            "Good enough resolution",
            &q.target_resolution,
            &effective.target_resolution,
            &base.target_resolution,
        ),
    ] {
        let mut options = vec![(String::new(), format!("Use inherited: {inherited}p"))];
        options.extend(["480", "576", "720", "1080", "2160"].map(|v| (v.into(), format!("{v}p"))));
        fields.push(RequirementField {
            name,
            label,
            value: value.clone().unwrap_or_default(),
            options,
            effective: format!("{current}p"),
            origin: if value.is_some() {
                if scope == "franchise" {
                    "Franchise"
                } else {
                    "Release"
                }
            } else {
                "App default"
            }
            .into(),
        });
    }
    for (name, label, value, current, inherited) in [
        (
            "minimum_subtitle_check",
            "Minimum subtitle check",
            q.minimum_subtitle_check,
            effective.minimum_subtitle_check,
            base.minimum_subtitle_check,
        ),
        (
            "target_subtitle_check",
            "Target subtitle check",
            q.target_subtitle_check,
            effective.target_subtitle_check,
            base.target_subtitle_check,
        ),
    ] {
        fields.push(RequirementField {
            name,
            label,
            value: value
                .map(|b| if b { "1" } else { "0" }.into())
                .unwrap_or_default(),
            options: vec![
                (
                    String::new(),
                    format!(
                        "Use inherited: {}",
                        if inherited {
                            "Selectable subtitles"
                        } else {
                            "No additional check"
                        }
                    ),
                ),
                ("1".into(), "Require selectable full subtitles".into()),
                ("0".into(), "No additional subtitle check".into()),
            ],
            effective: if current {
                "Selectable full subtitles"
            } else {
                "No additional check"
            }
            .into(),
            origin: if value.is_some() {
                if scope == "franchise" {
                    "Franchise"
                } else {
                    "Release"
                }
            } else {
                "App default"
            }
            .into(),
        });
    }
    RequirementsView {
        scope: scope.into(),
        id: id.into(),
        fields,
        audio: l.audio_languages.clone().unwrap_or_default(),
        subtitles: l.subtitle_languages.clone().unwrap_or_default(),
        audio_match: l.audio_match.clone().unwrap_or_else(|| "all".into()),
        subtitle_match: l.subtitle_match.clone().unwrap_or_else(|| "all".into()),
        ..Default::default()
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RequirementForm {
    scope: String,
    id: String,
    return_to: String,
    audio_mode: String,
    subtitle_mode: String,
    audio_languages: String,
    subtitle_languages: String,
    audio_match: String,
    subtitle_match: String,
    minimum_resolution: String,
    target_resolution: String,
    minimum_subtitle_check: String,
    target_subtitle_check: String,
}
fn language(
    mode: &str,
    value: &str,
    match_mode: &str,
) -> Result<(Option<String>, Option<String>), String> {
    match mode {
        "inherit" => Ok((None, None)),
        "none" => Ok((Some(String::new()), Some("all".into()))),
        "require" => {
            let v = language_preferences::normalize_languages(value, "required")?;
            if v.is_empty() {
                return Err(
                    "Choose at least one language, or explicitly select no requirement.".into(),
                );
            }
            if !matches!(match_mode, "all" | "any") {
                return Err("Choose all or any listed languages.".into());
            }
            Ok((Some(v), Some(match_mode.into())))
        }
        _ => Err("Choose an inheritance or language option.".into()),
    }
}
fn flag(s: &str) -> Result<Option<bool>, String> {
    match s {
        "" => Ok(None),
        "1" => Ok(Some(true)),
        "0" => Ok(Some(false)),
        _ => Err("Choose a valid subtitle rule.".into()),
    }
}

async fn save(
    State(state): State<AppState>,
    HxRequest(hx): HxRequest,
    Form(form): Form<RequirementForm>,
) -> Response {
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        _ => return (StatusCode::SERVICE_UNAVAILABLE, "Settings unavailable.").into_response(),
    };
    let current = if form.scope == "release" {
        match form.id.parse::<i64>() {
            Ok(id) => release_view(&state.db, &cfg, id).await,
            Err(_) => Err("Release not found.".into()),
        }
    } else if form.scope == "franchise" {
        franchise_view(&state.db, &cfg, &form.id).await
    } else {
        Err("Invalid settings scope.".into())
    };
    let mut v = match current {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let outcome=async {
        let (audio_languages,audio_match)=language(&form.audio_mode,&form.audio_languages,&form.audio_match)?;
        let (subtitle_languages,subtitle_match)=language(&form.subtitle_mode,&form.subtitle_languages,&form.subtitle_match)?;
        let l=LanguagePreferences{audio_languages,audio_match,subtitle_languages,subtitle_match};
        let q=QualityPreferences{minimum_resolution:(!form.minimum_resolution.is_empty()).then(||form.minimum_resolution.clone()),target_resolution:(!form.target_resolution.is_empty()).then(||form.target_resolution.clone()),minimum_subtitle_check:flag(&form.minimum_subtitle_check)?,target_subtitle_check:flag(&form.target_subtitle_check)?};
        let base=if form.scope=="release" {
            let id=form.id.parse::<i64>().map_err(|e|e.to_string())?;
            let row=series::get_by_id(&state.db,id).await.map_err(|e|e.to_string())?.ok_or("Release not found.")?;
            if let Some(p)=language_preferences::franchise_for_release(&state.db,row.id).await? {language_preferences::franchise_config(&state.db,&cfg,&p).await?} else {cfg.clone()}
        } else {cfg.clone()};
        language_preferences::apply_quality(&language_preferences::apply(&base,&l)?,&q)?;
        if form.scope=="release" {
            sqlx::query("INSERT INTO series_language_preferences(series_id,audio_languages,audio_match,subtitle_languages,subtitle_match,minimum_resolution,target_resolution,minimum_subtitle_check,target_subtitle_check) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(series_id) DO UPDATE SET audio_languages=excluded.audio_languages,audio_match=excluded.audio_match,subtitle_languages=excluded.subtitle_languages,subtitle_match=excluded.subtitle_match,minimum_resolution=excluded.minimum_resolution,target_resolution=excluded.target_resolution,minimum_subtitle_check=excluded.minimum_subtitle_check,target_subtitle_check=excluded.target_subtitle_check")
                .bind(form.id.parse::<i64>().map_err(|e|e.to_string())?).bind(&l.audio_languages).bind(&l.audio_match).bind(&l.subtitle_languages).bind(&l.subtitle_match).bind(&q.minimum_resolution).bind(&q.target_resolution).bind(q.minimum_subtitle_check).bind(q.target_subtitle_check).execute(&state.db).await.map_err(|e|e.to_string())?;
        } else {requirement_preferences::save_franchise(&state.db,&FranchisePreferences{page_id:v.id.clone(),audio_languages:l.audio_languages,audio_match:l.audio_match,subtitle_languages:l.subtitle_languages,subtitle_match:l.subtitle_match,minimum_resolution:q.minimum_resolution,target_resolution:q.target_resolution,minimum_subtitle_check:q.minimum_subtitle_check,target_subtitle_check:q.target_subtitle_check}).await.map_err(|e|e.to_string())?;}
        Ok::<(),String>(())
    }.await;
    if let Err(error) = outcome {
        v.error = error;
        v.audio = form.audio_languages;
        v.subtitles = form.subtitle_languages;
        v.audio_match = form.audio_match;
        v.subtitle_match = form.subtitle_match;
        for f in &mut v.fields {
            f.value = match f.name {
                "audio_mode" => &form.audio_mode,
                "subtitle_mode" => &form.subtitle_mode,
                "minimum_resolution" => &form.minimum_resolution,
                "target_resolution" => &form.target_resolution,
                "minimum_subtitle_check" => &form.minimum_subtitle_check,
                _ => &form.target_subtitle_check,
            }
            .clone();
        }
    } else {
        v = if form.scope == "release" {
            release_view(&state.db, &cfg, form.id.parse().unwrap_or_default()).await
        } else {
            franchise_view(&state.db, &cfg, &v.id).await
        }
        .unwrap_or(v);
        v.message =
            "Requirements saved. Existing files, monitoring and upgrade switches are unchanged."
                .into();
        if !hx {
            return crate::handlers::responses::htmx_aware_redirect(
                false,
                &format!("{}?tab=settings", v.return_to),
            );
        }
    }
    let html = if hx {
        Partial { requirements: v }.render()
    } else {
        Page {
            page: "library",
            title_language: cfg.title_language,
            requirements: v,
        }
        .render()
    };
    match html {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not render requirements.",
        )
            .into_response(),
    }
}

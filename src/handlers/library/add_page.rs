//! Provider search and a single confirmation before adding a release.
use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::{
    AppState,
    handlers::responses::htmx_aware_redirect,
    models::{config, metadata_cache, monitoring::MonitorMode, series},
    services::{anilist, library_link},
};

use super::{
    AddSeriesForm,
    crud::{AddOptions, add_entry},
    detail,
    reconcile::resolve_series_request,
};

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct Browse {
    q: String,
    source: String,
    provider_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct Save {
    provider_id: i64,
    q: String,
    source: String,
    monitor_mode: Choice,
    #[serde(default)]
    search_now: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Choice {
    #[default]
    All,
    Future,
    None,
}

impl From<Choice> for MonitorMode {
    fn from(choice: Choice) -> Self {
        match choice {
            Choice::All => Self::All,
            Choice::Future => Self::Future,
            Choice::None => Self::None,
        }
    }
}

struct ResultRow {
    title: String,
    subtitle: String,
    cover: String,
    facts: String,
    href: String,
    existing: bool,
}

struct Setup {
    id: i64,
    title: String,
    cover: String,
    facts: String,
    details_url: String,
    monitor_mode: &'static str,
    search_now: bool,
    can_search: bool,
    search_hint: &'static str,
}

#[derive(Template)]
#[template(path = "add_anime.html")]
struct AddPage {
    page: &'static str,
    title_language: String,
    q: String,
    source: &'static str,
    back: String,
    results: Vec<ResultRow>,
    setup: Option<Setup>,
    error: &'static str,
}

impl AddPage {
    fn new(cfg: &config::Config, q: String, source: &str) -> Self {
        let source = match source {
            "al" => "al",
            "mal" => "mal",
            _ if cfg.force_mal_fallback => "mal",
            _ => "al",
        };
        let q: String = q.trim().chars().take(200).collect();
        let back = format!("/library/add?source={source}&q={}", urlencoding::encode(&q));
        Self {
            page: "library",
            title_language: cfg.title_language.clone(),
            q,
            source,
            back,
            results: Vec::new(),
            setup: None,
            error: "",
        }
    }

    fn into_response(self) -> Response {
        match Template::render(&self) {
            Ok(html) => Html(html).into_response(),
            Err(error) => {
                tracing::error!(%error, "Could not render Add Anime");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not open Add Anime.",
                )
                    .into_response()
            }
        }
    }

    async fn set_up(
        &mut self,
        state: &AppState,
        data: &anilist::AnimeDetail,
        cfg: &config::Config,
        choice: Choice,
        search_now: bool,
    ) {
        let (can_search, search_hint) = if !cfg.auto_grab_on_add {
            (false, "Search after adding is off in Settings.")
        } else if state.download_clients.read().await.clients.is_empty() {
            (false, "Set a download client to search after adding.")
        } else {
            (
                true,
                "Search for aired, monitored episodes missing from the library. Skip the search if none qualify.",
            )
        };
        self.setup = Some(Setup {
            id: data.id,
            title: title(
                &self.title_language,
                &data.title_english,
                &data.title_romaji,
                &data.title_native,
            ),
            cover: data.cover_url.clone(),
            facts: facts(&data.format, data.episodes, data.season_year),
            details_url: detail::path(data.id),
            monitor_mode: MonitorMode::from(choice).as_str(),
            search_now: search_now && can_search,
            can_search,
            search_hint,
        });
    }
}

fn title(lang: &str, english: &str, romaji: &str, native: &str) -> String {
    library_link::pick_title(lang, english, romaji, native).to_owned()
}

fn facts(format: &str, episodes: Option<i32>, year: Option<i32>) -> String {
    let mut facts = Vec::new();
    if !format.is_empty() {
        facts.push(format.replace('_', " "));
    }
    if let Some(episodes) = episodes {
        facts.push(if episodes == 1 {
            "1 episode".into()
        } else {
            format!("{episodes} episodes")
        });
    }
    if let Some(year) = year {
        facts.push(year.to_string());
    }
    facts.join(" · ")
}

async fn exact_detail(db: &sqlx::SqlitePool, id: i64) -> Result<anilist::AnimeDetail, String> {
    let data = if let Some(cached) = metadata_cache::get_by_provider_id(db, id)
        .await
        .map_err(|error| error.to_string())?
    {
        cached.detail
    } else {
        anilist::get_anime_detail(id).await?
    };
    if data.id != id {
        return Err("The provider returned a different release.".into());
    }
    Ok(data)
}

fn supported_provider(id: i64) -> bool {
    id != 0 && id != i64::MIN
}

async fn existing(state: &AppState, id: i64) -> Result<Option<series::Series>, sqlx::Error> {
    resolve_series_request(&state.db, id, true)
        .await
        .map(|(row, _)| row)
}

pub(super) async fn page(
    State(state): State<AppState>,
    Query(query): Query<Browse>,
    HxRequest(htmx): HxRequest,
) -> Response {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut view = AddPage::new(&cfg, query.q, &query.source);
    if let Some(id) = query.provider_id {
        if !supported_provider(id) {
            return (StatusCode::BAD_REQUEST, "Choose a valid anime.").into_response();
        }
        match existing(&state, id).await {
            Ok(Some(current)) => {
                return htmx_aware_redirect(htmx, &detail::path(current.anilist_id));
            }
            Ok(None) => {}
            Err(_) => {
                view.error = "Could not check the library. Try again.";
                return view.into_response();
            }
        }
        match exact_detail(&state.db, id).await {
            Ok(data) => {
                view.set_up(&state, &data, &cfg, Choice::All, cfg.auto_grab_on_add)
                    .await
            }
            Err(_) => view.error = "Could not load this anime. Search again or try later.",
        }
    } else if !view.q.is_empty() {
        if let Err(error) = find_results(&state, &mut view).await {
            tracing::warn!(%error, "Add Anime search failed");
            view.error = "Could not load search results. Try again or choose the other provider.";
        }
    }
    view.into_response()
}

async fn find_results(state: &AppState, view: &mut AddPage) -> Result<(), String> {
    let entries = anilist::search_anime_with_options(&view.q, view.source == "mal").await?;
    let ids = entries.iter().map(|entry| entry.id).collect::<Vec<_>>();
    let exact = series::get_by_anilist_ids(&state.db, &ids)
        .await
        .map_err(|error| error.to_string())?;
    let mut results = Vec::with_capacity(entries.len());
    for entry in entries
        .into_iter()
        .filter(|entry| supported_provider(entry.id))
    {
        let provider = match exact.get(&entry.id) {
            Some(row) => Some(row.anilist_id),
            None if entry.id < 0 || entry.id_mal.is_some() => existing(state, entry.id)
                .await
                .map_err(|error| error.to_string())?
                .map(|row| row.anilist_id),
            None => None,
        };
        let title = title(
            &view.title_language,
            &entry.title_english,
            &entry.title_romaji,
            &entry.title_native,
        );
        let subtitle = [&entry.title_english, &entry.title_romaji]
            .into_iter()
            .find(|candidate| !candidate.is_empty() && candidate.as_str() != title)
            .cloned()
            .unwrap_or_default();
        results.push(ResultRow {
            title,
            subtitle,
            cover: entry.cover_url,
            facts: facts(&entry.format, entry.episodes, entry.season_year),
            href: provider
                .map(detail::path)
                .unwrap_or_else(|| format!("{}&provider_id={}", view.back, entry.id)),
            existing: provider.is_some(),
        });
    }
    view.results = results;
    Ok(())
}

pub(super) async fn save(
    State(state): State<AppState>,
    HxRequest(htmx): HxRequest,
    Form(form): Form<Save>,
) -> Response {
    if !supported_provider(form.provider_id) {
        return (StatusCode::BAD_REQUEST, "Choose a valid anime.").into_response();
    }
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut view = AddPage::new(&cfg, form.q, &form.source);
    match existing(&state, form.provider_id).await {
        Ok(Some(row)) => return htmx_aware_redirect(htmx, &detail::path(row.anilist_id)),
        Ok(None) => {}
        Err(_) => {
            view.error = "Could not check the library. Try again.";
            return view.into_response();
        }
    }
    let Ok(data) = exact_detail(&state.db, form.provider_id).await else {
        view.error = "Could not load this anime. Search again or try later.";
        return view.into_response();
    };
    let add = AddSeriesForm {
        anilist_id: data.id,
        mal_id: data.id_mal,
        title: title(
            "english",
            &data.title_english,
            &data.title_romaji,
            &data.title_native,
        ),
        title_romaji: data.title_romaji.clone(),
        title_english: data.title_english.clone(),
        title_native: data.title_native.clone(),
        cover_url: data.cover_url.clone(),
        format: data.format.clone(),
        status: data.status.clone(),
        episodes: data.episodes,
        season_year: data.season_year,
    };
    match add_entry(
        state.clone(),
        add,
        Some(AddOptions {
            mode: form.monitor_mode.into(),
            search_now: form.search_now,
        }),
    )
    .await
    {
        Ok((id, _, _)) => htmx_aware_redirect(htmx, &format!("/series/{id}")),
        Err((_, error)) => {
            tracing::error!(%error, "Could not add release");
            view.error = if error.contains(series::AMBIGUOUS_MAL_ADD) {
                series::AMBIGUOUS_MAL_ADD
            } else {
                "Could not add this anime. Check the library and try again."
            };
            view.set_up(&state, &data, &cfg, form.monitor_mode, form.search_now)
                .await;
            view.into_response()
        }
    }
}

#[cfg(test)]
mod tests;

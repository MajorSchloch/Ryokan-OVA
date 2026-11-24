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
    franchise_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct Save {
    provider_id: i64,
    franchise_id: Option<i64>,
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

mod discovery;

#[derive(Template)]
#[template(path = "add_anime.html")]
struct AddPage {
    page: &'static str,
    title_language: String,
    q: String,
    source: &'static str,
    back: String,
    franchise_id: Option<i64>,
    franchise: Option<discovery::Franchise>,
    results: Vec<ResultRow>,
    setup: Option<Setup>,
    error: &'static str,
}

impl AddPage {
    fn new(cfg: &config::Config, q: String, source: &str, franchise_id: Option<i64>) -> Self {
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
            franchise_id: franchise_id.filter(|id| supported_provider(*id)),
            franchise: None,
            results: Vec::new(),
            setup: None,
            error: "",
        }
    }

    fn franchise_path(&self, id: i64) -> String {
        format!("{}&franchise_id={id}", self.back)
    }

    fn back_to_setup_parent(&self) -> String {
        self.franchise_id
            .map(|id| self.franchise_path(id))
            .unwrap_or_else(|| self.back.clone())
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
    let (row, _) = resolve_series_request(&state.db, id, true).await?;
    if let Some(row) = &row
        && id > 0
        && row.anilist_id < 0
    {
        // A title search can know about a new split before the local cache does.
        // An existing MAL row must explicitly identify this AniList release.
        let confirmed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM series_metadata_cache WHERE series_id=? AND provider_id=?)",
        )
        .bind(row.id)
        .bind(id)
        .fetch_one(&state.db)
        .await?;
        if !confirmed {
            return Ok(None);
        }
    }
    Ok(row)
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
    let mut view = AddPage::new(&cfg, query.q, &query.source, query.franchise_id);
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
    } else if let Some(seed) = view.franchise_id {
        match discovery::open(&state, &view, seed).await {
            Ok(franchise) => view.franchise = Some(franchise),
            Err(error) => {
                tracing::warn!(%error, "Could not open Add Anime franchise");
                view.error = "Could not open this franchise. Search again or try later.";
            }
        }
    } else if !view.q.is_empty()
        && let Err(error) = discovery::find_results(&state, &mut view).await
    {
        tracing::warn!(%error, "Add Anime search failed");
        view.error = "Could not load search results. Try again or choose the other provider.";
    }
    view.into_response()
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
    let mut view = AddPage::new(&cfg, form.q, &form.source, form.franchise_id);
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
            detail: &data,
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

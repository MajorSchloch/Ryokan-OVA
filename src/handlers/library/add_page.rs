//! Discover anime and confirm the releases to add.
use std::collections::HashSet;

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
    services::{anilist, calendar::discovery::FranchiseLink, library_link},
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
    select: bool,
    storyline: Option<i64>,
    branch: Option<i64>,
    order: ReleaseOrder,
    guide: bool,
}

#[derive(Deserialize)]
pub(super) struct Save {
    provider_id: i64,
    franchise_id: Option<i64>,
    q: String,
    source: String,
    monitor_mode: Choice,
    #[serde(default)]
    order: ReleaseOrder,
    branch: Option<i64>,
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

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ReleaseOrder {
    Premiere,
    Watch,
    Storylines,
    Branches,
    #[default]
    #[serde(other)]
    Formats,
}

impl ReleaseOrder {
    fn from_param(order: &str) -> Self {
        match order {
            "premiere" => Self::Premiere,
            "watch" => Self::Watch,
            "storylines" => Self::Storylines,
            "branches" => Self::Branches,
            _ => Self::Formats,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Formats => "formats",
            Self::Premiere => "premiere",
            Self::Watch => "watch",
            Self::Storylines => "storylines",
            Self::Branches => "branches",
        }
    }

    fn grouped(self) -> bool {
        matches!(self, Self::Storylines | Self::Branches)
    }

    fn query(self) -> String {
        if self == Self::Formats {
            String::new()
        } else {
            format!("&order={}", self.as_str())
        }
    }
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
    franchise: Option<FranchiseLink>,
    rating: Option<i32>,
    genres: Vec<String>,
    preview_path: String,
    href: String,
    existing: bool,
}

struct Setup {
    id: i64,
    title: String,
    cover: String,
    facts: String,
    rating: Option<i32>,
    details_url: String,
    monitor_mode: &'static str,
    search_now: bool,
    can_search: bool,
    search_hint: &'static str,
}

mod branch_map;
pub(super) mod bulk;
mod discovery;
mod guide;
pub(super) mod preview;
mod upcoming;

#[derive(Default)]
struct GuideInput {
    name: String,
    entries: String,
}

impl GuideInput {
    fn resolve(&self, group: &super::groups::Group) -> Result<guide::Guide, String> {
        guide::GuideDraft::parse(&self.name, &self.entries)?.resolve(group)
    }
}

#[derive(Deserialize)]
pub(super) struct GuideForm {
    franchise_id: i64,
    #[serde(default)]
    source: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    order: ReleaseOrder,
    branch: Option<i64>,
    guide_name: String,
    guide_entries: String,
}

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
    select: bool,
    order: ReleaseOrder,
    branch: Option<i64>,
    selection_notice: String,
    selected_releases: HashSet<i64>,
    selection_mode: Choice,
    guide_input: Option<GuideInput>,
    guide: Option<guide::Guide>,
    results: Vec<ResultRow>,
    upcoming: Vec<ResultRow>,
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
        let back = format!("/discover?source={source}&q={}", urlencoding::encode(&q));
        Self {
            page: "library",
            title_language: cfg.title_language.clone(),
            q,
            source,
            back,
            franchise_id: franchise_id.filter(|id| supported_provider(*id)),
            franchise: None,
            select: false,
            order: ReleaseOrder::default(),
            branch: None,
            selection_notice: String::new(),
            selected_releases: HashSet::new(),
            selection_mode: Choice::default(),
            guide_input: None,
            guide: None,
            results: Vec::new(),
            upcoming: Vec::new(),
            setup: None,
            error: "",
        }
    }

    fn franchise_path(&self, id: i64) -> String {
        format!("{}&franchise_id={id}{}", self.back, self.order_query())
    }

    fn set_order(&mut self, order: ReleaseOrder, branch: Option<i64>) {
        self.order = order;
        self.branch =
            branch.filter(|id| order == ReleaseOrder::Branches && supported_provider(*id));
    }

    fn order_query(&self) -> String {
        let mut query = self.order.query();
        if let Some(branch) = self.branch {
            query.push_str(&format!("&branch={branch}"));
        }
        query
    }

    fn branch_path(&self, seed: i64, branch: i64) -> String {
        format!(
            "{}&franchise_id={seed}&order=branches&branch={branch}",
            self.back
        )
    }

    fn visible_branch_map(&self) -> Option<&branch_map::BranchMap> {
        if self.select || self.guide_input.is_some() {
            return None;
        }
        self.franchise.as_ref()?.branch_map.as_ref()
    }

    fn setup_path(&self, seed: i64, release: i64) -> String {
        format!(
            "/library/add?source={}&q={}&franchise_id={seed}&provider_id={release}{}",
            self.source,
            urlencoding::encode(&self.q),
            self.order_query(),
        )
    }

    fn back_to_setup_parent(&self) -> String {
        self.franchise_id
            .map(|id| self.franchise_path(id))
            .unwrap_or_else(|| self.back.clone())
    }

    fn select_releases_path(&self) -> String {
        format!(
            "{}&select=true#add-anime-releases-title",
            self.back_to_setup_parent()
        )
    }

    fn selection_monitor_mode(&self) -> &'static str {
        MonitorMode::from(self.selection_mode).as_str()
    }

    fn guide_path(&self) -> String {
        format!("{}&guide=true#add-anime-guide", self.back_to_setup_parent())
    }

    fn can_preview_guide(&self) -> bool {
        self.franchise_id.is_some_and(|id| id > 0)
    }

    fn order_path(&self, order: &str) -> String {
        self.franchise_id.map_or_else(
            || self.back.clone(),
            |id| {
                let order = ReleaseOrder::from_param(order);
                let query = if order == self.order {
                    self.order_query()
                } else {
                    order.query()
                };
                format!(
                    "{}&franchise_id={id}{query}#add-anime-releases-title",
                    self.back
                )
            },
        )
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
            rating: anilist::detail_rating(data),
            details_url: detail::path(data.id),
            monitor_mode: MonitorMode::from(choice).as_str(),
            search_now: search_now && can_search,
            can_search,
            search_hint,
        });
    }
}

pub(super) fn storyline_selection_path(franchise_path: &str, head: i64) -> String {
    format!("{franchise_path}&select=true&storyline={head}#add-anime-releases-title")
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

fn poster_genres(genres: &[String]) -> Vec<String> {
    genres
        .iter()
        .map(|genre| genre.trim())
        .filter(|genre| !genre.is_empty())
        .take(2)
        .map(str::to_owned)
        .collect()
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
    view.set_order(query.order, query.branch);
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
        if query.guide && view.can_preview_guide() {
            view.guide_input = Some(GuideInput::default());
        }
        let storyline = query.storyline.filter(|_| view.guide_input.is_none());
        if storyline.is_some() && !view.order.grouped() {
            view.set_order(ReleaseOrder::Storylines, None);
        }
        match discovery::open(&state, &view, seed).await {
            Ok(franchise) => {
                if let Some(map) = &franchise.branch_map {
                    view.branch = Some(map.focus);
                }
                view.select = (query.select || storyline.is_some())
                    && franchise.can_select
                    && view.guide_input.is_none();
                if let Some(head) = storyline {
                    if let Some(section) = franchise
                        .sections
                        .iter()
                        .find(|section| section.storyline == Some(head))
                    {
                        view.selected_releases.extend(
                            section
                                .releases
                                .iter()
                                .filter(|release| !release.row.existing)
                                .map(|release| release.id),
                        );
                        if view.selected_releases.is_empty() {
                            view.selection_notice =
                                "Every release in this storyline is already in your library."
                                    .into();
                        }
                    } else {
                        view.selection_notice =
                            "This storyline is unavailable for selection. Choose releases individually."
                                .into();
                    }
                }
                view.franchise = Some(franchise);
            }
            Err(error) => {
                tracing::warn!(%error, "Could not open Add Anime franchise");
                view.error = "Could not open this franchise. Search again or try later.";
            }
        }
    } else if view.q.is_empty() {
        view.upcoming = upcoming::load(&state, &view).await;
    } else if let Err(error) = discovery::find_results(&state, &mut view).await {
        tracing::warn!(%error, "Add Anime search failed");
        view.error = "Could not load search results. Try again or choose the other provider.";
    }
    view.into_response()
}

pub(super) async fn preview_guide(
    State(state): State<AppState>,
    Form(form): Form<GuideForm>,
) -> Response {
    let seed = form.franchise_id;
    if seed <= 0 || !supported_provider(seed) {
        return (StatusCode::BAD_REQUEST, "Choose an AniList franchise.").into_response();
    }
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut view = AddPage::new(&cfg, form.q, &form.source, Some(seed));
    view.set_order(form.order, form.branch);
    let input = GuideInput {
        name: form.guide_name,
        entries: form.guide_entries,
    };
    let resolved = match discovery::resolve_group(&state, &view, seed).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(%error, seed, "Could not load franchise for watch guide");
            view.guide_input = Some(input);
            view.error = "Could not load this franchise. Try the preview again.";
            return (StatusCode::BAD_GATEWAY, view.into_response()).into_response();
        }
    };
    match input.resolve(&resolved.group) {
        Ok(guide) => {
            view.selected_releases.clone_from(&guide.selected);
            view.guide = Some(guide);
        }
        Err(message) => view.selection_notice = message,
    }
    view.guide_input = Some(input);
    let franchise = discovery::project(&state, &view, seed, resolved).await;
    view.select = view.guide.is_some() && franchise.can_select;
    view.franchise = Some(franchise);
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
    view.set_order(form.order, form.branch);
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
    match add_entry(
        state.clone(),
        add_series_form(&data),
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

fn add_series_form(data: &anilist::AnimeDetail) -> AddSeriesForm {
    AddSeriesForm {
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
    }
}

#[cfg(test)]
mod tests;

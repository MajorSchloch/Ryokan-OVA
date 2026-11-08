use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::services::release_follows::SETTINGS_CHANGED;
use crate::{
    AppState,
    handlers::responses::htmx_aware_redirect,
    models::{
        config,
        release_follows::{self, Follow},
    },
};

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/library/follows", get(page).post(start))
        .route("/library/follows/{id}", post(update))
        .route("/library/follows/{id}/stop", post(stop))
}

#[derive(Template)]
#[template(path = "release_follows.html")]
struct FollowsPage {
    page: &'static str,
    title_language: String,
    follows: Vec<Follow>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Selection {
    anilist_id: i64,
    title: String,
}

impl Selection {
    fn valid(&self) -> bool {
        (1..=i64::from(i32::MAX)).contains(&self.anilist_id)
            && !self.title.trim().is_empty()
            && self.title.len() <= 2048
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Options {
    movies: bool,
    ovas: bool,
    alternatives: bool,
}

#[derive(Deserialize)]
struct NewFollow {
    anilist_id: i64,
    title: String,
    #[serde(default)]
    movies: bool,
    #[serde(default)]
    ovas: bool,
    #[serde(default)]
    alternatives: bool,
}

fn internal_error(err: impl std::fmt::Display) -> (StatusCode, &'static str) {
    tracing::error!("Release follow failed: {err}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not load or save the follow. Try again.",
    )
}

async fn page(
    State(state): State<AppState>,
    Query(selection): Query<Selection>,
) -> Result<Html<String>, (StatusCode, &'static str)> {
    let mut follows = release_follows::list_with_matches(&state.db)
        .await
        .map_err(internal_error)?;
    if selection.valid() {
        if let Some(i) = follows
            .iter()
            .position(|f| f.anilist_id == selection.anilist_id)
        {
            follows[..=i].rotate_right(1);
        } else {
            follows.insert(
                0,
                Follow {
                    id: 0,
                    anilist_id: selection.anilist_id,
                    title: selection.title,
                    started_on: String::new(),
                    movies: false,
                    ovas: false,
                    alternatives: false,
                    ..Follow::default()
                },
            );
        }
    }
    FollowsPage {
        page: "library",
        title_language: config::get_naming_prefs(&state.db).await.title_language,
        follows,
    }
    .render()
    .map(Html)
    .map_err(internal_error)
}

async fn start(
    State(state): State<AppState>,
    HxRequest(htmx): HxRequest,
    Form(form): Form<NewFollow>,
) -> Result<Response, (StatusCode, &'static str)> {
    let selection = Selection {
        anilist_id: form.anilist_id,
        title: form.title,
    };
    if !selection.valid() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Select an AniList anime from its series page.",
        ));
    }
    release_follows::start(
        &state.db,
        selection.anilist_id,
        selection.title.trim(),
        form.movies,
        form.ovas,
        form.alternatives,
    )
    .await
    .map_err(internal_error)?;
    SETTINGS_CHANGED.notify_one();
    Ok(htmx_aware_redirect(htmx, "/library/follows"))
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    HxRequest(htmx): HxRequest,
    Form(opts): Form<Options>,
) -> Result<Response, (StatusCode, &'static str)> {
    release_follows::update(&state.db, id, opts.movies, opts.ovas, opts.alternatives)
        .await
        .map_err(internal_error)?;
    SETTINGS_CHANGED.notify_one();
    Ok(htmx_aware_redirect(htmx, "/library/follows"))
}

async fn stop(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    HxRequest(htmx): HxRequest,
) -> Result<Response, (StatusCode, &'static str)> {
    release_follows::remove(&state.db, id)
        .await
        .map_err(internal_error)?;
    Ok(htmx_aware_redirect(htmx, "/library/follows"))
}

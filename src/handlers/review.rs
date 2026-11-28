//! User decisions awaiting resolution. Opening the queue does not probe files or clients.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};

use crate::{
    AppState,
    handlers::library::reconcile::populate_series_cover_urls,
    models::{config, edition_review, grabbed_torrents},
};

#[derive(serde::Deserialize)]
pub struct Feedback {
    message: Option<String>,
    error: Option<String>,
}

impl Feedback {
    fn for_response(mut self, headers: &HeaderMap) -> Self {
        if !crate::handlers::auth::flash_allowed(headers) {
            self.message = None;
            self.error = None;
        }
        self
    }
}

#[derive(Template)]
#[template(path = "review.html")]
struct Queue {
    page: &'static str,
    title_language: String,
    editions: Vec<edition_review::Summary>,
    misgrabs: Vec<grabbed_torrents::MisgrabEntry>,
    feedback: Feedback,
    partial_error: bool,
}

#[derive(Template)]
#[template(path = "review_misgrab.html")]
struct MisgrabCase {
    page: &'static str,
    title_language: String,
    case: Option<grabbed_torrents::MisgrabEntry>,
    feedback: Feedback,
    error: bool,
}

async fn title_language(state: &AppState) -> String {
    config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map_or_else(|| "english".into(), |config| config.title_language)
}

fn render(template: impl Template) -> Response {
    match template.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "Could not render a review case");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not display the review page.",
            )
                .into_response()
        }
    }
}

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(feedback): Query<Feedback>,
) -> Response {
    let title_language = title_language(&state).await;
    let (editions, misgrabs) = tokio::join!(
        edition_review::summaries(&state.db),
        grabbed_torrents::list_held_misgrabs(&state.db, &title_language, None),
    );
    let partial_error = editions.is_err() || misgrabs.is_err();
    let mut editions = editions.unwrap_or_default();
    let mut misgrabs = misgrabs.unwrap_or_default();
    tokio::join!(
        populate_series_cover_urls(
            &state.db,
            &mut editions,
            |case| case.series_id,
            |case, url| case.cover_url = url,
        ),
        populate_series_cover_urls(
            &state.db,
            &mut misgrabs,
            |case| case.series_id,
            |case, url| case.cover_url = url,
        ),
    );
    render(Queue {
        page: "wanted",
        title_language,
        editions,
        misgrabs,
        feedback: feedback.for_response(&headers),
        partial_error,
    })
}

pub async fn misgrab(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(feedback): Query<Feedback>,
) -> Response {
    let title_language = title_language(&state).await;
    let case = grabbed_torrents::list_held_misgrabs(&state.db, &title_language, Some(id)).await;
    let error = case.is_err();
    render(MisgrabCase {
        page: "wanted",
        title_language,
        case: case.unwrap_or_default().pop(),
        feedback: feedback.for_response(&headers),
        error,
    })
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;

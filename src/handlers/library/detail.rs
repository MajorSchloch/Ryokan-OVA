//! Provider-addressed detail pages exist independently of library membership.
use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_htmx::HxRequest;

use crate::{AppState, handlers::responses::htmx_aware_redirect};

use super::{SeriesIdKind, pages, reconcile::resolve_series_request};

/// A positive provider id is AniList; a negative id is Ryokan's MAL sentinel.
/// Library row ids are deliberately not accepted here.
pub fn path(provider_id: impl std::borrow::Borrow<i64>) -> String {
    let provider_id = *provider_id.borrow();
    if provider_id < 0 {
        format!("/anime/mal/{}", provider_id.unsigned_abs())
    } else {
        format!("/anime/anilist/{provider_id}")
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/anime/anilist/{id}", get(anilist_detail))
        .route("/anime/mal/{id}", get(mal_detail))
        .route("/series/{id}", get(legacy_detail))
}

async fn anilist_detail(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    provider_detail(state, id, false).await
}

async fn mal_detail(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    provider_detail(state, id, true).await
}

async fn provider_detail(state: AppState, id: i64, mal: bool) -> Response {
    if id <= 0 {
        return (StatusCode::NOT_FOUND, "Anime not found.").into_response();
    }
    pages::series_detail(
        State(state),
        Path(if mal { -id } else { id }),
        Query(SeriesIdKind {
            by: Some("anilist".into()),
        }),
    )
    .await
    .into_response()
}

/// Preserve bookmarks without mixing provider ids with library row ids.
async fn legacy_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(kind): Query<SeriesIdKind>,
    HxRequest(is_htmx): HxRequest,
) -> Response {
    let provider_id = match resolve_series_request(&state.db, id, kind.by_anilist()).await {
        Ok((_, provider_id)) => provider_id,
        Err(error) => {
            tracing::error!(%error, "Could not resolve a legacy detail link");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not open this anime. Try again.",
            )
                .into_response();
        }
    };
    if provider_id == 0 || provider_id == i64::MIN {
        return (StatusCode::NOT_FOUND, "Anime not found.").into_response();
    }
    htmx_aware_redirect(is_htmx, &path(provider_id))
}

#[cfg(test)]
mod tests;

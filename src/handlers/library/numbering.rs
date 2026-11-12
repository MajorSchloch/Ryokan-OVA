//! Preview a finite numbering decision for one exact published source.
//! Saving records metadata only; it never adds or changes a client job.

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

use crate::AppState;
use crate::models::{series, source_numbering as decisions};
use crate::services::{nyaa, source_numbering};

#[derive(Default, serde::Deserialize)]
pub struct ReviewQuery {
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub root: Option<i64>,
}

#[derive(Default, serde::Deserialize)]
pub struct ReviewForm {
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub root: i64,
    #[serde(default)]
    pub fingerprint: String,
}

#[derive(Template)]
#[template(path = "numbering_review.html")]
struct ReviewPage {
    page: &'static str,
    title_language: String,
    series_id: i64,
    title: String,
    back_url: String,
    link: String,
    hash: String,
    source_title: String,
    choices: Vec<source_numbering::DomainChoice>,
    selected_root: i64,
    preview: Option<source_numbering::ReviewPreview>,
    eligible_count: usize,
    message: String,
    saved: bool,
}

impl ReviewPage {
    fn response(self, status: StatusCode) -> Result<Response, (StatusCode, String)> {
        let html = self
            .render()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok((status, Html(html)).into_response())
    }
}

fn validate_source(link: &str, hash: &str) -> Result<(), String> {
    if link.len() > 2048 || nyaa::view_page_url(link).is_none() {
        return Err("Choose a Nyaa listing with a published file list.".into());
    }
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("The source must have a complete torrent info hash.".into());
    }
    Ok(())
}

async fn page(
    state: &AppState,
    series_id: i64,
    input: &ReviewQuery,
) -> Result<ReviewPage, (StatusCode, String)> {
    // This route takes only the tracked internal ID. An AniList number must
    // never fall through to an unrelated library row.
    let series = series::get_by_id(&state.db, series_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read the release. Try again.".to_string(),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Release not found.".to_string()))?;
    validate_source(&input.link, &input.hash)
        .map_err(|reason| (StatusCode::BAD_REQUEST, reason))?;
    let saved = decisions::lookup_exact(&state.db, &input.hash, series.anilist_id).await;
    let choices = source_numbering::domain_choices(&state.db, series.anilist_id)
        .await
        .map_err(|reason| (StatusCode::BAD_GATEWAY, reason))?;
    let selected_root = input
        .root
        .or_else(|| {
            saved
                .as_ref()
                .ok()?
                .as_ref()
                .map(|saved| saved.proof.domain.root_anilist_id)
        })
        .or_else(|| choices.first().map(|choice| choice.root_anilist_id))
        .unwrap_or(0);
    if selected_root != 0
        && !choices
            .iter()
            .any(|choice| choice.root_anilist_id == selected_root)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose a numbering source listed for this release.".to_string(),
        ));
    }
    let mut result = ReviewPage {
        page: "library",
        title_language: crate::models::config::get_config(&state.db)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not read settings. Try again.".to_string(),
                )
            })?
            .unwrap_or_default()
            .title_language,
        series_id,
        title: if series.title_english.is_empty() {
            series.title.clone()
        } else {
            series.title_english.clone()
        },
        back_url: format!("/anime/anilist/{}", series.anilist_id),
        link: input.link.clone(),
        hash: input.hash.to_ascii_lowercase(),
        source_title: String::new(),
        choices,
        selected_root,
        preview: None,
        eligible_count: 0,
        message: String::new(),
        saved: false,
    };
    if selected_root == 0 {
        result.message = "No verified finite numbering rules are available for this release. Its numbering hold stays in place.".into();
        return Ok(result);
    }
    let domain = match source_numbering::load_domain(&state.db, selected_root).await {
        Ok(domain) => domain,
        Err(reason) => {
            result.message = reason;
            return Ok(result);
        }
    };
    // The existing Nyaa fetch validates the permitted origin, caps the body
    // and filenames, and verifies the page's magnet against this exact hash.
    let published = match nyaa::fetch_published_files(&input.link, &input.hash).await {
        Ok(published) => published,
        Err(reason) => {
            result.message = format!("Could not verify this source: {reason}. Try Preview again.");
            return Ok(result);
        }
    };
    result.source_title = published.title;
    match source_numbering::prepare_review(
        domain,
        &result.hash,
        &published.files,
        series.anilist_id,
    ) {
        Ok(preview) => {
            result.eligible_count = preview.files.iter().filter(|file| file.eligible).count();
            let selected: Vec<_> = preview
                .files
                .iter()
                .filter(|file| file.eligible)
                .map(|file| file.name.clone())
                .collect();
            if let Ok(proof) = preview.confirm(&selected) {
                match saved {
                    Ok(Some(saved)) if saved.proof.same_geometry(&proof) => {
                        match source_numbering::loader::refresh(&state.db,&saved.proof).await {
                            Ok(()) => {
                                result.saved = true;
                                result.message = if saved.proof.origin() == Ok(source_numbering::ProofOrigin::AutomaticUnique) {
                                    "Numbering verified automatically for this exact source. Choose files to download for this release."
                                } else { "Numbering saved for this exact source. Choose files to download for this release." }.into();
                            }
                            Err(reason) => result.message = reason,
                        }
                    }
                    Ok(Some(_)) => result.message = "The current source or numbering rules differ from the saved review. File selection stays unavailable.".into(),
                    Err(reason) => result.message = reason,
                    Ok(None) => {}
                }
            }
            result.preview = Some(preview);
        }
        Err(reason) => result.message = reason,
    }
    Ok(result)
}

pub async fn review(
    State(state): State<AppState>,
    Path(series_id): Path<i64>,
    Query(input): Query<ReviewQuery>,
) -> Result<Response, (StatusCode, String)> {
    page(&state, series_id, &input)
        .await?
        .response(StatusCode::OK)
}

pub async fn save_review(
    State(state): State<AppState>,
    Path(series_id): Path<i64>,
    Form(input): Form<ReviewForm>,
) -> Result<Response, (StatusCode, String)> {
    // Reconstruct every rule and filename on the server. A stale page or an
    // edited form cannot turn arbitrary offsets/files into a reviewed source.
    let query = ReviewQuery {
        link: input.link,
        hash: input.hash,
        root: Some(input.root),
    };
    let mut result = page(&state, series_id, &query).await?;
    // A failed save must not offer an action based on an earlier GET state.
    result.saved = false;
    let Some(preview) = &result.preview else {
        return result.response(StatusCode::OK);
    };
    if input.fingerprint.is_empty() || input.fingerprint != preview.fingerprint {
        result.message =
            "The source or numbering rules changed. Check the fresh preview before saving.".into();
        return result.response(StatusCode::OK);
    }
    let selected: Vec<_> = preview
        .files
        .iter()
        .filter(|file| file.eligible)
        .map(|file| file.name.clone())
        .collect();
    match preview.confirm(&selected) {
        Ok(proof) => match decisions::save_reviewed(&state.db, &proof).await {
            Ok(_) => {
                result.saved = true;
                result.message = "Numbering saved for this exact source. Automatic search can now use these episode assignments. No download was started.".into();
            }
            Err(reason) => result.message = reason,
        },
        Err(reason) => result.message = reason,
    }
    result.response(StatusCode::OK)
}

#[cfg(test)]
#[path = "numbering_tests.rs"]
mod tests;

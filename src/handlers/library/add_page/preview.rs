//! Read-only metadata for the exact release represented by a Discover poster.
use std::time::Duration;

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

use crate::{
    AppState,
    handlers::library::availability::premiere_label,
    models::{config, library_layout, metadata_cache},
    services::{
        anilist::{self, AnimeDetail},
        calendar::discovery::{FranchiseLink, reference_link},
        html::sanitize_rich_description,
        jikan,
    },
};

use super::{facts, supported_provider, title, upcoming};

const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);
const SYNOPSIS_LIMIT: usize = 430;

#[derive(Template)]
#[template(path = "partials/library/discover_preview.html")]
struct Preview {
    title: String,
    cover: String,
    synopsis: String,
    genres: Vec<String>,
    rating: Option<i32>,
    facts: String,
    premiere: String,
    franchise: Option<FranchiseLink>,
}

async fn detail(db: &sqlx::SqlitePool, id: i64) -> Result<AnimeDetail, String> {
    let mut cached = metadata_cache::exact_cached_details(db, &[id])
        .await
        .map_err(|error| error.to_string())?;
    if let Some(detail) = cached.remove(&id) {
        return Ok(detail);
    }
    let provider = async {
        if id < 0 {
            jikan::get_anime_preview(-id).await
        } else {
            anilist::get_anime_detail(id).await
        }
    };
    let detail = tokio::time::timeout(PREVIEW_TIMEOUT, provider)
        .await
        .map_err(|_| "Metadata lookup timed out".to_owned())??;
    if detail.id != id {
        return Err("The provider returned a different release.".into());
    }
    Ok(detail)
}

fn synopsis_excerpt(description: &str) -> String {
    let safe = sanitize_rich_description(description, true);
    let document = scraper::Html::parse_fragment(&safe);
    let text = document
        .root_element()
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    let Some((end, _)) = text.char_indices().nth(SYNOPSIS_LIMIT) else {
        return text;
    };
    let prefix = &text[..end];
    let break_at = prefix.rfind(' ').filter(|&index| index > end * 3 / 4);
    format!("{}…", break_at.map_or(prefix, |index| &prefix[..index]))
}

fn premiere(date: Option<&str>) -> String {
    date.map(premiere_label)
        .filter(|label| label != "TBA")
        .unwrap_or_default()
}

pub(in crate::handlers::library) async fn page(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if !supported_provider(id) {
        return (StatusCode::BAD_REQUEST, "Choose a valid anime.").into_response();
    }
    let (detail, prefs, saved) = tokio::join!(
        detail(&state.db, id),
        config::get_naming_prefs(&state.db),
        upcoming::saved_hit(&state.db, id),
    );
    let lang = &prefs.title_language;
    let mut preview = match detail {
        Ok(data) => {
            let premiere = premiere(
                saved
                    .as_ref()
                    .and_then(|hit| hit.release_date.as_deref())
                    .or(data.release_date.as_deref()),
            );
            let year = premiere.is_empty().then_some(data.season_year).flatten();
            let rating = anilist::detail_rating(&data).or_else(|| {
                saved
                    .as_ref()
                    .and_then(|hit| anilist::valid_rating(hit.entry.average_score))
            });
            Preview {
                title: title(
                    lang,
                    &data.title_english,
                    &data.title_romaji,
                    &data.title_native,
                ),
                cover: data.cover_url,
                synopsis: synopsis_excerpt(&data.description),
                genres: if data.genres.is_empty() {
                    saved
                        .as_ref()
                        .map_or_else(Vec::new, |hit| hit.genres.clone())
                } else {
                    data.genres
                },
                rating,
                facts: facts(&data.format, data.episodes, year),
                premiere,
                franchise: reference_link(id, &data.relations, lang).or_else(|| {
                    saved
                        .as_ref()
                        .and_then(|hit| reference_link(id, &hit.relations, lang))
                }),
            }
        }
        Err(error) => {
            tracing::debug!(provider_id = id, %error, "Discover preview detail unavailable");
            let Some(hit) = saved else {
                return (StatusCode::SERVICE_UNAVAILABLE, "Preview unavailable.").into_response();
            };
            let entry = hit.entry;
            let premiere = premiere(hit.release_date.as_deref());
            let year = premiere.is_empty().then_some(entry.season_year).flatten();
            Preview {
                title: title(
                    lang,
                    &entry.title_english,
                    &entry.title_romaji,
                    &entry.title_native,
                ),
                cover: entry.cover_url,
                synopsis: String::new(),
                genres: hit.genres,
                rating: anilist::valid_rating(entry.average_score),
                facts: facts(&entry.format, entry.episodes, year),
                premiere,
                franchise: reference_link(id, &hit.relations, lang),
            }
        }
    };
    if let Ok(Some((page_id, title))) = library_layout::page_for_provider(&state.db, id).await
        && !title.eq_ignore_ascii_case(&preview.title)
    {
        preview.franchise = Some(FranchiseLink {
            title,
            href: format!("/franchises/{page_id}"),
            label: "Franchise",
        });
    }
    match preview.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "Could not render Discover preview");
            (StatusCode::INTERNAL_SERVER_ERROR, "Preview unavailable.").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synopsis_is_plain_text_and_keeps_unicode_boundaries() {
        assert_eq!(
            synopsis_excerpt("<p>One &amp; two</p><script>alert(1)</script><p><em>Three</em></p>"),
            "One & two Three"
        );
        let long = "薬".repeat(500);
        assert_eq!(
            synopsis_excerpt(&long),
            format!("{}…", "薬".repeat(SYNOPSIS_LIMIT))
        );
    }
}

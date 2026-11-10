//! Franchise navigation is a cached projection. Browsing never adds releases,
//! follows, or paths; storage membership is the saved grouping boundary.
use super::{CardProgress, groups, inventory, reconcile::populate_series_cover_urls};
use crate::{
    AppState,
    models::{config, release_follows, series},
};
use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Html,
};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

pub(super) struct FranchiseCard {
    pub path: String,
    pub title: String,
    pub cover_url: String,
    pub search_titles: String,
    pub matches_search: bool,
    pub tracked: usize,
    pub available: usize,
    pub progress: CardProgress,
    pub following: bool,
    pub formats: String,
    added_at: String,
    score: Option<f64>,
}

pub(crate) async fn parent_for_provider(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    if let Some(parent) = crate::models::library_layout::page_for_provider(db, provider_id).await? {
        return Ok(Some(parent));
    }
    let groups = groups::load_groups(db, false, None).await?.groups;
    Ok(groups.into_iter().find_map(|group| {
        if group.entries.iter().any(|e| e.provider_id == provider_id) {
            group.page_id.map(|id| (id, group.title))
        } else {
            None
        }
    }))
}

fn path(group: &groups::Group) -> String {
    if let Some(id) = &group.page_id {
        return format!("/franchises/{id}");
    }
    // Libraries predating franchise storage remain browsable without adopting
    // paths as a side effect. The next explicit storage adoption assigns IDs.
    let seed = group
        .owner
        .or_else(|| {
            group
                .entries
                .iter()
                .find(|e| e.series_id.is_some())
                .map(|e| e.provider_id)
        })
        .unwrap_or_default();
    if seed > 0 {
        format!("/library/groups/{seed}/preview")
    } else {
        super::detail::path(seed)
    }
}

fn search_titles(group: &groups::Group, library: &HashMap<i64, &series::Series>) -> String {
    let mut titles = vec![group.title.clone()];
    for entry in &group.entries {
        titles.push(entry.title.clone());
        if let Some(row) = entry.series_id.and_then(|id| library.get(&id)) {
            titles.extend([
                row.title.clone(),
                row.title_english.clone(),
                row.title_romaji.clone(),
                row.title_native.clone(),
            ]);
        }
        if let Some(related) = &entry.related {
            titles.extend([
                related.title_english.clone(),
                related.title_romaji.clone(),
                related.title_native.clone(),
            ]);
        }
    }
    titles.join("\n")
}

fn representative<'a>(
    group: &groups::Group,
    library: &HashMap<i64, &'a series::Series>,
) -> Option<&'a series::Series> {
    group
        .entries
        .iter()
        .find(|e| Some(e.provider_id) == group.owner)
        .and_then(|e| e.series_id)
        .and_then(|id| library.get(&id).copied())
        .or_else(|| {
            group
                .entries
                .iter()
                .filter(|e| !matches!(e.format.as_str(), "SPECIAL" | "MUSIC"))
                .find_map(|e| e.series_id.and_then(|id| library.get(&id).copied()))
        })
        .or_else(|| {
            group
                .entries
                .iter()
                .find_map(|e| e.series_id.and_then(|id| library.get(&id).copied()))
        })
}

fn aggregate<'a>(
    progress: impl Iterator<Item = &'a CardProgress>,
    files_available: bool,
) -> CardProgress {
    let rows: Vec<_> = progress.collect();
    let aired = rows.iter().map(|p| p.aired).sum();
    let downloaded = rows.iter().map(|p| p.downloaded.min(p.aired)).sum();
    let downloading = rows.iter().any(|p| p.state == "downloading");
    let (state, label) = if !files_available {
        ("idle", "Files unavailable".to_owned())
    } else if downloading {
        (
            "downloading",
            format!("Downloading; {downloaded} of {aired} aired episodes available"),
        )
    } else if aired == 0 {
        ("idle", "No confirmed aired episodes".to_owned())
    } else if downloaded >= aired {
        ("complete", format!("All {aired} aired episodes available"))
    } else {
        (
            "missing",
            format!("{downloaded} of {aired} aired episodes available"),
        )
    };
    CardProgress {
        state: state.into(),
        downloaded,
        aired,
        pct: if !files_available || aired == 0 {
            0
        } else {
            (downloaded * 100 / aired).clamp(0, 100)
        },
        monitored: rows.iter().any(|p| p.monitored),
        label,
    }
}

pub(super) async fn cards(
    db: &SqlitePool,
    releases: &[(series::Series, CardProgress)],
    matching_ids: Option<&HashSet<i64>>,
    search: &str,
    sort: &str,
    media_root: &str,
) -> Result<(usize, Vec<FranchiseCard>), sqlx::Error> {
    let groups = groups::load_groups(db, false, None).await?.groups;
    let library: HashMap<_, _> = releases.iter().map(|(s, _)| (s.id, s)).collect();
    let progress: HashMap<_, _> = releases.iter().map(|(s, p)| (s.id, p)).collect();
    let follows = release_follows::list(db).await?;
    let files_available = !media_root.is_empty()
        && tokio::fs::metadata(media_root)
            .await
            .is_ok_and(|m| m.is_dir());
    let total = groups.len();
    let needle = search.trim().to_lowercase();
    let mut cards = Vec::new();
    for group in groups {
        if matching_ids.is_some_and(|ids| {
            !group
                .entries
                .iter()
                .any(|e| e.series_id.is_some_and(|id| ids.contains(&id)))
        }) {
            continue;
        }
        let searchable = search_titles(&group, &library);
        let matches_search = needle.is_empty() || searchable.to_lowercase().contains(&needle);
        let representative = representative(&group, &library);
        let mut formats: Vec<_> = group
            .entries
            .iter()
            .filter(|e| e.series_id.is_some())
            .map(|e| e.format.replace('_', " "))
            .filter(|s| !s.is_empty())
            .collect();
        formats.sort();
        formats.dedup();
        let included =
            |e: &&groups::Entry| e.series_id.is_some() && e.choice != Some(false) && !e.excluded;
        let p = aggregate(
            group
                .entries
                .iter()
                .filter(included)
                .filter_map(|e| e.series_id.and_then(|id| progress.get(&id).copied())),
            files_available,
        );
        cards.push(FranchiseCard {
            path: path(&group),
            title: group.title.clone(),
            cover_url: representative
                .map(|s| s.cover_url.clone())
                .unwrap_or_default(),
            search_titles: searchable,
            matches_search,
            tracked: group.tracked,
            available: group.outside,
            progress: p,
            following: follows
                .iter()
                .any(|f| group.entries.iter().any(|e| e.provider_id == f.anilist_id)),
            formats: formats.join(" · "),
            added_at: group
                .entries
                .iter()
                .filter_map(|e| e.series_id.and_then(|id| library.get(&id)))
                .map(|s| s.added_at.as_str())
                .max()
                .unwrap_or_default()
                .into(),
            score: representative
                .and_then(|s| s.user_score)
                .filter(|s| *s > 0.0),
        });
    }
    cards.sort_by(|a, b| {
        let title = || a.title.to_lowercase().cmp(&b.title.to_lowercase());
        match sort {
            "title_asc" => title(),
            "title_desc" => title().reverse(),
            "oldest" => a.added_at.cmp(&b.added_at).then_with(title),
            "score" => b
                .score
                .unwrap_or(-1.0)
                .total_cmp(&a.score.unwrap_or(-1.0))
                .then_with(title),
            "score_asc" => a
                .score
                .unwrap_or(f64::INFINITY)
                .total_cmp(&b.score.unwrap_or(f64::INFINITY))
                .then_with(title),
            _ => b.added_at.cmp(&a.added_at).then_with(title),
        }
    });
    Ok((total, cards))
}

struct Release {
    title: String,
    cover_url: String,
    path: String,
    format: String,
    date: String,
    status: String,
    files: String,
    in_library: bool,
    included: bool,
}
#[derive(Template)]
#[template(path = "franchise.html")]
struct FranchisePage {
    page: &'static str,
    title_language: String,
    title: String,
    cover_url: String,
    releases: Vec<Release>,
    tracked: usize,
    following: bool,
    seed: i64,
    progress: CardProgress,
}

pub async fn page(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, (StatusCode, &'static str)> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::NOT_FOUND, "Franchise not found."));
    }
    let id = crate::models::library_layout::resolve_page_id(&state.db, &id)
        .await
        .map_err(read_error)?
        .ok_or((StatusCode::NOT_FOUND, "Franchise not found."))?;
    let loaded = groups::load_groups(&state.db, false, None)
        .await
        .map_err(read_error)?;
    let group = loaded
        .groups
        .into_iter()
        .find(|g| g.page_id.as_deref() == Some(id.as_str()))
        .ok_or((StatusCode::NOT_FOUND, "Franchise not found."))?;
    let mut library = series::get_all(&state.db).await.map_err(read_error)?;
    library.retain(|s| group.entries.iter().any(|e| e.series_id == Some(s.id)));
    populate_series_cover_urls(
        &state.db,
        &mut library,
        |s| s.id,
        |s, url| s.cover_url = url,
    )
    .await;
    let by_id: HashMap<_, _> = library.iter().map(|s| (s.id, s)).collect();
    let cfg = config::get_config(&state.db)
        .await
        .map_err(read_error)?
        .unwrap_or_default();
    let facts = inventory::load(&state.db, &cfg.media_root, &library)
        .await
        .map_err(read_error)?;
    let follows = release_follows::list(&state.db).await.map_err(read_error)?;
    let following = follows
        .iter()
        .any(|f| group.entries.iter().any(|e| e.provider_id == f.anilist_id));
    let representative = representative(&group, &by_id);
    let cover_url = representative
        .map(|s| s.cover_url.clone())
        .unwrap_or_default();
    let seed = group
        .owner
        .or_else(|| {
            group
                .entries
                .iter()
                .find(|e| e.series_id.is_some())
                .map(|e| e.provider_id)
        })
        .unwrap_or_default();
    let mut progress = Vec::new();
    let mut files_available = true;
    let releases = group
        .entries
        .iter()
        .map(|e| {
            let tracked = e.series_id.and_then(|id| by_id.get(&id));
            let included = tracked.is_some() && e.choice != Some(false) && !e.excluded;
            let facts = e.series_id.and_then(|id| facts.get(&id));
            if included {
                files_available &= facts.is_some_and(|f| f.on_disk.is_some());
                let downloaded = facts.and_then(|f| f.on_disk).unwrap_or_default() as i64;
                let missing = facts.and_then(|f| f.missing.as_ref());
                // Unknown aired sets do not imply completeness.
                files_available &= missing.is_some();
                let aired = downloaded + missing.map(|m| m.len() as i64).unwrap_or_default();
                progress.push(CardProgress {
                    downloaded,
                    aired,
                    monitored: tracked.is_some_and(|s| s.monitor_mode != "none"),
                    ..CardProgress::default()
                });
            }
            Release {
                title: e.title.clone(),
                cover_url: tracked
                    .map(|s| s.cover_url.clone())
                    .or_else(|| e.related.as_ref().map(|r| r.cover_url.clone()))
                    .unwrap_or_default(),
                path: super::detail::path(e.library_provider_id.unwrap_or(e.provider_id)),
                format: e.format.replace('_', " "),
                date: if e.date.is_empty() {
                    "Date unknown".into()
                } else {
                    e.date.clone()
                },
                status: tracked
                    .map(|s| s.status.replace('_', " "))
                    .or_else(|| e.related.as_ref().map(|r| r.status_display.clone()))
                    .unwrap_or_default(),
                files: facts.map(groups::file_status).unwrap_or_else(|| {
                    if e.excluded {
                        "Excluded from sync".into()
                    } else {
                        "Outside library".into()
                    }
                }),
                in_library: tracked.is_some(),
                included,
            }
        })
        .collect();
    let page = FranchisePage {
        page: "library",
        title_language: loaded.title_language,
        title: group.title,
        cover_url,
        releases,
        tracked: group.tracked,
        following,
        seed,
        progress: aggregate(progress.iter(), files_available),
    };
    page.render().map(Html).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not display this franchise.",
        )
    })
}
fn read_error(error: sqlx::Error) -> (StatusCode, &'static str) {
    tracing::error!("Cannot read franchise navigation: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not load franchises. Try again.",
    )
}

#[cfg(test)]
mod tests;

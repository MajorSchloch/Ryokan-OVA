//! Franchise navigation is a cached projection. Browsing never adds releases,
//! follows, or paths; storage membership is the saved grouping boundary.
use super::{CardProgress, availability, groups, reconcile::populate_series_cover_urls};
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
    pub next_airing: String,
    pub added_date: String,
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

fn next_airing<'a>(rows: impl Iterator<Item = &'a CardProgress>) -> String {
    let rows: Vec<_> = rows.collect();
    rows.iter()
        .filter(|p| p.next_airing_at.is_some())
        .min_by_key(|p| p.next_airing_at)
        .or_else(|| {
            rows.iter().find(|p| {
                !p.next_airing.is_empty() && p.next_airing != "Finished" && p.next_airing != "TBA"
            })
        })
        .map(|p| p.next_airing.clone())
        .unwrap_or_default()
}

pub(super) async fn cards(
    db: &SqlitePool,
    releases: &[(series::Series, CardProgress)],
    matching_ids: Option<&HashSet<i64>>,
    search: &str,
    sort: &str,
    _media_root: &str,
) -> Result<(usize, Vec<FranchiseCard>), sqlx::Error> {
    let groups = groups::load_groups(db, false, None).await?.groups;
    let library: HashMap<_, _> = releases.iter().map(|(s, _)| (s.id, s)).collect();
    let progress: HashMap<_, _> = releases.iter().map(|(s, p)| (s.id, p)).collect();
    let follows = release_follows::list(db).await?;
    let total = groups.len();
    let needle = search.trim().to_lowercase();
    let aliases = super::summary::titles(
        db,
        &library.values().map(|s| (**s).clone()).collect::<Vec<_>>(),
    )
    .await?;
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
        let mut searchable = search_titles(&group, &library);
        for entry in &group.entries {
            if let Some(names) = entry.series_id.and_then(|id| aliases.get(&id)) {
                searchable.push('\n');
                searchable.push_str(names);
            }
        }
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
        let p = availability::aggregate(
            group
                .entries
                .iter()
                .filter_map(|e| e.series_id.and_then(|id| progress.get(&id).copied())),
        );
        let upcoming = next_airing(
            group
                .entries
                .iter()
                .filter_map(|e| e.series_id.and_then(|id| progress.get(&id).copied())),
        );
        let added_at = group
            .entries
            .iter()
            .filter_map(|e| e.series_id.and_then(|id| library.get(&id)))
            .map(|s| s.added_at.as_str())
            .max()
            .unwrap_or_default()
            .to_owned();
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
            following: follows.iter().any(|f| {
                (f.tv || f.movies || f.ovas)
                    && group.entries.iter().any(|e| e.provider_id == f.anilist_id)
            }),
            formats: formats.join(" · "),
            next_airing: upcoming,
            added_date: availability::added_label(&added_at),
            added_at,
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
    in_library: bool,
    series_id: Option<i64>,
    monitor_mode: String,
    requirements_label: String,
    upcoming: bool,
    aliases: String,
    allow_upgrades: bool,
    progress: CardProgress,
    monitoring_label: String,
    next_airing: String,
    added_date: String,
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
    next_airing: String,
    id: String,
    requirements: super::requirements::RequirementsView,
    activity: Vec<crate::models::library_activity::Activity>,
    future: super::groups::FutureFollowView,
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
    let progress_by_id = availability::load(&state.db, &cfg.media_root, &library)
        .await
        .map_err(read_error)?;
    let follows = release_follows::list(&state.db).await.map_err(read_error)?;
    let following = follows.iter().any(|f| {
        (f.tv || f.movies || f.ovas) && group.entries.iter().any(|e| e.provider_id == f.anilist_id)
    });
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
    let combined =
        availability::aggregate(library.iter().filter_map(|s| progress_by_id.get(&s.id)));
    let next_airing = next_airing(library.iter().filter_map(|s| progress_by_id.get(&s.id)));
    let future = if seed > 0 {
        super::groups::future_controls(&state.db, &id, seed)
            .await
            .map_err(read_error)?
    } else {
        super::groups::FutureFollowView::default()
    };
    let aliases = super::summary::titles(&state.db, &library)
        .await
        .map_err(read_error)?;
    let requirements = super::requirements::franchise_view(&state.db, &cfg, &id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Franchise requirements unavailable.",
            )
        })?;
    let activity = crate::models::library_activity::recent(
        &state.db,
        &library.iter().map(|s| s.id).collect::<Vec<_>>(),
        Some(&id),
    )
    .await
    .map_err(read_error)?;
    let policy_snapshot = crate::services::language_preferences::resolve_snapshot(
        &state.db,
        &cfg,
        &library.iter().map(|s| s.id).collect::<Vec<_>>(),
    )
    .await
    .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "Requirements unavailable."))?;
    let provider_aliases = super::summary::provider_titles(
        &state.db,
        &group
            .entries
            .iter()
            .map(|e| e.provider_id)
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(read_error)?;
    let inherited =
        crate::services::language_preferences::franchise_config(&state.db, &cfg, &id).await;
    let releases = group
        .entries
        .iter()
        .map(|e| {
            let tracked = e.series_id.and_then(|id| by_id.get(&id));
            let summary = e
                .series_id
                .and_then(|id| progress_by_id.get(&id))
                .cloned()
                .unwrap_or_else(|| CardProgress {
                    label: "Outside your library".into(),
                    monitoring_label: "Not monitored".into(),
                    ..Default::default()
                });
            let next =
                if tracked.is_some() {
                    summary.next_airing.clone()
                } else if e.related.as_ref().is_some_and(|r| {
                    matches!(r.status.as_str(), "NOT_YET_RELEASED" | "NOT_YET_AIRED")
                }) {
                    format!("Premiere · {}", availability::premiere_label(&e.date))
                } else {
                    String::new()
                };
            Release {
                series_id: e.series_id,
                monitor_mode: if summary.monitoring_label.contains(" of ") {
                    "custom".into()
                } else {
                    tracked
                        .map(|s| s.monitor_mode.clone())
                        .unwrap_or_else(|| "none".into())
                },
                requirements_label: match e
                    .series_id
                    .and_then(|id| policy_snapshot.get(&id))
                    .and_then(|p| p.as_ref().ok())
                    .map(|p| &p.config)
                    .or_else(|| {
                        e.series_id
                            .is_none()
                            .then(|| inherited.as_ref().ok())
                            .flatten()
                    }) {
                    Some(cfg) => super::requirements::brief(cfg),
                    None => "Requirements unavailable".into(),
                },
                upcoming: tracked
                    .map(|s| s.status.as_str())
                    .or_else(|| e.related.as_ref().map(|r| r.status.as_str()))
                    .is_some_and(|s| matches!(s, "NOT_YET_RELEASED" | "NOT_YET_AIRED")),
                aliases: tracked
                    .and_then(|s| aliases.get(&s.id))
                    .cloned()
                    .unwrap_or_else(|| {
                        let mut names = vec![
                            e.title.clone(),
                            provider_aliases
                                .get(&e.provider_id)
                                .cloned()
                                .unwrap_or_default(),
                        ];
                        if let Some(r) = &e.related {
                            names.extend([
                                r.title_english.clone(),
                                r.title_romaji.clone(),
                                r.title_native.clone(),
                            ]);
                        }
                        names.join("\n")
                    }),
                allow_upgrades: tracked.is_some_and(|s| s.allow_upgrades),
                title: e.title.clone(),
                cover_url: tracked
                    .map(|s| s.cover_url.clone())
                    .or_else(|| e.related.as_ref().map(|r| r.cover_url.clone()))
                    .unwrap_or_default(),
                path: super::detail::path(e.library_provider_id.unwrap_or(e.provider_id)),
                format: e.format.replace('_', " "),
                date: availability::premiere_label(&e.date),
                status: tracked
                    .map(|s| s.status.replace('_', " "))
                    .or_else(|| e.related.as_ref().map(|r| r.status_display.clone()))
                    .unwrap_or_default(),
                monitoring_label: summary.monitoring_label.clone(),
                next_airing: next,
                added_date: summary.added_date.clone(),
                progress: summary,
                in_library: tracked.is_some(),
            }
        })
        .collect();
    let page = FranchisePage {
        id,
        requirements,
        activity,
        future,
        page: "library",
        title_language: loaded.title_language,
        title: group.title,
        cover_url,
        releases,
        tracked: group.tracked,
        following,
        seed,
        progress: combined,
        next_airing,
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

//! Franchise navigation is a cached projection. Browsing never adds releases,
//! follows, or paths; storage membership is the saved grouping boundary.
use super::{CardProgress, availability, groups, reconcile::populate_series_cover_urls};
use crate::{
    AppState,
    models::{config, release_follows, series},
};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

pub(super) struct FranchiseCard {
    pub path: String,
    pub has_detail_page: bool,
    pub statuses: Vec<PosterStatus>,
    pub title: String,
    pub alternate_titles: Vec<String>,
    pub provider_rating: Option<i32>,
    pub cover_url: String,
    pub search_titles: String,
    pub matches_search: bool,
    pub tracked: usize,
    pub available: usize,
    pub progress: CardProgress,
    pub upcoming_releases: usize,
    pub formats: String,
    pub next_airing: String,
    pub added_date: String,
    added_at: String,
    score: Option<f64>,
}

pub(super) struct PosterStatus {
    pub filter: &'static str,
    pub label: String,
}

fn poster_statuses(progress: &CardProgress) -> Vec<PosterStatus> {
    [
        ("review", format!("{} to review", progress.reviews)),
        ("missing", format!("{} missing", progress.missing)),
        (
            "downloading",
            format!("{} downloading", progress.downloading),
        ),
        ("upcoming", "Upcoming".into()),
        ("attention", "Needs attention".into()),
    ]
    .into_iter()
    .filter(|(filter, _)| {
        (*filter != "attention" || progress.reviews == 0)
            && progress.matches_overview_status(filter)
    })
    .take(2)
    .map(|(filter, label)| PosterStatus { filter, label })
    .collect()
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

fn upcoming_release(entry: &groups::Entry, tracked: Option<&series::Series>) -> bool {
    tracked
        .map(|row| row.status.as_str())
        .or_else(|| {
            entry
                .related
                .as_ref()
                .map(|related| related.status.as_str())
        })
        .is_some_and(availability::upcoming_status)
}

fn upcoming_releases(group: &groups::Group, library: &HashMap<i64, &series::Series>) -> usize {
    group
        .entries
        .iter()
        .filter(|entry| {
            upcoming_release(
                entry,
                entry.series_id.and_then(|id| library.get(&id).copied()),
            )
        })
        .count()
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
    let tracked_rows: Vec<_> = library.values().map(|s| (**s).clone()).collect();
    let aliases = super::summary::titles(db, &tracked_rows).await?;
    let owner_aliases = super::summary::franchise_owner_titles(db, &groups, &library).await?;
    let owner_ids = groups
        .iter()
        .filter_map(super::summary::franchise_title_provider)
        .collect::<Vec<_>>();
    let provider_ratings = super::summary::provider_ratings(db, &owner_ids).await;
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
        if let Some(names) =
            super::summary::franchise_title_provider(&group).and_then(|id| owner_aliases.get(&id))
        {
            searchable.push('\n');
            searchable.push_str(names);
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
        let mut p = availability::aggregate(
            group
                .entries
                .iter()
                .filter_map(|e| e.series_id.and_then(|id| progress.get(&id).copied())),
        );
        p.following = follows.iter().any(|f| {
            (f.tv || f.movies || f.ovas)
                && group.entries.iter().any(|e| e.provider_id == f.anilist_id)
        });
        let upcoming_releases = upcoming_releases(&group, &library);
        p.upcoming |= upcoming_releases > 0;
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
            has_detail_page: group.page_id.is_some(),
            statuses: poster_statuses(&p),
            title: group.title.clone(),
            alternate_titles: super::summary::franchise_display_aliases(&group, &owner_aliases),
            provider_rating: super::summary::franchise_title_provider(&group)
                .and_then(|id| provider_ratings.get(&id).copied()),
            cover_url: representative
                .map(|s| s.cover_url.clone())
                .unwrap_or_default(),
            search_titles: searchable,
            matches_search,
            tracked: group.tracked,
            available: group.outside,
            progress: p,
            upcoming_releases,
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
    provider_id: i64,
    title: String,
    cover_url: String,
    provider_rating: Option<i32>,
    path: String,
    format: String,
    date: String,
    relationships: Vec<String>,
    followed: bool,
    upcoming: bool,
    in_library: bool,
    series_id: Option<i64>,
    monitor_mode: String,
    requirements_label: String,
    aliases: String,
    allow_upgrades: bool,
    progress: CardProgress,
    monitoring_label: String,
    next_airing: String,
    added_date: String,
    matches_filter: bool,
}

impl Release {
    fn monitor_filter(&self) -> &'static str {
        if !self.in_library {
            "outside"
        } else if self.progress.monitored {
            "monitored"
        } else {
            "unmonitored"
        }
    }

    fn format_filter(&self) -> &'static str {
        match self.format.as_str() {
            "ONA" => "tv",
            kind if kind.starts_with("TV") => "tv",
            "MOVIE" => "movie",
            "OVA" => "ova",
            _ => "other",
        }
    }

    fn matches_status(&self, status: &str) -> bool {
        status.is_empty()
            || (self.in_library || matches!(status, "upcoming" | "following"))
                && self.progress.matches_overview_status(status)
    }

    fn overview_states(&self) -> String {
        availability::OVERVIEW_STATUSES
            .into_iter()
            .filter(|status| self.matches_status(status))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub struct FranchiseFilters {
    f_q: String,
    f_monitor: String,
    f_type: String,
    f_status: String,
    f_order: String,
    f_hide_unfollowed: String,
}

impl FranchiseFilters {
    fn normalize(&mut self) {
        self.f_q = self.f_q.trim().to_owned();
        for (value, allowed) in [
            (
                &mut self.f_monitor,
                &["monitored", "unmonitored", "outside"][..],
            ),
            (&mut self.f_type, &["tv", "movie", "ova", "other"][..]),
            (&mut self.f_status, &availability::OVERVIEW_STATUSES[..]),
            (&mut self.f_order, &["watch", "storylines"][..]),
            (&mut self.f_hide_unfollowed, &["1"][..]),
        ] {
            if !allowed.contains(&value.as_str()) {
                value.clear();
            }
        }
    }

    fn matches(&self, release: &Release, search: &str) -> bool {
        (search.is_empty()
            || release.title.to_lowercase().contains(search)
            || release.aliases.to_lowercase().contains(search))
            && (self.f_monitor.is_empty() || self.f_monitor == release.monitor_filter())
            && (self.f_type.is_empty() || self.f_type == release.format_filter())
            && release.matches_status(&self.f_status)
            && (self.f_hide_unfollowed.is_empty() || release.followed)
    }
}

struct ReleaseSection {
    title: String,
    releases: Vec<Release>,
    extras: bool,
    links: Vec<groups::storylines::StorylineLink>,
    selection_path: Option<String>,
}

impl ReleaseSection {
    fn has_matches(&self) -> bool {
        self.releases.iter().any(|release| release.matches_filter)
    }
}

fn release_sections(
    entries: &[groups::Entry],
    storylines: Vec<groups::storylines::Storyline>,
    releases: Vec<Release>,
    order: &str,
    seed: i64,
) -> Vec<ReleaseSection> {
    let sections = match order {
        "storylines" => storylines
            .into_iter()
            .map(|section| {
                let head = section.kind.can_select().then(|| section.head());
                (
                    section.title,
                    section.releases,
                    section.extras,
                    section.links,
                    head,
                )
            })
            .collect(),
        "watch" => groups::watch_sequence(entries).map_or_else(Vec::new, |sequence| {
            let known: HashSet<_> = sequence.iter().copied().collect();
            let other = entries
                .iter()
                .map(|entry| entry.provider_id)
                .filter(|id| !known.contains(id))
                .collect();
            vec![
                (
                    "Known watch sequence".into(),
                    sequence,
                    false,
                    Vec::new(),
                    None,
                ),
                (
                    "No known watch position".into(),
                    other,
                    false,
                    Vec::new(),
                    None,
                ),
            ]
        }),
        _ => Vec::new(),
    };
    if sections.is_empty() {
        return vec![ReleaseSection {
            title: String::new(),
            releases,
            extras: false,
            links: Vec::new(),
            selection_path: None,
        }];
    }

    let mut by_id: HashMap<_, _> = releases
        .into_iter()
        .map(|release| (release.provider_id, release))
        .collect();
    let mut grouped: Vec<_> = sections
        .into_iter()
        .filter_map(|(title, ids, extras, links, head)| {
            let releases: Vec<_> = ids.into_iter().filter_map(|id| by_id.remove(&id)).collect();
            let selection_path = head
                .filter(|_| seed != 0 && releases.iter().any(|release| !release.in_library))
                .map(|head| {
                    super::add_page::storyline_selection_path(
                        &format!("/discover?franchise_id={seed}&order=storylines"),
                        head,
                    )
                });
            (!releases.is_empty()).then_some(ReleaseSection {
                title,
                releases,
                extras,
                links,
                selection_path,
            })
        })
        .collect();
    if !by_id.is_empty() {
        grouped.push(ReleaseSection {
            title: "Other releases".into(),
            releases: entries
                .iter()
                .filter_map(|entry| by_id.remove(&entry.provider_id))
                .collect(),
            extras: false,
            links: Vec::new(),
            selection_path: None,
        });
    }
    grouped
}

#[derive(Template)]
#[template(path = "franchise.html")]
struct FranchisePage {
    page: &'static str,
    title_language: String,
    title: String,
    cover_url: String,
    provider_rating: Option<i32>,
    release_sections: Vec<ReleaseSection>,
    branch_map_path: Option<String>,
    watch_fallback: bool,
    filters: FranchiseFilters,
    total_releases: usize,
    visible_releases: usize,
    tracked: usize,
    upcoming_releases: usize,
    seed: i64,
    progress: CardProgress,
    next_airing: String,
    id: String,
    requirements: super::requirements::RequirementsView,
    activity: Vec<crate::models::library_activity::Activity>,
    future: super::groups::FutureFollowView,
    search_on_monitoring_change: bool,
}

pub async fn page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(mut filters): Query<FranchiseFilters>,
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
    let mut combined =
        availability::aggregate(library.iter().filter_map(|s| progress_by_id.get(&s.id)));
    let upcoming_releases = upcoming_releases(&group, &by_id);
    combined.upcoming |= upcoming_releases > 0;
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
    let provider_ids = group
        .entries
        .iter()
        .map(|e| e.provider_id)
        .collect::<Vec<_>>();
    let provider_aliases = super::summary::provider_titles(&state.db, &provider_ids)
        .await
        .map_err(read_error)?;
    let provider_ratings = super::summary::provider_ratings(&state.db, &provider_ids).await;
    let provider_rating = super::summary::franchise_title_provider(&group)
        .and_then(|id| provider_ratings.get(&id).copied());
    let inherited =
        crate::services::language_preferences::franchise_config(&state.db, &cfg, &id).await;
    filters.normalize();
    let search = filters.f_q.to_lowercase();
    let mut releases = Vec::with_capacity(group.entries.len());
    for e in &group.entries {
        let tracked = e.series_id.and_then(|id| by_id.get(&id));
        let mut summary = e
            .series_id
            .and_then(|id| progress_by_id.get(&id))
            .cloned()
            .unwrap_or_else(|| CardProgress {
                label: "Outside your library".into(),
                monitoring_label: "Not monitored".into(),
                ..Default::default()
            });
        let upcoming = upcoming_release(e, tracked.copied());
        summary.upcoming |= upcoming;
        summary.following |= e.choice == Some(true);
        let followed = summary.monitored || summary.following;
        let next = if tracked.is_some() {
            summary.next_airing.clone()
        } else if upcoming {
            format!("Premiere · {}", availability::premiere_label(&e.date))
        } else {
            String::new()
        };
        let mut release = Release {
            provider_id: e.provider_id,
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
            provider_rating: provider_ratings.get(&e.provider_id).copied(),
            cover_url: tracked
                .map(|s| s.cover_url.clone())
                .or_else(|| e.related.as_ref().map(|r| r.cover_url.clone()))
                .unwrap_or_default(),
            path: super::detail::path(e.library_provider_id.unwrap_or(e.provider_id)),
            format: e.format.replace('_', " "),
            date: availability::premiere_label(&e.date),
            relationships: if filters.f_order == "storylines" {
                e.relationships()
                    .map(|(label, title, _)| format!("{label} {title}"))
                    .collect()
            } else {
                Vec::new()
            },
            followed,
            upcoming,
            monitoring_label: summary.monitoring_label.clone(),
            next_airing: next,
            added_date: summary.added_date.clone(),
            progress: summary,
            in_library: tracked.is_some(),
            matches_filter: false,
        };
        release.matches_filter = filters.matches(&release, &search);
        releases.push(release);
    }
    let visible_releases = releases
        .iter()
        .filter(|release| release.matches_filter)
        .count();
    let storylines = groups::storylines::sections(&group.entries);
    let branch_map_path = (seed != 0
        && !groups::storylines::branches::timeline_edges(&group.entries, &storylines).is_empty())
    .then(|| format!("/discover?franchise_id={seed}&order=branches#discover-branch-map"));
    let release_sections =
        release_sections(&group.entries, storylines, releases, &filters.f_order, seed);
    let watch_fallback = filters.f_order == "watch"
        && release_sections
            .first()
            .is_some_and(|section| section.title.is_empty());
    let page = FranchisePage {
        id,
        requirements,
        activity,
        future,
        search_on_monitoring_change: cfg.search_on_monitoring_change,
        page: "library",
        title_language: loaded.title_language,
        title: group.title,
        cover_url,
        provider_rating,
        release_sections,
        branch_map_path,
        watch_fallback,
        filters,
        total_releases: group.entries.len(),
        visible_releases,
        tracked: group.tracked,
        upcoming_releases,
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

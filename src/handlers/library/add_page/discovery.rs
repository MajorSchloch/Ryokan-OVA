//! Read-only franchise results and release choices for Add Anime.
use std::collections::{HashMap, HashSet};

use crate::{
    AppState,
    services::anilist::{self, AnimeSearchHit},
};

use super::{AddPage, ResultRow, detail, exact_detail, existing, facts, supported_provider};
use crate::handlers::library::groups::{self, Entry, Group};

pub(super) struct Franchise {
    pub title: String,
    pub subtitle: String,
    pub cover: String,
    pub library_url: Option<String>,
    pub releases: Vec<ResultRow>,
}

async fn search(state: &AppState, view: &AddPage) -> Result<Vec<AnimeSearchHit>, String> {
    let hits = anilist::search_anime_for_franchises(&view.q, view.source == "mal").await?;
    let mut results = Vec::with_capacity(hits.len());
    for mut hit in hits
        .into_iter()
        .filter(|hit| supported_provider(hit.entry.id))
    {
        // Preserve a MAL link after an unambiguous promotion. A split MAL ID
        // cannot choose an AniList part and stays available on its own.
        if hit.entry.id < 0
            && let Some(row) = existing(state, hit.entry.id)
                .await
                .map_err(|error| error.to_string())?
        {
            hit.entry.id = row.anilist_id;
        }
        results.push(hit);
    }
    Ok(results)
}

fn representative(group: &Group) -> &Entry {
    group
        .owner
        .and_then(|owner| {
            group
                .entries
                .iter()
                .find(|release| release.provider_id == owner)
        })
        .or_else(|| {
            group
                .entries
                .iter()
                .find(|e| matches!(e.format.as_str(), "TV" | "TV_SHORT" | "ONA"))
        })
        .unwrap_or(&group.entries[0])
}

fn display_title(group: &Group, main: &Entry) -> String {
    if group.page_id.is_some()
        || group
            .entries
            .iter()
            .any(|release| release.franchise_name.is_some())
    {
        group.title.clone()
    } else {
        main.title.clone()
    }
}

fn subtitle(entry: &Entry, display_title: &str) -> String {
    entry
        .related
        .as_ref()
        .and_then(|related| {
            [
                &related.title_english,
                &related.title_romaji,
                &related.title_native,
            ]
            .into_iter()
            .find(|title| !title.is_empty() && !title.eq_ignore_ascii_case(display_title))
        })
        .cloned()
        .unwrap_or_default()
}

fn cover(entry: &Entry) -> String {
    entry
        .related
        .as_ref()
        .map(|data| data.cover_url.clone())
        .unwrap_or_default()
}

fn year(entry: &Entry) -> Option<i32> {
    entry.date.get(..4).and_then(|value| value.parse().ok())
}

pub(super) async fn find_results(state: &AppState, view: &mut AddPage) -> Result<(), String> {
    let hits = search(state, view).await?;
    if hits.is_empty() {
        return Ok(());
    }
    let groups = groups::discover_groups(&state.db, &hits, None)
        .await
        .map_err(|error| error.to_string())?;
    let by_provider: HashMap<_, _> = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| {
            group.entries.iter().flat_map(move |release| {
                std::iter::once(release.provider_id)
                    .chain(release.library_provider_id)
                    .map(move |id| (id, index))
            })
        })
        .collect();
    let mut displayed = HashSet::new();
    for hit in &hits {
        let id = hit.entry.id;
        let Some(&index) = by_provider.get(&id) else {
            continue;
        };
        if !displayed.insert(index) {
            continue;
        }
        let group = &groups[index];
        let main = representative(group);
        let title = display_title(group, main);
        let info = if group.entries.len() > 1 {
            format!("{} releases", group.entries.len())
        } else {
            facts(&main.format, main.episodes, year(main))
        };
        view.results.push(ResultRow {
            subtitle: subtitle(main, &title),
            cover: cover(main),
            facts: info,
            href: view.franchise_path(id),
            existing: group.tracked > 0,
            title,
        });
    }
    Ok(())
}

pub(super) async fn open(state: &AppState, view: &AddPage, seed: i64) -> Result<Franchise, String> {
    let hits = if view.q.is_empty() {
        Vec::new()
    } else {
        // A deep link can still open its exact release if title search is unavailable.
        search(state, view).await.unwrap_or_default()
    };
    let tracked = existing(state, seed)
        .await
        .map_err(|error| error.to_string())?;
    let id = tracked.as_ref().map_or(seed, |row| row.anilist_id);
    let detail = if tracked.is_none() && !hits.iter().any(|hit| hit.entry.id == id) {
        Some(exact_detail(&state.db, id).await?)
    } else {
        None
    };
    let group = groups::discover_groups(&state.db, &hits, detail.as_ref())
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|g| {
            g.entries
                .iter()
                .any(|e| e.provider_id == id || e.library_provider_id == Some(id))
        })
        .ok_or_else(|| "Franchise unavailable".to_string())?;

    let main = representative(&group);
    let title = display_title(&group, main);
    let back = view.franchise_path(seed);
    let releases = group
        .entries
        .iter()
        .map(|entry| {
            let href = entry
                .library_provider_id
                .map(detail::path)
                .unwrap_or_else(|| format!("{back}&provider_id={}", entry.provider_id));
            ResultRow {
                title: entry.title.clone(),
                subtitle: subtitle(entry, &entry.title),
                cover: cover(entry),
                facts: facts(&entry.format, entry.episodes, year(entry)),
                href,
                existing: entry.series_id.is_some(),
            }
        })
        .collect();
    Ok(Franchise {
        subtitle: subtitle(main, &title),
        cover: cover(main),
        title,
        library_url: group.page_id.as_ref().map(|id| format!("/franchises/{id}")),
        releases,
    })
}

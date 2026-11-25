//! Read-only franchise results and release choices for Add Anime.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use crate::{
    AppState,
    models::{metadata_cache, series},
    services::{
        anilist::{self, AnimeDetail, AnimeSearchHit},
        html::sanitize_rich_description,
    },
};

use super::{
    AddPage, ResultRow, detail, exact_detail, existing, facts, supported_provider, upcoming,
};
use crate::handlers::library::groups::{self, Entry, Group};

const DETAIL_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Franchise {
    pub title: String,
    pub synopsis: String,
    pub alternate_titles: Vec<String>,
    pub cover: String,
    pub library_url: Option<String>,
    pub rating: Option<i32>,
    pub can_select: bool,
    pub sections: Vec<ReleaseSection>,
}

pub(super) struct ReleaseSection {
    pub id: &'static str,
    pub title: &'static str,
    pub releases: Vec<FranchiseRelease>,
}

pub(super) struct FranchiseRelease {
    pub id: i64,
    pub row: ResultRow,
    pub relationship: String,
}

pub(super) struct ResolvedFranchise {
    pub(super) group: Group,
    tracked: Option<series::Series>,
    detail: Option<AnimeDetail>,
    fetch_overview: bool,
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

async fn representative_detail(
    db: &sqlx::SqlitePool,
    main: &Entry,
    loaded: Option<AnimeDetail>,
    fetch: bool,
) -> Option<AnimeDetail> {
    let id = main.provider_id;
    if !supported_provider(id) {
        return None;
    }
    if let Some(detail) = loaded.filter(|detail| detail.id == id) {
        return Some(detail);
    }
    if let Some(series_id) = main.series_id
        && let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, series_id).await
        && cached.provider_id == id
        && cached.detail.id == id
    {
        return Some(cached.detail);
    }
    if let Ok(Some(cached)) = metadata_cache::get_by_provider_id(db, id).await
        && cached.detail.id == id
    {
        return Some(cached.detail);
    }
    if !fetch {
        return None;
    }
    // Provider latency must not make the release choices unavailable.
    let detail = tokio::time::timeout(DETAIL_TIMEOUT, anilist::get_anime_detail(id))
        .await
        .ok()?
        .ok()?;
    (detail.id == id).then_some(detail)
}

fn alternate_titles(
    title: &str,
    main: &Entry,
    detail: Option<&AnimeDetail>,
    tracked: Option<&series::Series>,
) -> Vec<String> {
    let mut names = vec![main.title.as_str()];
    if let Some(data) = &main.related {
        names.extend([
            data.title_english.as_str(),
            data.title_romaji.as_str(),
            data.title_native.as_str(),
        ]);
    }
    if let Some(detail) = detail {
        names.extend([
            detail.title_english.as_str(),
            detail.title_romaji.as_str(),
            detail.title_native.as_str(),
        ]);
        names.extend(detail.synonyms.iter().map(String::as_str));
    }
    let manual_aliases = tracked
        .map(series::Series::alternate_title_list)
        .unwrap_or_default();
    if let Some(row) = tracked {
        names.extend([
            row.title_english.as_str(),
            row.title_romaji.as_str(),
            row.title_native.as_str(),
        ]);
    }
    names.extend(manual_aliases.iter().map(String::as_str));
    let mut seen = HashSet::from([title.trim().to_lowercase()]);
    names
        .into_iter()
        .map(str::trim)
        .filter(|name| !name.is_empty() && seen.insert(name.to_lowercase()))
        .map(str::to_owned)
        .collect()
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
            rating: main.rating,
            href: view.franchise_path(id),
            existing: group.tracked > 0,
            title,
        });
    }
    Ok(())
}

pub(super) async fn resolve_group(
    state: &AppState,
    view: &AddPage,
    seed: i64,
) -> Result<ResolvedFranchise, String> {
    let mut hits = if view.q.is_empty() {
        Vec::new()
    } else {
        // A deep link can still open its exact release if title search is unavailable.
        search(state, view).await.unwrap_or_default()
    };
    let tracked = existing(state, seed)
        .await
        .map_err(|error| error.to_string())?;
    let id = tracked.as_ref().map_or(seed, |row| row.anilist_id);
    let mut fetch_overview = true;
    let detail = if tracked.is_none() && !hits.iter().any(|hit| hit.entry.id == id) {
        if let Some(saved) = upcoming::saved_hit(&state.db, id).await {
            // Keep a saved premiere usable when the provider cannot supply the
            // full franchise. Setup still resolves the exact ID separately.
            hits.push(saved);
            let detail = tokio::time::timeout(DETAIL_TIMEOUT, exact_detail(&state.db, id))
                .await
                .ok()
                .and_then(Result::ok);
            fetch_overview = detail.is_some();
            detail
        } else {
            Some(exact_detail(&state.db, id).await?)
        }
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
    Ok(ResolvedFranchise {
        group,
        tracked,
        detail,
        fetch_overview,
    })
}

fn release_sections(group: &Group, view: &AddPage, seed: i64) -> Vec<ReleaseSection> {
    let mut sections = [
        ("series", "Series"),
        ("movies", "Movies"),
        ("specials", "OVAs and specials"),
        ("other", "Other releases"),
    ]
    .map(|(id, title)| ReleaseSection {
        id,
        title,
        releases: Vec::new(),
    });
    for entry in &group.entries {
        let index = match entry.format.as_str() {
            "TV" | "TV_SHORT" | "ONA" => 0,
            "MOVIE" => 1,
            "OVA" | "SPECIAL" => 2,
            _ => 3,
        };
        let href = entry
            .library_provider_id
            .map(detail::path)
            .unwrap_or_else(|| view.setup_path(seed, entry.provider_id));
        sections[index].releases.push(FranchiseRelease {
            id: entry.provider_id,
            row: ResultRow {
                title: entry.title.clone(),
                subtitle: subtitle(entry, &entry.title),
                cover: cover(entry),
                facts: facts(&entry.format, entry.episodes, year(entry)),
                rating: entry.rating,
                href,
                existing: entry.series_id.is_some(),
            },
            relationship: entry
                .relationship()
                .map_or_else(String::new, |(label, peer)| format!("{label} {peer}")),
        });
    }
    sections
        .into_iter()
        .filter(|section| !section.releases.is_empty())
        .collect()
}

pub(super) async fn open(state: &AppState, view: &AddPage, seed: i64) -> Result<Franchise, String> {
    let resolved = resolve_group(state, view, seed).await?;
    Ok(project(state, view, seed, resolved).await)
}

pub(super) async fn project(
    state: &AppState,
    view: &AddPage,
    seed: i64,
    resolved: ResolvedFranchise,
) -> Franchise {
    let ResolvedFranchise {
        group,
        tracked,
        detail,
        fetch_overview,
    } = resolved;

    let main = representative(&group);
    let title = display_title(&group, main);
    let main_detail = representative_detail(&state.db, main, detail, fetch_overview).await;
    let main_row = if let Some(id) = main.series_id {
        match tracked.filter(|row| row.id == id) {
            Some(row) => Some(row),
            None => series::get_by_id(&state.db, id).await.ok().flatten(),
        }
    } else {
        None
    };
    Franchise {
        synopsis: main_detail.as_ref().map_or_else(String::new, |detail| {
            sanitize_rich_description(&detail.description, true)
        }),
        alternate_titles: alternate_titles(&title, main, main_detail.as_ref(), main_row.as_ref()),
        cover: main_detail
            .as_ref()
            .map(|detail| &detail.cover_url)
            .filter(|url| !url.is_empty())
            .or_else(|| {
                main_row
                    .as_ref()
                    .map(|row| &row.cover_url)
                    .filter(|url| !url.is_empty())
            })
            .cloned()
            .unwrap_or_else(|| cover(main)),
        title,
        library_url: group.page_id.as_ref().map(|id| format!("/franchises/{id}")),
        rating: main_detail
            .as_ref()
            .and_then(groups::detail_rating)
            .or(main.rating),
        can_select: group.entries.iter().any(|entry| entry.series_id.is_none()),
        sections: release_sections(&group, view, seed),
    }
}

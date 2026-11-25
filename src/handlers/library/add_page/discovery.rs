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
    AddPage, ReleaseOrder, ResultRow, branch_map, detail, exact_detail, existing, facts,
    storyline_selection_path, supported_provider, upcoming,
};
use crate::handlers::library::{
    availability::premiere_label,
    groups::{self, Entry, Group},
};

const DETAIL_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Franchise {
    pub title: String,
    pub synopsis: String,
    pub alternate_titles: Vec<String>,
    pub cover: String,
    pub library_url: Option<String>,
    pub rating: Option<i32>,
    pub can_select: bool,
    pub has_multiple: bool,
    pub has_branch_map: bool,
    pub branch_map: Option<branch_map::BranchMap>,
    pub order_hint: &'static str,
    pub sections: Vec<ReleaseSection>,
}

pub(super) struct ReleaseSection {
    pub id: String,
    pub title: String,
    pub releases: Vec<FranchiseRelease>,
    pub storyline: Option<i64>,
    pub links: Vec<Relationship>,
    pub selection_path: Option<String>,
    pub selection_count: usize,
}

impl ReleaseSection {
    fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        releases: Vec<FranchiseRelease>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            releases,
            storyline: None,
            links: Vec::new(),
            selection_path: None,
            selection_count: 0,
        }
    }

    pub fn has_selected(&self, selected: &HashSet<i64>) -> bool {
        self.releases
            .iter()
            .any(|release| selected.contains(&release.id) && !release.row.existing)
    }
}

pub(super) struct FranchiseRelease {
    pub id: i64,
    pub row: ResultRow,
    pub date: String,
    pub relationships: Vec<Relationship>,
    pub optional: bool,
}

pub(super) struct Relationship {
    pub label: String,
    pub title: String,
    pub id: i64,
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
            franchise: None,
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
        if let Some(mut saved) = upcoming::saved_hit(&state.db, id).await {
            // Keep a saved premiere usable when the provider cannot supply the
            // full franchise. Setup still resolves the exact ID separately.
            let detail = tokio::time::timeout(DETAIL_TIMEOUT, exact_detail(&state.db, id))
                .await
                .ok()
                .and_then(Result::ok);
            fetch_overview = detail.is_some();
            if fetch_overview {
                // An exact detail can supersede relation edges in the older snapshot.
                saved.relations.clear();
            }
            hits.push(saved);
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

fn release_sections(
    group: &Group,
    view: &AddPage,
    seed: i64,
    overview: Option<&AnimeDetail>,
    storylines: Vec<groups::storylines::Storyline>,
) -> (Vec<ReleaseSection>, &'static str) {
    let rows = group.entries.iter().map(|entry| {
        let href = entry
            .library_provider_id
            .map(detail::path)
            .unwrap_or_else(|| view.setup_path(seed, entry.provider_id));
        let rating = overview
            .filter(|data| data.id == entry.provider_id)
            .and_then(anilist::detail_rating)
            .or(entry.rating);
        FranchiseRelease {
            id: entry.provider_id,
            row: ResultRow {
                title: entry.title.clone(),
                subtitle: subtitle(entry, &entry.title),
                cover: cover(entry),
                facts: facts(&entry.format, entry.episodes, None),
                franchise: None,
                rating,
                href,
                existing: entry.series_id.is_some(),
            },
            date: premiere_label(&entry.date),
            relationships: entry
                .relationships()
                .map(|(label, title, id)| Relationship {
                    label: label.to_owned(),
                    title: title.to_owned(),
                    id,
                })
                .collect(),
            optional: false,
        }
    });
    if let Some(guide) = &view.guide {
        let guide_ids: HashSet<_> = guide.releases.iter().map(|step| step.id).collect();
        let mut rows: HashMap<_, _> = rows.map(|row| (row.id, row)).collect();
        let releases = guide
            .releases
            .iter()
            .filter_map(|step| {
                let mut row = rows.remove(&step.id)?;
                row.optional = step.optional;
                row.relationships
                    .retain(|link| guide_ids.contains(&link.id));
                Some(row)
            })
            .collect();
        return (
            vec![ReleaseSection::new("guide", &guide.name, releases)],
            "",
        );
    }
    if view.order.grouped() {
        let mut rows: HashMap<_, _> = rows.map(|row| (row.id, row)).collect();
        let path = view.franchise_path(seed);
        let sections = storylines
            .into_iter()
            .enumerate()
            .map(|(index, section)| {
                let storyline = section.kind.can_select().then(|| section.head());
                let releases: Vec<_> = section
                    .releases
                    .into_iter()
                    .filter_map(|id| rows.remove(&id))
                    .collect();
                let selection_count = releases
                    .iter()
                    .filter(|release| !release.row.existing)
                    .count();
                ReleaseSection {
                    id: format!("{}-{index}", section.kind.slug()),
                    title: section.title,
                    releases,
                    storyline,
                    links: section
                        .links
                        .into_iter()
                        .map(|link| Relationship {
                            label: link.label.to_owned(),
                            title: link.peer_title,
                            id: link.peer_id,
                        })
                        .collect(),
                    selection_path: storyline.filter(|_| selection_count > 0).map(|head| {
                        if view.order == ReleaseOrder::Branches {
                            storyline_selection_path(&view.branch_path(seed, head), head)
                        } else {
                            storyline_selection_path(&path, head)
                        }
                    }),
                    selection_count,
                }
            })
            .collect();
        return (sections, "");
    }
    if view.order == ReleaseOrder::Watch
        && let Some(sequence) = groups::watch_sequence(&group.entries)
    {
        let positions: HashMap<_, _> = sequence
            .into_iter()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();
        let (mut known, others): (Vec<_>, Vec<_>) =
            rows.partition(|row| positions.contains_key(&row.id));
        known.sort_by_key(|row| positions[&row.id]);
        let mut sections = vec![ReleaseSection::new("watch", "Known watch sequence", known)];
        if !others.is_empty() {
            sections.push(ReleaseSection::new(
                "other",
                "No known watch position",
                others,
            ));
        }
        return (sections, "");
    }
    if view.order != ReleaseOrder::Formats {
        let hint = if view.order == ReleaseOrder::Watch {
            "A single watch sequence is unavailable. Releases appear in premiere order."
        } else {
            ""
        };
        return (
            vec![ReleaseSection::new(
                "premiere",
                "Premiere order",
                rows.collect(),
            )],
            hint,
        );
    }
    let mut sections = [
        ("series", "Series"),
        ("movies", "Movies"),
        ("specials", "OVAs and specials"),
        ("other", "Other releases"),
    ]
    .map(|(id, title)| ReleaseSection::new(id, title, Vec::new()));
    for (entry, row) in group.entries.iter().zip(rows) {
        let index = match entry.format.as_str() {
            "TV" | "TV_SHORT" | "ONA" => 0,
            "MOVIE" => 1,
            "OVA" | "SPECIAL" => 2,
            _ => 3,
        };
        sections[index].releases.push(row);
    }
    (
        sections
            .into_iter()
            .filter(|section| !section.releases.is_empty())
            .collect(),
        "",
    )
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
    let storylines = groups::storylines::sections(&group.entries);
    let edges = groups::storylines::branches::edges(&group.entries, &storylines);
    let has_branch_map = !edges.is_empty();
    let branch_map = if view.order == ReleaseOrder::Branches {
        branch_map::build(&storylines, &edges, view, seed)
    } else {
        None
    };
    let (sections, order_hint) =
        release_sections(&group, view, seed, main_detail.as_ref(), storylines);
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
            .and_then(anilist::detail_rating)
            .or(main.rating),
        can_select: sections
            .iter()
            .flat_map(|section| &section.releases)
            .any(|release| !release.row.existing),
        has_multiple: group.entries.len() > 1,
        has_branch_map,
        branch_map,
        order_hint: if view.order == ReleaseOrder::Branches && !has_branch_map {
            "Storyline connections are unavailable. Browse the release outline below."
        } else {
            order_hint
        },
        sections,
    }
}

//! Read-only library groups. Provider IDs connect the graph. Library row IDs
//! identify folders and links, and never double as provider IDs.
pub mod follow;
pub mod preview;

use crate::{
    models::{config, local_metadata, series},
    services::{
        anilist::{AnimeDetail, RelatedEntry},
        library_link::pick_title,
    },
};
use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use futures_util::TryStreamExt;
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Template)]
#[template(path = "library_groups.html")]
struct GroupsPage {
    page: &'static str,
    title_language: String,
    groups: Vec<Group>,
    media_root: String,
}

struct Group {
    title: String,
    tracked: usize,
    outside: usize,
    entries: Vec<Entry>,
}

#[derive(Default)]
struct Entry {
    provider_id: i64,
    folder_name: String,
    library_provider_id: Option<i64>,
    franchise_name: Option<String>,
    related: Option<RelatedEntry>,
    mal_id: Option<i64>,
    excluded: bool,
    choice: Option<bool>,
    title: String,
    format: String,
    episodes: Option<i32>,
    date: String,
    season: String,
    series_id: Option<i64>,
    files: String,
    links: Vec<RelationLabel>,
}

struct RelationLabel {
    label: String,
    peer: String,
    outside: bool,
}

#[derive(Default)]
struct Catalog {
    entries: BTreeMap<i64, Entry>,
    // The label describes the first entry's relationship to its neighbor.
    neighbors: HashMap<i64, BTreeSet<(i64, &'static str)>>,
    // A MAL-only library row can stand for a single known AniList title.
    // Two AniList parts sharing a MAL ID must remain separate.
    by_mal: HashMap<i64, BTreeSet<i64>>,
}

fn date_label(date: Option<&str>, year: Option<i32>) -> String {
    date.filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| year.map(|y| y.to_string()))
        .unwrap_or_default()
}

impl Entry {
    fn release_key(&self) -> String {
        if self.date.len() == 4 {
            let month = match self.season.as_str() {
                "Winter" => "-01",
                "Spring" => "-04",
                "Summer" => "-07",
                "Fall" => "-10",
                _ => "",
            };
            return format!("{}{month}", self.date);
        }
        self.date.clone()
    }
}

impl Catalog {
    fn remember(&mut self, id: i64, mal: Option<i64>, entry: Entry) {
        if let Some(mal) = mal.filter(|m| *m > 0) {
            self.by_mal.entry(mal).or_default().insert(id);
        }
        let old = self.entries.entry(id).or_default();
        old.mal_id = mal.or(old.mal_id);
        if old.date.len() < entry.date.len() {
            old.date = entry.date;
        }
        if !entry.season.is_empty() {
            old.season = entry.season;
        }
        if !entry.title.is_empty() {
            old.title = entry.title;
        }
        if !entry.format.is_empty() {
            old.format = entry.format;
        }
        old.episodes = entry.episodes.or(old.episodes);
    }

    fn relation(&mut self, owner: i64, rel: &RelatedEntry, lang: &str) {
        if owner <= 0 || rel.id <= 0 || owner == rel.id || rel.media_type != "ANIME" {
            return;
        }
        self.remember(
            rel.id,
            rel.id_mal,
            Entry {
                title: pick_title(
                    lang,
                    &rel.title_english,
                    &rel.title_romaji,
                    &rel.title_native,
                )
                .into(),
                format: rel.format.clone(),
                episodes: rel.episodes,
                date: date_label(rel.release_date.as_deref(), rel.season_year),
                ..Entry::default()
            },
        );
        self.entries
            .get_mut(&rel.id)
            .unwrap()
            .related
            .get_or_insert_with(|| rel.clone());
        let (target_label, owner_label) = match rel.relation_type.as_str() {
            "PREQUEL" => ("Prequel to", "Sequel to"),
            "SEQUEL" => ("Sequel to", "Prequel to"),
            "ALTERNATIVE" => ("Alternative to", "Alternative to"),
            "SIDE_STORY" => ("Side story of", "Parent of"),
            "SPIN_OFF" => ("Spin-off of", "Parent of"),
            "PARENT" | "PARENT_STORY" => ("Parent of", "Side story of"),
            "SUMMARY" => ("Summary of", "Full story of"),
            "FULL_STORY" => ("Full story of", "Summary of"),
            _ => return,
        };
        self.neighbors
            .entry(owner)
            .or_default()
            .insert((rel.id, owner_label));
        self.neighbors
            .entry(rel.id)
            .or_default()
            .insert((owner, target_label));
    }

    fn group(mut self, follow_seed: Option<i64>) -> Vec<Group> {
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        // Start only at tracked entries. Provider caches can contain unrelated titles.
        let seeds: Vec<_> = self
            .entries
            .iter()
            .filter(|(id, e)| e.series_id.is_some() || follow_seed == Some(**id))
            .map(|(&id, _)| id)
            .collect();
        for seed in seeds {
            if !seen.insert(seed) {
                continue;
            }
            let mut pending = vec![seed];
            let mut members = Vec::new();
            while let Some(id) = pending.pop() {
                members.push(id);
                for &(other, _) in self.neighbors.get(&id).into_iter().flatten() {
                    if self.entries.contains_key(&other) && seen.insert(other) {
                        pending.push(other);
                    }
                }
            }
            // Never use input order or provider ID as a release date.
            members.sort_by_cached_key(|id| {
                let e = &self.entries[id];
                (
                    e.date.is_empty(),
                    e.release_key(),
                    e.title.to_lowercase(),
                    *id,
                )
            });
            for &id in &members {
                let mut links: Vec<_> = self
                    .neighbors
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|(peer, label)| {
                        if label.is_empty() {
                            return None;
                        }
                        let other = self.entries.get(peer)?;
                        Some(RelationLabel {
                            label: (*label).into(),
                            peer: other.title.clone(),
                            outside: other.series_id.is_none(),
                        })
                    })
                    .collect();
                links.sort_by(|a, b| (&a.label, &a.peer).cmp(&(&b.label, &b.peer)));
                let entry = self.entries.get_mut(&id).unwrap();
                entry.provider_id = id;
                entry.links = links;
            }
            let rows: Vec<_> = members
                .into_iter()
                .map(|id| self.entries.remove(&id).unwrap())
                .collect();
            let tracked = rows.iter().filter(|e| e.series_id.is_some()).count();
            let title = rows
                .iter()
                .find_map(|e| e.franchise_name.clone())
                .unwrap_or_else(|| {
                    rows.iter()
                        .find(|e| e.series_id.is_some())
                        .unwrap_or(&rows[0])
                        .title
                        .clone()
                });
            groups.push(Group {
                title,
                tracked,
                outside: rows.len() - tracked,
                entries: rows,
            });
        }
        groups.sort_by_cached_key(|g| g.title.to_lowercase());
        groups
    }
}

async fn catalog(db: &SqlitePool, lang: &str) -> Result<Catalog, sqlx::Error> {
    let mut catalog = Catalog::default();
    let mut authoritative = HashSet::new();
    // Prefer the tracked detail, as the details page does. Stream the blobs
    // so the full descriptions and episode lists do not stay in memory.
    let mut details = sqlx::query_as::<_, (i64, String)>(
        "SELECT provider_id, detail_json FROM series_metadata_cache WHERE provider_id > 0
         UNION ALL SELECT provider_id, detail_json FROM provider_metadata_cache WHERE provider_id > 0"
    ).fetch(db);
    while let Some((id, json)) = details.try_next().await? {
        if authoritative.contains(&id) {
            continue;
        }
        let Ok(detail) = serde_json::from_str::<AnimeDetail>(&json) else {
            // A damaged blob should only lose its own extra metadata. The
            // normalized rows and the tracked library entry remain usable.
            tracing::debug!(id, "Skipping unreadable cached detail in library groups");
            continue;
        };
        if detail.id != id {
            continue;
        }
        catalog.remember(
            detail.id,
            detail.id_mal,
            Entry {
                title: pick_title(
                    lang,
                    &detail.title_english,
                    &detail.title_romaji,
                    &detail.title_native,
                )
                .into(),
                format: detail.format.clone(),
                episodes: detail.episodes,
                date: date_label(detail.release_date.as_deref(), detail.season_year),
                season: match detail.season.as_str() {
                    "WINTER" => "Winter",
                    "SPRING" => "Spring",
                    "SUMMER" => "Summer",
                    "FALL" => "Fall",
                    _ => "",
                }
                .into(),
                ..Entry::default()
            },
        );
        catalog.entries.get_mut(&id).unwrap().related = Some(RelatedEntry {
            id,
            id_mal: detail.id_mal,
            title_romaji: detail.title_romaji,
            title_english: detail.title_english,
            title_native: detail.title_native,
            cover_url: detail.cover_url,
            format: detail.format,
            status: detail.status,
            status_display: detail.status_display,
            episodes: detail.episodes,
            season_year: detail.season_year,
            release_date: detail.release_date,
            start_date: None,
            relation_type: String::new(),
            media_type: "ANIME".into(),
        });
        if !detail.relations.is_empty() {
            authoritative.insert(detail.id);
            for rel in &detail.relations {
                catalog.relation(detail.id, rel, lang);
            }
        }
    }
    drop(details);
    // Old or incomplete detail blobs can still have the normalized relation
    // rows used by the details page. Never combine stale and authoritative edges.
    let mut cached = sqlx::query("SELECT provider_id AS owner, r.* FROM provider_relations_cache r
        WHERE provider_id > 0 AND related_provider_id > 0 AND media_type = 'ANIME'
        UNION ALL SELECT s.anilist_id AS owner, r.* FROM series_relations_cache r
        JOIN series s ON s.id = r.series_id WHERE s.anilist_id > 0 AND related_provider_id > 0 AND media_type = 'ANIME'").fetch(db);
    while let Some(row) = cached.try_next().await? {
        let owner = row.get("owner");
        if !authoritative.contains(&owner) {
            catalog.relation(owner, &local_metadata::row_to_related(row), lang);
        }
    }
    drop(cached);
    // Existing caches can date a premiere from the entry's own first episode.
    let premieres = sqlx::query_as::<_, (i64, String)>(
        "SELECT provider_id, date(substr(aired, 1, 10)) FROM provider_episode_metadata
          WHERE episode_number = 1 AND date(substr(aired, 1, 10)) IS NOT NULL
         UNION SELECT s.anilist_id, date(substr(e.aired, 1, 10)) FROM series_episode_metadata e
          JOIN series s ON s.id = e.series_id WHERE episode_number = 1 AND date(substr(e.aired, 1, 10)) IS NOT NULL"
    )
    .fetch_all(db)
    .await?;
    for (id, date) in premieres {
        let entry = catalog.entries.entry(id).or_default();
        if entry.date.len() < 10 && date.starts_with(&entry.date) {
            entry.date = date;
        }
    }
    Ok(catalog)
}

fn file_status(facts: &super::inventory::Inventory) -> String {
    let Some(count) = facts.on_disk else {
        return "File status unknown".into();
    };
    let missing = facts.missing.as_deref().unwrap_or_default();
    if let [episode] = missing {
        return format!("Episode {episode} missing on disk");
    }
    if !missing.is_empty() {
        return format!("{} episodes missing on disk", missing.len());
    }
    match count {
        0 => "No episode files on disk".into(),
        1 => "1 episode on disk".into(),
        n => format!("{n} episodes on disk"),
    }
}

async fn load(db: &SqlitePool) -> Result<GroupsPage, sqlx::Error> {
    load_groups(db, true, None).await
}

async fn load_groups(
    db: &SqlitePool,
    include_disk_status: bool,
    follow_seed: Option<&crate::models::release_follows::Follow>,
) -> Result<GroupsPage, sqlx::Error> {
    let cfg = config::get_config(db).await?.unwrap_or_default();
    let mut catalog = catalog(db, &cfg.title_language).await?;
    let library = series::get_all(db).await?;
    let inventory = if include_disk_status {
        super::inventory::load(db, &cfg.media_root, &library).await?
    } else {
        HashMap::new()
    };
    let positive: HashSet<_> = library
        .iter()
        .filter(|s| s.anilist_id > 0)
        .map(|s| s.anilist_id)
        .collect();
    for s in library {
        let mut id = s.anilist_id;
        if id < 0
            && let Some(ids) = s.mal_id.and_then(|mal| catalog.by_mal.get(&mal))
            && ids.len() == 1
            && let Some(&al) = ids.first()
            && !positive.contains(&al)
        {
            id = al;
        }
        let e = catalog.entries.entry(id).or_default();
        e.mal_id = s.mal_id.or(e.mal_id);
        e.series_id = Some(s.id);
        e.library_provider_id = Some(s.anilist_id);
        e.folder_name = s.folder_name;
        let title = pick_title(
            &cfg.title_language,
            &s.title_english,
            &s.title_romaji,
            &s.title_native,
        );
        e.title = if title.is_empty() {
            s.title.clone()
        } else {
            title.into()
        };
        if !s.format.is_empty() {
            e.format.clone_from(&s.format);
        }
        e.episodes = s.episodes.or(e.episodes);
        if e.date.is_empty() {
            e.date = date_label(None, s.season_year);
        }
        if let Some(facts) = inventory.get(&s.id) {
            e.files = file_status(facts);
        }
    }
    // Storage membership survives cache changes without creating an acquisition follow.
    let storage:Vec<(String,i64,i64)>=sqlx::query_as("SELECT f.folder,f.owner_provider_id,m.provider_id FROM library_franchises f JOIN library_franchise_members m ON m.folder=f.folder")
        .fetch_all(db).await?;
    for (name, root, member) in storage {
        catalog.entries.entry(root).or_default().franchise_name = Some(name);
        if root != member {
            catalog
                .neighbors
                .entry(root)
                .or_default()
                .insert((member, ""));
            catalog
                .neighbors
                .entry(member)
                .or_default()
                .insert((root, ""));
        }
    }
    // Keep saved members reachable even if AniList later drops a relation.
    let choices: Vec<(i64, String, i64, String, bool)> = sqlx::query_as(
        "SELECT f.anilist_id,f.title,c.anilist_id,c.title,c.selected FROM franchise_choices c
             JOIN release_follows f ON f.id=c.follow_id",
    )
    .fetch_all(db)
    .await?;
    for (root, name, member, title, selected) in choices {
        catalog.entries.entry(root).or_default().franchise_name = Some(name);
        let entry = catalog.entries.entry(member).or_default();
        entry.choice = Some(selected);
        if entry.title.is_empty() {
            entry.title = title;
        }
        if root != member {
            catalog
                .neighbors
                .entry(root)
                .or_default()
                .insert((member, ""));
            catalog
                .neighbors
                .entry(member)
                .or_default()
                .insert((root, ""));
        }
    }
    if !include_disk_status {
        let exclusions = crate::models::sync_exclusions::load_set(db).await;
        for (id, e) in &mut catalog.entries {
            e.excluded = exclusions.contains(*id, e.mal_id);
        }
    }
    if let Some(follow) = follow_seed {
        let entry = catalog.entries.entry(follow.anilist_id).or_default();
        if entry.title.is_empty() {
            entry.title.clone_from(&follow.title);
        }
    }
    Ok(GroupsPage {
        page: "library",
        title_language: cfg.title_language,
        groups: catalog.group(follow_seed.map(|f| f.anilist_id)),
        media_root: cfg.media_root,
    })
}

pub async fn page(State(db): State<SqlitePool>) -> Result<Response, (StatusCode, &'static str)> {
    let page = load(&db).await.map_err(|err| {
        tracing::error!("Cannot read library groups: {err}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read library groups.",
        )
    })?;
    page.render()
        .map(|html| (super::view_cookie("groups"), Html(html)).into_response())
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not display library groups.",
            )
        })
}

#[cfg(test)]
mod tests;

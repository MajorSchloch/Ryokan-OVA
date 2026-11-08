//! Read-only library groups. Provider IDs connect the graph. Library row IDs
//! identify folders and links, and never double as provider IDs.
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
}

struct Group {
    title: String,
    tracked: usize,
    outside: usize,
    entries: Vec<Entry>,
}

#[derive(Default)]
struct Entry {
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
        let (target_label, owner_label) = match rel.relation_type.as_str() {
            "PREQUEL" => ("Prequel to", "Sequel to"),
            "SEQUEL" => ("Sequel to", "Prequel to"),
            "ALTERNATIVE" => ("Alternative to", "Alternative to"),
            "SIDE_STORY" => ("Side story of", "Parent of"),
            "SPIN_OFF" => ("Spin-off of", "Parent of"),
            "PARENT" | "PARENT_STORY" => ("Parent of", "Side story of"),
            "SUMMARY" => ("Summary of", "Full story of"),
            "FULL_STORY" => ("Full story of", "Summary of"),
            "ADAPTATION" => ("Adaptation of", "Source for"),
            "SOURCE" => ("Source for", "Adaptation of"),
            "CHARACTER" => ("Shares a character with", "Shares a character with"),
            "COMPILATION" => ("Compilation of", "Included in"),
            "CONTAINS" => ("Included in", "Contains"),
            "" => return,
            _ => ("Related to", "Related to"),
        };
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
        self.neighbors
            .entry(owner)
            .or_default()
            .insert((rel.id, owner_label));
        self.neighbors
            .entry(rel.id)
            .or_default()
            .insert((owner, target_label));
    }

    fn group(mut self) -> Vec<Group> {
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        // Start only at tracked entries. Provider caches can contain unrelated titles.
        let seeds: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| e.series_id.is_some())
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
                        let other = self.entries.get(peer)?;
                        Some(RelationLabel {
                            label: (*label).into(),
                            peer: other.title.clone(),
                            outside: other.series_id.is_none(),
                        })
                    })
                    .collect();
                links.sort_by(|a, b| (&a.label, &a.peer).cmp(&(&b.label, &b.peer)));
                self.entries.get_mut(&id).unwrap().links = links;
            }
            let rows: Vec<_> = members
                .into_iter()
                .map(|id| self.entries.remove(&id).unwrap())
                .collect();
            let tracked = rows.iter().filter(|e| e.series_id.is_some()).count();
            let title = rows
                .iter()
                .find(|e| e.series_id.is_some())
                .unwrap()
                .title
                .clone();
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
                format: detail.format,
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
    let cfg = config::get_config(db).await?.unwrap_or_default();
    let mut catalog = catalog(db, &cfg.title_language).await?;
    let library = series::get_all(db).await?;
    let inventory = super::inventory::load(db, &cfg.media_root, &library).await?;
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
        e.series_id = Some(s.id);
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
        e.files = file_status(&inventory[&s.id]);
    }
    Ok(GroupsPage {
        page: "library",
        title_language: cfg.title_language,
        groups: catalog.group(),
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

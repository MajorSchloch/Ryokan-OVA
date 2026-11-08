//! A projection of cached releases and follows. The query string owns all draft choices.
use super::{Entry, Group, load_groups};
use crate::{models::release_follows, services::media};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

#[derive(Template)]
#[template(path = "franchise_preview.html")]
struct Preview {
    page: &'static str,
    title_language: String,
    seed: i64,
    owner: Option<i64>,
    folder: String,
    name_override: String,
    selection_empty: bool,
    policy: Policy,
    rows: Vec<Row>,
    follows: Vec<release_follows::Follow>,
}

#[derive(Default)]
struct Policy {
    tv: bool,
    movies: bool,
    ovas: bool,
    alternatives: bool,
}

impl Policy {
    fn for_preview(
        query: &[(String, String)],
        submitted: bool,
        follows: &[release_follows::Follow],
    ) -> Self {
        if submitted {
            let includes = |option: &str| query.iter().any(|(k, v)| k == "include" && v == option);
            return Self {
                tv: includes("tv"),
                movies: includes("movies"),
                ovas: includes("ovas"),
                alternatives: includes("alternatives"),
            };
        }
        match follows {
            [] => Self {
                tv: true,
                ..Self::default()
            },
            [f] => Self {
                tv: true,
                movies: f.movies,
                ovas: f.ovas,
                alternatives: f.alternatives,
            },
            _ => Self::default(),
        }
    }
}

struct Row {
    entry: Entry,
    selected: bool,
    source: Option<String>,
    destination: String,
}

// A shared character can connect unrelated shows in the broader groups view.
fn franchise_members(entries: &HashMap<i64, &Entry>, seed: i64) -> HashSet<i64> {
    let mut found = HashSet::from([seed]);
    let mut pending = vec![seed];
    while let Some(id) = pending.pop() {
        for link in &entries[&id].links {
            if link.franchise_relation && found.insert(link.peer_id) {
                pending.push(link.peer_id);
            }
        }
    }
    found
}

fn folder_component(label: &str, id: i64) -> String {
    let name = media::sanitize_folder_name(label);
    let suffix = format!(" [AniList {id}]");
    // Keep the identity even when a long title reaches the filesystem limit.
    let end = name.floor_char_boundary(255 - suffix.len());
    format!("{}{suffix}", &name[..end]).trim().to_owned()
}

fn release_folder(entry: &Entry) -> String {
    let date = entry.release_key();
    let date = if date.is_empty() { "Undated" } else { &date };
    let format = if entry.format.is_empty() {
        "Release"
    } else {
        &entry.format
    };
    folder_component(
        &format!("{date} - {format} - {}", entry.title),
        entry.provider_id,
    )
}

fn proposed_owner(entries: &[Entry], candidates: &HashSet<i64>) -> Option<i64> {
    // AniList prequels can premiere years later (the three Kizumonogatari films).
    // The catalog already sorts by premiere. Do not break an owner tie with a title or ID.
    let mut dated = entries
        .iter()
        .filter(|e| candidates.contains(&e.provider_id) && !e.date.is_empty());
    match (dated.next(), dated.next()) {
        (Some(first), Some(next)) if first.release_key() == next.release_key() => None,
        (Some(first), _) => Some(first.provider_id),
        _ if candidates.len() == 1 => candidates.iter().next().copied(),
        _ => None,
    }
}

type Error = (StatusCode, &'static str);

fn project(
    group: Group,
    seed: i64,
    query: &[(String, String)],
    media_root: &str,
    follows: Vec<release_follows::Follow>,
    title_language: String,
) -> Result<Preview, Error> {
    let entries: HashMap<_, _> = group.entries.iter().map(|e| (e.provider_id, e)).collect();
    let candidates = franchise_members(&entries, seed);
    let mut owner = proposed_owner(&group.entries, &candidates);
    let mut name_override = String::new();
    let bad = (
        StatusCode::BAD_REQUEST,
        "Choose a release from this franchise preview.",
    );
    let mut selected = HashSet::new();
    let mut submitted = false;
    for (name, value) in query {
        match name.as_str() {
            "selection" => submitted = true,
            "name" => name_override = value.trim().to_owned(),
            "owner" if value.is_empty() => owner = None,
            "owner" | "release" => {
                let id = value.parse().map_err(|_| bad)?;
                if !candidates.contains(&id) {
                    return Err(bad);
                }
                if name == "owner" {
                    owner = Some(id);
                } else {
                    selected.insert(id);
                }
            }
            _ => {}
        }
    }
    if !submitted {
        selected.extend(
            candidates
                .iter()
                .filter(|id| entries[id].series_id.is_some()),
        );
    }
    let folder = owner
        .map(|id| {
            let title = if name_override.is_empty() {
                &entries[&id].title
            } else {
                &name_override
            };
            folder_component(title, id)
        })
        .unwrap_or_default();
    let folder = if folder.is_empty() || media_root.trim().is_empty() {
        String::new()
    } else {
        std::path::Path::new(media_root)
            .join(folder)
            .display()
            .to_string()
    };
    let follows: Vec<_> = follows
        .into_iter()
        .filter(|f| candidates.contains(&f.anilist_id))
        .collect();
    let policy = Policy::for_preview(query, submitted, &follows);
    let rows = group
        .entries
        .into_iter()
        .filter(|e| candidates.contains(&e.provider_id))
        .map(|entry| {
            let selected = selected.contains(&entry.provider_id);
            let source = (!media_root.trim().is_empty()
                && media::usable_folder_name(&entry.folder_name))
            .then(|| {
                std::path::Path::new(media_root)
                    .join(&entry.folder_name)
                    .display()
                    .to_string()
            });
            let destination = if selected && !folder.is_empty() {
                release_folder(&entry)
            } else {
                String::new()
            };
            Row {
                entry,
                selected,
                source,
                destination,
            }
        })
        .collect();
    Ok(Preview {
        page: "library",
        title_language,
        seed,
        owner,
        folder,
        name_override,
        policy,
        selection_empty: selected.is_empty(),
        rows,
        follows,
    })
}

pub async fn page(
    State(db): State<SqlitePool>,
    Path(seed): Path<i64>,
    Query(query): Query<Vec<(String, String)>>,
) -> Result<Html<String>, Error> {
    let read_error = |err: sqlx::Error| {
        tracing::error!(%err, "Cannot read franchise preview");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read the franchise preview.",
        )
    };
    let follows = release_follows::list(&db).await.map_err(read_error)?;
    let follow_seed = follows.iter().find(|f| f.anilist_id == seed);
    let view = load_groups(&db, false, follow_seed)
        .await
        .map_err(read_error)?;
    let group = view
        .groups
        .into_iter()
        .find(|g| {
            g.entries.iter().any(|e| {
                e.provider_id == seed
                    && seed > 0
                    && (e.series_id.is_some() || follow_seed.is_some())
            })
        })
        .ok_or((
            StatusCode::NOT_FOUND,
            "This release is outside your library.",
        ))?;
    project(
        group,
        seed,
        &query,
        &view.media_root,
        follows,
        view.title_language,
    )?
    .render()
    .map(Html)
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not display the franchise preview.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::library::groups::Catalog;
    use serde_json::json;
    #[test]
    fn grouped_library_monogatari_uses_premiere_order_across_formats() {
        // The first Kizumonogatari film is a story prequel that premiered after Bakemonogatari.
        let mut catalog = Catalog::default();
        catalog.remember(
            5081,
            None,
            Entry {
                title: "Bakemonogatari".into(),
                date: "2009-07-03".into(),
                format: "TV".into(),
                ..Default::default()
            },
        );
        for (id, title, date, format, relation) in [
            (
                9260,
                "Kizumonogatari Part 1",
                "2016-01-08",
                "MOVIE",
                "PREQUEL",
            ),
            (21520, "Koyomimonogatari", "2016-01-09", "ONA", "SIDE_STORY"),
        ] {
            let related = serde_json::from_value(json!({"id":id,"title_romaji":title,"title_english":title,"title_native":"","cover_url":"","format":format,"status":"FINISHED","status_display":"Finished","relation_type":relation,"media_type":"ANIME","release_date":date})).unwrap();
            catalog.relation(5081, &related, "english");
        }
        catalog.entries.get_mut(&21520).unwrap().series_id = Some(42);
        let group = catalog.group(None).pop().unwrap();
        let db_query = Query::<Vec<(String, String)>>::try_from_uri(
            &"/?selection=1&name=Monogatari&include=movies&release=5081&release=9260"
                .parse()
                .unwrap(),
        )
        .unwrap();
        let preview = project(
            group,
            21520,
            &db_query.0,
            "/data/anime",
            vec![],
            "english".into(),
        )
        .unwrap();
        assert!(preview.policy.movies);
        assert!(!preview.policy.tv);
        let folders: Vec<_> = preview
            .rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.destination.as_str())
            .collect();
        assert_eq!(preview.owner, Some(5081));
        assert_eq!(
            folders,
            [
                "2009-07-03 - TV - Bakemonogatari [AniList 5081]",
                "2016-01-08 - MOVIE - Kizumonogatari Part 1 [AniList 9260]"
            ]
        );
    }
}

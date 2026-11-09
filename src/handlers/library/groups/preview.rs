//! A projection of cached releases and follows. The query string owns all draft choices.
use super::{Entry, Group, load_groups};
use crate::{
    models::release_follows,
    services::{franchise_files, media},
};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
};
use sqlx::SqlitePool;
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

#[derive(Template)]
#[template(path = "franchise_preview.html")]
pub(super) struct Preview {
    page: &'static str,
    title_language: String,
    seed: i64,
    pub(super) owner: Option<i64>,
    folder: String,
    pub(super) name_override: String,
    selection_empty: bool,
    pub(super) policy: Policy,
    pub(super) rows: Vec<Row>,
    pub(super) follows: Vec<release_follows::Follow>,
    pub(super) version: String,
    files_requested: bool,
    file_issues: Vec<String>,
}

#[derive(Default)]
pub(super) struct Policy {
    pub(super) tv: bool,
    pub(super) movies: bool,
    pub(super) ovas: bool,
    pub(super) alternatives: bool,
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
                tv: f.tv,
                movies: f.movies,
                ovas: f.ovas,
                alternatives: f.alternatives,
            },
            _ => Self::default(),
        }
    }
}

pub(super) struct Row {
    pub(super) entry: Entry,
    pub(super) selected: bool,
    source: Option<String>,
    destination: String,
    file_preview: Option<franchise_files::ReleaseFiles>,
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

fn proposed_owner(entries: &[Entry]) -> Option<i64> {
    // AniList prequels can premiere years later (the three Kizumonogatari films).
    // The catalog already sorts by premiere. Do not break an owner tie with a title or ID.
    let mut dated = entries.iter().filter(|e| !e.date.is_empty());
    match (dated.next(), dated.next()) {
        (Some(first), Some(next)) if first.release_key() == next.release_key() => None,
        (Some(first), _) => Some(first.provider_id),
        _ if entries.len() == 1 => Some(entries[0].provider_id),
        _ => None,
    }
}

pub(super) type Error = (StatusCode, &'static str);

fn project(
    group: Group,
    seed: i64,
    query: &[(String, String)],
    media_root: &str,
    follows: Vec<release_follows::Follow>,
    title_language: String,
) -> Result<Preview, Error> {
    let entries: HashMap<_, _> = group.entries.iter().map(|e| (e.provider_id, e)).collect();
    let follows: Vec<_> = follows
        .into_iter()
        .filter(|f| entries.contains_key(&f.anilist_id))
        .collect();
    let managed = follows.iter().find(|f| f.revision > 0);
    let mut owner = managed
        .map(|f| f.anilist_id)
        .or_else(|| proposed_owner(&group.entries));
    let mut name_override = managed
        .filter(|f| entries[&f.anilist_id].title != f.title)
        .map(|f| f.title.clone())
        .unwrap_or_default();
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
                if !entries.contains_key(&id) {
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
        selected.extend(entries.keys().filter(|id| {
            let e = entries[id];
            !e.excluded && e.series_id.is_some() && e.choice.unwrap_or(managed.is_none())
        }));
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
    // Reject an old form if the library, relation cache, exclusions, or follow changed
    // after it was rendered. The form only needs one small value even for long franchises.
    let mut fingerprint = DefaultHasher::new();
    version(&follows).hash(&mut fingerprint);
    for entry in &group.entries {
        (
            entry.provider_id,
            entry.series_id,
            entry.choice,
            entry.excluded,
        )
            .hash(&mut fingerprint);
    }
    let version = fingerprint.finish().to_string();
    selected.retain(|id| !entries[id].excluded);
    let policy = Policy::for_preview(query, submitted, &follows);
    let rows = group
        .entries
        .into_iter()
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
                file_preview: None,
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
        version,
        files_requested: false,
        file_issues: Vec::new(),
    })
}

pub(super) fn version(follows: &[release_follows::Follow]) -> String {
    follows
        .iter()
        .map(|f| {
            format!(
                "{}-{}-{}{}{}",
                f.id,
                f.revision,
                u8::from(f.movies),
                u8::from(f.ovas),
                u8::from(f.alternatives)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn read_error(err: sqlx::Error) -> Error {
    tracing::error!(%err, "Cannot access franchise follow");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not load or save the franchise follow.",
    )
}

pub(super) async fn draft(
    db: &SqlitePool,
    seed: i64,
    query: &[(String, String)],
) -> Result<Preview, Error> {
    let follows = release_follows::list(db).await.map_err(read_error)?;
    let follow_seed = follows.iter().find(|f| f.anilist_id == seed);
    let view = load_groups(db, false, follow_seed)
        .await
        .map_err(read_error)?;
    let (group, seed) = view
        .groups
        .into_iter()
        .find_map(|group| {
            let id = group
                .entries
                .iter()
                .find(|e| {
                    (e.provider_id == seed || e.library_provider_id == Some(seed))
                        && (e.series_id.is_some() || follow_seed.is_some())
                })
                .map(|e| e.provider_id)?;
            Some((group, id))
        })
        .ok_or((
            StatusCode::NOT_FOUND,
            "This release is outside your library.",
        ))?;
    project(
        group,
        seed,
        query,
        &view.media_root,
        follows,
        view.title_language,
    )
}

pub async fn page(
    State(db): State<SqlitePool>,
    Path(seed): Path<i64>,
    Query(query): Query<Vec<(String, String)>>,
) -> Result<Html<String>, Error> {
    let mut preview = draft(&db, seed, &query).await?;
    if query.iter().any(|(k, v)| k == "files" && v == "1") && !preview.folder.is_empty() {
        let cfg = crate::models::config::get_config(&db)
            .await
            .map_err(read_error)?
            .unwrap_or_default();
        let owners = crate::models::series::get_all(&db)
            .await
            .map_err(read_error)?
            .into_iter()
            .map(|s| s.folder_name)
            .collect::<Vec<_>>();
        let pending: HashSet<i64> = sqlx::query_scalar(
            "SELECT series_id FROM grabbed_torrents WHERE state='pending'
             UNION SELECT r.series_id FROM grabbed_torrent_series r
                 JOIN grabbed_torrents g ON g.id=r.grab_id WHERE g.state='pending'
             UNION SELECT series_id FROM pending_grabs WHERE series_id IS NOT NULL",
        )
        .fetch_all(&db)
        .await
        .map_err(read_error)?
        .into_iter()
        .collect();
        let active = preview
            .rows
            .iter()
            .any(|r| r.selected && r.entry.series_id.is_some_and(|id| pending.contains(&id)));
        preview = tokio::task::spawn_blocking(move || {
            let releases = preview
                .rows
                .iter()
                .filter(|r| r.selected)
                .map(|r| franchise_files::Release {
                    anilist_id: r.entry.provider_id,
                    source: r.entry.series_id.map(|_| r.entry.folder_name.clone()),
                    destination: r.destination.clone(),
                    episodes: r.entry.episodes,
                })
                .collect::<Vec<_>>();
            let report = franchise_files::inspect(
                std::path::Path::new(&cfg.media_root),
                std::path::Path::new(&preview.folder),
                &releases,
                &owners,
            );
            preview.files_requested = true;
            preview.file_issues = report.issues;
            if active {
                preview.file_issues.push("Downloads or imports are active for a selected release. Refresh this preview after they finish.".into());
            }
            for files in report.releases {
                if let Some(row) = preview
                    .rows
                    .iter_mut()
                    .find(|r| r.entry.provider_id == files.anilist_id)
                {
                    row.file_preview = Some(files);
                }
            }
            preview
        })
        .await
        .map_err(|e| {
            tracing::error!(%e,"Cannot preview franchise files");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read the franchise files.",
            )
        })?;
    }
    preview.render().map(Html).map_err(|_| {
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

//! Optional filesystem view for Jellyfin. A release keeps its original flat
//! path while Ryokan owns its physical franchise path. No media is copied.
use super::media;
use crate::models::series;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const STATE: &str = ".ryokan-library-view.json";
const TEMP_LINK: &str = ".ryokan-library-view-link";
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
type Links = BTreeMap<String, String>;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    media_root: PathBuf,
    source_root: PathBuf,
    links: Links,
    pending: Option<Links>,
}

fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn directories(root: &Path) -> Result<Vec<String>, String> {
    fs::read_dir(root)
        .map_err(error)?
        .map(|entry| {
            let entry = entry.map_err(error)?;
            let meta = entry.file_type().map_err(error)?;
            if meta.is_symlink() && entry.path().is_dir() {
                return Err(
                    "Resolve linked top-level media folders before enabling the Jellyfin view"
                        .into(),
                );
            }
            Ok((meta.is_dir() && !meta.is_symlink())
                .then(|| entry.file_name().to_string_lossy().into_owned()))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(|rows| rows.into_iter().flatten().collect())
}

async fn aliases(
    db: &SqlitePool,
    root: &Path,
    view: &Path,
) -> Result<(Links, HashSet<String>), String> {
    let previous: State = if view.join(STATE).exists() {
        serde_json::from_slice(&fs::read(view.join(STATE)).map_err(error)?).map_err(error)?
    } else {
        State::default()
    };
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(error)?;
    let rows = sqlx::query("SELECT id,folder_name,season_year FROM series ORDER BY id")
        .fetch_all(&mut *tx)
        .await
        .map_err(error)?;
    let mut claimed: Vec<String> = Vec::new();
    for row in &rows {
        let path: String = row.get("folder_name");
        if path.is_empty() {
            continue;
        }
        if claimed
            .iter()
            .any(|other| media::library_paths_overlap(other, &path))
        {
            return Err("Library releases share a folder or a parent folder. Resolve their ownership before enabling the Jellyfin view".into());
        }
        claimed.push(path);
    }
    let mut parents: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT library_folder FROM release_follows WHERE library_folder != ''",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(error)?
    .into_iter()
    .collect();
    for row in &rows {
        let path: String = row.get("folder_name");
        if let Some((parent, _)) = path.split_once('/') {
            parents.insert(parent.into());
        }
    }
    let disk: HashSet<_> = directories(root)?
        .into_iter()
        .filter(|p| !parents.contains(p))
        .map(|p| p.to_ascii_lowercase())
        .collect();
    let mut taken: HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT lower(folder_name) FROM series_library_aliases")
            .fetch_all(&mut *tx)
            .await
            .map_err(error)?
            .into_iter()
            .collect();
    let registered = taken.clone();
    taken.extend(
        previous
            .links
            .keys()
            .chain(previous.pending.iter().flat_map(|p| p.keys()))
            .map(|name| name.to_ascii_lowercase()),
    );
    for row in rows {
        let id: i64 = row.get("id");
        let path: String = row.get("folder_name");
        if path.is_empty() {
            continue;
        }
        if !media::usable_library_path(&path) {
            return Err(format!("Invalid library path for series {id}"));
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM series_library_aliases WHERE series_id=?)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(error)?;
        if exists {
            continue;
        }
        let leaf = path.rsplit('/').next().unwrap();
        let retained = previous
            .links
            .iter()
            .chain(previous.pending.iter().flat_map(|p| p.iter()))
            .find(|(name, relative)| {
                **relative == path && !registered.contains(&name.to_ascii_lowercase())
            });
        let alias = retained.map(|(name, _)| name.clone()).unwrap_or_else(|| {
            series::unique_folder_among(leaf.into(), row.get("season_year"), |candidate| {
                taken.contains(&candidate.to_ascii_lowercase())
                    || (disk.contains(&candidate.to_ascii_lowercase())
                        && !path.eq_ignore_ascii_case(candidate))
            })
        });
        sqlx::query("INSERT INTO series_library_aliases(series_id,folder_name) VALUES(?,?)")
            .bind(id)
            .bind(&alias)
            .execute(&mut *tx)
            .await
            .map_err(error)?;
        taken.insert(alias.to_ascii_lowercase());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.folder_name,s.folder_name FROM series_library_aliases a JOIN series s ON s.id=a.series_id ORDER BY a.folder_name",
    ).fetch_all(&mut *tx).await.map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok((rows.into_iter().collect(), parents))
}

fn save(root: &Path, state: &State) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(error)?;
    serde_json::to_writer(&mut file, state).map_err(error)?;
    file.flush().map_err(error)?;
    file.as_file().sync_all().map_err(error)?;
    file.persist(root.join(STATE)).map_err(error)?;
    fs::File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(error)
}

fn available(root: &Path, relative: &str) -> Result<bool, String> {
    if !media::valid_library_location(root, relative) {
        return Err(format!("Invalid or linked media path: {relative}"));
    }
    match root.join(relative).symlink_metadata() {
        Ok(meta) if meta.is_dir() && !meta.is_symlink() => Ok(true),
        Ok(_) => Err(format!(
            "The media path is no longer a real directory: {relative}"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(error(e)),
    }
}

fn sync_files(
    root: &Path,
    view: &Path,
    source: &Path,
    aliases: Links,
    parents: HashSet<String>,
) -> Result<(), String> {
    let root = root.canonicalize().map_err(error)?;
    if !view.is_absolute()
        || !source.is_absolute()
        || source == Path::new("/")
        || source == view
        || source == root
        || view.starts_with(&root)
        || root.starts_with(view)
    {
        return Err("The Jellyfin view and source mount must be separate absolute paths outside the media root".into());
    }
    if view.is_symlink() {
        return Err("The Jellyfin view directory cannot be a link".into());
    }
    fs::create_dir_all(view).map_err(error)?;
    let actual = view.canonicalize().map_err(error)?;
    if actual.starts_with(&root) || root.starts_with(&actual) {
        return Err("The Jellyfin view overlaps the media root".into());
    }
    let file = view.join(STATE);
    if file.is_symlink() {
        return Err("The Jellyfin view state cannot be a link".into());
    }
    let old = if file.exists() {
        let state: State =
            serde_json::from_slice(&fs::read(&file).map_err(error)?).map_err(error)?;
        if state.version != 1 || state.media_root != root || state.source_root != source {
            return Err("The Jellyfin view belongs to another media root or source mount".into());
        }
        state
    } else {
        State {
            version: 1,
            media_root: root.clone(),
            source_root: source.into(),
            ..State::default()
        }
    };
    for links in std::iter::once(&old.links).chain(old.pending.iter()) {
        for (name, relative) in links {
            if !media::usable_folder_name(name) || !media::usable_library_path(relative) {
                return Err("Invalid path in the Jellyfin view state".into());
            }
        }
    }
    let mut desired = Links::new();
    for (name, relative) in aliases {
        if available(&root, &relative)? {
            desired.insert(name, relative);
        }
    }
    // Retain untracked files, including a title removed from Ryokan with files kept.
    for (name, relative) in old
        .links
        .iter()
        .chain(old.pending.iter().flat_map(|p| p.iter()))
    {
        if available(&root, relative)? {
            desired
                .entry(name.clone())
                .or_insert_with(|| relative.clone());
        }
    }
    for name in directories(&root)? {
        if parents.contains(&name) || !media::usable_folder_name(&name) {
            continue;
        }
        if desired
            .iter()
            .any(|(alias, relative)| alias.eq_ignore_ascii_case(&name) && relative != &name)
        {
            return Err(format!(
                "An untracked folder conflicts with Jellyfin alias {name}"
            ));
        }
        desired.insert(name.clone(), name);
    }
    let mut current = Links::new();
    for entry in fs::read_dir(view).map_err(error)? {
        let entry = entry.map_err(error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "A Jellyfin view name is not valid text")?;
        if name == STATE {
            continue;
        }
        let target = fs::read_link(entry.path())
            .map_err(|_| format!("Unmanaged file in Jellyfin view: {name}"))?;
        let owned = old
            .links
            .iter()
            .chain(old.pending.iter().flat_map(|p| p.iter()))
            .any(|(alias, relative)| {
                (name == *alias || name == TEMP_LINK) && target == source.join(relative)
            });
        if !owned {
            return Err(format!(
                "A Jellyfin view link changed outside Ryokan: {name}"
            ));
        }
        if name == TEMP_LINK {
            fs::remove_file(entry.path()).map_err(error)?;
            continue;
        }
        let relative = target
            .strip_prefix(source)
            .map_err(error)?
            .to_string_lossy()
            .into_owned();
        current.insert(name, relative);
    }
    if old.pending.is_none() && current == desired && old.links == desired {
        return Ok(());
    }
    let state = State {
        version: 1,
        media_root: root,
        source_root: source.into(),
        links: current.clone(),
        pending: Some(desired.clone()),
    };
    save(view, &state)?;
    for (name, relative) in &desired {
        if current.get(name) == Some(relative) {
            continue;
        }
        let temporary = view.join(TEMP_LINK);
        std::os::unix::fs::symlink(source.join(relative), &temporary).map_err(error)?;
        fs::rename(&temporary, view.join(name)).map_err(error)?;
    }
    for name in current.keys().filter(|name| !desired.contains_key(*name)) {
        fs::remove_file(view.join(name)).map_err(error)?;
    }
    save(
        view,
        &State {
            links: desired,
            pending: None,
            ..state
        },
    )
}

pub async fn sync_with_paths(
    db: &SqlitePool,
    root: PathBuf,
    view: PathBuf,
    source: PathBuf,
) -> Result<(), String> {
    let _lock = LOCK.lock().await;
    let (aliases, parents) = aliases(db, &root, &view).await?;
    tokio::task::spawn_blocking(move || sync_files(&root, &view, &source, aliases, parents))
        .await
        .map_err(error)?
}

/// Both variables are required. The source root names the real-media mount
/// inside Jellyfin; it can differ from the media root inside Ryokan.
pub async fn sync(db: &SqlitePool, media_root: &str) -> Result<(), String> {
    let view = super::paths::env_nonblank("RYOKAN_JELLYFIN_VIEW_ROOT");
    let source = super::paths::env_nonblank("RYOKAN_JELLYFIN_VIEW_SOURCE");
    match (view, source) {
        (None, None) => Ok(()),
        (Some(view), Some(source)) if !media_root.is_empty() => {
            sync_with_paths(db, media_root.into(), view.into(), source.into()).await
        }
        _ => Err("Set both Jellyfin view paths and the Ryokan media root".into()),
    }
}

#[cfg(test)]
#[path = "jellyfin_view_tests.rs"]
mod tests;

//! Offline moves of a release directory. A durable marker binds filesystem
//! recovery to the original directory inode and prevents normal app startup.
use super::{
    folder_cleanup::{clear_marker, dir_id, move_directory, publish, sync_dir},
    library_guard::{LibraryLease, sibling_path},
    media, paths,
};
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::{
    error::Error,
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Move {
    operation: String,
    series_id: i64,
    root: PathBuf,
    from: String,
    to: String,
    directory: (u64, u64),
    staging: Option<String>,
    container: Option<(u64, u64)>,
}

fn names(from: &str, to: &str) -> Result<()> {
    if !media::usable_library_path(from) || !media::valid_link_folder(to) || from == to {
        return Err("Choose distinct valid library paths".into());
    }
    Ok(())
}

fn tree(path: &Path) -> Result<()> {
    let device = path.symlink_metadata()?.dev();
    let mut pending = vec![(path.to_owned(), 0)];
    let mut count = 0;
    while let Some((dir, depth)) = pending.pop() {
        if depth > 6 {
            return Err("The release tree exceeds the supported depth".into());
        }
        for entry in dir.read_dir()? {
            count += 1;
            if count > 100_000 {
                return Err("The release tree exceeds the file review limit".into());
            }
            let path = entry?.path();
            let meta = path.symlink_metadata()?;
            if meta.is_symlink() || meta.dev() != device || (!meta.is_file() && !meta.is_dir()) {
                return Err(format!(
                    "Review linked, mounted, or unsupported path {}",
                    path.display()
                )
                .into());
            }
            if meta.is_dir() {
                pending.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

fn location(root: &Path, name: &str) -> Result<PathBuf> {
    if !media::valid_library_location(root, name) {
        return Err("A library path contains a linked or invalid directory".into());
    }
    let path = root.join(name);
    if let Some((parent, _)) = name.split_once('/') {
        for entry in root.read_dir()? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name != parent && name.eq_ignore_ascii_case(parent) {
                return Err("Case-insensitive franchise folder conflict".into());
            }
        }
    }
    for entry in path.parent().unwrap().read_dir().into_iter().flatten() {
        let entry = entry?;
        let other = entry.file_name().to_string_lossy().into_owned();
        let leaf = path.file_name().unwrap().to_string_lossy();
        if other != leaf && other.eq_ignore_ascii_case(&leaf) {
            return Err("Case-insensitive library path conflict".into());
        }
    }
    Ok(path)
}

fn parent(root: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().unwrap();
    if dir_id(parent)?.is_some() {
        return Ok(());
    }
    if parent.parent() != Some(root) {
        return Err("Missing library parent".into());
    }
    let meta = root.metadata()?;
    fs::DirBuilder::new()
        .mode(meta.mode() & 0o7777)
        .create(parent)?;
    let created = parent.metadata()?;
    if (created.uid() != meta.uid() || created.gid() != meta.gid())
        && let Err(e) = std::os::unix::fs::chown(parent, Some(meta.uid()), Some(meta.gid()))
    {
        let _ = fs::remove_dir(parent);
        return Err(e.into());
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    sync_dir(root)?;
    Ok(())
}

async fn owners(pool: &SqlitePool, id: i64, from: &str, to: &str) -> Result<()> {
    let others: Vec<String> = sqlx::query_scalar("SELECT folder_name FROM series WHERE id != ?")
        .bind(id)
        .fetch_all(pool)
        .await?;
    if others.iter().any(|other| {
        !other.is_empty()
            && (media::library_paths_overlap(from, other)
                || media::library_paths_overlap(to, other))
    }) {
        return Err("Another release owns the source, destination, or a parent folder".into());
    }
    Ok(())
}

fn forward(plan: &Move) -> Result<()> {
    let old = location(&plan.root, &plan.from)?;
    let new = location(&plan.root, &plan.to)?;
    if dir_id(&new)? == Some(plan.directory) {
        if plan
            .staging
            .as_ref()
            .is_some_and(|p| plan.root.join(p).exists())
        {
            return Err("Unexpected staged directory".into());
        }
        return Ok(());
    }
    if let Some(staging) = &plan.staging {
        let stage = location(&plan.root, staging)?;
        if dir_id(&old)? == Some(plan.directory) && dir_id(&stage)?.is_none() {
            move_directory(&old, &stage)?;
            sync_dir(old.parent().unwrap())?;
            sync_dir(&plan.root)?;
        }
        if dir_id(&stage)? != Some(plan.directory) {
            return Err("The staged release identity changed".into());
        }
        if let Some(container) = plan.container {
            // Roll a one-off's nested layout back into its original flat folder.
            match dir_id(&new)? {
                Some(current) if current == container && new.read_dir()?.next().is_none() => {
                    fs::remove_dir(&new)?;
                    sync_dir(&plan.root)?;
                }
                None => {}
                _ => {
                    return Err(
                        "The former franchise parent changed or contains other files".into(),
                    );
                }
            }
        }
        parent(&plan.root, &new)?;
        if dir_id(&new)?.is_some() {
            return Err("Destination already exists".into());
        }
        move_directory(&stage, &new)?;
    } else {
        if dir_id(&old)? != Some(plan.directory) || dir_id(&new)?.is_some() {
            return Err("A release directory changed since the move began".into());
        }
        parent(&plan.root, &new)?;
        move_directory(&old, &new)?;
        sync_dir(old.parent().unwrap())?;
    }
    sync_dir(new.parent().unwrap())?;
    sync_dir(&plan.root)?;
    Ok(())
}

async fn apply(pool: &SqlitePool, root: &Path, marker: &Path, id: i64, target: &str) -> Result<()> {
    let current: String = sqlx::query_scalar("SELECT folder_name FROM series WHERE id=?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    let pending = match marker.symlink_metadata() {
        Ok(m) if m.is_file() && !m.is_symlink() => {
            Some(serde_json::from_slice::<Move>(&fs::read(marker)?)?)
        }
        Ok(_) => return Err("Unexpected library recovery marker".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if pending.is_none() && current == target {
        println!("Already at {target}");
        return Ok(());
    }
    let from = pending
        .as_ref()
        .map(|p| p.from.as_str())
        .unwrap_or(&current);
    names(from, target)?;
    owners(pool, id, from, target).await?;
    let bin: String = sqlx::query_scalar("SELECT recycle_bin_path FROM config WHERE id=1")
        .fetch_one(pool)
        .await?;
    if super::recycle::list_entries(&bin).await?.iter().any(|e| {
        e.manifest.series_id == Some(id)
            || Path::new(&e.manifest.original_path).starts_with(root.join(from))
    }) {
        return Err("Resolve this release's recycle-bin entries before moving its folder".into());
    }
    let old = location(root, from)?;
    let new = location(root, target)?;
    let plan = if let Some(plan) = pending {
        if plan.operation != "library-folder"
            || plan.series_id != id
            || plan.root != root
            || plan.to != target
            || (current != plan.from && current != plan.to)
            || plan
                .staging
                .as_ref()
                .is_some_and(|s| !media::usable_folder_name(s) || !s.starts_with(".ryokan-move-"))
        {
            return Err("A different library move needs recovery".into());
        }
        plan
    } else {
        let inward = new.parent() == Some(old.as_path());
        let outward = old.parent() == Some(new.as_path());
        if media::library_paths_overlap(from, target) && !inward && !outward {
            return Err("Unsupported overlapping move".into());
        }
        if dir_id(&new)?.is_some() && !outward {
            return Err("Destination already exists".into());
        }
        let Some(directory) = dir_id(&old)? else {
            if dir_id(&new)?.is_some() {
                return Err("Missing source and occupied destination".into());
            }
            sqlx::query("UPDATE series SET folder_name=? WHERE id=? AND folder_name=?")
                .bind(target)
                .bind(id)
                .bind(&current)
                .execute(pool)
                .await?;
            println!("{id}: {current} -> {target} (future import path)");
            return Ok(());
        };
        tree(&old)?;
        let container = if outward {
            let children: Vec<_> = new.read_dir()?.collect::<std::result::Result<_, _>>()?;
            if children.len() != 1 || children[0].path() != old {
                return Err("The franchise parent contains other files".into());
            }
            dir_id(&new)?
        } else {
            None
        };
        let staging =
            (inward || outward).then(|| format!(".ryokan-move-{id}-{}", std::process::id()));
        if staging
            .as_ref()
            .is_some_and(|p| root.join(p).symlink_metadata().is_ok())
        {
            return Err("A staging path already exists".into());
        }
        let plan = Move {
            operation: "library-folder".into(),
            series_id: id,
            root: root.into(),
            from: current.clone(),
            to: target.into(),
            directory,
            staging,
            container,
        };
        publish(marker, &plan)?;
        plan
    };
    forward(&plan)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let stored: String = sqlx::query_scalar("SELECT folder_name FROM series WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if stored != plan.from && stored != plan.to {
        return Err("The release path changed during the move".into());
    }
    sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
        .bind(&plan.to)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let saved: Move = serde_json::from_slice(&fs::read(marker)?)?;
    if serde_json::to_value(saved)? != serde_json::to_value(&plan)? {
        return Err("The recovery marker changed".into());
    }
    clear_marker(marker)?;
    println!("{id}: {} -> {}", plan.from, plan.to);
    Ok(())
}

async fn set_franchise(pool: &SqlitePool, owner: i64, folder: &str) -> Result<()> {
    if !folder.is_empty()
        && (!media::usable_folder_name(folder) || folder.contains('_') || folder.len() > 255)
    {
        return Err("Choose one readable franchise folder name".into());
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let id: i64 =
        sqlx::query_scalar("SELECT id FROM release_follows WHERE anilist_id=? AND revision > 0")
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
    let selected: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT c.anilist_id,s.folder_name FROM franchise_choices c LEFT JOIN series s ON s.anilist_id=c.anilist_id WHERE c.follow_id=? AND c.selected=1",
    ).bind(id).fetch_all(&mut *tx).await?;
    if selected.is_empty()
        || selected.iter().any(|(_, path)| {
            path.as_ref().is_none_or(|p| {
                !media::usable_library_path(p)
                    || if folder.is_empty() {
                        p.contains('/')
                    } else {
                        !p.starts_with(&format!("{folder}/"))
                    }
            })
        })
    {
        return Err(
            "Move every selected release to its reviewed path before saving the franchise folder"
                .into(),
        );
    }
    sqlx::query("UPDATE release_follows SET library_folder=?,revision=revision+1 WHERE id=? AND library_folder != ?")
        .bind(folder).bind(id).bind(folder).execute(&mut *tx).await?;
    tx.commit().await?;
    println!("AniList {owner}: future releases use {folder}");
    Ok(())
}

pub async fn run(args: &[String]) -> Result<()> {
    let (operation, id, target) = match args {
        [op, id, target] if matches!(op.as_str(), "--move-library-folder" | "--preview-library-folder" | "--set-franchise-library-folder") => (op.as_str(), id.parse::<i64>()?, target.as_str()),
        _ => return Err("Use --preview-library-folder SERIES_ID PATH, --move-library-folder SERIES_ID PATH, or --set-franchise-library-folder OWNER_ANILIST_ID FOLDER".into()),
    };
    let preview = operation == "--preview-library-folder";
    let db = paths::live_db_path().canonicalize()?;
    let lease = if preview {
        None
    } else {
        Some(LibraryLease::acquire_configured(
            &paths::live_db_path(),
            paths::database_url().as_deref(),
        )?)
    };
    if operation == "--set-franchise-library-folder" {
        lease.as_ref().unwrap().require_clean()?;
    }
    let pool =
        SqlitePool::connect_with(SqliteConnectOptions::new().filename(&db).read_only(preview))
            .await?;
    let configured: String = sqlx::query_scalar("SELECT media_root FROM config WHERE id=1")
        .fetch_one(&pool)
        .await?;
    if configured.trim().is_empty() {
        return Err("Set the library media root first".into());
    }
    let root = Path::new(&configured).canonicalize()?;
    match operation {
        "--preview-library-folder" => {
            let from: String = sqlx::query_scalar("SELECT folder_name FROM series WHERE id=?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
            names(&from, target)?;
            owners(&pool, id, &from, target).await?;
            let old = location(&root, &from)?;
            let new = location(&root, target)?;
            if old.is_dir() {
                tree(&old)?;
            }
            println!(
                "{}",
                serde_json::json!({"series_id":id,"from":old,"to":new,"source_exists":old.is_dir(),"destination_exists":new.is_dir()})
            );
            Ok(())
        }
        "--move-library-folder" => {
            let marker = sibling_path(&db, ".library-migration.json");
            if let Ok(meta) = marker.symlink_metadata() {
                if !meta.is_file() || meta.is_symlink() {
                    return Err("Unexpected library recovery marker".into());
                }
                let plan: Move = serde_json::from_slice(&fs::read(&marker)?)?;
                if plan.operation != "library-folder"
                    || plan.series_id != id
                    || plan.to != target
                    || plan.root != root
                {
                    return Err("A different library operation needs recovery".into());
                }
            }
            super::jellyfin_view::sync(&pool, &configured).await?;
            apply(
                &pool,
                &root,
                &sibling_path(&db, ".library-migration.json"),
                id,
                target,
            )
            .await?;
            super::jellyfin_view::sync(&pool, &configured).await?;
            Ok(())
        }
        _ => set_franchise(&pool, id, target).await,
    }
}

#[cfg(test)]
#[path = "library_move_tests.rs"]
mod tests;

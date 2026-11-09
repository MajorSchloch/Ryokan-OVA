//! Offline cleanup of legacy series folder names. Move the whole directory so
//! its contents keep their names and file identities. The normal app holds the
//! same lease and refuses startup while a rename is awaiting recovery.
use super::{
    library_guard::{LibraryLease, sibling_path},
    media, paths,
};
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::{
    error::Error,
    fs,
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(sqlx::FromRow)]
struct Folder {
    id: i64,
    folder_name: String,
    title: String,
    title_english: String,
    title_romaji: String,
    title_native: String,
}

impl Folder {
    fn destination(&self) -> String {
        // Recover punctuation only when the pre-Smart Replace spelling matches
        // the stored title. Preserve any year or numbered collision suffix.
        for title in [
            &self.title,
            &self.title_english,
            &self.title_romaji,
            &self.title_native,
        ] {
            if title.is_empty() {
                continue;
            }
            let legacy: String = title
                .chars()
                .map(|c| {
                    if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let legacy = legacy.trim_matches(|c: char| c == '.' || c.is_whitespace());
            if let Some(suffix) = self.folder_name.strip_prefix(legacy)
                && (suffix.is_empty()
                    || suffix
                        .strip_prefix(" (")
                        .and_then(|s| s.strip_suffix(')'))
                        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
            {
                return media::sanitize_folder_name(&format!("{title}{suffix}"));
            }
        }
        media::sanitize_folder_name(&self.folder_name)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FolderRename {
    series_id: i64,
    root: PathBuf,
    from: String,
    to: String,
    directory: (u64, u64),
}

fn dir_id(path: &Path) -> Result<Option<(u64, u64)>> {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => Ok(Some((m.dev(), m.ino()))),
        Ok(_) => Err(format!("Expected a real directory at {}", path.display()).into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

fn publish(marker: &Path, rename: &FolderRename) -> Result<()> {
    let parent = marker.parent().unwrap();
    let mut draft = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut draft, rename)?;
    draft.flush()?;
    draft.as_file().sync_all()?;
    draft.persist_noclobber(marker)?;
    sync_dir(parent)?;
    Ok(())
}

fn move_directory(old: &Path, new: &Path) -> io::Result<()> {
    // Ordinary POSIX rename can silently replace an existing empty directory.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use nix::fcntl::{AT_FDCWD, RenameFlags, renameat2};
        renameat2(AT_FDCWD, old, AT_FDCWD, new, RenameFlags::RENAME_NOREPLACE).map_err(Into::into)
    }
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let old = CString::new(old.as_os_str().as_bytes())?;
        let new = CString::new(new.as_os_str().as_bytes())?;
        // SAFETY: the two C strings remain alive for the duration of the call.
        if unsafe { nix::libc::renamex_np(old.as_ptr(), new.as_ptr(), nix::libc::RENAME_EXCL) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
    {
        let _ = (old, new);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "No exclusive directory rename on this platform",
        ))
    }
}

fn check_names(from: &str, to: &str) -> Result<()> {
    if !media::usable_folder_name(from)
        || !media::usable_folder_name(to)
        || !from.contains('_')
        || to.contains('_')
        || to.len() > 255
    {
        return Err("Invalid source or destination folder name".into());
    }
    Ok(())
}

fn check_collisions<'a>(
    root: &Path,
    from: &str,
    to: &str,
    others: impl Iterator<Item = &'a Folder>,
) -> Result<()> {
    let forbidden = [from.to_lowercase(), to.to_lowercase()];
    for other in others {
        if forbidden.contains(&other.folder_name.to_lowercase()) {
            return Err("Another series owns the source or destination folder".into());
        }
        if other.folder_name.contains('_') && other.destination().to_lowercase() == forbidden[1] {
            return Err(format!("Series {} would use the same clean folder name", other.id).into());
        }
    }
    for entry in root.read_dir()? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name != from && name != to && forbidden.contains(&name.to_lowercase()) {
            return Err(format!("Case-insensitive folder collision with {name}").into());
        }
    }
    Ok(())
}

fn clear_marker(marker: &Path) -> Result<()> {
    fs::remove_file(marker)?;
    sync_dir(marker.parent().unwrap())?;
    Ok(())
}

async fn apply(pool: &SqlitePool, root: &Path, marker: &Path, id: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let folder: Folder = sqlx::query_as("SELECT id,folder_name,title,title_english,title_romaji,title_native FROM series WHERE id=?")
        .bind(id).fetch_one(&mut *tx).await?;
    let pending: Option<FolderRename> = match marker.symlink_metadata() {
        Ok(m) if m.is_file() => Some(serde_json::from_slice(&fs::read(marker)?)?),
        Ok(_) => return Err("Unexpected library recovery marker".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let (from, to) = if let Some(plan) = &pending {
        if plan.series_id != id
            || plan.root != root
            || (folder.folder_name != plan.from && folder.folder_name != plan.to)
        {
            return Err("A different library operation needs recovery".into());
        }
        (plan.from.clone(), plan.to.clone())
    } else {
        if !folder.folder_name.contains('_') {
            println!("Already clean: {}", folder.folder_name);
            return Ok(());
        }
        (folder.folder_name.clone(), folder.destination())
    };
    check_names(&from, &to)?;
    let bin: String = sqlx::query_scalar("SELECT recycle_bin_path FROM config WHERE id=1")
        .fetch_one(&mut *tx)
        .await?;
    if super::recycle::list_entries(&bin).await?.iter().any(|e| {
        e.manifest.series_id == Some(id)
            || Path::new(&e.manifest.original_path).starts_with(root.join(&from))
    }) {
        return Err("Resolve this series’ recycle-bin entries before renaming its folder".into());
    }
    let others: Vec<Folder> = sqlx::query_as("SELECT id,folder_name,title,title_english,title_romaji,title_native FROM series WHERE id != ?")
        .bind(id).fetch_all(&mut *tx).await?;
    check_collisions(root, &from, &to, others.iter())?;
    let old = root.join(&from);
    let new = root.join(&to);
    let source = dir_id(&old)?;
    let target = dir_id(&new)?;
    let moved_directory = match pending {
        Some(plan) => {
            match (source, target) {
                (Some(inode), None) if inode == plan.directory && folder.folder_name == from => {
                    move_directory(&old, &new)?
                }
                (None, Some(inode)) if inode == plan.directory => {}
                _ => return Err("Folder identity changed since the interrupted rename".into()),
            }
            true
        }
        None if target.is_some() => return Err("Destination already exists".into()),
        None => {
            if let Some(directory) = source {
                publish(
                    marker,
                    &FolderRename {
                        series_id: id,
                        root: root.to_owned(),
                        from: from.clone(),
                        to: to.clone(),
                        directory,
                    },
                )?;
                if let Err(err) = move_directory(&old, &new) {
                    // No rename occurred. Keep normal startup available on e.g. EACCES.
                    clear_marker(marker)?;
                    return Err(err.into());
                }
            }
            source.is_some()
        }
    };
    if moved_directory {
        sync_dir(root)?;
    }
    sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
        .bind(&to)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if moved_directory {
        clear_marker(marker)?;
    }
    println!("{id}: {from} -> {to}");
    Ok(())
}

pub async fn run(args: &[String]) -> Result<()> {
    let id = match args {
        [flag] if flag == "--preview-folder-cleanup" => None,
        [flag, id] if flag == "--apply-folder-cleanup" => Some(id.parse::<i64>()?),
        _ => return Err("Use --preview-folder-cleanup or --apply-folder-cleanup SERIES_ID".into()),
    };
    let db = paths::live_db_path().canonicalize()?;
    let _lease = if id.is_some() {
        Some(LibraryLease::acquire_configured(
            &paths::live_db_path(),
            paths::database_url().as_deref(),
        )?)
    } else {
        None
    };
    let opts = SqliteConnectOptions::new()
        .filename(&db)
        .read_only(id.is_none());
    let pool = SqlitePool::connect_with(opts).await?;
    let configured: String = sqlx::query_scalar("SELECT media_root FROM config WHERE id=1")
        .fetch_one(&pool)
        .await?;
    if configured.trim().is_empty() {
        return Err("Set the library media root first".into());
    }
    let root = Path::new(&configured).canonicalize()?;
    if !root.is_dir() {
        return Err("Media root must be a directory".into());
    }
    if let Some(id) = id {
        return apply(
            &pool,
            &root,
            &sibling_path(&db, ".library-migration.json"),
            id,
        )
        .await;
    }
    let folders: Vec<Folder> = sqlx::query_as("SELECT id,folder_name,title,title_english,title_romaji,title_native FROM series ORDER BY id").fetch_all(&pool).await?;
    for folder in folders.iter().filter(|f| f.folder_name.contains('_')) {
        let from = &folder.folder_name;
        let to = folder.destination();
        let status = (|| -> Result<&str> {
            check_names(from, &to)?;
            check_collisions(
                &root,
                from,
                &to,
                folders.iter().filter(|f| f.id != folder.id),
            )?;
            if dir_id(&root.join(&to))?.is_some() {
                return Err("Destination already exists".into());
            }
            Ok(if dir_id(&root.join(from))?.is_some() {
                "Rename directory"
            } else {
                "Update stored name only"
            })
        })()
        .unwrap_or_else(|err| {
            eprintln!("Series {}: {err}", folder.id);
            "Blocked"
        });
        println!(
            "{}: {} -> {} ({status})",
            folder.id,
            root.join(from).display(),
            root.join(to).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::config,
        test_support::{in_memory_pool, seed_series},
    };

    const TITLE: &str = "Kizumonogatari Part 1: Tekketsu";
    const OLD: &str = "Kizumonogatari Part 1_ Tekketsu (2016)";
    const NEW: &str = "Kizumonogatari Part 1 - Tekketsu (2016)";

    async fn setup() -> (tempfile::TempDir, SqlitePool, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        config::save_config(&db, &config::Config::default())
            .await
            .unwrap();
        let id = seed_series(&db, 9260, TITLE).await;
        sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
            .bind(OLD)
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        (dir, db, id)
    }

    #[tokio::test]
    async fn cleanup_moves_the_whole_directory_and_rejects_an_occupied_destination() {
        let (dir, db, id) = setup().await;
        let root = dir.path();
        let marker = root.join(".marker");
        let old = root.join(OLD);
        let new = root.join(NEW);
        fs::create_dir(&old).unwrap();
        fs::write(old.join("movie.mkv"), b"media").unwrap();
        fs::write(old.join("movie.de.srt"), b"subtitle").unwrap();
        let identity = dir_id(&old).unwrap();
        fs::create_dir(&new).unwrap();
        assert!(apply(&db, root, &marker, id).await.is_err());
        assert!(!marker.exists());
        assert_eq!(dir_id(&old).unwrap(), identity);
        fs::remove_dir(&new).unwrap();
        apply(&db, root, &marker, id).await.unwrap();
        assert_eq!(dir_id(&new).unwrap(), identity);
        assert!(!old.exists() && !marker.exists());
        assert_eq!(fs::read(new.join("movie.de.srt")).unwrap(), b"subtitle");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT folder_name FROM series WHERE id=?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap(),
            NEW
        );
        apply(&db, root, &marker, id).await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_can_resume_after_a_rename_or_after_database_commit() {
        let (dir, db, id) = setup().await;
        let root = dir.path();
        let marker = root.join(".marker");
        let new = root.join(NEW);
        fs::create_dir(&new).unwrap();
        let plan = FolderRename {
            series_id: id,
            root: root.into(),
            from: OLD.into(),
            to: NEW.into(),
            directory: dir_id(&new).unwrap().unwrap(),
        };
        for _ in 0..2 {
            publish(&marker, &plan).unwrap();
            apply(&db, root, &marker, id).await.unwrap();
            assert!(!marker.exists());
            assert_eq!(dir_id(&new).unwrap(), Some(plan.directory));
        }
    }

    #[tokio::test]
    async fn cleanup_of_an_uncreated_folder_preserves_other_owners_and_creates_no_directory() {
        let (dir, db, id) = setup().await;
        let root = dir.path();
        let marker = root.join(".marker");
        let other = seed_series(&db, 99, "Another movie").await;
        sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
            .bind(NEW.to_lowercase())
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        assert!(apply(&db, root, &marker, id).await.is_err());
        sqlx::query("UPDATE series SET folder_name='Elsewhere' WHERE id=?")
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        apply(&db, root, &marker, id).await.unwrap();
        assert!(!root.join(NEW).exists());
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT folder_name FROM series WHERE id=?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap(),
            NEW
        );
    }
}

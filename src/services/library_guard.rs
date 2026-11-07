//! One lifetime lock shared by the app and an offline library migration.
//! Never unlink the lock file: all participants must lock the same inode.
use nix::fcntl::{Flock, FlockArg};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub struct LibraryLease {
    _lock: Flock<File>,
    marker: PathBuf,
}

pub fn sibling_path(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

impl LibraryLease {
    /// Startup's backup/restore path and SQLx must name the same database.
    /// Encoded URLs must not silently lock a different file from the pool.
    pub fn acquire_configured(db: &Path, url: Option<&str>) -> Result<Self, String> {
        if let Some(url) = url {
            let options = url
                .parse::<sqlx::sqlite::SqliteConnectOptions>()
                .map_err(|_| "Library migration: invalid DATABASE_URL".to_string())?;
            if options.get_filename() != db || db == Path::new(":memory:") {
                return Err("Library migration: DATABASE_URL must name the same regular file used by backup and restore; encoded or memory paths are unsupported".into());
            }
        }
        Self::acquire(db)
    }

    /// Acquire before restore, schema migrations, pool creation or CLI reads.
    /// The caller keeps this object until all application work has stopped.
    pub fn acquire(db: &Path) -> Result<Self, String> {
        let error = |e: String| format!("Library migration: {e}");
        if db.is_symlink() {
            return Err(error("a linked database path is unsupported".into()));
        }
        if let Ok(meta) = db.metadata()
            && (!meta.is_file() || meta.nlink() != 1)
        {
            return Err(error(
                "the database must be a regular file with one link".into(),
            ));
        }
        let parent = db
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent
            .canonicalize()
            .map_err(|e| error(format!("cannot resolve database directory: {e}")))?;
        let db = parent.join(
            db.file_name()
                .ok_or_else(|| error("missing database filename".into()))?,
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(sibling_path(&db, ".library.lock"))
            .map_err(|e| error(format!("cannot open library lock: {e}")))?;
        let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| {
            error(format!(
                "the database is in use by another app or migration: {e}"
            ))
        })?;
        Ok(Self {
            _lock: lock,
            marker: sibling_path(&db, ".library-migration.json"),
        })
    }

    /// Any marker, including a malformed file or dangling link, blocks startup.
    /// Only verified transaction completion or recovery can remove it.
    pub fn require_clean(&self) -> Result<(), String> {
        match self.marker.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(format!(
                "Library migration: an unfinished migration blocks startup; recover the journal referenced by {} before starting Ryokan",
                self.marker.display()
            )),
            Err(e) => Err(format!(
                "Library migration: cannot check recovery marker: {e}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_excludes_another_owner_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ryokan.db");
        let lease = LibraryLease::acquire(&db).unwrap();
        lease.require_clean().unwrap();
        assert!(LibraryLease::acquire(&db).is_err());
        drop(lease);
        LibraryLease::acquire(&db).unwrap().require_clean().unwrap();
        assert!(!db.exists(), "the guard must not create a database");
    }

    #[test]
    fn malformed_and_linked_markers_block_startup_without_being_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ryokan.db");
        let marker = sibling_path(&db, ".library-migration.json");
        let lease = LibraryLease::acquire(&db).unwrap();
        std::fs::write(&marker, "incomplete").unwrap();
        assert!(lease.require_clean().is_err());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "incomplete");
        std::fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &marker).unwrap();
        assert!(lease.require_clean().is_err());
    }

    #[test]
    fn linked_lock_and_database_paths_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ryokan.db");
        let target = tmp.path().join("other");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &db).unwrap();
        assert!(LibraryLease::acquire(&db).is_err());
        std::fs::remove_file(&db).unwrap();
        std::fs::hard_link(&target, &db).unwrap();
        assert!(LibraryLease::acquire(&db).is_err());
        std::fs::remove_file(&db).unwrap();
        std::os::unix::fs::symlink(&target, sibling_path(&db, ".library.lock")).unwrap();
        assert!(LibraryLease::acquire(&db).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    }

    #[test]
    fn encoded_urls_cannot_lock_another_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ryokan.db");
        let url = format!("sqlite://{}?mode=rwc", db.display());
        LibraryLease::acquire_configured(&db, Some(&url)).unwrap();
        let encoded = tmp.path().join("ryokan%20copy.db");
        let url = format!("sqlite://{}", encoded.display());
        assert!(LibraryLease::acquire_configured(&encoded, Some(&url)).is_err());
        assert!(
            LibraryLease::acquire_configured(Path::new(":memory:"), Some("sqlite::memory:"))
                .is_err()
        );
        assert!(!db.exists());
    }
}

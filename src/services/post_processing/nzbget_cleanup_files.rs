//! Remove only download videos whose bytes are still present in the library.

use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use sha2::{Digest, Sha256};

use crate::services::media_info::FileIdentity;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct DownloadCopy {
    pub source: PathBuf,
    pub library: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    identity: FileIdentity,
    #[cfg(unix)]
    change_time: (i64, i64),
}

impl FileStamp {
    fn from_metadata(meta: &Metadata) -> Result<Self, String> {
        let identity = FileIdentity::from_metadata(meta).map_err(|error| error.to_string())?;
        Ok(Self {
            identity,
            #[cfg(unix)]
            change_time: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

struct VerifiedCopy {
    copy: DownloadCopy,
    source: PathStamp,
    library: PathStamp,
}

#[derive(Debug, PartialEq, Eq)]
struct PathStamp {
    parent: Option<(u64, u64)>,
    file: Option<FileStamp>,
}

/// The paths and identities observed during verification. The caller can check
/// durable import ownership and native job state before consuming this batch.
pub(super) struct VerifiedBatch {
    copies: Vec<VerifiedCopy>,
}

impl VerifiedBatch {
    pub(super) fn sources_absent(&self) -> bool {
        self.copies.iter().all(|pair| pair.source.file.is_none())
    }

    /// Bind the proof to the import receipt, in the same order as the input
    /// pairs. An absent source is valid when retrying after an earlier unlink.
    pub(super) fn matches_import(&self, identities: &[(&str, &str)]) -> bool {
        self.copies.len() == identities.len()
            && self.copies.iter().zip(identities).all(|(pair, expected)| {
                pair.source
                    .file
                    .as_ref()
                    .is_none_or(|source| source.identity.cache_key() == expected.0)
                    && pair
                        .library
                        .file
                        .as_ref()
                        .is_some_and(|library| library.identity.cache_key() == expected.1)
            })
    }

    /// Only metadata is re-read here. All content proof happened in prepare.
    pub(super) async fn remove(self) -> Result<usize, String> {
        tokio::task::spawn_blocking(move || remove_verified(&self.copies))
            .await
            .map_err(|error| format!("Download copy removal failed: {error}"))?
    }
}

pub(super) async fn prepare_cleanup(
    source_root: &Path,
    media_root: &Path,
    copies: &[DownloadCopy],
) -> Result<VerifiedBatch, String> {
    let source_root = source_root.to_path_buf();
    let media_root = media_root.to_path_buf();
    let copies = copies.to_vec();
    tokio::task::spawn_blocking(move || verify_copies(&source_root, &media_root, &copies))
        .await
        .map_err(|error| format!("Download copy check failed: {error}"))?
}

fn verify_copies(
    source_root: &Path,
    media_root: &Path,
    copies: &[DownloadCopy],
) -> Result<VerifiedBatch, String> {
    let source_directory = check_directory(source_root)?;
    let media_directory = check_directory(media_root)?;
    if source_root.starts_with(media_root)
        || media_root.starts_with(source_root)
        || (source_directory.is_some() && source_directory == media_directory)
    {
        return Err("Download and library directories overlap".into());
    }
    let mut source_paths = HashSet::new();
    let mut library_paths = HashSet::new();
    let mut source_entries = HashSet::new();
    let mut library_entries = HashSet::new();
    let mut verified = Vec::with_capacity(copies.len());
    for copy in copies {
        check_video_path(&copy.source, source_root)?;
        check_video_path(&copy.library, media_root)?;
        if !source_paths.insert(&copy.source) || !library_paths.insert(&copy.library) {
            return Err("Download copy list repeats a source or library path".into());
        }
        let source = path_stamp(&copy.source)?;
        let library = path_stamp(&copy.library)?;
        // Bind mounts can give one directory entry two canonical paths.
        if source
            .parent
            .is_some_and(|parent| !source_entries.insert((parent, copy.source.file_name())))
            || library
                .parent
                .is_some_and(|parent| !library_entries.insert((parent, copy.library.file_name())))
        {
            return Err("Download copy list repeats a physical directory entry".into());
        }
        let retained = library
            .file
            .as_ref()
            .ok_or_else(|| format!("Library copy is missing: {}", copy.library.display()))?;
        if let Some(original) = &source.file {
            let same_inode = original.identity.device.is_some()
                && original.identity.inode.is_some()
                && original.identity == retained.identity;
            if !same_inode
                && (original.identity.size != retained.identity.size
                    || hash_file(&copy.source, original)? != hash_file(&copy.library, retained)?)
            {
                return Err(format!(
                    "Download and library copies differ: {}",
                    copy.source.display()
                ));
            }
        }
        verified.push(VerifiedCopy {
            copy: copy.clone(),
            source,
            library,
        });
    }
    if !source_entries.is_disjoint(&library_entries) {
        return Err("A download and library path name the same physical directory entry".into());
    }
    Ok(VerifiedBatch { copies: verified })
}

fn check_directory(path: &Path) -> Result<Option<(u64, u64)>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if !path.is_absolute()
        || !metadata.is_dir()
        || fs::canonicalize(path).map_err(|error| path_error(path, error))? != path
    {
        return Err(format!("Directory must be canonical: {}", path.display()));
    }
    #[cfg(unix)]
    return Ok(Some((metadata.dev(), metadata.ino())));
    #[cfg(not(unix))]
    Ok(None)
}

fn check_video_path(path: &Path, root: &Path) -> Result<(), String> {
    if !path.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    }) || !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(super::is_video_file)
    {
        return Err(format!(
            "Video path is outside its directory or invalid: {}",
            path.display()
        ));
    }
    Ok(())
}

fn path_stamp(path: &Path) -> Result<PathStamp, String> {
    let parent = path.parent().expect("validated video path has a parent");
    let parent = check_directory(parent)?;
    let file = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Some(FileStamp::from_metadata(&metadata)?),
        Ok(_) => return Err(format!("Expected a regular video: {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(path_error(path, error)),
    };
    Ok(PathStamp { parent, file })
}

fn hash_file(path: &Path, expected: &FileStamp) -> Result<[u8; 32], String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .map_err(|error| path_error(path, error))?;
    check_open_file(&file, path, expected)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| path_error(path, error))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    check_open_file(&file, path, expected)?;
    Ok(hash.finalize().into())
}

fn check_open_file(file: &File, path: &Path, expected: &FileStamp) -> Result<(), String> {
    let metadata = file.metadata().map_err(|error| path_error(path, error))?;
    if !metadata.is_file() || FileStamp::from_metadata(&metadata)? != *expected {
        return Err(format!("Video changed during proof: {}", path.display()));
    }
    Ok(())
}

fn check_unchanged(pair: &VerifiedCopy) -> Result<(), String> {
    if path_stamp(&pair.copy.source)? != pair.source
        || path_stamp(&pair.copy.library)? != pair.library
    {
        return Err(format!(
            "Download or library video changed during proof: {}",
            pair.copy.source.display()
        ));
    }
    Ok(())
}

fn remove_verified(verified: &[VerifiedCopy]) -> Result<usize, String> {
    for pair in verified {
        check_unchanged(pair)?;
    }
    let mut removed = 0;
    for pair in verified.iter().filter(|pair| pair.source.file.is_some()) {
        unlink(pair)?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(unix)]
fn unlink(pair: &VerifiedCopy) -> Result<(), String> {
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let path = &pair.copy.source;
    let parent = path.parent().expect("validated video path has a parent");
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|error| path_error(parent, error))?;
    check_unchanged(pair)?;
    let opened = dir.metadata().map_err(|error| path_error(parent, error))?;
    if Some((opened.dev(), opened.ino())) != pair.source.parent {
        return Err(format!("Download directory changed: {}", parent.display()));
    }
    let name = path.file_name().expect("validated video path has a name");
    unlinkat(&dir, name, UnlinkatFlags::NoRemoveDir).map_err(|error| path_error(path, error))
}

#[cfg(not(unix))]
fn unlink(pair: &VerifiedCopy) -> Result<(), String> {
    check_unchanged(pair)?;
    fs::remove_file(&pair.copy.source).map_err(|error| path_error(&pair.copy.source, error))
}

fn path_error(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{}: {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn verify_and_remove(
        source_root: &Path,
        media_root: &Path,
        copies: &[DownloadCopy],
    ) -> Result<usize, String> {
        prepare_cleanup(source_root, media_root, copies)
            .await?
            .remove()
            .await
    }

    struct Fixture {
        _root: tempfile::TempDir,
        downloads: PathBuf,
        library: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let base = root.path().canonicalize().unwrap();
            let downloads = base.join("downloads");
            let library = base.join("library");
            fs::create_dir(&downloads).unwrap();
            fs::create_dir(&library).unwrap();
            Self {
                _root: root,
                downloads,
                library,
            }
        }

        fn pair(&self, name: &str, library_bytes: Option<&[u8]>) -> DownloadCopy {
            let source = self.downloads.join(name);
            let library = self.library.join(name);
            fs::write(&source, b"original video").unwrap();
            if let Some(library_bytes) = library_bytes {
                fs::write(&library, library_bytes).unwrap();
            } else {
                fs::hard_link(&source, &library).unwrap();
            }
            DownloadCopy { source, library }
        }
    }

    #[tokio::test]
    async fn removes_verified_copy_and_hardlink_and_retries_after_success() {
        let fixture = Fixture::new();
        let copies = [
            fixture.pair("copy.mkv", Some(b"original video")),
            fixture.pair("link.mkv", None),
        ];
        let sidecar = fixture.downloads.join("copy.en.srt");
        fs::write(&sidecar, b"subtitles").unwrap();
        assert_eq!(
            verify_and_remove(&fixture.downloads, &fixture.library, &copies).await,
            Ok(2)
        );
        for pair in &copies {
            assert!(!pair.source.exists());
            assert_eq!(fs::read(&pair.library).unwrap(), b"original video");
        }
        assert!(sidecar.exists());
        assert_eq!(
            verify_and_remove(&fixture.downloads, &fixture.library, &copies).await,
            Ok(0)
        );
    }

    #[tokio::test]
    async fn preflights_all_files_and_preserves_same_size_different_source() {
        let fixture = Fixture::new();
        let copies = [
            fixture.pair("exact.mkv", None),
            fixture.pair("different.mkv", Some(b"different data")),
        ];
        assert!(
            verify_and_remove(&fixture.downloads, &fixture.library, &copies)
                .await
                .is_err()
        );
        assert!(copies.iter().all(|pair| pair.source.exists()));
    }

    #[tokio::test]
    async fn library_replacement_after_proof_preserves_the_entire_set() {
        let fixture = Fixture::new();
        let copies = [
            fixture.pair("unchanged.mkv", None),
            fixture.pair("changed.mkv", None),
        ];
        let verified = prepare_cleanup(&fixture.downloads, &fixture.library, &copies)
            .await
            .unwrap();
        let replacement = fixture.library.join("new.mkv");
        fs::write(&replacement, b"transcoded video").unwrap();
        fs::rename(&replacement, &copies[1].library).unwrap();
        assert!(verified.remove().await.is_err());
        assert!(copies.iter().all(|pair| pair.source.exists()));
    }

    #[tokio::test]
    async fn equal_new_files_do_not_match_the_old_import_receipt() {
        let fixture = Fixture::new();
        let pair = fixture.pair("replace.mkv", None);
        let original = FileStamp::from_metadata(&fs::metadata(&pair.source).unwrap())
            .unwrap()
            .identity
            .cache_key();
        let copies = [pair];
        let expected = [(original.as_str(), original.as_str())];
        let verified = prepare_cleanup(&fixture.downloads, &fixture.library, &copies)
            .await
            .unwrap();
        assert!(verified.matches_import(&expected));

        let replacement = fixture.pair("replacement.mkv", None);
        fs::rename(replacement.source, &copies[0].source).unwrap();
        fs::rename(replacement.library, &copies[0].library).unwrap();
        let verified = prepare_cleanup(&fixture.downloads, &fixture.library, &copies)
            .await
            .unwrap();
        assert!(!verified.matches_import(&expected));
        assert!(copies[0].source.exists());
    }

    #[tokio::test]
    async fn rejects_overlapping_roots_and_library_paths() {
        let fixture = Fixture::new();
        let pair = fixture.pair("same.mkv", None);
        let overlap = fixture.downloads.join("overlap");
        fs::create_dir(&overlap).unwrap();
        assert!(
            verify_and_remove(&fixture.downloads, &overlap, std::slice::from_ref(&pair))
                .await
                .is_err()
        );
        let outside = DownloadCopy {
            source: pair.source.clone(),
            library: pair.source.clone(),
        };
        assert!(
            verify_and_remove(&fixture.downloads, &fixture.library, &[outside])
                .await
                .is_err()
        );
        assert!(pair.source.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_parent_and_missing_retained_video_on_retry() {
        let fixture = Fixture::new();
        let pair = fixture.pair("exact.mkv", None);
        let alias = fixture.downloads.join("alias");
        std::os::unix::fs::symlink(&fixture.downloads, &alias).unwrap();
        let through_alias = DownloadCopy {
            source: alias.join("exact.mkv"),
            library: pair.library.clone(),
        };
        assert!(
            verify_and_remove(&fixture.downloads, &fixture.library, &[through_alias])
                .await
                .is_err()
        );
        assert!(pair.source.exists());
        fs::remove_file(&pair.source).unwrap();
        fs::remove_file(&pair.library).unwrap();
        assert!(
            verify_and_remove(&fixture.downloads, &fixture.library, &[pair])
                .await
                .is_err()
        );
    }
}

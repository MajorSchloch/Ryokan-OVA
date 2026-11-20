//! Filesystem primitives for the recycle bin: companion discovery,
//! rename-with-cross-fs-fallback moves, sizes, entry ids. Everything in
//! here is synchronous and expected to run inside `spawn_blocking`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Suffix appended to an in-flight cross-filesystem copy so a partially
/// written file can never be observed at the final name. Same convention
/// as `post_processing::do_file_op`.
pub(super) const TMP_SUFFIX: &str = ".ryokan-tmp";

/// 8 hex chars = 32 bits. Two recycle actions on the same day for the
/// same series can't collide without astronomically bad luck, and the
/// caller retries on `AlreadyExists` anyway.
pub(super) fn new_entry_id() -> String {
    hex::encode(rand::random::<[u8; 4]>())
}

/// Entry ids come back from the browser as path segments; only the exact
/// shape we mint is accepted so a crafted id can't walk the tree.
pub fn is_valid_entry_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Files that travel with an episode: everything in the same directory
/// whose name is `<stem>` followed by `.` or `-`. The separator
/// requirement is what keeps `Show - S01E07` from sweeping up
/// `Show - S01E070.mkv`, while still matching Jellyfin's language-tagged
/// subtitle convention (`Show - S01E07.en.srt`) and `-thumb.jpg`. The
/// main file itself is excluded; only regular files qualify.
pub fn companions(main: &Path) -> Vec<PathBuf> {
    let Some(parent) = main.parent() else {
        return Vec::new();
    };
    let Some(stem) = main.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let Some(main_name) = main.file_name() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name() != main_name)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            let name = e.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            match name.strip_prefix(stem) {
                Some(rest) => rest.starts_with('.') || rest.starts_with('-'),
                None => false,
            }
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Move `src` to `dst`. Same-filesystem rename is the happy path (atomic,
/// instant, hardlinks preserved). On `CrossesDevices` (or when
/// `force_copy` is set, which tests use to exercise the fallback without
/// a second filesystem) fall back to copy-then-unlink through a
/// `.ryokan-tmp` sibling so an interrupted copy leaves nothing at `dst`.
pub(super) fn move_path(src: &Path, dst: &Path, force_copy: bool) -> io::Result<()> {
    #[cfg(test)]
    let paused_copy = super::test_support::copy_for(src);
    #[cfg(test)]
    let force_copy = force_copy || paused_copy.is_some();
    if !force_copy {
        match fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(e),
        }
    }
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        copy_dir_via_tmp(
            src,
            dst,
            #[cfg(test)]
            paused_copy.as_deref(),
        )?;
        fs::remove_dir_all(src)
    } else {
        copy_file_via_tmp(src, dst)?;
        #[cfg(test)]
        if let Some(copy) = paused_copy {
            copy.pause();
        }
        fs::remove_file(src)
    }
}

fn tmp_name(dst: &Path) -> PathBuf {
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(TMP_SUFFIX);
    PathBuf::from(tmp)
}

/// Sonarr-style transfer verification: a cross-filesystem copy is only
/// trusted when the destination is exactly as long as the source. A short
/// copy (disk full, connection dropped) is removed and reported instead of
/// being renamed into place.
pub(super) fn verify_size(src: &Path, dst: &Path) -> io::Result<()> {
    let expected = fs::metadata(src)?.len();
    let actual = fs::metadata(dst)?.len();
    if expected != actual {
        return Err(io::Error::other(format!(
            "size mismatch after copy: {} is {} bytes, {} is {} bytes",
            src.display(),
            expected,
            dst.display(),
            actual
        )));
    }
    Ok(())
}

fn copy_file_via_tmp(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = tmp_name(dst);
    if let Err(e) = fs::copy(src, &tmp).and_then(|_| verify_size(src, &tmp)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, dst) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileStamp {
    fn read(meta: fs::Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            size: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }
}

// A directory is represented by None. Retain relative names as well as
// lengths, since equal totals can hide a removed or replaced file.
fn tree_snapshot(root: &Path) -> io::Result<BTreeMap<PathBuf, Option<FileStamp>>> {
    let mut snapshot = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let relative = relative.join(entry.file_name());
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(relative.clone());
                snapshot.insert(relative, None);
            } else if meta.is_file() {
                snapshot.insert(relative, Some(FileStamp::read(meta)?));
            }
        }
    }
    Ok(snapshot)
}

fn copy_dir_via_tmp(
    src: &Path,
    dst: &Path,
    #[cfg(test)] paused_copy: Option<&super::test_support::CopyBarrier>,
) -> io::Result<()> {
    let before = tree_snapshot(src)?;
    let tmp = tmp_name(dst);
    fs::create_dir(&tmp)?;
    let result = (|| {
        copy_dir_recursive(src, &tmp)?;
        #[cfg(test)]
        if let Some(copy) = paused_copy {
            copy.pause();
        }
        let copied = tree_snapshot(&tmp)?;
        if before
            .iter()
            .map(|(path, stamp)| (path, stamp.as_ref().map(|s| s.size)))
            .ne(copied
                .iter()
                .map(|(path, stamp)| (path, stamp.as_ref().map(|s| s.size))))
        {
            return Err(io::Error::other(format!(
                "files or sizes differ after copying {}",
                src.display()
            )));
        }
        if before != tree_snapshot(src)? {
            return Err(io::Error::other(format!(
                "folder changed during copy: {}. Retry when its files are idle",
                src.display()
            )));
        }
        fs::rename(&tmp, dst)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            fs::create_dir(&to)?;
            copy_dir_recursive(&entry.path(), &to)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &to)?;
            verify_size(&entry.path(), &to)?;
        }
        // Symlinks are skipped on purpose: a recycled series folder
        // pointing back into the downloads dir shouldn't drag the
        // target along on a cross-fs move, and rename-mode moves keep
        // the link as-is anyway.
    }
    Ok(())
}

/// Bytes under `path` (file size, or recursive sum for a directory).
/// Best-effort: unreadable entries count as zero.
pub(super) fn path_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            if ty.is_dir() {
                stack.push(entry.path());
            } else if ty.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Remove an empty directory, ignoring every failure (non-empty,
/// permission, already gone). Used to tidy date buckets after a restore
/// or per-entry purge empties them.
pub(super) fn remove_dir_if_empty(path: &Path) {
    let _ = fs::remove_dir(path);
}

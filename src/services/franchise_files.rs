//! Read-only folder and file observations for a franchise proposal.
//! This review report cannot execute a move.
use super::media;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Release {
    pub anilist_id: i64,
    pub source: Option<String>,
    pub destination: String,
    pub episodes: Option<i32>,
}

#[derive(Default, Serialize)]
pub struct Review {
    pub issues: Vec<String>,
    pub releases: Vec<ReleaseFiles>,
}

#[derive(Default, Serialize)]
pub struct ReleaseFiles {
    pub anilist_id: i64,
    pub status: String,
    pub media_files: usize,
    pub episodes: usize,
    pub missing: Vec<i32>,
    pub issues: Vec<String>,
    pub files: Vec<FilePath>,
}

#[derive(Serialize)]
pub struct FilePath {
    pub from: PathBuf,
    pub to: PathBuf,
    pub kind: &'static str,
    pub size: u64,
}

fn children(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|e| e.path()))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| format!("Cannot read {}. {e}", dir.display()))?;
    paths.sort();
    Ok(paths)
}

fn component(path: &Path) -> bool {
    path.to_str().is_some_and(media::usable_folder_name)
}

pub fn inspect(root: &Path, franchise: &Path, releases: &[Release], owners: &[String]) -> Review {
    let mut review = Review::default();
    if !franchise.strip_prefix(root).is_ok_and(component) {
        review
            .issues
            .push("The franchise destination must be directly inside the media root.".into());
        return review;
    }
    let root_entries = match children(root) {
        Ok(entries) => entries,
        Err(e) => {
            review.issues.push(e);
            return review;
        }
    };
    let target_name = franchise
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let relative_root = franchise.strip_prefix(root).unwrap().to_string_lossy();
    let known_root = owners
        .iter()
        .any(|path| path.starts_with(&format!("{relative_root}/")));
    if !known_root
        && root_entries
            .iter()
            .any(|path| path.file_name().unwrap().to_string_lossy().to_lowercase() == target_name)
    {
        review.issues.push(format!(
            "A franchise destination already exists at {}. Review it before a move.",
            franchise.display()
        ));
    }
    let mut destinations = BTreeSet::new();
    for release in releases {
        let mut found = ReleaseFiles {
            anilist_id: release.anilist_id,
            ..Default::default()
        };
        if !component(Path::new(&release.destination))
            || !destinations.insert(release.destination.to_lowercase())
        {
            found.status = "Needs review".into();
            found
                .issues
                .push("Invalid or conflicting release destination.".into());
            review.releases.push(found);
            continue;
        }
        let Some(source) = &release.source else {
            found.status = "Outside library".into();
            review.releases.push(found);
            continue;
        };
        if !media::valid_library_location(root, source)
            || owners
                .iter()
                .filter(|other| other.to_lowercase() == source.to_lowercase())
                .count()
                != 1
        {
            found.status = "Needs review".into();
            found
                .issues
                .push("The source folder is invalid or shared by library entries.".into());
            review.releases.push(found);
            continue;
        }
        let source = root.join(source);
        let destination = franchise.join(&release.destination);
        if source != destination && destination.symlink_metadata().is_ok() {
            found.status = "Needs review".into();
            found
                .issues
                .push("The release destination is already occupied.".into());
            review.releases.push(found);
            continue;
        }
        match source.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                found.status = "No current folder".into()
            }
            Err(e) => {
                found.status = "Needs review".into();
                found.issues.push(e.to_string());
            }
            Ok(metadata) if !metadata.is_dir() || metadata.is_symlink() => {
                found.status = "Needs review".into();
                found
                    .issues
                    .push("The source is a linked folder or is not a directory.".into());
            }
            Ok(_) => {
                let mut slots = BTreeSet::new();
                let mut names = BTreeSet::new();
                let mut pending = vec![(source.clone(), 0)];
                found.status = "Scanned".into();
                while let Some((dir, depth)) = pending.pop() {
                    if depth > 6 || found.files.len() >= 10_000 {
                        found
                            .issues
                            .push("The file preview reached its directory or file limit.".into());
                        break;
                    }
                    let entries = match children(&dir) {
                        Ok(entries) => entries,
                        Err(e) => {
                            found.issues.push(e);
                            continue;
                        }
                    };
                    for path in entries {
                        if found.files.len() >= 10_000 {
                            found
                                .issues
                                .push("The file preview reached its file limit.".into());
                            break;
                        }
                        let metadata = match path.symlink_metadata() {
                            Ok(m) => m,
                            Err(e) => {
                                found
                                    .issues
                                    .push(format!("Cannot inspect {}. {e}", path.display()));
                                continue;
                            }
                        };
                        let relative = path.strip_prefix(&source).unwrap();
                        let Some(name) = relative.to_str() else {
                            found
                                .issues
                                .push("A path cannot be represented as text.".into());
                            continue;
                        };
                        if !names.insert(name.to_lowercase()) {
                            found
                                .issues
                                .push(format!("Case-insensitive path conflict at {name}."));
                        }
                        let kind = if metadata.is_symlink() {
                            found
                                .issues
                                .push(format!("Linked path needs review. {name}"));
                            "link"
                        } else if metadata.is_dir() {
                            pending.push((path.clone(), depth + 1));
                            "directory"
                        } else if metadata.is_file() {
                            if media::is_library_file(&path) {
                                found.media_files += 1;
                                match media::parse_episode_file(&path, &source) {
                                    Some(ep)
                                        if !ep.is_special
                                            && ep.is_own_season()
                                            && ep.episode_number > 0 =>
                                    {
                                        for n in ep.episodes() {
                                            if !slots.insert(n) {
                                                found.issues.push(format!(
                                                    "More than one file covers episode {n}."
                                                ));
                                            }
                                        }
                                        if release.episodes.is_some_and(|n| ep.episode_last > n) {
                                            found.issues.push(format!(
                                                "Episode number exceeds the stored total. {name}"
                                            ));
                                        }
                                    }
                                    _ => found.issues.push(format!(
                                        "Review this media file's episode identity. {name}"
                                    )),
                                }
                            }
                            "file"
                        } else {
                            found.issues.push(format!("Unsupported file type. {name}"));
                            "unsupported"
                        };
                        found.files.push(FilePath {
                            to: franchise.join(&release.destination).join(relative),
                            from: path,
                            kind,
                            size: metadata.len(),
                        });
                    }
                }
                found.episodes = slots.len();
                if found.issues.is_empty() {
                    if let Some(total) = release.episodes.filter(|n| *n > 0) {
                        found.missing = (1..=total).filter(|n| !slots.contains(n)).collect();
                    }
                } else {
                    found.status = "Needs review".into();
                }
                found.files.sort_by(|a, b| a.from.cmp(&b.from));
            }
        }
        review.releases.push(found);
    }
    review
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(source: &str) -> Release {
        Release {
            anilist_id: 1,
            source: Some(source.into()),
            destination: "2000 - TV - Show [AniList 1]".into(),
            episodes: Some(4),
        }
    }

    #[test]
    fn franchise_files_preserve_ranges_sidecars_and_report_gaps() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("Show/Season 01")).unwrap();
        for name in [
            "Show - S01E01-E02.mkv",
            "Show - S01E04.mkv",
            "Show - S01E04.nfo",
            "Show - S01E04.en.ass",
        ] {
            fs::write(root.join("Show/Season 01").join(name), []).unwrap();
        }
        let report = inspect(
            root,
            &root.join("Franchise"),
            &[release("Show")],
            &["Show".into()],
        );
        let row = &report.releases[0];
        assert!(report.issues.is_empty() && row.issues.is_empty());
        assert_eq!((row.media_files, row.episodes), (2, 3));
        assert_eq!(row.missing, [3]);
        assert_eq!(row.files.len(), 5);
        assert!(row.files.iter().any(|f| {
            f.to.ends_with("2000 - TV - Show [AniList 1]/Season 01/Show - S01E04.en.ass")
        }));
        assert!(!root.join("Franchise").exists());
        assert!(root.join("Show/Season 01/Show - S01E04.nfo").is_file());
    }

    #[test]
    fn franchise_files_distinguish_missing_shared_and_colliding_folders() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("FRANCHISE")).unwrap();
        let report = inspect(
            root,
            &root.join("Franchise"),
            &[release("Absent")],
            &["Absent".into()],
        );
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.releases[0].status, "No current folder");
        let report = inspect(
            root,
            &root.join("New"),
            &[release("Absent")],
            &["Absent".into(), "ABSENT".into()],
        );
        assert_eq!(report.releases[0].status, "Needs review");
    }

    #[cfg(unix)]
    #[test]
    fn franchise_files_flag_links_and_duplicate_episode_coverage() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("Show")).unwrap();
        fs::write(root.join("Show/A - S01E01.mkv"), []).unwrap();
        fs::write(root.join("Show/B - S01E01.mkv"), []).unwrap();
        std::os::unix::fs::symlink(root, root.join("Show/Loop")).unwrap();
        let report = inspect(
            root,
            &root.join("New"),
            &[release("Show")],
            &["Show".into()],
        );
        let row = &report.releases[0];
        assert_eq!(row.status, "Needs review");
        assert!(row.issues.iter().any(|s| s.contains("Linked path")));
        assert!(row.issues.iter().any(|s| s.contains("More than one file")));
    }
}

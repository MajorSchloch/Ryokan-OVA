//! Read-only summaries of managed episode files and current requirement facts.
use crate::{
    models::{config, series},
    services::{language_preferences, media, media_info, media_requirements},
};
use sqlx::SqlitePool;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Debug, Default, serde::Serialize, utoipa::ToSchema)]
pub struct FileGate {
    pub state: &'static str,
    pub label: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct FileSummary {
    pub files: usize,
    pub episodes: usize,
    pub bytes: u64,
    paths: HashMap<std::path::PathBuf, u64>,
    pub complete_scan: bool,
    pub target: usize,
    pub minimum: usize,
    pub unmet: usize,
    pub unverified: usize,
    pub disabled: usize,
    pub gates: HashMap<i32, FileGate>,
}
impl FileSummary {
    pub fn size(&self) -> String {
        if self.complete_scan {
            if self.bytes < 1024 {
                format!("{} B", self.bytes)
            } else {
                media::format_size(self.bytes)
            }
        } else {
            "Unavailable".into()
        }
    }
    pub fn quality(&self) -> String {
        if !self.complete_scan {
            return "File scan unavailable".into();
        }
        if self.files == 0 {
            return "No episode files".into();
        }
        format!(
            "{} target · {} minimum only · {} unmet · {} unverified{}",
            self.target,
            self.minimum,
            self.unmet,
            self.unverified,
            if self.disabled > 0 {
                format!(" · {} checks disabled", self.disabled)
            } else {
                String::new()
            }
        )
    }
    pub fn add(&mut self, other: &Self) {
        self.complete_scan &= other.complete_scan;

        self.episodes += other.episodes;
        for (path, bytes) in &other.paths {
            self.paths.entry(path.clone()).or_insert(*bytes);
        }
        self.bytes = self.paths.values().sum();
        self.files = self.paths.len();
        self.target += other.target;
        self.minimum += other.minimum;
        self.unmet += other.unmet;
        self.unverified += other.unverified;
        self.disabled += other.disabled;
    }
}

pub async fn load(
    db: &SqlitePool,
    root: &str,
    library: &[series::Series],
) -> Result<HashMap<i64, FileSummary>, sqlx::Error> {
    let cfg = config::get_config(db).await?.unwrap_or_default();
    let policies = language_preferences::resolve_snapshot(
        db,
        &cfg,
        &library.iter().map(|s| s.id).collect::<Vec<_>>(),
    )
    .await
    .map_err(sqlx::Error::Protocol)?;
    let probes: HashMap<String, (Option<String>, String)> =
        sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT path,identity,probe_json FROM media_probe_cache",
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|(p, i, j)| (p, (i, j)))
        .collect();
    let mut result = HashMap::new();
    for row in library {
        let mut summary = FileSummary::default();
        let files = match media::scan_series_folder_checked(root, &row.folder_name).await {
            Ok(files) => files,
            Err(_)
                if !root.is_empty()
                    && Path::new(root).is_dir()
                    && media::valid_library_location(Path::new(root), &row.folder_name)
                    && !Path::new(root).join(&row.folder_name).exists() =>
            {
                Vec::new()
            }
            Err(_) => {
                result.insert(row.id, summary);
                continue;
            }
        };
        summary.complete_scan = true;
        let media_paths = match logical_paths(root, &row.folder_name, &cfg.recycle_bin_path).await {
            Ok(p) => p,
            Err(_) => {
                summary.complete_scan = false;
                result.insert(row.id, summary);
                continue;
            }
        };
        summary.paths = media_paths;
        summary.bytes = summary.paths.values().sum();
        summary.files = summary.paths.len();
        let base = Path::new(root).join(&row.folder_name);
        let Ok(real_root) = Path::new(root).canonicalize() else {
            summary.complete_scan = false;
            result.insert(row.id, summary);
            continue;
        };
        let mut seen = HashSet::new();
        let mut episodes = HashSet::new();
        let resolved = policies.get(&row.id).and_then(|p| p.as_ref().ok());
        let policy_error = resolved.is_none();
        let policy =
            media_requirements::Policy::from_config(resolved.map(|p| &p.config).unwrap_or(&cfg));
        for file in files {
            let Ok(path) = base.join(&file.filename).canonicalize() else {
                summary.complete_scan = false;
                continue;
            };
            if !path.starts_with(&real_root)
                || !summary.paths.contains_key(&path)
                || !seen.insert(path.clone())
            {
                continue;
            }
            episodes.extend(file.episodes());
            let info = match path.to_str().and_then(|p| probes.get(p)) {
                Some((Some(identity), json)) => {
                    media_info::inspect_cached_json(&path, identity, json).await
                }
                _ => None,
            };
            let gate = if policy_error {
                summary.unverified += 1;
                FileGate{state:"unverified",label:"Requirements unavailable",reason:"Correct this release's requirements in Settings before assuming it meets a gate.".into()}
            } else if !policy.enabled {
                if !policy.required_audio_languages.is_empty() && info.as_ref().is_none() {
                    summary.unverified += 1;
                    FileGate{state:"unverified",label:"Audio unverified",reason:"Quality gates are disabled; the configured audio language is still required and has not been verified.".into()}
                } else if info
                    .as_ref()
                    .is_some_and(|info| !policy.observe(info).audio_satisfied)
                {
                    summary.unmet += 1;
                    FileGate{state:"unmet",label:"Audio requirement unmet",reason:"Quality gates are disabled; the file still fails the required dialogue audio language.".into()}
                } else {
                    summary.disabled += 1;
                    FileGate{state:"disabled",label:"Quality checks disabled",reason:"Quality gates are disabled. Any configured audio requirement has been satisfied.".into()}
                }
            } else if let Some(info) = info {
                let status = policy.status(&info);
                if status.target_met {
                    summary.target += 1;
                    FileGate{state:"target",label:"Target met",reason:"Actual file meets the configured target. Target-based upgrades stop here.".into()}
                } else if status.minimum_met {
                    summary.minimum += 1;
                    FileGate {
                        state: "minimum",
                        label: "Minimum met",
                        reason: format!(
                            "{} Automatic upgrades are {}.",
                            status.reasons.join(" "),
                            if row.allow_upgrades { "enabled" } else { "off" }
                        ),
                    }
                } else {
                    summary.unmet += 1;
                    FileGate {
                        state: "unmet",
                        label: "Requirements unmet",
                        reason: status.reasons.join(" "),
                    }
                }
            } else {
                summary.unverified += 1;
                FileGate{state:"unverified",label:"Unverified",reason:"Current stream facts are unavailable or stale. Inspect the file to verify it.".into()}
            };
            let mut gate = gate;
            if policy.required_audio_languages.is_empty() {
                gate.reason
                    .push_str(" Audio language check disabled: no requirement.");
            }
            if policy.required_languages.is_empty() {
                gate.reason
                    .push_str(" Subtitle language check disabled: no requirement.");
            }
            if !policy.minimum_subtitle_check {
                gate.reason.push_str(" Minimum subtitle check disabled.");
            }
            if !policy.target_subtitle_check {
                gate.reason.push_str(" Target subtitle check disabled.");
            }
            for ep in file.episodes() {
                summary.gates.entry(ep).and_modify(|existing| {
                    *existing=FileGate{state:"unverified",label:"Multiple files",reason:"Several files cover this episode. Inspect the files before assuming its requirements are met.".into()};
                }).or_insert_with(||gate.clone());
            }
        }
        summary.unverified += summary.paths.keys().filter(|p| !seen.contains(*p)).count();
        summary.episodes = episodes.len();
        result.insert(row.id, summary);
    }
    Ok(result)
}

/// Count each canonical media path once, including specials and unparsed names.
/// Do not follow directory links into compatibility views or recovery folders.
async fn logical_paths(
    root: &str,
    folder: &str,
    recovery: &str,
) -> Result<HashMap<std::path::PathBuf, u64>, String> {
    let root = std::path::PathBuf::from(root);
    let folder = folder.to_owned();
    let recovery = std::path::PathBuf::from(recovery);
    tokio::task::spawn_blocking(move || {
        let real_root = root.canonicalize().map_err(|e| e.to_string())?;
        let base = root.join(folder);
        if !base.exists() {
            return Ok(HashMap::new());
        }
        let real_base = base.canonicalize().map_err(|e| e.to_string())?;
        if !real_base.starts_with(&real_root) || real_base == real_root {
            return Err("Invalid release location".into());
        }
        let bin = recovery.canonicalize().ok();
        let mut paths = HashMap::new();
        for entry in walkdir::WalkDir::new(&real_base)
            .follow_links(false)
            .max_depth(33)
            .into_iter()
            .filter_entry(|e| bin.as_ref().is_none_or(|bin| !e.path().starts_with(bin)))
        {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.depth() > 32 {
                return Err("Media scan depth exceeded".into());
            }
            if entry.file_type().is_dir() {
                continue;
            }
            let path = entry.path();
            if !media::is_library_file(path) {
                continue;
            }
            let real = path.canonicalize().map_err(|e| e.to_string())?;
            if !real.starts_with(&real_base)
                || bin.as_ref().is_some_and(|bin| real.starts_with(bin))
            {
                continue;
            }
            let metadata = real.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file() {
                continue;
            }
            let bytes = if media::is_stream_stub(path.to_str().unwrap_or_default()) {
                0
            } else {
                metadata.len()
            };
            paths.insert(real, bytes);
        }
        Ok(paths)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Provider synonyms help recognition and browsing; acquisition aliases remain
/// governed by the existing release-specific matcher.
pub async fn titles(
    db: &SqlitePool,
    library: &[series::Series],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    let cached:HashMap<i64,String>=sqlx::query_as::<_,(i64,String)>("SELECT s.id,COALESCE(c.detail_json,p.detail_json,'{}') FROM series s LEFT JOIN series_metadata_cache c ON c.series_id=s.id LEFT JOIN provider_metadata_cache p ON p.provider_id=s.anilist_id").fetch_all(db).await?.into_iter().collect();
    Ok(library
        .iter()
        .map(|s| {
            let mut names = vec![
                s.title.clone(),
                s.title_english.clone(),
                s.title_romaji.clone(),
                s.title_native.clone(),
            ];
            names.extend(s.alternate_title_list());
            if let Some(value) = cached
                .get(&s.id)
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            {
                for field in ["title_english", "title_romaji", "title_native"] {
                    if let Some(title) = value.get(field).and_then(|v| v.as_str()) {
                        names.push(title.into());
                    }
                }
                if let Some(synonyms) = value.get("synonyms").and_then(|s| s.as_array()) {
                    names.extend(
                        synonyms
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned)),
                    );
                }
            }
            (s.id, names.join("\n"))
        })
        .collect())
}

/// Cached provider titles for outside-library releases on a franchise page.
pub async fn provider_titles(
    db: &SqlitePool,
    ids: &[i64],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    let rows=sqlx::query_as::<_,(i64,String)>("SELECT provider_id,detail_json FROM provider_metadata_cache WHERE provider_id IN(SELECT value FROM json_each(?))").bind(serde_json::to_string(ids).unwrap_or_default()).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|(id, json)| {
            let mut names = Vec::new();
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
                for key in ["title_english", "title_romaji", "title_native"] {
                    if let Some(s) = value.get(key).and_then(|v| v.as_str()) {
                        names.push(s.to_owned());
                    }
                }
                if let Some(values) = value.get("synonyms").and_then(|v| v.as_array()) {
                    names.extend(values.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                }
            }
            (id, names.join("\n"))
        })
        .collect())
}

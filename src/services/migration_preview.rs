//! Read-only migration proposals from a collected snapshot and explicit evidence.
//!
//! No database, filesystem, network, import, or application-startup calls belong
//! here. A proposal is NOT an apply journal. Source claims/routes remain source
//! evidence; current ownership is proposed separately. An executor must still
//! implement locking, fresh preconditions, backup, staging, rollback and recovery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::episode_mapping::{MappingRule, resolve_episode, resolve_file};
use super::naming::{self, NameContext, SeriesNames, TemplateKind};
use super::{media, post_processing};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub collected_at: String,
    pub tables: BTreeMap<String, Vec<Value>>,
    pub columns: BTreeMap<String, Vec<String>>,
    pub root_children: Vec<String>,
    pub files: Vec<ObservedFile>,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedFile {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub device: u64,
    pub inode: u64,
    pub links: u64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    /// SHA-256 of the exact input bytes, including whitespace.
    pub snapshot_sha256: String,
    pub entries: Vec<EntryPlan>,
    pub special_files: Vec<SpecialFile>,
    pub file_evidence: Vec<FileEvidence>,
    /// Explicit canonical metadata, not an episode-count extrapolation.
    pub metadata: Vec<EpisodeMetadata>,
    /// Optional, explicit bindings from original imported sources to library files.
    #[serde(default)]
    pub replay_decisions: Vec<ReplayDecision>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayDecision {
    pub grab_id: i64,
    pub source_root: String,
    pub files: Vec<ReplayBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayBinding {
    pub source_name: String,
    pub library_path: String,
    pub evidence: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPlan {
    pub series_id: i64,
    pub anilist_id: i64,
    pub old_count: i32,
    pub target_count: i32,
    pub canonical_prior_episodes: i32,
    /// Explicit all/none reset for a damaged monitor set. None preserves choices.
    #[serde(default)]
    pub reset_monitoring: Option<bool>,
    pub evidence: String,
    /// Maps the OLD LIBRARY's regular numbers, not raw download numbers.
    pub rules: Vec<MappingRule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecialFile {
    pub path: String,
    pub series_id: i64,
    pub season: i32,
    pub episode: i32,
    pub title: String,
    pub evidence: String,
    /// Explicit choice for a promoted special with no old monitor row.
    pub monitored: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileEvidence {
    pub path: String,
    /// A reviewed classification. Empty strings mean unknown, never guessed.
    pub resolution: String,
    pub source: String,
    pub release_group: String,
    pub evidence: String,
    /// Optional provenance for the NEW canonical ownership record.
    pub grab_id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeMetadata {
    pub series_id: i64,
    pub episode: i32,
    pub title: String,
    pub aired: String,
    pub evidence: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub preview_only: bool,
    pub snapshot_sha256: String,
    pub collected_at: String,
    pub folders: Vec<FolderChange>,
    pub files: Vec<FileChange>,
    pub records: Vec<RecordChange>,
    pub blockers: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderChange {
    pub series_id: i64,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub from: String,
    pub to: String,
    pub kind: String,
    /// Exact observation for a future fresh precondition check, not a byte hash.
    pub observed: ObservedFile,
    pub series_id: Option<i64>,
    pub old_season: Option<i32>,
    pub old_first: Option<i32>,
    pub season: Option<i32>,
    pub first: Option<i32>,
    pub last: Option<i32>,
    /// Desired episode fields. Other NFO fields must be preserved by an executor.
    pub nfo_fields: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordChange {
    pub table: String,
    pub key: Value,
    pub before: Value,
    /// Only changed fields for updates; full proposed fields for inserts.
    pub after: Value,
    pub action: String,
    pub reason: String,
}

fn fail(message: impl std::fmt::Display) -> String {
    format!("Migration preview: {message}")
}

fn text(row: &Value, field: &str) -> String {
    row[field].as_str().unwrap_or_default().to_string()
}

fn number(row: &Value, field: &str) -> Result<i64, String> {
    row[field]
        .as_i64()
        .ok_or_else(|| fail(format!("missing integer field {field}")))
}

fn rows<'a>(snapshot: &'a Snapshot, table: &str) -> Result<&'a [Value], String> {
    snapshot
        .tables
        .get(table)
        .map(Vec::as_slice)
        .ok_or_else(|| fail(format!("missing table observation {table}")))
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn evidence(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(fail("mapping or file evidence is empty"));
    }
    Ok(())
}

fn change(
    preview: &mut Preview,
    table: &str,
    key: Value,
    before: &Value,
    after: Value,
    reason: &str,
) {
    let changed = after
        .as_object()
        .is_none_or(|fields| fields.iter().any(|(key, value)| &before[key] != value));
    if changed {
        preview.records.push(RecordChange {
            table: table.to_string(),
            key,
            before: before.clone(),
            after,
            action: if before.is_null() { "insert" } else { "update" }.to_string(),
            reason: reason.to_string(),
        });
    }
}

/// Review only. This function cannot open or modify the observed paths/database.
pub fn build(snapshot_bytes: &[u8], manifest: &Manifest) -> Result<Preview, String> {
    let digest = hex::encode(Sha256::digest(snapshot_bytes));
    if manifest.version != 1 || manifest.snapshot_sha256 != digest {
        return Err(fail("manifest version or snapshot digest does not match"));
    }
    let snapshot: Snapshot = serde_json::from_slice(snapshot_bytes).map_err(fail)?;
    if snapshot.version != 1 || !snapshot.issues.is_empty() {
        return Err(fail(format!("incomplete snapshot: {:?}", snapshot.issues)));
    }
    let mut preview = Preview {
        preview_only: true,
        snapshot_sha256: digest,
        collected_at: snapshot.collected_at.clone(),
        folders: Vec::new(), files: Vec::new(), records: Vec::new(), blockers: Vec::new(),
        notes: vec![
            "This is a review document, not an executable migration or backup.".into(),
            "Apply requires stopped writers, a fresh snapshot, database backup, staged file moves and a recovery journal.".into(),
            "Raw provider caches, download source paths, original claims and saved source routes retain their original numbering.".into(),
            "AniList episode_airings already use provider numbers and must not receive the old-library offset.".into(),
            "No video bytes were read or changed. File size/inode/mtime are observations, not content verification.".into(),
        ],
    };
    let config = rows(&snapshot, "config")?;
    if !snapshot
        .columns
        .get("series")
        .is_some_and(|cols| cols.iter().any(|c| c == "cumulative_offset_reviewed"))
    {
        preview.blockers.push(
            "Installed database cannot protect reviewed episode offsets on metadata refresh".into(),
        );
    }
    if config.len() != 1 {
        return Err(fail("expected one naming configuration"));
    }
    let config = &config[0];
    let folder_template = text(config, "series_folder_format");
    let season_template = text(config, "season_folder_format");
    let file_template = text(config, "episode_file_format");
    for (kind, template) in [
        (TemplateKind::SeriesFolder, &folder_template),
        (TemplateKind::SeasonFolder, &season_template),
        (TemplateKind::EpisodeFile, &file_template),
    ] {
        naming::validate(kind, template).map_err(fail)?;
    }
    let language = text(config, "title_language");
    let mut entries = BTreeMap::new();
    for entry in rows(&snapshot, "series")? {
        let id = number(entry, "id")?;
        if entries.insert(id, entry).is_some() {
            return Err(fail("duplicate series observation"));
        }
    }
    let mut plans = BTreeMap::new();
    let mut contexts = BTreeMap::new();
    let mut folders = BTreeMap::new();
    let mut new_folders = BTreeSet::new();
    for plan in &manifest.entries {
        evidence(&plan.evidence)?;
        if plans.insert(plan.series_id, plan).is_some() {
            return Err(fail("duplicate entry plan"));
        }
        let row = entries
            .get(&plan.series_id)
            .ok_or_else(|| fail("entry not observed"))?;
        if number(row, "anilist_id")? != plan.anilist_id
            || number(row, "episodes")? != i64::from(plan.old_count)
            || !(1..=5000).contains(&plan.old_count)
            || !(1..=5000).contains(&plan.target_count)
            || plan.canonical_prior_episodes < 0
        {
            return Err(fail("entry identity/count precondition does not match"));
        }
        let names = SeriesNames {
            title: row["title"].as_str().unwrap_or_default(),
            romaji: row["title_romaji"].as_str().unwrap_or_default(),
            english: row["title_english"].as_str().unwrap_or_default(),
            native: row["title_native"].as_str().unwrap_or_default(),
            year: row["season_year"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok()),
        };
        let ctx = NameContext::for_series(&names, &language);
        let from = text(row, "folder_name");
        let to = naming::render(TemplateKind::SeriesFolder, &folder_template, &ctx)
            .map_err(fail)?
            .name;
        if !safe_path(&from) || from.contains('/') || !new_folders.insert(to.to_lowercase()) {
            return Err(fail("unsafe or conflicting series folders"));
        }
        if from != to
            && snapshot
                .root_children
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&to))
        {
            return Err(fail(format!("destination folder already exists: {to}")));
        }
        if !snapshot
            .files
            .iter()
            .any(|f| f.path == from && f.kind == "directory")
        {
            return Err(fail("series folder observation is missing"));
        }
        folders.insert(plan.series_id, (from.clone(), to.clone()));
        contexts.insert(plan.series_id, ctx);
        preview.folders.push(FolderChange {
            series_id: plan.series_id,
            from,
            to: to.clone(),
        });
        change(
            &mut preview,
            "series",
            json!({"id":plan.series_id}),
            row,
            json!({"episodes":plan.target_count,"folder_name":to,"cumulative_prior_episodes":plan.canonical_prior_episodes,"cumulative_offset_known":1,"cumulative_offset_reviewed":1}),
            "Canonical entry count, folder and prior-entry count",
        );
    }
    if plans.len() != entries.len() {
        return Err(fail("every observed entry needs a plan"));
    }
    let mut identity_map = BTreeMap::new();
    let mut identity_targets = BTreeSet::new();
    for plan in plans.values() {
        let mut source_episodes = BTreeSet::new();
        for rule in &plan.rules {
            // Validate before expanding. Counts do not define a source scheme;
            // the explicit ranges can include zero or cover only known slots.
            resolve_episode(&[*rule], rule.source_first).map_err(fail)?;
            source_episodes.extend(rule.source_first..=rule.source_last);
        }
        if source_episodes.is_empty() {
            return Err(fail("entry has no source episode rules"));
        }
        for episode in source_episodes {
            let target = resolve_episode(&plan.rules, episode).map_err(fail)?;
            if plans
                .get(&target.entry)
                .is_none_or(|p| target.number > p.target_count)
                || !identity_targets.insert((target.entry, target.number))
            {
                return Err(fail("conflicting or out-of-range library episode mapping"));
            }
            identity_map.insert((plan.series_id, episode), (target.entry, target.number));
        }
    }
    let mut metadata = BTreeMap::new();
    for row in &manifest.metadata {
        evidence(&row.evidence)?;
        if !plans
            .get(&row.series_id)
            .is_some_and(|p| (1..=p.target_count).contains(&row.episode))
            || row.title.trim().is_empty()
            || metadata.insert((row.series_id, row.episode), row).is_some()
        {
            return Err(fail("invalid or duplicate canonical metadata"));
        }
    }
    for plan in plans.values() {
        for ep in 1..=plan.target_count {
            if !metadata.contains_key(&(plan.series_id, ep)) {
                return Err(fail("canonical metadata is incomplete"));
            }
        }
    }
    let mut observed = BTreeMap::new();
    for file in &snapshot.files {
        if !safe_path(&file.path)
            || !matches!(file.kind.as_str(), "file" | "directory")
            || observed.insert(file.path.clone(), file).is_some()
        {
            return Err(fail("unsafe, linked or duplicate file observation"));
        }
    }
    let mut facts = BTreeMap::new();
    for fact in &manifest.file_evidence {
        evidence(&fact.evidence)?;
        if !observed.contains_key(&fact.path) || facts.insert(fact.path.clone(), fact).is_some() {
            return Err(fail("duplicate or unobserved file evidence"));
        }
    }
    let mut specials = BTreeMap::new();
    for special in &manifest.special_files {
        evidence(&special.evidence)?;
        if !observed.contains_key(&special.path)
            || !matches!(special.season, 0 | 1)
            || special.episode < 1
            || special.episode > 5000
            || specials.insert(&special.path, special).is_some()
        {
            return Err(fail("invalid explicit special mapping"));
        }
    }
    let mut slots = BTreeSet::new();
    let mut coverage: BTreeMap<i64, BTreeMap<i64, BTreeSet<i32>>> = BTreeMap::new();
    let mut used_facts = BTreeSet::new();
    let mut used_specials = BTreeSet::new();
    let mut video_old_identity = BTreeMap::new();
    for file in snapshot
        .files
        .iter()
        .filter(|f| f.kind == "file" && is_video(&f.path))
    {
        let (&owner, _) = folders
            .iter()
            .find(|(_, (old, _))| file.path.starts_with(&format!("{old}/")))
            .ok_or_else(|| fail("file is outside the selected entries"))?;
        let filename = Path::new(&file.path)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| fail("invalid filename"))?;
        let span = media::parse_episode_span(&filename.to_lowercase())
            .ok_or_else(|| fail(format!("unparsed video: {}", file.path)))?;
        let old_season = if media::path_names_specials(Path::new(&file.path)) {
            0
        } else {
            span.season.unwrap_or(1)
        };
        let (target, season, first, last, special_title) = if let Some(explicit) =
            specials.get(&file.path)
        {
            used_specials.insert(file.path.clone());
            if old_season != 0 || span.first != span.last {
                return Err(fail("explicit special is not a single special file"));
            }
            (
                explicit.series_id,
                explicit.season,
                explicit.episode,
                explicit.episode,
                explicit.title.clone(),
            )
        } else {
            if old_season != 1 {
                return Err(fail(format!(
                    "special needs an explicit keep/promote decision: {}",
                    file.path
                )));
            }
            let mapped = resolve_file(&plans[&owner].rules, span.first, span.last).map_err(fail)?;
            (mapped.entry, 1, mapped.first, mapped.last, String::new())
        };
        let target_plan = plans
            .get(&target)
            .ok_or_else(|| fail("target entry not selected"))?;
        if season == 1 && last > target_plan.target_count {
            return Err(fail("file exceeds canonical entry"));
        }
        for ep in first..=last {
            if !slots.insert((target, season, ep)) {
                return Err(fail("two videos would occupy one episode"));
            }
        }
        let fact = facts
            .get(&file.path)
            .ok_or_else(|| fail(format!("file classification needs review: {}", file.path)))?;
        used_facts.insert(file.path.clone());
        let mut ctx = contexts[&target].clone();
        ctx.season_number = season;
        ctx.episode_number = first;
        ctx.episode_last = last;
        ctx.episode_absolute = if season == 1 {
            target_plan.canonical_prior_episodes.checked_add(first)
        } else {
            None
        };
        ctx.episode_title = if season == 1 {
            metadata[&(target, first)].title.clone()
        } else {
            special_title
        };
        ctx.quality_resolution = fact.resolution.clone();
        ctx.quality_source = fact.source.clone();
        ctx.release_group = fact.release_group.clone();
        ctx.ext = Path::new(filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .into();
        let folder = if season == 0 {
            "Specials".into()
        } else {
            naming::render(TemplateKind::SeasonFolder, &season_template, &ctx)
                .map_err(fail)?
                .name
        };
        let rendered =
            naming::render(TemplateKind::EpisodeFile, &file_template, &ctx).map_err(fail)?;
        let check = media::parse_episode_span(&rendered.name.to_lowercase())
            .ok_or_else(|| fail("rendered name has no episode span"))?;
        if check.first != first || check.last != last || check.season.is_some_and(|s| s != season) {
            return Err(fail(
                "rendered name does not preserve the mapped episode span",
            ));
        }
        let to = format!("{}/{folder}/{}", folders[&target].1, rendered.name);
        let nfo_fields = (first..=last).map(|ep| json!({
            "season":season,"episode":ep,"showtitle":ctx.series_title,
            "title":if season == 1 {&metadata[&(target,ep)].title} else {&ctx.episode_title},
            "aired":if season == 1 {metadata[&(target,ep)].aired.as_str()} else {""}
        })).collect();
        if old_season == 1 {
            for old in span.first..=span.last {
                video_old_identity.insert((owner, old), preview.files.len());
            }
        }
        if let Some(grab_id) = fact.grab_id {
            let grab = rows(&snapshot, "grabbed_torrents")?
                .iter()
                .find(|r| r["id"].as_i64() == Some(grab_id))
                .ok_or_else(|| fail("file owner grab was not observed"))?;
            if text(grab, "state") != "imported" {
                return Err(fail("file owner is not an imported grab"));
            }
            if season == 1 {
                coverage
                    .entry(grab_id)
                    .or_default()
                    .entry(target)
                    .or_default()
                    .extend(first..=last);
            }
        }
        preview.files.push(FileChange {
            from: file.path.clone(),
            to,
            kind: "video".into(),
            observed: file.clone(),
            series_id: Some(target),
            old_season: Some(old_season),
            old_first: Some(span.first),
            season: Some(season),
            first: Some(first),
            last: Some(last),
            nfo_fields,
        });
    }
    if used_facts.len() != facts.len() || used_specials.len() != specials.len() {
        return Err(fail("unused file evidence or special decisions"));
    }
    let videos = preview.files.clone();
    for file in snapshot
        .files
        .iter()
        .filter(|f| f.kind == "file" && !is_video(&f.path))
    {
        let path = Path::new(&file.path);
        let companions: Vec<_> = videos
            .iter()
            .filter(|video| {
                let v = Path::new(&video.from);
                path.parent() == v.parent()
                    && path.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
                        s.starts_with(&format!(
                            "{}.",
                            v.file_stem().unwrap_or_default().to_string_lossy()
                        ))
                    })
            })
            .collect();
        if companions.len() > 1 {
            return Err(fail("companion belongs to more than one video"));
        }
        let (to, fields, kind) = if let Some(video) = companions.first() {
            let old_stem = Path::new(&video.from)
                .with_extension("")
                .to_string_lossy()
                .to_string();
            let new_stem = Path::new(&video.to)
                .with_extension("")
                .to_string_lossy()
                .to_string();
            let to = format!(
                "{new_stem}{}",
                file.path
                    .strip_prefix(&old_stem)
                    .ok_or_else(|| fail("companion prefix mismatch"))?
            );
            let nfo = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("nfo"));
            if nfo && (file.text.is_none() || file.sha256.is_none()) {
                return Err(fail("NFO content observation is missing"));
            }
            (
                to,
                if nfo {
                    video.nfo_fields.clone()
                } else {
                    Vec::new()
                },
                if nfo { "episode_nfo" } else { "companion" },
            )
        } else {
            let (_, (old, new)) = folders
                .iter()
                .find(|(_, (old, _))| file.path.starts_with(&format!("{old}/")))
                .ok_or_else(|| fail("unowned auxiliary file"))?;
            let mut to = format!("{new}{}", file.path.strip_prefix(old).unwrap_or_default());
            // Season artwork and season.nfo follow a uniquely identified video folder.
            let destinations: BTreeSet<_> = videos
                .iter()
                .filter(|v| Path::new(&v.from).parent() == path.parent())
                .filter_map(|v| Path::new(&v.to).parent())
                .collect();
            if destinations.len() > 1 {
                return Err(fail("split folder has unassigned auxiliary files"));
            }
            if let Some(parent) = destinations.first() {
                to = parent
                    .join(path.file_name().unwrap_or_default())
                    .to_string_lossy()
                    .into();
            }
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("nfo"))
                && !matches!(
                    path.file_name().and_then(|s| s.to_str()),
                    Some("tvshow.nfo" | "season.nfo")
                )
            {
                preview
                    .blockers
                    .push(format!("Unassigned NFO needs review: {}", file.path));
            }
            (to, Vec::new(), "auxiliary")
        };
        preview.files.push(FileChange {
            from: file.path.clone(),
            to,
            kind: kind.into(),
            observed: file.clone(),
            series_id: None,
            old_season: None,
            old_first: None,
            season: None,
            first: None,
            last: None,
            nfo_fields: fields,
        });
    }
    check_destinations(&preview.files, &observed)?;
    build_records(
        &snapshot,
        manifest,
        &identity_map,
        &metadata,
        &video_old_identity,
        &facts,
        &coverage,
        &mut preview,
    )?;
    preview.files.sort_by(|a, b| a.from.cmp(&b.from));
    preview.blockers.sort();
    preview.blockers.dedup();
    Ok(preview)
}

fn is_video(path: &str) -> bool {
    post_processing::is_video_file(path)
        || Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("strm"))
}

fn check_destinations(
    files: &[FileChange],
    observed: &BTreeMap<String, &ObservedFile>,
) -> Result<(), String> {
    let mut destinations = BTreeSet::new();
    let moving: BTreeSet<_> = files
        .iter()
        .filter(|f| f.from != f.to)
        .map(|f| f.from.to_lowercase())
        .collect();
    for file in files {
        let to = file.to.to_lowercase();
        if !safe_path(&file.to) || !destinations.insert(to.clone()) {
            return Err(fail("duplicate or unsafe destination"));
        }
        if observed.keys().any(|p| p.to_lowercase() == to)
            && file.from != file.to
            && !moving.contains(&to)
        {
            return Err(fail(format!("destination is occupied: {}", file.to)));
        }
        for parent in Path::new(&file.to).ancestors().skip(1) {
            if observed.values().any(|f| {
                f.path.eq_ignore_ascii_case(&parent.to_string_lossy()) && f.kind != "directory"
            }) {
                return Err(fail("destination parent is an existing file"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_records(
    snapshot: &Snapshot,
    manifest: &Manifest,
    identity_map: &BTreeMap<(i64, i32), (i64, i32)>,
    metadata: &BTreeMap<(i64, i32), &EpisodeMetadata>,
    video_old_identity: &BTreeMap<(i64, i32), usize>,
    facts: &BTreeMap<String, &FileEvidence>,
    coverage: &BTreeMap<i64, BTreeMap<i64, BTreeSet<i32>>>,
    preview: &mut Preview,
) -> Result<(), String> {
    for table in [
        "episode_monitor_state",
        "episode_quality_tags",
        "episode_grab_history",
    ] {
        for row in rows(snapshot, table)? {
            let old = (
                number(row, "series_id")?,
                i32::try_from(number(row, "episode_number")?).map_err(fail)?,
            );
            if table == "episode_monitor_state"
                && manifest
                    .entries
                    .iter()
                    .any(|p| p.series_id == old.0 && p.reset_monitoring.is_some())
            {
                continue;
            }
            let &(series, ep) = identity_map
                .get(&old)
                .ok_or_else(|| fail(format!("unmapped {table} row")))?;
            let key = if table == "episode_grab_history" {
                json!({"id":number(row,"id")?})
            } else {
                json!({"series_id":old.0,"episode_number":old.1})
            };
            let mut after = json!({"series_id":series,"episode_number":ep});
            if let Some(index) = video_old_identity.get(&old) {
                let video = &preview.files[*index];
                let fact = facts[&video.from];
                if table == "episode_quality_tags" {
                    // Classification is retained; only reviewed group attribution changes.
                    let source = naming::quality_source_label(
                        &text(row, "source"),
                        row["is_remux"].as_i64() == Some(1),
                        &text(row, "web_kind"),
                    );
                    if text(row, "resolution") != fact.resolution || source != fact.source {
                        preview.blockers.push(format!(
                            "Naming classification differs from the stored quality record for {}",
                            video.from
                        ));
                    }
                    after["release_group"] = json!(fact.release_group);
                } else if table == "episode_grab_history" && text(row, "state") == "completed" {
                    let old_name = Path::new(&video.from)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    if text(row, "file_name") == old_name {
                        after["file_name"] = json!(
                            Path::new(&video.to)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        );
                        after["release_group"] = json!(fact.release_group);
                    } else {
                        preview.blockers.push(format!(
                            "Completed history {} does not match its current video",
                            number(row, "id")?
                        ));
                    }
                }
            }
            change(
                preview,
                table,
                key,
                row,
                after,
                "Keep the record attached to the same episode content",
            );
        }
    }
    for plan in &manifest.entries {
        let Some(monitored) = plan.reset_monitoring else {
            continue;
        };
        let old_rows: Vec<_> = rows(snapshot, "episode_monitor_state")?
            .iter()
            .filter(|r| r["series_id"].as_i64() == Some(plan.series_id))
            .collect();
        for ep in 1..=plan.target_count {
            let before = old_rows
                .iter()
                .copied()
                .find(|r| r["episode_number"].as_i64() == Some(i64::from(ep)))
                .unwrap_or(&Value::Null);
            let key = json!({"series_id":plan.series_id,"episode_number":ep});
            change(
                preview,
                "episode_monitor_state",
                key,
                before,
                json!({"series_id":plan.series_id,"episode_number":ep,"monitored":i32::from(monitored)}),
                "Explicit canonical all/none monitoring reset",
            );
        }
        for row in old_rows {
            let ep = number(row, "episode_number")?;
            if !(1..=i64::from(plan.target_count)).contains(&ep) {
                preview.records.push(RecordChange {
                    table: "episode_monitor_state".into(),
                    key: json!({"series_id":plan.series_id,"episode_number":ep}),
                    before: row.clone(),
                    after: Value::Null,
                    action: "delete_monitor_row".into(),
                    reason: "Explicit monitoring reset removes an obsolete slot".into(),
                });
            }
        }
    }
    for special in &manifest.special_files {
        if special.season != 1 {
            continue;
        }
        let key = json!({"series_id":special.series_id,"episode_number":special.episode});
        if identity_map
            .values()
            .any(|v| *v == (special.series_id, special.episode))
        {
            return Err(fail(
                "promoted special conflicts with an existing record identity",
            ));
        }
        let reset = manifest
            .entries
            .iter()
            .find(|p| p.series_id == special.series_id)
            .and_then(|p| p.reset_monitoring);
        if let Some(monitored) = reset {
            if monitored != special.monitored {
                return Err(fail(
                    "special monitoring choice conflicts with the explicit reset",
                ));
            }
        } else {
            change(
                preview,
                "episode_monitor_state",
                key.clone(),
                &Value::Null,
                json!({"series_id":special.series_id,"episode_number":special.episode,"monitored":i32::from(special.monitored)}),
                "Explicit promoted-special monitoring choice",
            );
        }
        let fact = facts[&special.path];
        let video = preview
            .files
            .iter()
            .find(|v| v.from == special.path)
            .ok_or_else(|| fail("promoted file is missing"))?;
        let filename = Path::new(&video.to)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let size = video.observed.size;
        let (source, remux, web_kind) = match fact.source.as_str() {
            "" => ("", 0, ""),
            "BluRay" => ("BluRay", 0, ""),
            "BluRay Remux" => ("BluRay", 1, ""),
            "WEB" => ("Web", 0, ""),
            "WEB-DL" => ("Web", 0, "WEB-DL"),
            "WEBRip" => ("Web", 0, "WEBRip"),
            "DVD" => ("DVD", 0, ""),
            "HDTV" => ("HDTV", 0, ""),
            "TV" => ("TV", 0, ""),
            _ => return Err(fail("unsupported promoted-file classification")),
        };
        let grab = rows(snapshot, "grabbed_torrents")?
            .iter()
            .find(|g| g["id"].as_i64() == fact.grab_id);
        let release = grab.map(|g| text(g, "torrent_name")).unwrap_or_default();
        let quality = super::source::classification_from_stored_full(
            source,
            &fact.resolution,
            remux == 1,
            false,
            super::source::WebKind::from_str(web_kind),
            0.0,
            true,
        )
        .label();
        change(
            preview,
            "episode_quality_tags",
            key.clone(),
            &Value::Null,
            json!({"series_id":special.series_id,"episode_number":special.episode,
                "quality_tag":quality,"release_title":release,"release_group":fact.release_group,
                "state":"completed","source":source,"resolution":fact.resolution,
                "is_remux":remux,"is_bdmv":0,"web_kind":web_kind,
                "manual_override":0,"needs_review":1,"classification_confidence":0.0}),
            "Create from this promoted file's evidence; classification is not yet measured or pinned",
        );
        change(
            preview,
            "episode_grab_history",
            json!({"series_id":special.series_id,"episode_number":special.episode,"file_name":filename}),
            &Value::Null,
            json!({"series_id":special.series_id,"episode_number":special.episode,
                "quality_tag":quality,"release_title":release,"release_group":fact.release_group,
                "file_name":filename,"size_bytes":size,"is_batch":0,"state":"completed"}),
            "Record the existing promoted file during migration; do not invent an original grab time",
        );
    }
    for table in ["series_episode_metadata", "provider_episode_metadata"] {
        let field = if table == "series_episode_metadata" {
            "series_id"
        } else {
            "provider_id"
        };
        for (&(series, ep), meta) in metadata {
            let provider = manifest
                .entries
                .iter()
                .find(|p| p.series_id == series)
                .ok_or_else(|| fail("metadata entry missing"))?
                .anilist_id;
            let owner = if field == "series_id" {
                series
            } else {
                provider
            };
            let before = rows(snapshot, table)?
                .iter()
                .find(|r| {
                    r[field].as_i64() == Some(owner)
                        && r["episode_number"].as_i64() == Some(i64::from(ep))
                })
                .unwrap_or(&Value::Null);
            change(
                preview,
                table,
                json!({field:owner,"episode_number":ep}),
                before,
                json!({field:owner,"episode_number":ep,"title":meta.title,"title_english":meta.title,"title_romaji":"","title_native":"","aired":meta.aired,"source":"mapped_migration"}),
                &meta.evidence,
            );
        }
        for row in rows(snapshot, table)? {
            let owner = number(row, field)?;
            let series = if field == "series_id" {
                owner
            } else {
                manifest
                    .entries
                    .iter()
                    .find(|p| p.anilist_id == owner)
                    .ok_or_else(|| fail("provider metadata owner missing"))?
                    .series_id
            };
            let ep = i32::try_from(number(row, "episode_number")?).map_err(fail)?;
            if !metadata.contains_key(&(series, ep)) {
                preview.records.push(RecordChange {
                    table: table.into(),
                    key: json!({field:owner,"episode_number":ep}),
                    before: row.clone(),
                    after: Value::Null,
                    action: "delete_cache_row".into(),
                    reason: "Outside the verified canonical metadata set".into(),
                });
            }
        }
    }
    for (&grab_id, owners) in coverage {
        let grab = rows(snapshot, "grabbed_torrents")?
            .iter()
            .find(|r| r["id"].as_i64() == Some(grab_id))
            .ok_or_else(|| fail("ownership grab missing"))?;
        // Do not discard coverage for an entry outside this migration.
        let mut after: BTreeMap<i64, BTreeSet<i32>> =
            match grab["imported_episode_coverage"].as_str() {
                Some(s) => serde_json::from_str(s).map_err(fail)?,
                None => BTreeMap::new(),
            };
        for plan in &manifest.entries {
            after.remove(&plan.series_id);
        }
        after.extend(owners.clone());
        change(
            preview,
            "grabbed_torrents",
            json!({"id":grab_id}),
            grab,
            json!({"imported_episode_coverage":serde_json::to_string(&after).map_err(fail)?}),
            "Current canonical file ownership; original download claims remain unchanged",
        );
        if !snapshot
            .columns
            .get("grabbed_torrents")
            .is_some_and(|c| c.iter().any(|c| c == "imported_episode_coverage"))
        {
            preview.blockers.push("Installed database lacks canonical import coverage; the compatible application schema is required".into());
        }
    }
    let mut replay_ids = BTreeSet::new();
    for decision in &manifest.replay_decisions {
        if !replay_ids.insert(decision.grab_id)
            || !rows(snapshot, "grabbed_torrents")?
                .iter()
                .any(|g| g["id"].as_i64() == Some(decision.grab_id))
        {
            return Err(fail("duplicate or unknown replay decision"));
        }
    }
    for grab in rows(snapshot, "grabbed_torrents")? {
        let grab_id = number(grab, "id")?;
        if !grab["library_migration_bridge"].is_null() {
            preview.blockers.push(format!("Grab {grab_id} already has a migration bridge; chained migrations need an explicit composition review"));
        }
        let identities: Vec<_> = identity_map.iter().map(|(&(sid,ep), &(target,n))|
            json!({"from_series":sid,"from_episode":ep,"to_series":target,"to_episode":n})).collect();
        let mut bridge = json!({"version":1,"snapshot_sha256":preview.snapshot_sha256,
            "replay_policy":"review_required","identities":identities,
            "files":preview.files.iter().filter(|f| f.kind == "video" && facts[&f.from].grab_id == Some(grab_id))
                .map(|f| json!({"from":f.from,"to":f.to,"series_id":f.series_id,"season":f.season,"first":f.first,"last":f.last})).collect::<Vec<_>>()});
        if let Some(decision) = manifest
            .replay_decisions
            .iter()
            .find(|d| d.grab_id == grab_id)
        {
            let replay = build_replay(decision, grab, manifest, facts, &preview.files)?;
            bridge["version"] = json!(2);
            bridge["replay_policy"] = json!("verified_files");
            bridge["replay"] = serde_json::to_value(replay).map_err(fail)?;
            bridge["replay_evidence"] = serde_json::to_value(decision).map_err(fail)?;
        }
        // Merge with the ownership update so one physical row has one proposal.
        if let Some(record) = preview
            .records
            .iter_mut()
            .find(|r| r.table == "grabbed_torrents" && r.key["id"] == grab_id)
        {
            record.after["library_migration_bridge"] =
                json!(serde_json::to_string(&bridge).map_err(fail)?);
        } else {
            change(
                preview,
                "grabbed_torrents",
                json!({"id":grab_id}),
                grab,
                json!({"library_migration_bridge":serde_json::to_string(&bridge).map_err(fail)?}),
                "Preserve the old download decision and require review before replay",
            );
        }
        if text(grab, "state") == "imported" && !coverage.contains_key(&number(grab, "id")?) {
            preview.blockers.push(format!(
                "Imported grab {} has no reviewed current file ownership; its legacy claims cannot be assumed canonical",
                number(grab, "id")?
            ));
        }
    }
    if !rows(snapshot, "pending_grabs")?.is_empty()
        || rows(snapshot, "grabbed_torrents")?
            .iter()
            .any(|r| text(r, "state") == "pending")
    {
        preview
            .blockers
            .push("An active download or picker must finish or be held before migration".into());
    }
    if !rows(snapshot, "grabbed_torrents")?.is_empty() {
        if !snapshot
            .columns
            .get("grabbed_torrents")
            .is_some_and(|cols| cols.iter().any(|c| c == "library_migration_bridge"))
        {
            preview.blockers.push("Installed database lacks the migration replay guard; compatible application support is required".into());
        }
        preview.notes.push("Old download records retain their source claims. Verified source-file bindings can replay automatically; records without them stay held for review. Library shifts never establish raw release numbering.".into());
    }
    Ok(())
}

fn build_replay(
    decision: &ReplayDecision,
    grab: &Value,
    manifest: &Manifest,
    facts: &BTreeMap<String, &FileEvidence>,
    changes: &[FileChange],
) -> Result<crate::models::grabbed_torrents::migration_replay::ReplayPlan, String> {
    use crate::models::grabbed_torrents::migration_replay::{ReplayFile, ReplayPlan};
    let root = Path::new(&decision.source_root);
    if text(grab, "state") != "imported"
        || !root.is_absolute()
        || !root
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        || decision.source_root.contains('\\')
    {
        return Err(fail("invalid reviewed source root or grab state"));
    }
    let source_paths: Vec<String> =
        serde_json::from_str(&text(grab, "imported_source_paths")).map_err(fail)?;
    let source_paths: BTreeSet<_> = source_paths.into_iter().filter(|p| is_video(p)).collect();
    let expected: BTreeSet<_> = changes
        .iter()
        .filter(|f| f.kind == "video" && facts[&f.from].grab_id == Some(decision.grab_id))
        .map(|f| f.from.as_str())
        .collect();
    let mut used_sources = BTreeSet::new();
    let mut used_library = BTreeSet::new();
    let mut files = Vec::new();
    for binding in &decision.files {
        evidence(&binding.evidence)?;
        post_processing::validate_relative_path_fragment(&binding.source_name).map_err(fail)?;
        let source_path = root
            .join(&binding.source_name)
            .to_string_lossy()
            .into_owned();
        if !source_paths.contains(&source_path)
            || !used_sources.insert(source_path)
            || !expected.contains(binding.library_path.as_str())
            || !used_library.insert(binding.library_path.as_str())
        {
            return Err(fail(
                "replay binding is duplicate or outside the observed source/library ownership",
            ));
        }
        let change = changes
            .iter()
            .find(|f| f.from == binding.library_path)
            .ok_or_else(|| fail("replay library file missing"))?;
        let sid = change
            .series_id
            .ok_or_else(|| fail("replay target missing"))?;
        let owner = manifest
            .entries
            .iter()
            .find(|e| e.series_id == sid)
            .ok_or_else(|| fail("replay owner missing"))?;
        let fact = facts[&change.from];
        files.push(ReplayFile {
            name: binding.source_name.clone(),
            series_id: sid,
            anilist_id: owner.anilist_id,
            season: change.season.unwrap(),
            first: change.first.unwrap(),
            last: change.last.unwrap(),
            title: change
                .nfo_fields
                .first()
                .map(|v| text(v, "title"))
                .unwrap_or_default(),
            resolution: fact.resolution.clone(),
            source: fact.source.clone(),
            release_group: fact.release_group.clone(),
        });
    }
    if used_library != expected || used_sources != source_paths {
        return Err(fail(
            "replay decision does not cover the complete imported file set",
        ));
    }
    let plan = ReplayPlan { version: 1, files };
    plan.validate().map_err(fail)?;
    Ok(plan)
}

#[cfg(test)]
mod tests;

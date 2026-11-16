//! Continued seeding is permitted only for reviewed original source files.
//! This is pause-only: native seed limits and every user stop remain intact.
//! BitTorrent may upload pieces while downloading or before the next check.
use crate::{
    AppState,
    models::{config::Config, download_clients, grabbed_torrents, seed_policy::Review},
    services::{
        download_client::{
            DownloadClient, DownloadFile, DownloadItem, DownloadItemState, normalize_info_hash,
            translate_client_path,
        },
        language_preferences, media_info,
        media_requirements::Policy,
        post_processing,
    },
};
use sqlx::{FromRow, SqlitePool};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

pub static CHECK_LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

#[derive(Debug, FromRow, PartialEq, Eq)]
struct Owner {
    id: i64,
    series_id: i64,
    state: String,
    imported_at: Option<String>,
    import_mode: Option<String>,
    imported_source_paths: Option<String>,
    imported_episode_coverage: Option<String>,
    verification: Option<String>,
    failure_reason: String,
    download_client_id: Option<i64>,
}

async fn owners(db: &SqlitePool, hash: &str) -> Result<Vec<Owner>, String> {
    sqlx::query_as(
        "SELECT id, series_id, state, imported_at, import_mode, imported_source_paths,
        imported_episode_coverage, verification, failure_reason, download_client_id
        FROM grabbed_torrents WHERE lower(hash)=?",
    )
    .bind(hash.to_ascii_lowercase())
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Accepted,
    Rejected(String),
    Unverified(String),
}
impl Decision {
    fn fields(&self) -> (&str, String) {
        match self {
            Self::Accepted => ("Accepted", "Every available source video has imported coverage and meets its current minimum requirements".into()),
            Self::Rejected(s) => ("Rejected",s.clone()),
            Self::Unverified(s) => ("Unverified",s.clone()),
        }
    }
}
fn unknown(s: impl Into<String>) -> Decision {
    Decision::Unverified(s.into())
}

#[derive(Debug)]
struct Slot {
    series_id: i64,
    anilist_id: i64,
    first: i32,
    last: i32,
}

/// Reuse frozen decisions. There is no new numbering or matching resolver here.
async fn bind_slots(
    db: &SqlitePool,
    owner: &Owner,
    hash: &str,
    files: &[(usize, &str, bool)],
) -> Result<BTreeMap<usize, Slot>, String> {
    let videos: Vec<_> = files
        .iter()
        .copied()
        .filter(|(_, name, _)| post_processing::is_video_file(name))
        .collect();
    if let Some(plan) = grabbed_torrents::get_migration_replay(db, owner.id).await? {
        return plan
            .bind_files(&videos)?
            .into_iter()
            .map(|(i, f)| {
                if f.season != 1 {
                    return Err("Specials need source review".into());
                }
                Ok((
                    i,
                    Slot {
                        series_id: f.series_id,
                        anilist_id: f.anilist_id,
                        first: f.first,
                        last: f.last,
                    },
                ))
            })
            .collect();
    }
    if let Some(snapshot) = grabbed_torrents::get_numbering_snapshot(db, owner.id)
        .await
        .map_err(|e| e.to_string())?
    {
        return snapshot
            .bind_files(&videos)?
            .into_iter()
            .map(|(i, f)| {
                let m = f.mapped_episode()?.ok_or("Specials need source review")?;
                Ok((
                    i,
                    Slot {
                        series_id: f.series_id,
                        anilist_id: m.entry,
                        first: m.first,
                        last: m.last,
                    },
                ))
            })
            .collect();
    }
    let mapping = grabbed_torrents::get_release_mapping(db, owner.id)
        .await?
        .ok_or("No saved source-file mapping; review required")?;
    if mapping.bound_source().is_some() {
        let names: Vec<_> = files.iter().map(|(_, n, w)| (*n, *w)).collect();
        mapping.validate_bound_files(hash, &names)?;
    }
    let mut result = BTreeMap::new();
    for &(i, name, wanted) in files {
        if !wanted || !post_processing::is_video_file(name) {
            continue;
        }
        let basename = Path::new(name)
            .file_name()
            .and_then(|p| p.to_str())
            .ok_or("Invalid source filename")?;
        let m = if mapping.bound_source().is_some() {
            mapping.resolve_bound_file(name)?
        } else {
            mapping.resolve_named_file(basename)?
        };
        result.insert(
            i,
            Slot {
                series_id: owner.series_id,
                anilist_id: m.entry,
                first: m.first,
                last: m.last,
            },
        );
    }
    Ok(result)
}

async fn safe_path(root: &Path, name: &str, excluded: &[PathBuf]) -> Result<PathBuf, String> {
    post_processing::validate_relative_path_fragment(name).map_err(str::to_string)?;
    let path = root.join(name);
    let canonical = tokio::fs::canonicalize(&path)
        .await
        .map_err(|e| format!("Source file unavailable: {e}"))?;
    // No symlink may make a manifest entry stand for another file.
    if canonical != path
        || !canonical.starts_with(root)
        || excluded.iter().any(|p| canonical.starts_with(p))
    {
        return Err(
            "Source path leaves the download root or points into library/recovery files".into(),
        );
    }
    if !tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("Source is not a regular file".into());
    }
    Ok(canonical)
}
fn subtitle(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|p| p.to_str())
        .is_some_and(|p| matches!(p.to_ascii_lowercase().as_str(), "srt" | "ass" | "ssa"))
}

/// All evidence is read again; the persisted review is never an authorization.
/// `probe_sources` is reserved for the explicit, owned source-check operation.
pub async fn evaluate(
    state: &AppState,
    cfg: &Config,
    client_id: i64,
    item: &DownloadItem,
    client: &Arc<dyn DownloadClient>,
    probe_sources: bool,
) -> Decision {
    match evaluate_inner(state, cfg, client_id, item, client, probe_sources).await {
        Ok(d) => d,
        Err(reason) => unknown(reason),
    }
}
async fn evaluate_inner(
    state: &AppState,
    cfg: &Config,
    client_id: i64,
    item: &DownloadItem,
    client: &Arc<dyn DownloadClient>,
    probe_sources: bool,
) -> Result<Decision, String> {
    if client.protocol() != "torrent" || normalize_info_hash(&item.hash).is_none() {
        return Err("Not a supported torrent identity".into());
    }
    if !item.state_kind.is_complete()
        || item.state_kind == DownloadItemState::CheckingSeed
        || item.progress != 1.0
    {
        return Err("Native job completion is not verified".into());
    }
    let row = download_clients::get_by_id(&state.db, client_id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|r| r.enabled && !r.is_usenet())
        .ok_or("Recorded client is disabled or unavailable")?;
    let owned = owners(&state.db, &item.hash).await?;
    if owned.len() != 1 || owned[0].download_client_id != Some(client_id) {
        return Err("Torrent has unresolved or conflicting ownership".into());
    }
    let owner = &owned[0];
    if owner.verification.as_deref() == Some("misgrab")
        || matches!(owner.state.as_str(), "failed" | "removed" | "cancelled")
    {
        return Ok(Decision::Rejected(format!(
            "Import state: {}. {}",
            owner.state, owner.failure_reason
        )));
    }
    if !matches!(owner.state.as_str(), "imported" | "replaced")
        || owner.imported_at.is_none()
        || !matches!(owner.import_mode.as_deref(), Some("copy" | "hardlink"))
    {
        return Err("No complete copy/hardlink import; pending, partial, protected and moved sources require review".into());
    }
    if !matches!(
        owner.verification.as_deref(),
        Some("verified" | "whitelisted")
    ) {
        return Err("Source title identity was not verified".into());
    }
    let coverage: BTreeMap<i64, Vec<i32>> = serde_json::from_str(
        owner
            .imported_episode_coverage
            .as_deref()
            .ok_or("Legacy import has no recorded actual coverage; review required")?,
    )
    .map_err(|_| "Invalid imported coverage")?;
    let paths: Vec<String> = serde_json::from_str(
        owner
            .imported_source_paths
            .as_deref()
            .ok_or("Import has no original source paths")?,
    )
    .map_err(|_| "Invalid imported source paths")?;
    if coverage.is_empty() || paths.is_empty() {
        return Err("Import did not establish source ownership".into());
    }
    if item.save_path.is_empty() {
        return Err("Client did not report a download root".into());
    }
    let local = translate_client_path(&item.save_path, &item.save_path, &row.download_path);
    let root = tokio::fs::canonicalize(&local)
        .await
        .map_err(|_| "Download mount is unavailable")?;
    let mut excluded = Vec::new();
    if cfg.media_root.trim().is_empty() {
        return Err("Library root is not configured".into());
    }
    // An unavailable exclusion root cannot prove source/library separation.
    excluded.push(
        tokio::fs::canonicalize(&cfg.media_root)
            .await
            .map_err(|_| "Library mount is unavailable")?,
    );
    if !cfg.recycle_bin_path.trim().is_empty() {
        excluded.push(
            tokio::fs::canonicalize(&cfg.recycle_bin_path)
                .await
                .map_err(|_| "Recovery mount is unavailable")?,
        );
    }
    if excluded.iter().any(|p| root.starts_with(p)) {
        return Err("Download root overlaps library or recovery files".into());
    }
    let files = client.get_files(&item.hash).await?;
    if files.is_empty() || files.len() > 4096 {
        return Err("Source manifest is empty or too large".into());
    }
    let mut names = BTreeSet::new();
    let mut media = Vec::new();
    let mut sidecars = Vec::new();
    let mut available = BTreeSet::new();
    for (i, f) in files.iter().enumerate() {
        post_processing::validate_relative_path_fragment(&f.name).map_err(str::to_string)?;
        if !names.insert(&f.name) {
            return Err("Duplicate source path".into());
        }
        if !f.progress.is_finite() || !(0.0..=1.0).contains(&f.progress) || f.size < 0 {
            return Err("Invalid native file status".into());
        }
        if !post_processing::is_video_file(&f.name) && !subtitle(&f.name) {
            continue;
        }
        let present = tokio::fs::try_exists(root.join(&f.name))
            .await
            .map_err(|e| e.to_string())?;
        // Unwanted files can still contain uploadable pieces. They are not an upload ACL.
        if !f.wanted && f.progress == 0.0 && !present {
            continue;
        }
        let path = safe_path(&root, &f.name, &excluded).await?;
        if f.progress != 1.0
            || f.size <= 0
            || tokio::fs::metadata(&path)
                .await
                .map_err(|e| e.to_string())?
                .len()
                != f.size as u64
        {
            return Err(format!("Source file is partial or changed: {}", f.name));
        }
        if post_processing::is_video_file(&f.name) {
            available.insert(i);
            media.push((i, path));
        } else {
            sidecars.push(path);
        }
    }
    if media.is_empty() {
        return Err("No complete source videos".into());
    }
    let manifest: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.name.as_str(), available.contains(&i)))
        .collect();
    let total_videos = files
        .iter()
        .filter(|f| post_processing::is_video_file(&f.name))
        .count();
    let slots = bind_slots(&state.db, owner, &item.hash, &manifest).await?;
    let ids: Vec<_> = slots
        .values()
        .map(|s| s.series_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let settings = language_preferences::resolve_snapshot(&state.db, cfg, &ids).await?;
    let mut identities = Vec::new();
    for (i, path) in &media {
        let slot = slots
            .get(i)
            .ok_or("Available source video has no saved owner")?;
        let s = crate::models::series::get_by_id(&state.db, slot.series_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Source owner was removed")?;
        if s.anilist_id != slot.anilist_id
            || slot.first < 1
            || slot.last < slot.first
            || s.episodes.is_some_and(|n| slot.last > n)
        {
            return Err("Saved source owner no longer matches the library identity".into());
        }
        if !(slot.first..=slot.last).all(|ep| {
            coverage
                .get(&slot.series_id)
                .is_some_and(|v| v.contains(&ep))
        }) {
            return Err("Available source video was not fully accepted by the import".into());
        }
        // Match the original source, not the current library copy.
        if !paths.iter().any(|p| Path::new(p) == path) {
            return Err("Available source video has no recorded imported path".into());
        }
        let effective = &settings
            .get(&slot.series_id)
            .ok_or("Requirements could not be resolved")?
            .as_ref()
            .map_err(Clone::clone)?
            .config;
        if probe_sources {
            let span = path
                .file_name()
                .and_then(|p| p.to_str())
                .and_then(|n| crate::services::media::parse_episode_span(&n.to_ascii_lowercase()));
            media_info::inspect_with_sidecars(&state.db, path, &sidecars, span, total_videos)
                .await
                .map_err(|e| e.to_string())?;
        }
        let info =
            media_info::inspect_cached_source(&state.db, path, &sidecars, total_videos).await?;
        identities.push((path.clone(), info.identity.clone()));
        let policy = Policy::from_config(effective);
        if let Err(reason) = policy.validate_minimum(&info) {
            return Ok(Decision::Rejected(format!("{}: {reason}", files[*i].name)));
        }
        // The importer also enforces required subtitles in legacy quality mode.
        if !policy.enabled {
            let verdict = media_info::evaluate_subtitle_requirements(
                &info,
                &policy.required_languages,
                &policy.language_match,
            );
            if !verdict.satisfied {
                return Ok(Decision::Rejected(format!(
                    "{}: {}",
                    files[*i].name,
                    verdict.reasons.join("; ")
                )));
            }
        }
    }
    // The complete native manifest and strong file identities must still agree.
    let again = client.get_files(&item.hash).await?;
    if !same_manifest(&files, &again) {
        return Err("Native source manifest or file selection changed during review".into());
    }
    for (path, id) in identities {
        if media_info::FileIdentity::from_metadata(
            &tokio::fs::metadata(path).await.map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?
            != id
        {
            return Err("Source changed during review".into());
        }
    }
    let current_cfg = crate::models::config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    let current_settings =
        language_preferences::resolve_snapshot(&state.db, &current_cfg, &ids).await?;
    for id in &ids {
        let before = settings
            .get(id)
            .ok_or("Requirements unavailable")?
            .as_ref()
            .map_err(Clone::clone)?;
        let after = current_settings
            .get(id)
            .ok_or("Requirements unavailable")?
            .as_ref()
            .map_err(Clone::clone)?;
        if Policy::from_config(&before.config) != Policy::from_config(&after.config) {
            return Err("Requirements changed during source review".into());
        }
    }
    if owners(&state.db, &item.hash).await? != owned {
        return Err("Import ownership changed during source review".into());
    }
    Ok(Decision::Accepted)
}
fn same_manifest(a: &[DownloadFile], b: &[DownloadFile]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.name == b.name && a.size == b.size && a.wanted == b.wanted && a.progress == b.progress
        })
}

/// Explicit source audits own CHECK_LOCK. The minute sweep can run alongside
/// them: all controls are pause-only and recheck current ownership and policy.
/// Checks never delete, resume, reselect or acquire.
pub async fn check_all(state: &AppState, probe_sources: bool) -> Result<(), String> {
    let cfg = crate::models::config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    if !cfg.accepted_only_seeding && !probe_sources {
        return Ok(());
    }
    let pool = state.download_clients.read().await.clone();
    for (id, client) in &pool.clients {
        if client.protocol() != "torrent" {
            continue;
        }
        let row = download_clients::get_by_id(&state.db, *id)
            .await
            .map_err(|e| e.to_string())?;
        if !row.as_ref().is_some_and(|r| r.enabled && !r.is_usenet()) {
            continue;
        }
        let items = match client.list_scoped().await {
            Ok(items) => items,
            Err(e) => {
                crate::services::logger::warn(
                    &state.db,
                    crate::models::log::LogCategory::DownloadClient,
                    "Seeding review could not read a client; other clients will still be checked",
                    &format!("Client {id}: {e}"),
                )
                .await;
                sqlx::query("UPDATE torrent_seed_reviews SET verdict='Unverified', reason='Native client listing unavailable', control='No control sent', checked_at=CURRENT_TIMESTAMP WHERE client_id=?")
                    .bind(id).execute(&state.db).await.map_err(|e|e.to_string())?;
                continue;
            }
        };
        for item in items {
            let Some(hash) = normalize_info_hash(&item.hash) else {
                continue;
            };
            let owned = owners(&state.db, &hash).await?;
            if owned.is_empty() {
                continue;
            }
            let exact_owner = owned.len() == 1 && owned[0].download_client_id == Some(*id);
            let (verdict, reason, mut control) = if item.state_kind.is_complete() {
                let d = evaluate(state, &cfg, *id, &item, client, probe_sources).await;
                let (v, r) = d.fields();
                (
                    v.to_string(),
                    r,
                    if item.state_kind == DownloadItemState::PausedComplete {
                        "Already paused; never resumed".into()
                    } else {
                        "No control change".into()
                    },
                )
            } else {
                (
                    "Downloading / unavailable".into(),
                    "Incomplete downloads and client rechecks are not seed-certified".into(),
                    "No control change".into(),
                )
            };
            let active = matches!(
                item.state_kind,
                DownloadItemState::Seeding
                    | DownloadItemState::SeedingStalled
                    | DownloadItemState::SeedingQueued
            );
            if cfg.accepted_only_seeding && verdict != "Accepted" && active && exact_owner {
                // Persist the reason before control; a failed stop remains visible and is retried.
                let pending = Review {
                    client_id: *id,
                    hash: hash.clone(),
                    name: item.name.clone(),
                    verdict: verdict.clone(),
                    reason: reason.clone(),
                    control: "Stop pending".into(),
                    checked_at: String::new(),
                };
                crate::models::seed_policy::save(&state.db, &pending)
                    .await
                    .map_err(|e| e.to_string())?;
                // Do not adopt an owner or route through a replacement default client.
                let _config_guard = crate::handlers::settings::CONFIG_WRITE_LOCK.lock().await;
                let still_enabled = crate::models::config::get_config(&state.db)
                    .await
                    .map_err(|e| e.to_string())?
                    .is_some_and(|c| c.accepted_only_seeding);
                let current = owners(&state.db, &hash).await?;
                let current_row = download_clients::get_by_id(&state.db, *id)
                    .await
                    .map_err(|e| e.to_string())?;
                let enabled = current_row == row;
                let same_client = state
                    .download_clients
                    .read()
                    .await
                    .clients
                    .get(id)
                    .is_some_and(|c| Arc::ptr_eq(c, client));
                let still_complete = client.list_scoped().await.ok().is_some_and(|list| {
                    list.iter().any(|i| {
                        i.hash.eq_ignore_ascii_case(&hash)
                            && i.save_path == item.save_path
                            && i.content_path == item.content_path
                            && i.size == item.size
                            && i.progress == 1.0
                            && matches!(
                                i.state_kind,
                                DownloadItemState::Seeding
                                    | DownloadItemState::SeedingStalled
                                    | DownloadItemState::SeedingQueued
                            )
                    })
                });
                if still_enabled && enabled && same_client && current == owned && still_complete {
                    control = match client.pause(&hash).await {
                        Err(e) => format!("Stop failed; retry pending: {e}"),
                        Ok(()) => match client.list_scoped().await {
                            Ok(list)
                                if list.iter().any(|i| {
                                    i.hash.eq_ignore_ascii_case(&hash)
                                        && i.state_kind == DownloadItemState::PausedComplete
                                }) =>
                            {
                                "Paused; files retained".into()
                            }
                            _ => "Stop requested; awaiting native confirmation".into(),
                        },
                    };
                } else {
                    control = "Policy, ownership or native state changed; no control sent".into();
                }
            }
            if !exact_owner {
                control = "Unresolved ownership; no control sent".into();
            }
            crate::models::seed_policy::save(
                &state.db,
                &Review {
                    client_id: *id,
                    hash,
                    name: item.name,
                    verdict,
                    reason,
                    control,
                    checked_at: String::new(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Retain rejected BT sources before legacy automatic cleanup can remove them.
/// Retry, blocklist and import-history decisions remain with the original caller.
/// Caller holds CONFIG_WRITE_LOCK through this check and any legacy delete.
pub async fn retain_rejected_source(
    state: &AppState,
    grab: &grabbed_torrents::GrabbedTorrent,
    client: &Arc<dyn DownloadClient>,
) -> bool {
    if client.protocol() != "torrent" {
        return false;
    }
    let retain = match crate::models::config::get_config(&state.db).await {
        Ok(Some(cfg)) => cfg.accepted_only_seeding,
        Ok(None) => true,
        Err(_) => true,
    };
    if !retain {
        return false;
    }
    let Some(id) = grab.download_client_id else {
        return true;
    };
    let current = state
        .download_clients
        .read()
        .await
        .clients
        .get(&id)
        .is_some_and(|c| Arc::ptr_eq(c, client));
    let enabled = download_clients::get_by_id(&state.db, id)
        .await
        .ok()
        .flatten()
        .is_some_and(|c| c.enabled && !c.is_usenet());
    let owner: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM grabbed_torrents WHERE id=? AND download_client_id=? AND lower(hash)=? AND series_id=? AND torrent_name=? AND grabbed_at=?)")
        .bind(grab.id).bind(id).bind(grab.hash.to_ascii_lowercase()).bind(grab.series_id).bind(&grab.torrent_name).bind(&grab.grabbed_at).fetch_one(&state.db).await.unwrap_or(false);
    let exact = owners(&state.db, &grab.hash)
        .await
        .ok()
        .is_some_and(|rows| {
            rows.len() == 1
                && rows[0].id == grab.id
                && rows[0].download_client_id == Some(id)
                && matches!(rows[0].state.as_str(), "pending" | "failed")
        });
    let mut control = "Unresolved ownership/client; files retained".to_string();
    if current && enabled && owner && exact {
        control = match client.list_scoped().await {
            Ok(items) => match items
                .iter()
                .find(|i| i.hash.eq_ignore_ascii_case(&grab.hash))
            {
                Some(i)
                    if matches!(
                        i.state_kind,
                        DownloadItemState::Paused | DownloadItemState::PausedComplete
                    ) =>
                {
                    "Already paused; files retained".into()
                }
                Some(_) => match client.pause(&grab.hash).await {
                    Ok(()) => "Stop requested; files retained".into(),
                    Err(e) => format!("Stop failed; files retained: {e}"),
                },
                None => "Job absent; no control sent".into(),
            },
            Err(_) => "Native state unavailable; files retained".into(),
        };
    }
    let _ = crate::models::seed_policy::save(
        &state.db,
        &Review {
            client_id: id,
            hash: grab.hash.to_ascii_lowercase(),
            name: grab.torrent_name.clone(),
            verdict: "Rejected".into(),
            reason: "Rejected source retained for review; automatic file removal suppressed".into(),
            control,
            checked_at: String::new(),
        },
    )
    .await;
    true
}

pub async fn tick(state: &AppState) {
    if let Err(e) = check_all(state, false).await {
        crate::services::logger::warn(
            &state.db,
            crate::models::log::LogCategory::DownloadClient,
            "Accepted-only seeding check failed",
            &e,
        )
        .await;
    }
}

#[cfg(test)]
mod tests;

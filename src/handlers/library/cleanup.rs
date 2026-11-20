//! Shared per-series file/torrent cleanup. Both the per-row Remove
//! handler ([`crud::remove_series`]) and the bulk delete handler
//! ([`bulk::delete_one_series`]) call into [`cleanup_series_files`] so
//! the security-critical canonicalize-and-`starts_with` guard, the PT
//! seed-rule honor (issue #28), the SAB stamped-source-path
//! cleanup, and the Jellyfin refresh nudge all stay in lockstep.
//!
//! Before this lived in one place, the bulk path silently bypassed the
//! traversal guard and the seed-rule check — issue surfaced in PR #164
//! review. Whenever one path changes, the other has to change too;
//! keeping them in one helper is the only way that doesn't drift.
//!
//! [`crud::remove_series`]: super::crud::remove_series
//! [`bulk::delete_one_series`]: super::bulk

use std::path::Path;

use tokio::sync::MutexGuard;

use crate::AppState;
use crate::models::{grabbed_torrents, series};
use crate::services::recycle::{self, RecycleKind, RecycleOutcome};

/// Outcome of a single series's filesystem + torrent cleanup pass. Each
/// field tracks one substage so callers can compose their own UX:
/// `crud::remove_series` serializes the whole report into its JSON
/// response, while `bulk::delete_one_series` checks
/// [`SeriesCleanupReport::is_clean`] to decide whether to count the
/// series as `succeeded` or fold the partial-failure signal into a
/// `BulkFailure.reason`.
#[derive(Debug)]
pub struct SeriesCleanupReport {
    /// Count of grabs whose torrent client accepted the delete (or
    /// whose seed rules say to keep seeding — counted as "handled" so
    /// the user sees the grab was processed even though the file
    /// stays on the seedbox).
    pub torrents_removed: u64,
    /// Per-grab error strings: `"Download client not configured"`,
    /// `"<hash>: <client error>"`, or `"clear grabbed_torrents: <db
    /// error>"`. Empty when every grab was handled cleanly.
    pub torrent_failures: Vec<String>,
    /// One of `"skipped"` (delete_files=false or empty media_root /
    /// folder_name), `"recycled"` (moved into the recycle bin, #123),
    /// `"removed"`, `"missing"` (canonicalize-NotFound;
    /// folder already gone), or `"shared"` (another series row uses the
    /// folder, so it and its files are kept).
    pub folder_status: &'static str,
    /// Detail for `folder_status`, such as the removed path, the recycle
    /// entry ID, or the other series' title for `"shared"`.
    pub folder_detail: String,
    /// `"skipped"` (no Jellyfin client configured), `"refreshed"`, or
    /// `"error"`. Refresh runs whenever the cleanup actually does
    /// work (delete_files=true); `"skipped"` is the no-Jellyfin
    /// posture, distinct from "we didn't ask."
    pub jellyfin_status: &'static str,
}

impl SeriesCleanupReport {
    /// `true` when the cleanup completed without any partial-failure
    /// signal — every torrent client accepted the delete and the
    /// folder either removed cleanly, was already missing, or was
    /// skipped (delete_files=false). Used by the bulk path to decide
    /// whether to count the series as `succeeded` or fold the partial
    /// signal into `BulkFailure.reason`. The per-row path doesn't use
    /// this — its JSON response always reports the full breakdown.
    pub fn is_clean(&self) -> bool {
        self.torrent_failures.is_empty()
            && matches!(
                self.folder_status,
                "recycled" | "removed" | "missing" | "skipped"
            )
    }

    /// One-line summary for the bulk failure-modal copy. Only called
    /// when `is_clean()` is false; assembles the partial-failure
    /// reason out of the substage signals so the user sees what
    /// didn't get cleaned. The reason rides back to the frontend as
    /// JSON via [`BulkOutcome`](super::bulk::BulkOutcome) — no
    /// HX-Trigger involvement — so any UTF-8 the filesystem can
    /// produce in `folder_detail` is fine to interpolate as-is.
    ///
    /// [`BulkOutcome`]: super::bulk::BulkOutcome
    pub fn partial_failure_reason(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.folder_status == "shared" {
            parts.push(format!(
                "files kept because {} uses the same folder",
                self.folder_detail
            ));
        }
        if !self.torrent_failures.is_empty() {
            parts.push(format!("torrent: {}", self.torrent_failures.join("; ")));
        }
        if parts.is_empty() {
            "partial cleanup failure".to_string()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Debug)]
pub struct SeriesCleanupFailure {
    pub stage: &'static str,
    pub message: String,
}

/// Keep Ryokan importers out until the caller finishes the catalog removal.
/// The recycle copy still verifies changes from external writers such as Tdarr.
pub(super) async fn lock_media_removal() -> (MutexGuard<'static, ()>, MutexGuard<'static, ()>) {
    use crate::services::manual_import::import::IMPORT_LOCK;
    use crate::services::post_processing::POST_PROC_LOCK;

    loop {
        let post_processing = POST_PROC_LOCK.lock().await;
        if let Ok(manual_import) = IMPORT_LOCK.try_lock() {
            return (post_processing, manual_import);
        }
        // A manual import takes the post-processing lock when classifying
        // files. Release it before waiting for that import to finish.
        drop(post_processing);
        drop(IMPORT_LOCK.lock().await);
    }
}

/// Per-series cleanup pass shared by [`crud::remove_series`] and
/// [`bulk::delete_one_series`]. Runs whenever the user asked for
/// `delete_files=true`; callers that don't want filesystem cleanup
/// shouldn't call this at all.
///
/// 1. List grabs for the series without changing them.
/// 2. Validate and remove the series folder. A failed move or verification
///    stops removal before touching the download jobs or the catalog.
/// 3. Remove owned jobs and downloaded source files, respecting seed rules.
///    Clear their grab rows after client cleanup.
/// 4. Nudge Jellyfin to rescan so the user's library doesn't show
///    ghost entries until the next refresh tick. Best-effort; logged
///    on failure but doesn't gate cleanup success.
///
/// The DB cascade (`series::remove`) is **not** part of this helper —
/// callers run it after the helper returns a report.
///
/// [`crud::remove_series`]: super::crud::remove_series
/// [`bulk::delete_one_series`]: super::bulk
pub async fn cleanup_series_files(
    state: &AppState,
    series_id: i64,
    folder_name: &str,
    series_title: &str,
    media_root: Option<&str>,
    recycle_bin_path: &str,
) -> Result<SeriesCleanupReport, SeriesCleanupFailure> {
    let hashes = grabbed_torrents::get_all_for_series(&state.db, series_id)
        .await
        .map_err(|error| SeriesCleanupFailure {
            stage: "list_grabbed_torrents",
            message: format!("Could not list the download jobs ({error})."),
        })?;
    let (folder_status, folder_detail) =
        remove_series_folder(state, series_id, folder_name, series_title, media_root, recycle_bin_path)
            .await
            .map_err(|error| SeriesCleanupFailure {
                stage: "cleanup_folder",
                message: format!(
                    "Could not remove the series folder. The library entry and download jobs were kept. {error}"
                ),
            })?;
    let mut report = SeriesCleanupReport {
        torrents_removed: 0,
        torrent_failures: Vec::new(),
        folder_status,
        folder_detail,
        jellyfin_status: "skipped",
    };

    for (grab_id, hash, dc_id) in &hashes {
        if hash.is_empty() {
            continue;
        }
        // Issue #28 — preserve PT seed rules across removal. A
        // user wiping a series typically wants ratio policies
        // honored; the `grabbed_torrents` row gets dropped below
        // either way (delete_all_for_series), so the upgrade sweep
        // can't re-grab the same hash.
        if grabbed_torrents::respects_seed_rules(&state.db, hash).await {
            report.torrents_removed += 1;
            continue;
        }
        // Every hash the series ever had used to be deleted with its
        // data, including one another series now downloads (a cancelled
        // grab here, re-grabbed there) or a batch that also feeds a
        // sibling. That download is the other series' to remove.
        if grabbed_torrents::hash_in_use_elsewhere(&state.db, hash, series_id, *grab_id).await {
            tracing::info!(
                grab_id,
                "series delete: leaving torrent {hash} in the client, another series uses it"
            );
            continue;
        }
        let Some(client) = state.resolve_grab_client(*dc_id, hash).await else {
            report
                .torrent_failures
                .push("Download client not configured".to_string());
            continue;
        };
        match client.delete(hash, true).await {
            Ok(()) => report.torrents_removed += 1,
            Err(err) => report.torrent_failures.push(format!("{}: {}", hash, err)),
        }

        // SAB stamped-source-path cleanup. Same rationale as
        // `delete_episode_file`: the client's delete is unreliable
        // for SAB jobs whose history `storage` field is the parent
        // complete dir. Stamped paths are precise and mode-agnostic.
        let stamped = grabbed_torrents::get_imported_source_paths(&state.db, *grab_id).await;
        if !stamped.is_empty() {
            super::episodes::remove_stamped_source_paths(&stamped).await;
        }
    }

    // Drop grabbed_torrents rows so they don't dangle.
    if let Err(err) = grabbed_torrents::delete_all_for_series(&state.db, series_id).await {
        report
            .torrent_failures
            .push(format!("clear grabbed_torrents: {}", err));
    }

    // Nudge Jellyfin. Best-effort.
    if let Some(root) = media_root
        && let Err(e) = crate::services::jellyfin_view::sync(&state.db, root).await
    {
        report.jellyfin_status = "error";
        report
            .folder_detail
            .push_str(&format!("; Jellyfin view: {e}"));
        return Ok(report);
    }
    let jellyfin_opt = state.jellyfin.read().await.clone();
    if let Some(jelly) = jellyfin_opt {
        report.jellyfin_status = match jelly.refresh_library().await {
            Ok(()) => "refreshed",
            Err(_) => "error",
        };
    }

    Ok(report)
}

async fn remove_series_folder(
    state: &AppState,
    series_id: i64,
    folder_name: &str,
    series_title: &str,
    media_root: Option<&str>,
    recycle_bin_path: &str,
) -> Result<(&'static str, String), String> {
    let Some(root) =
        media_root.filter(|root| !root.trim().is_empty() && !folder_name.trim().is_empty())
    else {
        return Ok(("skipped", String::new()));
    };
    let root = Path::new(root);
    if !crate::services::media::valid_library_location(root, folder_name) {
        return Err(format!("Refused to use the folder name {folder_name:?}."));
    }
    // Legacy entries can share a folder. Keep it for the other title.
    if let Some(other) = series::other_series_in_folder(&state.db, series_id, folder_name)
        .await
        .map_err(|err| format!("Could not check folder ownership ({err})."))?
    {
        return Ok(("shared", other));
    }

    let folder = root.join(folder_name);
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|err| format!("Could not resolve the media root ({err})."))?;
    let canonical = match tokio::fs::canonicalize(&folder).await {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(("missing", folder.display().to_string()));
        }
        Err(err) => return Err(format!("Could not resolve {} ({err}).", folder.display())),
    };
    // The media root itself is never a series folder.
    if !canonical.starts_with(&root) || canonical == root {
        return Err(format!(
            "Refused to remove {}. Its resolved path ({}) is at or outside the media root.",
            folder.display(),
            canonical.display()
        ));
    }
    let outcome = recycle::recycle(
        &state.db,
        recycle_bin_path,
        RecycleKind::SeriesFolder,
        Some(series_id),
        series_title,
        &canonical,
    )
    .await?;
    let path = canonical.display();
    Ok(match outcome {
        RecycleOutcome::Recycled { entry_id } => {
            ("recycled", format!("{path} (recycle entry {entry_id})"))
        }
        RecycleOutcome::DirectDeleted => ("removed", path.to_string()),
        RecycleOutcome::Missing => ("missing", path.to_string()),
    })
}

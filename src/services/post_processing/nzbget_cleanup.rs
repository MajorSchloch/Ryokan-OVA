//! A receipt binds cleanup to the actual files of a verified new import.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::{config, download_clients, grabbed_torrents};
use crate::services::download_client::DownloadClient;
use crate::services::media_info::FileIdentity;

use super::nzbget_cleanup_files::{DownloadCopy, prepare_cleanup};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImportedCopy {
    paths: DownloadCopy,
    source_identity: String,
    library_identity: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    grabbed_at: String,
    source_root: PathBuf,
    files: Vec<ImportedCopy>,
}

pub(super) fn is_nzbget(client: &dyn DownloadClient) -> bool {
    client.protocol() == "usenet" && client.sonarr_impl_name() == "Nzbget"
}

async fn file_identity(path: &Path) -> Result<Option<String>, String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_file() && meta.len() > 0 => FileIdentity::from_metadata(&meta)
            .map(|identity| Some(identity.cache_key()))
            .map_err(|error| error.to_string()),
        Ok(_) => Err("Import cleanup requires a regular, nonempty video".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

impl ImportedCopy {
    pub(super) async fn observe(source: &Path, library: &Path) -> Option<Self> {
        Some(Self {
            paths: DownloadCopy {
                source: source.to_path_buf(),
                library: library.to_path_buf(),
            },
            source_identity: file_identity(source).await.ok()??,
            library_identity: file_identity(library).await.ok()??,
        })
    }
}

impl Receipt {
    pub(super) fn new(
        grab: &grabbed_torrents::GrabbedTorrent,
        source_root: &str,
        files: Vec<ImportedCopy>,
    ) -> Self {
        Self {
            grabbed_at: grab.grabbed_at.clone(),
            source_root: source_root.into(),
            files,
        }
    }
}

pub(super) async fn finish_import(
    db: &SqlitePool,
    grab: &grabbed_torrents::GrabbedTorrent,
    mode: &str,
    receipt: Receipt,
) -> Result<bool, sqlx::Error> {
    let sources: Vec<_> = receipt
        .files
        .iter()
        .map(|file| &file.paths.source)
        .collect();
    let encoded_sources =
        serde_json::to_string(&sources).map_err(|error| sqlx::Error::Decode(error.into()))?;
    let json =
        serde_json::to_string(&receipt).map_err(|error| sqlx::Error::Decode(error.into()))?;
    let result = sqlx::query(
        "UPDATE grabbed_torrents SET nzbget_cleanup_receipt=?1, import_mode=?2,
             state='imported', imported_at=CURRENT_TIMESTAMP
         WHERE id=?3 AND hash=?4 AND grabbed_at=?5 AND download_client_id=?6 AND state='pending'
           AND library_migration_bridge IS NULL AND imported_source_paths=?7
           AND imported_episode_coverage IS NOT NULL AND imported_episode_coverage != '{}'
           AND EXISTS (SELECT 1 FROM download_clients d WHERE d.id=?6 AND d.kind='nzbget'
                       AND d.enabled=1 AND d.remove_completed=1)",
    )
    .bind(json)
    .bind(mode)
    .bind(grab.id)
    .bind(&grab.hash)
    .bind(&grab.grabbed_at)
    .bind(grab.download_client_id)
    .bind(encoded_sources)
    .execute(db)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn receipt_json(
    db: &SqlitePool,
    grab: &grabbed_torrents::GrabbedTorrent,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT nzbget_cleanup_receipt FROM grabbed_torrents WHERE id=? AND hash=? AND grabbed_at=?
         AND download_client_id=? AND state='imported' AND imported_at IS NOT NULL
         AND import_mode IN ('copy', 'hardlink') AND client_removed_at IS NULL",
    )
    .bind(grab.id)
    .bind(&grab.hash)
    .bind(&grab.grabbed_at)
    .bind(grab.download_client_id)
    .fetch_optional(db)
    .await
    .map(Option::flatten)
}

pub(super) async fn is_registered(
    db: &SqlitePool,
    grab: &grabbed_torrents::GrabbedTorrent,
) -> Result<bool, sqlx::Error> {
    receipt_json(db, grab)
        .await
        .map(|receipt| receipt.is_some())
}

pub(super) async fn remove(
    state: &AppState,
    grab: &grabbed_torrents::GrabbedTorrent,
    client_id: i64,
    client: &Arc<dyn DownloadClient>,
) -> Result<usize, String> {
    let _config_guard = crate::handlers::settings::CONFIG_WRITE_LOCK.lock().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|error| error.to_string())?
        .filter(|cfg| cfg.post_processing_enabled && !cfg.media_root.trim().is_empty())
        .ok_or("Import settings changed")?;
    let route = download_clients::get_by_id(&state.db, client_id)
        .await
        .map_err(|error| error.to_string())?
        .filter(|row| row.kind == "nzbget" && row.enabled && row.remove_completed)
        .ok_or("NZBGet cleanup settings changed")?;
    if grab.download_client_id != Some(client_id)
        || !state
            .download_clients
            .read()
            .await
            .clients
            .get(&client_id)
            .is_some_and(|current| Arc::ptr_eq(current, client))
    {
        return Err("NZBGet cleanup settings or client changed".into());
    }
    let json = receipt_json(&state.db, grab)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("The completed import receipt is unavailable")?;
    let receipt: Receipt = serde_json::from_str(&json).map_err(|error| error.to_string())?;
    if receipt.grabbed_at != grab.grabbed_at || receipt.files.is_empty() {
        return Err("NZBGet import receipt or job folder changed".into());
    }
    if !cfg.recycle_bin_path.trim().is_empty() {
        let bin = tokio::fs::canonicalize(&cfg.recycle_bin_path)
            .await
            .map_err(|error| format!("Recovery root unavailable: {error}"))?;
        if receipt.source_root.starts_with(&bin) || bin.starts_with(&receipt.source_root) {
            return Err("The download folder overlaps the recovery bin".into());
        }
    }
    let copies: Vec<_> = receipt
        .files
        .iter()
        .map(|file| file.paths.clone())
        .collect();
    let identities: Vec<_> = receipt
        .files
        .iter()
        .map(|file| {
            (
                file.source_identity.as_str(),
                file.library_identity.as_str(),
            )
        })
        .collect();
    let proof = prepare_cleanup(&receipt.source_root, Path::new(&cfg.media_root), &copies).await?;
    if !proof.matches_import(&identities) {
        return Err("An imported source or its library copy changed".into());
    }

    // A re-grab can reuse the same row. Keep the attempt check and removal
    // under a writer lease after the potentially slow byte comparison.
    let mut tx = state
        .db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|error| error.to_string())?;
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM grabbed_torrents WHERE id=?1 AND hash=?2
         AND grabbed_at=?3 AND download_client_id=?4 AND state='imported'
         AND client_removed_at IS NULL AND nzbget_cleanup_receipt=?5
         AND NOT EXISTS (SELECT 1 FROM pending_grabs WHERE info_hash=?2))",
    )
    .bind(grab.id)
    .bind(&grab.hash)
    .bind(&grab.grabbed_at)
    .bind(client_id)
    .bind(&json)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| error.to_string())?;
    if !owned
        || download_clients::get_by_id(&mut *tx, client_id)
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&route)
        || !state
            .download_clients
            .read()
            .await
            .clients
            .get(&client_id)
            .is_some_and(|current| Arc::ptr_eq(current, client))
    {
        return Err("The verified NZBGet import attempt changed".into());
    }
    let job_folder = receipt
        .source_root
        .to_str()
        .ok_or("Invalid NZBGet job path")?;
    let removed = match client
        .verify_completed_cleanup(&grab.hash, job_folder)
        .await
    {
        Ok(nzbid) => {
            let removed = proof.remove().await?;
            // The adapter checks the same native ID and success flags again.
            client
                .finish_completed_cleanup(&grab.hash, job_folder, nzbid)
                .await?;
            removed
        }
        // An acknowledged native removal can outlive a failed DB stamp or
        // a lost RPC response. Reconcile only after the source files are gone.
        Err(error) if proof.sources_absent() => {
            if client
                .list_scoped()
                .await?
                .iter()
                .any(|job| job.hash == grab.hash)
            {
                return Err(error);
            }
            proof.remove().await?
        }
        Err(error) => return Err(error),
    };
    sqlx::query("UPDATE grabbed_torrents SET client_removed_at=CURRENT_TIMESTAMP WHERE id=?")
        .bind(grab.id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(removed)
}

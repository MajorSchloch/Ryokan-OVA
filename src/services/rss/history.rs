//! Logical queue protection belongs to current owners, never RSS decision history.
use super::*;
use crate::services::download_client::{DownloadClient, DownloadItemState};

pub(super) async fn load(
    db: &sqlx::SqlitePool,
    client: Option<&dyn DownloadClient>,
    client_id: Option<i64>,
    all_meta: &[SeriesMeta],
) -> Result<CanonicalHistory, String> {
    let mut history = CanonicalHistory::default();
    // Routes are the selected ownership when a grab spans several releases.
    let pending = sqlx::query_as::<_, (i64, String, bool, String, String)>(
        "SELECT g.series_id,g.episode_numbers,g.is_batch,g.torrent_name,g.hash
         FROM grabbed_torrents g WHERE g.state='pending'
         AND NOT EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id)
         UNION ALL SELECT r.series_id,r.episode_numbers,g.is_batch,g.torrent_name,g.hash
         FROM grabbed_torrent_series r JOIN grabbed_torrents g ON g.id=r.grab_id
         WHERE g.state='pending'",
    )
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    let owned: HashSet<String> = pending
        .iter()
        .map(|row| row.4.to_ascii_lowercase())
        .collect();
    for (id, json, batch, title, _) in pending {
        let episodes: HashSet<i32> =
            serde_json::from_str(&json).map_err(|_| "Pending episode ownership is unreadable")?;
        if episodes
            .iter()
            .any(|ep| !(1..=crate::services::anilist::MAX_PLAUSIBLE_EPISODES).contains(ep))
        {
            return Err("Pending episode ownership is invalid".into());
        }
        if let Some(meta) = all_meta.iter().find(|m| m.series.id == id) {
            let family = related_family(meta, all_meta);
            let absolute = canonical_absolute_numbers(meta, &family, &episodes);
            let key = logical_key(id, &canonical_family_key(&family), &absolute, batch);
            if !key.is_empty() {
                history.items.entry(key).or_default().push(HistoryRelease {
                    revision: media::parse_release_revision(&title).version,
                    group: extract_group(&title),
                });
            }
        }
    }
    let terminal: HashSet<String> = sqlx::query_scalar(
        "SELECT lower(hash) FROM grabbed_torrents WHERE download_client_id = ?
         AND state IN ('failed','removed','cancelled','replaced','imported')",
    )
    .bind(client_id)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .collect();
    let previews: HashSet<String> = sqlx::query_scalar(
        "SELECT lower(info_hash) FROM pending_grabs WHERE (download_client_id IS ? OR download_client_id IS NULL)",
    )
    .bind(client_id)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .collect();
    if let Some(client) = client
        && let Ok(items) = client.list_scoped().await
    {
        for item in items {
            let hash = item.hash.to_ascii_lowercase();
            let preview = previews.contains(&hash);
            let active = matches!(
                item.state_kind,
                DownloadItemState::Downloading
                    | DownloadItemState::DownloadingStalled
                    | DownloadItemState::DownloadingQueued
                    | DownloadItemState::CheckingDownload
                    | DownloadItemState::Paused
            );
            if !owned.contains(&hash) && (preview || (active && !terminal.contains(&hash))) {
                history.insert_title(&item.name, all_meta);
            }
        }
    }
    Ok(history)
}

/// Inputs that must remain stable between RSS discovery and native dispatch.
/// Poll counters are excluded; credentials stay in memory only.
pub(super) struct Settings {
    config: serde_json::Value,
    clients: Vec<crate::models::download_clients::DownloadClientRow>,
    sources: serde_json::Value,
    feeds: serde_json::Value,
    pool: std::sync::Arc<crate::DownloadClientPool>,
    cfs: std::sync::Arc<Vec<crate::services::custom_formats::CompiledCustomFormat>>,
}
async fn feed_settings(db: &sqlx::SqlitePool) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(
        crate::models::direct_rss_feeds::list_all(db)
            .await
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    for row in value.as_array_mut().into_iter().flatten() {
        if let Some(row) = row.as_object_mut() {
            for key in [
                "last_polled_at",
                "last_poll_error",
                "last_item_count",
                "created_at",
                "updated_at",
            ] {
                row.remove(key);
            }
        }
    }
    Ok(value)
}
impl Settings {
    pub(super) async fn capture(state: &AppState, cfg: &config::Config) -> Result<Self, String> {
        Ok(Self {
            config: serde_json::to_value(cfg).map_err(|e| e.to_string())?,
            clients: crate::models::download_clients::list_all(&state.db)
                .await
                .map_err(|e| e.to_string())?,
            sources: crate::services::recent_episodes::indexer_settings(&state.db).await?,
            feeds: feed_settings(&state.db).await?,
            pool: state.download_clients.read().await.clone(),
            cfs: state.custom_formats.read().await.clone(),
        })
    }
    pub(super) async fn recheck(
        &self,
        state: &AppState,
        row: &series::Series,
        effective: &config::Config,
        monitored: &HashSet<i32>,
    ) -> Result<(), String> {
        let cfg = config::get_config(&state.db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Settings unavailable")?;
        let current = series::get_by_id(&state.db, row.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Release removed")?;
        let policy =
            crate::services::language_preferences::effective_config(&state.db, &cfg, row.id)
                .await?;
        let current_monitored: HashSet<_> =
            monitoring::get_monitored_episode_numbers(&state.db, row.id)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect();
        if !cfg.rss_master_enabled
            || self.config != serde_json::to_value(&cfg).map_err(|e| e.to_string())?
            || self.clients
                != crate::models::download_clients::list_all(&state.db)
                    .await
                    .map_err(|e| e.to_string())?
            || self.sources != crate::services::recent_episodes::indexer_settings(&state.db).await?
            || self.feeds != feed_settings(&state.db).await?
            || !std::sync::Arc::ptr_eq(&self.pool, &state.download_clients.read().await.clone())
            || !std::sync::Arc::ptr_eq(&self.cfs, &state.custom_formats.read().await.clone())
            || serde_json::to_value(current).map_err(|e| e.to_string())?
                != serde_json::to_value(row).map_err(|e| e.to_string())?
            || serde_json::to_value(policy).map_err(|e| e.to_string())?
                != serde_json::to_value(effective).map_err(|e| e.to_string())?
            || &current_monitored != monitored
        {
            return Err("Settings, monitoring, or source ownership changed during RSS discovery; deferred to the next check".into());
        }
        Ok(())
    }
}

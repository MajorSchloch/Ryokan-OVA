//! Bounded recovery for monitored episodes that have fallen out of current RSS.
//! This worker supplies demand only; the existing search and import gates decide
//! whether a source is usable. A queued source is never reported as an import.
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use serde_json::Value;
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::{config, download_clients, grabbed_torrents, library_activity, series};
use crate::services::{custom_formats::CompiledCustomFormat, media, redownload};

pub const TASK: &str = "recent_episode_search";
pub const STARTED: &str = "Automatic episode check started";
pub const FINISHED: &str = "Automatic episode check finished";
const PASS_LIMIT: usize = 12;
static RUN_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct Target {
    pub series_id: i64,
    pub provider_id: i64,
    pub episode: i32,
    pub airing_at: i64,
    pub folder_name: String,
    pub title: String,
}

pub fn enabled(cfg: &config::Config) -> bool {
    cfg.rss_master_enabled && cfg.recent_episode_search_enabled && cfg.post_processing_enabled
}

/// Routes, when present, replace the parent grab's episode ownership.
pub(crate) async fn has_pending(db: &SqlitePool, sid: i64, ep: i32) -> Result<bool, String> {
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM grabbed_torrents g WHERE g.state='pending' AND (
         (g.series_id=?1 AND NOT EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id)
          AND EXISTS(SELECT 1 FROM json_each(g.episode_numbers) WHERE value=?2)) OR
         EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id AND r.series_id=?1
          AND EXISTS(SELECT 1 FROM json_each(r.episode_numbers) WHERE value=?2))))",
    )
    .bind(sid)
    .bind(ep)
    .fetch_one(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(pending)
}

pub(crate) async fn retry_allowed(
    db: &SqlitePool,
    cfg: &config::Config,
    sid: i64,
    ep: i32,
) -> Result<(), String> {
    let last: Option<(String, String)> = sqlx::query_as(
        "SELECT g.state,COALESCE(g.failure_reason,'') FROM grabbed_torrents g WHERE (
         (g.series_id=?1 AND NOT EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id)
          AND EXISTS(SELECT 1 FROM json_each(g.episode_numbers) WHERE value=?2)) OR
         EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id AND r.series_id=?1
          AND EXISTS(SELECT 1 FROM json_each(r.episode_numbers) WHERE value=?2))) ORDER BY g.grabbed_at DESC,g.id DESC LIMIT 1",
    ).bind(sid).bind(ep).fetch_optional(db).await.map_err(|e| e.to_string())?;
    if let Some((state, reason)) = last {
        // Existing queue revision rules remain with the RSS caller.
        if state == "pending" {
            return Ok(());
        }
        if state != "failed" || !matches!(reason.as_str(), "client_error" | "requirements_rejected")
        {
            return Err(format!(
                "Previous attempt is {state}; review it before another automatic download"
            ));
        }
        if !cfg.auto_redownload_failed {
            return Err("Automatic retry of failed downloads is disabled".into());
        }
    }
    let failed = grabbed_torrents::count_recent_failed(db, sid, redownload::WINDOW_HOURS)
        .await
        .map_err(|e| e.to_string())?;
    if failed >= redownload::LOOP_BREAKER {
        return Err(format!(
            "Retry limit reached: {failed} failures in {} hours",
            redownload::WINDOW_HOURS
        ));
    }
    Ok(())
}

/// A fresh, explicit monitor row is required. Unknown monitoring is a hold.
async fn demand(db: &SqlitePool, cfg: &config::Config, t: &Target) -> Result<(), String> {
    if !enabled(cfg) {
        return Err("Automatic new episode recovery is disabled".into());
    }
    let wanted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM series s JOIN episode_monitor_state m ON m.series_id=s.id
         JOIN episode_airings a ON a.series_id=s.id AND a.episode=m.episode_number
         WHERE s.id=? AND s.anilist_id=? AND s.monitor_mode!='none' AND m.monitored=1
         AND m.episode_number=? AND a.airing_at=? AND a.airing_at<=strftime('%s','now')
         AND a.airing_at>=strftime('%s','now')-? AND s.format IN('TV','TV_SHORT','ONA'))",
    )
    .bind(t.series_id)
    .bind(t.provider_id)
    .bind(t.episode)
    .bind(t.airing_at)
    .bind(i64::from(cfg.recent_episode_search_days.clamp(1, 30)) * 86400)
    .fetch_one(db)
    .await
    .map_err(|e| e.to_string())?;
    if !wanted {
        return Err("Episode identity, airing, or monitoring changed".into());
    }
    let tags = crate::models::episode_tags::get_for_series(db, t.series_id)
        .await
        .map_err(|e| e.to_string())?;
    if tags
        .get(&t.episode)
        .is_some_and(|tag| tag.manual_override || tag.state == "grabbed")
    {
        return Err(
            "Episode has a manual protection or a recorded download awaiting review".into(),
        );
    }
    let slots = media::library_slots(
        cfg.media_root.clone(),
        HashSet::from([t.folder_name.clone()]),
    )
    .await;
    let have = slots
        .get(&t.folder_name)
        .ok_or("Library files could not be checked completely")?;
    if have.contains(&t.episode) {
        return Err("Episode is already available on disk".into());
    }
    let preview: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_grabs WHERE series_id=?)")
            .bind(t.series_id)
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())?;
    if preview || has_pending(db, t.series_id, t.episode).await? {
        return Err("A download, import, or file selection is already pending".into());
    }
    retry_allowed(db, cfg, t.series_id, t.episode).await
}

/// Keep settings in memory, never in an activity record (they contain secrets).
/// A settings or identity change cancels a candidate scored before that change.
pub(crate) struct Demand {
    target: Target,
    settings: Value,
    identity: Value,
    indexers: Value,
    clients: Vec<download_clients::DownloadClientRow>,
    cfs: Arc<Vec<CompiledCustomFormat>>,
}
pub(crate) async fn indexer_settings(db: &SqlitePool) -> Result<Value, String> {
    let rows = crate::models::indexers::list_all(db)
        .await
        .map_err(|e| e.to_string())?;
    let mut value = serde_json::to_value(rows).map_err(|e| e.to_string())?;
    for row in value.as_array_mut().into_iter().flatten() {
        if let Some(row) = row.as_object_mut() {
            for key in [
                "rss_last_polled_at",
                "rss_last_poll_error",
                "rss_last_item_count",
                "caps_json",
                "caps_refreshed_at",
                "created_at",
                "updated_at",
            ] {
                row.remove(key);
            }
        }
    }
    Ok(value)
}
impl Demand {
    pub(crate) async fn capture(state: &AppState, target: Target) -> Result<Self, String> {
        let cfg = config::get_config(&state.db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Settings unavailable")?;
        let cfg = crate::services::language_preferences::effective_config(
            &state.db,
            &cfg,
            target.series_id,
        )
        .await?;
        demand(&state.db, &cfg, &target).await?;
        let row = series::get_by_id(&state.db, target.series_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Release removed")?;
        Ok(Self {
            settings: serde_json::to_value(cfg).map_err(|e| e.to_string())?,
            identity: serde_json::to_value(row).map_err(|e| e.to_string())?,
            indexers: indexer_settings(&state.db).await?,
            clients: download_clients::list_all(&state.db)
                .await
                .map_err(|e| e.to_string())?,
            cfs: state.custom_formats.read().await.clone(),
            target,
        })
    }
    pub(crate) async fn recheck(&self, state: &AppState) -> Result<config::Config, String> {
        let cfg = config::get_config(&state.db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Settings unavailable")?;
        let cfg = crate::services::language_preferences::effective_config(
            &state.db,
            &cfg,
            self.target.series_id,
        )
        .await?;
        let row = series::get_by_id(&state.db, self.target.series_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Release removed")?;
        if self.settings != serde_json::to_value(&cfg).map_err(|e| e.to_string())?
            || self.identity != serde_json::to_value(row).map_err(|e| e.to_string())?
            || self.indexers != indexer_settings(&state.db).await?
            || self.clients
                != download_clients::list_all(&state.db)
                    .await
                    .map_err(|e| e.to_string())?
            || !Arc::ptr_eq(&self.cfs, &state.custom_formats.read().await.clone())
        {
            return Err(
                "Settings or release identity changed during the search; it will be checked again"
                    .into(),
            );
        }
        demand(&state.db, &cfg, &self.target).await?;
        Ok(cfg)
    }
}

async fn targets(db: &SqlitePool, cfg: &config::Config) -> Result<Vec<Target>, String> {
    sqlx::query_as(
        "SELECT s.id AS series_id,s.anilist_id AS provider_id,a.episode,a.airing_at,s.folder_name,s.title
         FROM episode_airings a JOIN series s ON s.id=a.series_id
         JOIN episode_monitor_state m ON m.series_id=s.id AND m.episode_number=a.episode
         WHERE s.monitor_mode!='none' AND m.monitored=1 AND s.format IN('TV','TV_SHORT','ONA')
         AND a.airing_at<=strftime('%s','now') AND a.airing_at>=strftime('%s','now')-?
         ORDER BY COALESCE((SELECT MAX(h.id) FROM library_activity h WHERE h.series_id=s.id
          AND h.provider_id=s.anilist_id AND h.action=? AND json_valid(h.detail)
          AND json_extract(h.detail,'$.episode')=a.episode),0),a.airing_at DESC,s.id,a.episode",
    ).bind(i64::from(cfg.recent_episode_search_days.clamp(1,30))*86400).bind(STARTED)
        .fetch_all(db).await.map_err(|e| e.to_string())
}

pub async fn run_once(state: &AppState) -> Result<String, String> {
    let _run = RUN_LOCK
        .try_lock()
        .map_err(|_| "Recent episode check is already running")?;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    if !enabled(&cfg) {
        return Ok("Automatic new episode recovery is disabled".into());
    }
    let candidates = targets(&state.db, &cfg).await?;
    let slots = media::library_slots(
        cfg.media_root.clone(),
        candidates.iter().map(|t| t.folder_name.clone()).collect(),
    )
    .await;
    let mut checked = 0;
    let mut queued = 0;
    let mut held = 0;
    let mut errors = 0;
    for t in candidates {
        if slots
            .get(&t.folder_name)
            .is_some_and(|have| have.contains(&t.episode))
        {
            continue;
        }
        if has_pending(&state.db, t.series_id, t.episode).await? {
            continue;
        }
        if checked == PASS_LIMIT {
            break;
        }
        let key = format!(
            "recent:{}:{}:{}",
            t.provider_id,
            t.episode,
            rand::random::<u64>()
        );
        let mut event = library_activity::Event::new(
            format!("{key}:start"),
            STARTED,
            Some(t.series_id),
            &t.title,
            "Checking a monitored aired episode missing from the library",
        );
        event.detail = serde_json::json!({"episode":t.episode,"airing_at":t.airing_at}).to_string();
        if !library_activity::try_append(&state.db, event)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("Could not record automatic episode check".into());
        }
        checked += 1;
        let outcome =
            crate::handlers::library::search::run_recent_missing_target(state, t.clone()).await;
        let (status, reason) = match outcome {
            Ok(report) if !report.grabbed.is_empty() => {
                queued += 1;
                (
                    "queued",
                    format!(
                        "{} release queued; awaiting download and verified import",
                        report.grabbed.len()
                    ),
                )
            }
            Ok(report) => {
                held += 1;
                (
                    "held",
                    report
                        .skipped
                        .into_iter()
                        .chain(report.notes)
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            }
            Err(reason) => {
                errors += 1;
                ("error", reason)
            }
        };
        let mut event = library_activity::Event::new(
            format!("{key}:finish"),
            FINISHED,
            Some(t.series_id),
            &t.title,
            &reason,
        );
        event.detail = serde_json::json!({"episode":t.episode,"outcome":status}).to_string();
        if !library_activity::try_append(&state.db, event)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("Could not record automatic episode outcome".into());
        }
    }
    let summary = format!(
        "Checked {checked} recent episodes: {queued} queued, {held} held, {errors} errors. Queued releases still require verified import."
    );
    if errors > 0 {
        Err(summary)
    } else {
        Ok(summary)
    }
}

#[cfg(test)]
mod tests;

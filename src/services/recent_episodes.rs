//! Paced recovery for monitored episodes that have fallen out of current RSS.
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
const COOLDOWN_HELD: &str = "Automatic recovery held";
const PASS_LIMIT: usize = 12;
static RUN_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

// The same airing evidence is used during discovery and immediately before
// dispatch. Calendar timestamps win over date-only metadata. A finished series
// establishes that its declared episodes aired even without individual dates;
// zero represents that evidence, not a fabricated premiere date. Unknown and
// future episodes remain ineligible. Missing episodes never expire by age.
const AIRED_TARGETS: &str = "WITH due AS (
    SELECT s.id AS series_id,s.anilist_id AS provider_id,m.episode_number AS episode,
      CASE WHEN a.airing_at IS NOT NULL THEN a.airing_at
           WHEN unixepoch(substr(e.aired,1,10)) IS NOT NULL THEN unixepoch(substr(e.aired,1,10))
           WHEN s.status IN('FINISHED','FINISHED_AIRING') AND m.episode_number<=s.episodes THEN 0 END AS airing_at,
      s.folder_name,s.title
    FROM series s JOIN episode_monitor_state m ON m.series_id=s.id
    LEFT JOIN episode_airings a ON a.series_id=s.id AND a.episode=m.episode_number
    LEFT JOIN series_episode_metadata e ON e.series_id=s.id AND e.episode_number=m.episode_number
    WHERE s.monitor_mode!='none' AND m.monitored=1 AND m.episode_number>0
      AND s.status NOT IN('NOT_YET_RELEASED','NOT_YET_AIRED','UNRELEASED','UPCOMING')
      AND s.format IN('TV','TV_SHORT','ONA','OVA','MOVIE','SPECIAL','MUSIC')
      AND (s.format!='MOVIE' OR m.episode_number=1)
), aired AS (SELECT * FROM due WHERE airing_at<=unixepoch('now'))";

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
    match retry_limit_reason(db, sid).await? {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

async fn retry_limit_reason(db: &SqlitePool, series_id: i64) -> Result<Option<String>, String> {
    let failed = grabbed_torrents::count_recent_failed(db, series_id, redownload::WINDOW_HOURS)
        .await
        .map_err(|e| e.to_string())?;
    Ok((failed >= redownload::LOOP_BREAKER).then(|| {
        format!(
            "Retry limit reached: {failed} failures in {} hours",
            redownload::WINDOW_HOURS
        )
    }))
}

/// A fresh, explicit monitor row is required. Unknown monitoring is a hold.
async fn demand(db: &SqlitePool, cfg: &config::Config, t: &Target) -> Result<(), String> {
    if !enabled(cfg) {
        return Err("Automatic new episode recovery is disabled".into());
    }
    let wanted: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{AIRED_TARGETS} SELECT EXISTS(SELECT 1 FROM aired
         WHERE series_id=? AND provider_id=? AND episode=? AND airing_at=?)"
    )))
    .bind(t.series_id)
    .bind(t.provider_id)
    .bind(t.episode)
    .bind(t.airing_at)
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
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{AIRED_TARGETS} SELECT * FROM aired a
         ORDER BY COALESCE((SELECT MAX(h.id) FROM library_activity h WHERE h.series_id=a.series_id
          AND h.provider_id=a.provider_id AND h.action=?1 AND json_valid(h.detail)
          AND json_extract(h.detail,'$.episode')=a.episode),0),
          CASE WHEN a.airing_at>=unixepoch('now')-?2 THEN 0 ELSE 1 END,
          a.airing_at DESC,a.series_id,a.episode"
    )))
    .bind(STARTED)
    .bind(i64::from(cfg.recent_episode_search_days.clamp(1, 30)) * 86400)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())
}

pub async fn run_once(state: &AppState) -> Result<String, String> {
    let _run = RUN_LOCK
        .try_lock()
        .map_err(|_| "Missing episode check is already running")?;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    if !enabled(&cfg) {
        return Ok("Automatic missing episode recovery is disabled".into());
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
    let mut cooldown_series = HashSet::new();
    for t in candidates {
        if checked == PASS_LIMIT {
            break;
        }
        if cooldown_series.contains(&t.series_id)
            || slots
                .get(&t.folder_name)
                .is_some_and(|have| have.contains(&t.episode))
        {
            continue;
        }
        if has_pending(&state.db, t.series_id, t.episode).await? {
            continue;
        }
        if let Some(reason) = retry_limit_reason(&state.db, t.series_id).await? {
            let mut event = library_activity::Event::new(
                format!("recent:cooldown:{}:{}", t.series_id, rand::random::<u64>()),
                COOLDOWN_HELD,
                Some(t.series_id),
                &t.title,
                &reason,
            );
            event.provider_id = Some(t.provider_id);
            event.detail = serde_json::json!({"scope":"release","outcome":"held"}).to_string();
            if !library_activity::try_append(&state.db, event)
                .await
                .map_err(|e| e.to_string())?
            {
                return Err("Could not record release retry cooldown".into());
            }
            cooldown_series.insert(t.series_id);
            continue;
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
    let mut summary = format!(
        "Checked {checked} missing episodes: {queued} queued, {held} held, {errors} errors. Queued releases still require verified import."
    );
    if !cooldown_series.is_empty() {
        let count = cooldown_series.len();
        let noun = if count == 1 { "release" } else { "releases" };
        summary.push_str(&format!(" Retry cooldown held {count} {noun}."));
    }
    if errors > 0 {
        Err(summary)
    } else {
        Ok(summary)
    }
}

#[cfg(test)]
mod tests;

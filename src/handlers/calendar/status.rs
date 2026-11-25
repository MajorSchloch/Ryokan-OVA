//! Page-only file availability. Schedule feeds do not trigger disk scans.
use crate::models::{config::Config, series};
use crate::services::{calendar::UpcomingEpisode, media, recent_episodes};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub(super) struct EpisodeStatus {
    pub kind: String,
    pub label: String,
    pub explanation: String,
    pub last_check: String,
    on_hold: bool,
}
impl Default for EpisodeStatus {
    fn default() -> Self {
        Self::new(
            "unknown",
            "Unknown",
            "File availability could not be checked completely",
        )
    }
}
impl EpisodeStatus {
    fn new(kind: &str, label: &str, explanation: &str) -> Self {
        Self {
            kind: kind.into(),
            label: label.into(),
            explanation: explanation.into(),
            last_check: String::new(),
            on_hold: false,
        }
    }

    pub fn matches(&self, filter: &str) -> bool {
        match filter {
            "all" => true,
            "held" => self.on_hold,
            kind => self.kind == kind,
        }
    }
}

pub(super) async fn load(
    db: &SqlitePool,
    cfg: &Config,
    episodes: &mut [UpcomingEpisode],
    now: i64,
) -> Result<HashMap<(i64, i32), EpisodeStatus>, String> {
    if episodes.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: HashSet<_> = episodes.iter().map(|e| e.series_id).collect();
    let scope = serde_json::to_string(&ids).map_err(|e| e.to_string())?;
    let library: HashMap<_, _> = series::get_all(db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|s| ids.contains(&s.id))
        .map(|s| (s.id, s))
        .collect();
    let monitor:HashMap<_,_>=sqlx::query_as::<_,(i64,i32,bool)>("SELECT series_id,episode_number,monitored FROM episode_monitor_state WHERE series_id IN(SELECT value FROM json_each(?))")
        .bind(&scope).fetch_all(db).await.map_err(|e|e.to_string())?.into_iter().map(|(sid,ep,m)|((sid,ep),m)).collect();
    let owned=sqlx::query_as::<_,(i64,i64,String,String,String,String)>(
        "SELECT g.id,g.series_id,g.episode_numbers,g.state,COALESCE(g.failure_reason,''),g.grabbed_at FROM grabbed_torrents g
         WHERE g.series_id IN(SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id)
         UNION ALL SELECT g.id,r.series_id,r.episode_numbers,g.state,COALESCE(g.failure_reason,''),g.grabbed_at
         FROM grabbed_torrent_series r JOIN grabbed_torrents g ON g.id=r.grab_id
         WHERE r.series_id IN(SELECT value FROM json_each(?1)) ORDER BY 6 DESC,1 DESC")
        .bind(&scope).fetch_all(db).await.map_err(|e|e.to_string())?;
    let mut attempts = HashMap::new();
    for (gid, sid, json, state, reason, _) in owned {
        let eps: Vec<i32> =
            serde_json::from_str(&json).map_err(|_| "Episode ownership could not be read")?;
        for ep in eps {
            let entry = attempts
                .entry((sid, ep))
                .or_insert((gid, state.clone(), reason.clone()));
            if state == "pending" && entry.1 != "pending" {
                *entry = (gid, state.clone(), reason.clone());
            }
        }
    }
    let last_checks = recent_episodes::last_checks(db, &ids.into_iter().collect::<Vec<_>>())
        .await
        .map_err(|e| e.to_string())?;
    let holds:HashMap<_,_>=sqlx::query_as::<_,(i64,String,String)>(
        "SELECT a.grab_id,a.occurred_at,a.reason FROM library_activity a JOIN grabbed_torrents g ON g.id=a.grab_id
         WHERE g.state='pending' AND a.action='Held' ORDER BY a.id DESC")
        .fetch_all(db).await.map_err(|e|e.to_string())?.into_iter().rev().map(|(id,at,r)|(id,format!("Last import held {at} UTC: {r}"))).collect();
    let slots = media::library_slots(
        cfg.media_root.clone(),
        library.values().map(|s| s.folder_name.clone()).collect(),
    )
    .await;
    let mut out = HashMap::new();
    for ep in episodes {
        let Some(row) = library.get(&ep.series_id) else {
            continue;
        };
        let key = (ep.series_id, ep.episode);
        let have = slots.get(&row.folder_name);
        let present = have.is_some_and(|h| h.contains(&ep.episode));
        let future = ep.airing_at > now;
        ep.monitored = super::super::library::availability::is_monitored(
            row,
            monitor.get(&key).copied(),
            present,
            !future,
            future,
        );
        let attempt = attempts.get(&key);
        let mut status = if present {
            EpisodeStatus::new(
                "available",
                "Available",
                "A library file covers this episode. File presence does not imply that its language or quality requirements passed.",
            )
        } else if attempt.is_some_and(|(_, state, _)| state == "pending") {
            EpisodeStatus::new(
                "pending",
                "Download / import pending",
                "Ryokan owns a pending download or import for this episode. It is not available in the library yet.",
            )
        } else if have.is_none() {
            EpisodeStatus::default()
        } else if !ep.monitored {
            EpisodeStatus::new(
                "unmonitored",
                "Unmonitored",
                "This episode is not monitored. It does not count as missing.",
            )
        } else if future {
            EpisodeStatus::new(
                "upcoming",
                "Upcoming",
                "The scheduled airing is in the future. This episode does not count as missing.",
            )
        } else {
            EpisodeStatus::new(
                "missing",
                "Missing",
                "This monitored episode has aired, but no library file or pending download was found.",
            )
        };
        status.last_check = last_checks.get(&key).cloned().unwrap_or_default();
        if let Some((id, state, reason)) = attempt {
            if state == "pending"
                && let Some(hold) = holds.get(id)
            {
                status.on_hold = true;
                if status.kind == "pending" {
                    status.label = "Held for review".into();
                }
                status.explanation.push_str(&format!(" {hold}"));
            }
            if status.kind == "missing" && state == "failed" {
                status.explanation.push_str(&format!(
                    " Last attempt failed: {}.",
                    match reason.as_str() {
                        "" => "Reason not recorded",
                        "requirements_rejected" =>
                            "The downloaded file did not meet the configured requirements",
                        "client_error" => "The download client reported a failure",
                        other => other,
                    }
                ));
            }
        }
        if status.kind == "missing" && !recent_episodes::enabled(cfg) {
            status
                .explanation
                .push_str(" Automatic missing episode recovery is disabled.");
        }
        out.insert(key, status);
    }
    Ok(out)
}

pub(super) fn filter(value: Option<&String>) -> &str {
    match value.map(String::as_str) {
        Some(
            v @ ("available" | "pending" | "held" | "missing" | "upcoming" | "unmonitored"
            | "unknown"),
        ) => v,
        _ => "all",
    }
}

#[cfg(test)]
mod tests;

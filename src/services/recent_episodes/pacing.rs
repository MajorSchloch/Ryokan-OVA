use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{
    AIRED_TARGETS, FINISHED, RETRY_DISABLED, STARTED, TASK, Target, enabled,
    retry::failure_cooldowns,
};
use crate::models::config;
use sqlx::SqlitePool;

const SHORT_DELAY: i64 = 30 * 60;
const FRESH_DELAY_CAP: i64 = 60 * 60;
const DELAY_CAP: i64 = 8 * 60 * 60;
const FRESH_FOR: i64 = 24 * 60 * 60;
const MAX_STREAK: u8 = 6;
const IDENTITY_UNAVAILABLE: &str = "Source identity unavailable; no download started";
const TITLE_HOLD: &str = "Could not verify the release identity from ";
const LEGACY_TITLE_HOLD: &str = "looked close but none named the series exactly";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RetryTiming {
    After(i64),
    NextPass,
    Checking,
    Blocked,
}

#[derive(Clone, Debug)]
pub(crate) struct RetryStatus {
    pub reason: String,
    pub timing: RetryTiming,
}

#[derive(Deserialize, Serialize)]
pub(super) struct CheckDetail {
    episode: i32,
    outcome: String,
    #[serde(default)]
    airing_at: Option<i64>,
    #[serde(default)]
    hold_streak: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
pub(super) struct CheckHistory {
    series_id: i64,
    episode: i32,
    occurred_at: String,
    action: String,
    reason: String,
    started_airing_at: Option<i64>,
    finished_reason: Option<String>,
    finished_detail: Option<String>,
}

pub(super) type EpisodeChecks = HashMap<(i64, i32), CheckHistory>;

pub(super) async fn history(
    db: &SqlitePool,
    series_ids: &[i64],
) -> Result<EpisodeChecks, sqlx::Error> {
    if series_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let scope = serde_json::to_string(series_ids).unwrap_or_else(|_| "[]".into());
    let rows = sqlx::query_as::<_, CheckHistory>(
        "WITH checks AS (
            SELECT a.id, a.series_id, a.action, CASE WHEN json_valid(a.detail)
                THEN json_extract(a.detail, '$.episode') END AS episode
            FROM library_activity a JOIN series s ON s.id=a.series_id AND s.anilist_id=a.provider_id
            WHERE a.series_id IN (SELECT value FROM json_each(?1)) AND a.action IN (?2,?3)
        ), latest AS (
            SELECT series_id, episode, MAX(id) AS id,
                   MAX(CASE WHEN action=?3 THEN id END) AS finished_id
            FROM checks
            WHERE typeof(episode)='integer' AND episode BETWEEN 1 AND 2147483647
            GROUP BY series_id, episode
        )
        SELECT l.series_id, l.episode, a.occurred_at, a.action, a.reason,
               CASE WHEN a.action=?2 AND json_valid(a.detail)
                    THEN json_extract(a.detail, '$.airing_at') END AS started_airing_at,
               f.reason AS finished_reason, f.detail AS finished_detail
        FROM latest l JOIN library_activity a ON a.id=l.id
        LEFT JOIN library_activity f ON f.id=l.finished_id",
    )
    .bind(scope)
    .bind(STARTED)
    .bind(FINISHED)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ((row.series_id, row.episode), row))
        .collect())
}

impl CheckHistory {
    fn finished(&self) -> Option<CheckDetail> {
        serde_json::from_str(self.finished_detail.as_deref()?).ok()
    }

    pub(super) fn retry_at(&self, airing_at: i64) -> Option<i64> {
        if self.action == STARTED {
            return None;
        }
        self.finished()
            .filter(|detail| detail.airing_at == Some(airing_at))?
            .retry_at
    }

    pub(super) fn description(&self) -> String {
        if self.action == STARTED {
            format!(
                "Last automatic check started at {} UTC. It has no recorded result.",
                self.occurred_at
            )
        } else {
            format!(
                "Last automatic check at {} UTC. {}",
                self.occurred_at,
                display_reason(&self.reason)
            )
        }
    }

    fn visible_reason(&self, airing_at: i64) -> String {
        if self
            .finished()
            .and_then(|detail| detail.airing_at)
            .is_some_and(|at| at != airing_at)
        {
            return String::new();
        }
        display_reason(self.finished_reason.as_deref().unwrap_or_default())
    }
}

fn without_target_label(reason: &str) -> &str {
    reason
        .split_once(": ")
        .filter(|(label, _)| {
            *label == "Single"
                || label
                    .strip_prefix("Episode ")
                    .is_some_and(|n| n.parse::<i32>().is_ok())
        })
        .map_or(reason, |(_, reason)| reason)
}

// Keep historical samples and any other notes, but remove obsolete advice.
fn display_reason(reason: &str) -> String {
    const LEGACY_ADVICE: &str =
        ". Add an alternate title on the series page if one of them is right.";
    let text = without_target_label(reason);
    let Some((count, rest)) =
        text.split_once(" release(s) looked close but none named the series exactly, for example ")
    else {
        return reason.to_owned();
    };
    let (Ok(count), Some((sample, notes))) =
        (count.parse::<usize>(), rest.rsplit_once(LEGACY_ADVICE))
    else {
        return reason.to_owned();
    };
    let label = &reason[..reason.len() - text.len()];
    let noun = if count == 1 { "title" } else { "titles" };
    format!(
        "{label}{TITLE_HOLD}{count} candidate {noun}. Review {sample} in Interactive Search.{notes}"
    )
}

// Verify the stable category even when the wording, count, or sample changes.
fn hold_key<'a>(outcome: &str, reason: &'a str) -> Option<&'a str> {
    let reason = without_target_label(reason);
    if outcome == "error" {
        return (reason == IDENTITY_UNAVAILABLE).then_some(reason);
    }
    if outcome != "held"
        || reason.is_empty()
        || reason.contains("Settings or release identity changed")
        || reason == "Automatic new episode recovery is disabled"
        || reason == "Episode identity, airing, or monitoring changed"
        || reason == "Episode is already available on disk"
        || reason == "A download, import, or file selection is already pending"
    {
        return None;
    }
    if reason.starts_with(TITLE_HOLD) || reason.contains(LEGACY_TITLE_HOLD) {
        return Some("No exact title match");
    }
    Some(reason)
}

pub(super) fn check_detail(
    target: &Target,
    outcome: &str,
    reason: &str,
    previous: Option<&CheckHistory>,
    now: i64,
) -> CheckDetail {
    let hold_streak = match hold_key(outcome, reason) {
        Some(key) => previous
            .and_then(|history| {
                let detail = history.finished()?;
                let old_key = hold_key(&detail.outcome, history.finished_reason.as_deref()?)?;
                (detail.airing_at == Some(target.airing_at)
                    && detail.outcome == outcome
                    && old_key == key)
                    .then_some(detail.hold_streak)
            })
            .unwrap_or_default()
            .saturating_add(1)
            .min(MAX_STREAK),
        None => 0,
    };
    let retry_at = (hold_streak >= 2).then(|| {
        let cap = if target.airing_at > 0 && now.saturating_sub(target.airing_at) < FRESH_FOR {
            FRESH_DELAY_CAP
        } else {
            DELAY_CAP
        };
        now.saturating_add((SHORT_DELAY << (hold_streak - 2)).min(cap))
    });
    CheckDetail {
        episode: target.episode,
        outcome: outcome.into(),
        airing_at: Some(target.airing_at),
        hold_streak,
        retry_at,
    }
}

pub(crate) async fn retry_statuses(
    db: &SqlitePool,
    cfg: &config::Config,
    series_ids: &[i64],
) -> Result<HashMap<(i64, i32), RetryStatus>, String> {
    if series_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let scope = serde_json::to_string(series_ids).map_err(|e| e.to_string())?;
    // Wanted can include a slot from the declared episode total while recovery
    // still needs a confirmed airing. Explain that gap without promising a date.
    let slots: Vec<(i64, i32, Option<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{AIRED_TARGETS} SELECT series_id, episode, airing_at FROM due
         WHERE series_id IN (SELECT value FROM json_each(?1))"
    )))
    .bind(scope)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    if !enabled(cfg) {
        return Ok(slots
            .into_iter()
            .map(|(series_id, episode, _)| {
                (
                    (series_id, episode),
                    RetryStatus {
                        reason: "Automatic missing episode recovery is disabled".into(),
                        timing: RetryTiming::Blocked,
                    },
                )
            })
            .collect());
    }
    let now = chrono::Utc::now().timestamp();
    let (checks, cooldowns, running_since) = tokio::try_join!(
        history(db, series_ids),
        failure_cooldowns(db, series_ids, now),
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT last_started_at FROM scheduled_task_runs WHERE task_key=? AND last_status='running'"
        ).bind(TASK).fetch_optional(db),
    )
    .map_err(|e| e.to_string())?;
    let running_since = running_since.flatten();
    Ok(slots
        .into_iter()
        .map(|(series_id, episode, airing_at)| {
            let key = (series_id, episode);
            let Some(airing_at) = airing_at.filter(|at| *at <= now) else {
                let reason = match airing_at {
                    Some(_) => "Episode has not aired yet",
                    None => "Waiting for a confirmed air date",
                };
                return (
                    key,
                    RetryStatus {
                        reason: reason.into(),
                        timing: RetryTiming::Blocked,
                    },
                );
            };
            if !cfg.auto_redownload_failed && cooldowns.episodes.contains_key(&key) {
                return (
                    key,
                    RetryStatus {
                        reason: RETRY_DISABLED.into(),
                        timing: RetryTiming::Blocked,
                    },
                );
            }
            let check = checks.get(&key);
            let status = if check.is_some_and(|h| {
                h.action == STARTED
                    && h.started_airing_at == Some(airing_at)
                    && running_since
                        .as_ref()
                        .is_some_and(|at| h.occurred_at >= *at)
            }) {
                RetryStatus {
                    reason: String::new(),
                    timing: RetryTiming::Checking,
                }
            } else {
                let retry_at = check
                    .and_then(|h| h.retry_at(airing_at))
                    .filter(|at| *at > now);
                if let Some(cooldown) = cooldowns
                    .releases
                    .get(&series_id)
                    .into_iter()
                    .chain(cooldowns.episodes.get(&key))
                    .max_by_key(|hold| hold.retry_at)
                    .filter(|hold| retry_at.is_none_or(|at| at <= hold.retry_at))
                {
                    RetryStatus {
                        reason: cooldown.reason.clone(),
                        timing: if cooldown.retry_at > now {
                            RetryTiming::After(cooldown.retry_at)
                        } else {
                            RetryTiming::NextPass
                        },
                    }
                } else {
                    RetryStatus {
                        reason: check
                            .map(|h| h.visible_reason(airing_at))
                            .unwrap_or_default(),
                        timing: retry_at.map_or(RetryTiming::NextPass, RetryTiming::After),
                    }
                }
            };
            (key, status)
        })
        .collect())
}

#[cfg(test)]
pub(super) mod tests;

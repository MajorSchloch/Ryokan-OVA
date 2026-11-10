//! Read-only availability. A follow selection never changes episode demand.
use super::{AvailabilitySegment, CardProgress, inventory};
use crate::{
    models::{
        episode_tags,
        monitoring::{EpisodeMonitorState, MonitorMode},
        series,
    },
    services::anilist::{AnimeDetail, plausible_episode_count},
};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashMap, HashSet};

pub(crate) fn monitoring_label(row: &series::Series, states: &[EpisodeMonitorState]) -> String {
    if !states.is_empty() {
        let total = row
            .episodes
            .and_then(|n| plausible_episode_count(n.into()))
            .map(|n| n as usize)
            .unwrap_or_else(|| {
                states
                    .iter()
                    .map(|s| s.episode_number.max(0) as usize)
                    .max()
                    .unwrap_or_default()
            })
            .max(states.len());
        let wanted = states.iter().filter(|s| s.monitored).count();
        return if wanted == 0 {
            "Unmonitored".into()
        } else if wanted == total && total == 1 {
            "Monitoring 1 episode".into()
        } else if wanted == total {
            format!("Monitoring all {total} episodes")
        } else {
            format!("Monitoring {wanted} of {total} episodes")
        };
    }
    match row.monitor_mode_enum() {
        MonitorMode::All => "Monitoring all episodes".into(),
        MonitorMode::Future => "Monitoring future episodes".into(),
        MonitorMode::Missing => "Monitoring missing episodes".into(),
        MonitorMode::Existing => "Monitoring existing episodes".into(),
        MonitorMode::None => "Unmonitored".into(),
    }
}

fn upcoming_status(status: &str) -> bool {
    matches!(status, "NOT_YET_RELEASED" | "NOT_YET_AIRED")
}
fn finished_status(status: &str) -> bool {
    matches!(status, "FINISHED" | "FINISHED_AIRING")
}

/// Preserve the precision supplied by the provider. A year is never January 1.
pub(super) fn premiere_label(date: &str) -> String {
    match date.len() {
        10 => NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .map(|d| d.format("%b %-d, %Y").to_string()),
        7 => NaiveDate::parse_from_str(&format!("{date}-01"), "%Y-%m-%d")
            .ok()
            .map(|d| format!("{} (day TBA)", d.format("%B %Y"))),
        4 if date.bytes().all(|b| b.is_ascii_digit()) => Some(format!("{date} (date TBA)")),
        _ => None,
    }
    .unwrap_or_else(|| "TBA".into())
}

pub(super) fn added_label(raw: &str) -> String {
    raw.get(..10)
        .filter(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok())
        .map(premiere_label)
        .unwrap_or_default()
}

fn next_label(
    row: &series::Series,
    facts: &inventory::Inventory,
    detail: Option<&AnimeDetail>,
    now: i64,
) -> (String, Option<i64>) {
    let cached = detail
        .and_then(|d| d.next_airing_at.zip(d.next_airing_episode))
        .filter(|(at, _)| *at > now)
        .map(|(at, ep)| (at, ep, true));
    let known = facts.next_known.into_iter().chain(cached).min();
    if let Some((at, ep, exact)) = known {
        let date = DateTime::from_timestamp(if exact { at } else { at - 86_400 }, 0);
        if let Some(date) = date {
            return (
                if exact {
                    format!("Episode {ep} · {} UTC", date.format("%b %-d, %Y at %H:%M"))
                } else {
                    format!("Episode {ep} · {} (time TBA)", date.format("%b %-d, %Y"))
                },
                Some(at),
            );
        }
    }
    if upcoming_status(&row.status) {
        let date = detail
            .and_then(|d| d.release_date.clone())
            .or_else(|| row.season_year.map(|y| y.to_string()))
            .unwrap_or_default();
        return (format!("Premiere · {}", premiere_label(&date)), None);
    }
    (
        if finished_status(&row.status) {
            "Finished".into()
        } else {
            "TBA".into()
        },
        None,
    )
}

fn is_monitored(
    row: &series::Series,
    explicit: Option<bool>,
    have: bool,
    aired: bool,
    future: bool,
) -> bool {
    explicit.unwrap_or_else(|| match row.monitor_mode_enum() {
        MonitorMode::All => true,
        MonitorMode::None => false,
        MonitorMode::Existing => have,
        MonitorMode::Missing => aired && !have,
        MonitorMode::Future => !have && !finished_status(&row.status) && (future || !aired),
    })
}

pub(super) fn finish(mut p: CardProgress) -> CardProgress {
    let actionable = p.downloaded + p.downloading + p.missing;
    p.aired = actionable;
    p.monitored |= actionable + p.future + p.unknown > 0;
    p.pct = if actionable > 0 {
        (p.downloaded * 100 / actionable).clamp(0, 100)
    } else {
        0
    };
    let (state, label) = if p.missing > 0 {
        (
            "missing",
            format!(
                "{} monitored episode{} missing",
                p.missing,
                if p.missing == 1 { "" } else { "s" }
            ),
        )
    } else if p.downloading > 0 {
        (
            "downloading",
            format!(
                "{} episode{} downloading",
                p.downloading,
                if p.downloading == 1 { "" } else { "s" }
            ),
        )
    } else if p.unknown > 0 {
        ("idle", "Some file or airing details are unknown".into())
    } else if actionable > 0 {
        (
            "complete",
            "All aired, monitored episodes are available".into(),
        )
    } else if p.future > 0 {
        ("idle", "Waiting for monitored episodes to air".into())
    } else if p.monitored {
        ("idle", "Waiting for episode dates".into())
    } else if p.unmonitored > 0 {
        ("idle", "No episodes are monitored".into())
    } else {
        ("idle", "No confirmed episode demand".into())
    };
    p.state = state.into();
    p.label = label;
    let counts = [
        ("available", "Available", p.downloaded),
        ("downloading", "Downloading", p.downloading),
        ("missing", "Missing", p.missing),
        ("future", "Future", p.future),
        ("unmonitored", "Unmonitored", p.unmonitored),
        ("unknown", "Unknown", p.unknown),
    ];
    let total: i64 = counts.iter().map(|(_, _, n)| n).sum();
    p.segments = counts
        .into_iter()
        .filter(|(_, _, n)| *n > 0)
        .map(|(kind, label, count)| AvailabilitySegment {
            kind,
            label,
            count,
            percent: format!("{:.3}", count as f64 * 100.0 / total as f64),
        })
        .collect();
    p.description = p
        .segments
        .iter()
        .map(|s| format!("{}: {}", s.label, s.count))
        .collect::<Vec<_>>()
        .join("; ");
    if p.description.is_empty() {
        p.description.clone_from(&p.label);
    }
    p
}

pub(super) fn aggregate<'a>(rows: impl Iterator<Item = &'a CardProgress>) -> CardProgress {
    let mut p = CardProgress::default();
    for row in rows {
        p.monitored |= row.monitored;
        p.downloaded += row.downloaded;
        p.downloading += row.downloading;
        p.missing += row.missing;
        p.future += row.future;
        p.unmonitored += row.unmonitored;
        p.unknown += row.unknown;
    }
    finish(p)
}

pub(super) async fn load(
    db: &SqlitePool,
    root: &str,
    library: &[series::Series],
) -> Result<HashMap<i64, CardProgress>, sqlx::Error> {
    let (facts, monitor_rows, tags, details, followed) = tokio::join!(
        inventory::load(db,root,library),
        sqlx::query_as::<_,(i64,i32,bool)>("SELECT series_id,episode_number,monitored FROM episode_monitor_state").fetch_all(db),
        episode_tags::active_states_all_series(db),
        sqlx::query_as::<_,(i64,Option<String>)>("SELECT s.id,coalesce(sc.detail_json,pc.detail_json) FROM series s LEFT JOIN series_metadata_cache sc ON sc.series_id=s.id LEFT JOIN provider_metadata_cache pc ON pc.provider_id=s.anilist_id").fetch_all(db),
        sqlx::query_scalar::<_,i64>(sqlx::AssertSqlSafe(format!("{} SELECT l.series_id FROM local_anilist_ids l JOIN (
            SELECT f.anilist_id FROM release_follows f WHERE NOT EXISTS (
                SELECT 1 FROM franchise_choices c WHERE c.anilist_id=f.anilist_id AND c.selected=0)
            UNION SELECT anilist_id FROM franchise_choices WHERE selected=1
            UNION SELECT r.anilist_id FROM followed_releases r WHERE NOT EXISTS (
                SELECT 1 FROM franchise_choices c WHERE c.anilist_id=r.anilist_id AND c.selected=0)
        ) f USING(anilist_id)",series::LOCAL_ANILIST_IDS))).fetch_all(db),
    );
    let facts = facts?;
    let followed: HashSet<_> = followed?.into_iter().collect();
    let details: HashMap<i64, AnimeDetail> = details?
        .into_iter()
        .filter_map(|(id, json)| {
            json.and_then(|s| serde_json::from_str(&s).ok())
                .map(|d| (id, d))
        })
        .collect();
    let mut states = HashMap::<i64, Vec<EpisodeMonitorState>>::new();
    for (id, episode_number, monitored) in monitor_rows? {
        states.entry(id).or_default().push(EpisodeMonitorState {
            episode_number,
            monitored,
        });
    }
    let mut downloading = HashMap::<i64, HashSet<i32>>::new();
    for (id, ep, state) in tags? {
        if state == "grabbed" {
            downloading.entry(id).or_default().insert(ep);
        }
    }
    let now = Utc::now().timestamp();
    let mut result = HashMap::new();
    for row in library {
        let Some(facts) = facts.get(&row.id) else {
            continue;
        };
        let detail = details.get(&row.id);
        let states = states.get(&row.id).map(Vec::as_slice).unwrap_or_default();
        let explicit: HashMap<_, _> = states
            .iter()
            .map(|s| (s.episode_number, s.monitored))
            .collect();
        let grabbed = downloading.get(&row.id);
        let mut aired = facts.aired.clone();
        let mut frontier = facts.future_frontier;
        if let Some((ep, at)) = detail.and_then(|d| d.next_airing_episode.zip(d.next_airing_at)) {
            aired.extend(1..ep);
            if at <= now {
                aired.insert(ep);
            } else {
                frontier = Some(ep);
            }
        }
        let total = row
            .episodes
            .and_then(|n| plausible_episode_count(n.into()))
            .or_else(|| {
                detail
                    .map(AnimeDetail::effective_episode_count)
                    .filter(|n| *n > 0)
            })
            .unwrap_or_default();
        if finished_status(&row.status) {
            aired.extend(1..=total);
        }
        let mut slots: BTreeSet<i32> = (1..=total).collect();
        slots.extend(aired.iter().copied());
        slots.extend(facts.upcoming.iter().copied());
        slots.extend(explicit.keys().copied());
        if let Some(have) = &facts.have {
            slots.extend(have.iter().copied());
        }
        if let Some(grabbed) = grabbed {
            slots.extend(grabbed.iter().copied());
        }
        slots.retain(|ep| plausible_episode_count((*ep).into()).is_some());
        let (next_airing, next_airing_at) = next_label(row, facts, detail, now);
        let mut p = CardProgress {
            added_date: added_label(&row.added_at),
            following: followed.contains(&row.id),
            next_airing_at,
            monitored: if states.is_empty() {
                row.monitor_mode != "none"
            } else {
                states.iter().any(|s| s.monitored)
            },
            monitoring_label: monitoring_label(row, states),
            files_label: facts
                .on_disk
                .map(|n| format!("{n} episode{} on disk", if n == 1 { "" } else { "s" }))
                .unwrap_or_else(|| "Files unavailable".into()),
            next_airing,
            ..Default::default()
        };
        for ep in slots {
            let have = facts.have.as_ref().is_some_and(|have| have.contains(&ep));
            let future = upcoming_status(&row.status)
                || facts.upcoming.contains(&ep)
                || frontier.is_some_and(|n| ep >= n);
            let is_aired = !future && aired.contains(&ep);
            let wanted = if total > 0 && ep > total && !explicit.contains_key(&ep) {
                false
            } else {
                is_monitored(row, explicit.get(&ep).copied(), have, is_aired, future)
            };
            if !wanted {
                p.unmonitored += 1;
            } else if have {
                p.downloaded += 1;
            } else if grabbed.is_some_and(|g| g.contains(&ep)) {
                p.downloading += 1;
            } else if future {
                p.future += 1;
            } else if facts.have.is_none() || !is_aired {
                p.unknown += 1;
            } else {
                p.missing += 1;
            }
        }
        result.insert(row.id, finish(p));
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

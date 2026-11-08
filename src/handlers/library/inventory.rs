//! Read-only episode facts shared by the grouped and table views.
use crate::{
    models::series::Series,
    services::{anilist::plausible_episode_count, media},
};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
pub(super) struct Inventory {
    pub on_disk: Option<usize>,
    pub missing: Option<Vec<i32>>,
    pub next: Option<(i64, i32)>,
}

#[derive(Default)]
struct Schedule {
    aired: BTreeSet<i32>,
    upcoming: BTreeSet<i32>,
    next: Option<(i64, i32)>,
}

pub(super) async fn load(
    db: &SqlitePool,
    root: &str,
    library: &[Series],
) -> Result<HashMap<i64, Inventory>, sqlx::Error> {
    let now = chrono::Utc::now().timestamp();
    let folders = library.iter().map(|s| s.folder_name.clone()).collect();
    // Prefer the calendar's exact timestamp over a date for the same slot.
    // A date without a time establishes that it aired once that UTC day ends.
    let dates = sqlx::query_as::<_, (i64, i32, i64, bool, bool)>(
        "WITH detail AS (
             SELECT series_id, json_extract(detail_json,'$.next_airing_episode') AS episode,
                    json_extract(detail_json,'$.next_airing_at') AS stamp
             FROM series_metadata_cache WHERE json_valid(detail_json)
         ), facts(series_id,episode,stamp,exact,frontier) AS (
             SELECT series_id, episode, airing_at, 1, 0 FROM episode_airings
             UNION ALL
             SELECT d.series_id,d.episode,coalesce(a.airing_at,d.stamp),1,1
             FROM detail d LEFT JOIN episode_airings a USING(series_id,episode)
             UNION ALL
             SELECT e.series_id, e.episode_number,
                    CASE WHEN length(e.aired)=10 THEN unixepoch(e.aired,'+1 day')
                         ELSE unixepoch(e.aired) END, 0, 0
             FROM series_episode_metadata e WHERE length(e.aired)>=10
             AND NOT EXISTS (SELECT 1 FROM episode_airings a
                 WHERE a.series_id=e.series_id AND a.episode=e.episode_number)
             AND NOT EXISTS (SELECT 1 FROM detail d
                 WHERE d.series_id=e.series_id AND d.episode=e.episode_number AND d.stamp>0)
         ) SELECT * FROM facts WHERE stamp>0 AND episode>0",
    )
    .fetch_all(db);
    let (disk, dates) = tokio::join!(media::library_slots(root.to_owned(), folders), dates);
    let mut schedules = HashMap::<i64, Schedule>::new();
    for (id, ep, at, exact, frontier) in dates? {
        if plausible_episode_count(ep.into()).is_none() {
            continue;
        }
        let schedule = schedules.entry(id).or_default();
        // Only AniList's designated next episode establishes a frontier.
        // Calendar results can contain an arbitrary subset of future slots.
        if frontier {
            schedule.aired.extend(1..ep);
        }
        if at <= now {
            schedule.aired.insert(ep);
        } else {
            schedule.upcoming.insert(ep);
            if exact {
                let candidate = (at, ep);
                schedule.next = Some(schedule.next.map_or(candidate, |next| next.min(candidate)));
            }
        }
    }

    let mut inventory = HashMap::new();
    for s in library {
        let mut schedule = schedules.remove(&s.id).unwrap_or_default();
        let total = s.episodes.and_then(|n| plausible_episode_count(n.into()));
        if matches!(s.status.as_str(), "FINISHED" | "FINISHED_AIRING")
            && let Some(total) = total
        {
            schedule.aired.extend(1..=total);
        }
        schedule
            .aired
            .retain(|ep| !schedule.upcoming.contains(ep) && total.is_none_or(|n| *ep <= n));
        let slots = disk.get(&s.folder_name);
        let missing = slots.and_then(|have| {
            let enough = !schedule.aired.is_empty()
                || matches!(s.status.as_str(), "NOT_YET_RELEASED" | "NOT_YET_AIRED")
                || schedule.upcoming.contains(&1)
                || total.is_some_and(|n| (1..=n).all(|ep| have.contains(&ep)));
            enough.then(|| {
                schedule
                    .aired
                    .into_iter()
                    .filter(|ep| !have.contains(ep))
                    .collect()
            })
        });
        inventory.insert(
            s.id,
            Inventory {
                on_disk: slots.map(|s| s.len()),
                missing,
                next: schedule.next,
            },
        );
    }
    Ok(inventory)
}

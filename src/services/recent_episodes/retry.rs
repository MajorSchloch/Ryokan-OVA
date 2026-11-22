//! Failure budgets shared by RSS, recovery and automatic re-search.
use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::models::{
    grabbed_torrents::source_replay::{self, SourceRef},
    indexers, library_activity,
    series::Series,
};
use crate::services::{
    download_client::normalize_info_hash, post_processing::REQUIREMENTS_REJECTED_REASON, redownload,
};

const TRIAL_INTERVAL: i64 = 3600;
const TRIAL: &str = "Automatic source retry attempted";

pub(crate) struct Cooldown {
    pub reason: String,
    pub retry_at: i64,
}

#[derive(Default)]
pub(crate) struct FailureCooldowns {
    pub releases: HashMap<i64, Cooldown>,
    pub episodes: HashMap<(i64, i32), Cooldown>,
}

impl FailureCooldowns {
    pub fn needs_trial(&self, sid: i64, ep: i32, now: i64) -> Result<bool, String> {
        if let Some(hold) = self.releases.get(&sid) {
            return Err(hold.reason.clone());
        }
        match self.episodes.get(&(sid, ep)) {
            Some(hold) if hold.retry_at > now => Err(hold.reason.clone()),
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }
}

pub(crate) async fn failure_cooldowns(
    db: &SqlitePool,
    series_ids: &[i64],
    now: i64,
) -> Result<FailureCooldowns, sqlx::Error> {
    if series_ids.is_empty() {
        return Ok(FailureCooldowns::default());
    }
    let scope =
        serde_json::to_string(series_ids).map_err(|error| sqlx::Error::Encode(error.into()))?;
    let window = redownload::WINDOW_HOURS * 3600;
    let rows: Vec<(i64, Option<i32>, i64, i64)> = sqlx::query_as(
        "WITH scope AS (SELECT value FROM json_each(?1)),
        recent_grabs AS (
            SELECT id,series_id,episode_numbers,COALESCE(failure_reason,'') AS reason,
                   unixepoch(grabbed_at) AS at,grabbed_at
            FROM grabbed_torrents WHERE state='failed' AND grabbed_at>=datetime(?2,'unixepoch')
        ), release_failures AS (
            SELECT series_id,at,COUNT(*) OVER (PARTITION BY series_id) AS failures,
                   ROW_NUMBER() OVER (PARTITION BY series_id ORDER BY grabbed_at DESC,id DESC) AS n
            FROM recent_grabs WHERE reason!=?3 AND series_id IN scope
        ), media_slots AS (
            SELECT g.id,g.series_id,e.value AS episode,g.at FROM recent_grabs g
            JOIN json_each(CASE WHEN json_valid(g.episode_numbers) THEN g.episode_numbers ELSE '[]' END) e
            WHERE g.reason=?3 AND g.series_id IN scope AND e.type='integer'
            AND NOT EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id)
            UNION
            SELECT g.id,r.series_id,e.value,g.at FROM recent_grabs g
            JOIN grabbed_torrent_series r ON r.grab_id=g.id JOIN json_each(CASE WHEN json_valid(r.episode_numbers) THEN r.episode_numbers ELSE '[]' END) e
            WHERE g.reason=?3 AND r.series_id IN scope AND e.type='integer'
        ), media_failures AS (
            SELECT series_id,episode,COUNT(*) AS failures,MAX(at) AS latest FROM media_slots
            WHERE episode BETWEEN 1 AND 2147483647 GROUP BY series_id,episode HAVING COUNT(*)>=?4
        ), leases AS (
            SELECT a.series_id,json_extract(a.detail,'$.episode') AS episode,
                   MAX(unixepoch(a.occurred_at)) AS latest
            FROM library_activity a JOIN series s ON s.id=a.series_id AND s.anilist_id=a.provider_id
            WHERE a.series_id IN scope AND a.action=?5 AND a.occurred_at>datetime(?6,'unixepoch')
            GROUP BY a.series_id,episode
        ), episode_budget AS (
            SELECT series_id,episode,MAX(failures) AS failures,MAX(latest) AS latest FROM (
                SELECT * FROM media_failures UNION ALL SELECT series_id,episode,0,latest FROM leases
            ) GROUP BY series_id,episode
        )
        SELECT series_id,NULL,failures,at+?7+1 FROM release_failures WHERE n=?4
        UNION ALL
        SELECT series_id,episode,failures,latest+?8 FROM episode_budget",
    )
    .bind(scope)
    .bind(now - window)
    .bind(REQUIREMENTS_REJECTED_REASON)
    .bind(redownload::LOOP_BREAKER)
    .bind(TRIAL)
    .bind(now - TRIAL_INTERVAL)
    .bind(window)
    .bind(TRIAL_INTERVAL)
    .fetch_all(db)
    .await?;
    let mut cooldowns = FailureCooldowns::default();
    for (sid, episode, failures, retry_at) in rows {
        let reason = if episode.is_none() {
            format!(
                "Retry limit reached: {failures} download failures in {} hours",
                redownload::WINDOW_HOURS
            )
        } else if retry_at <= now {
            "Verified media failures: waiting for a different eligible source".into()
        } else {
            "Verified media failures: another source can be tried after the hourly retry hold"
                .into()
        };
        let hold = Cooldown { reason, retry_at };
        if let Some(episode) = episode {
            cooldowns.episodes.insert((sid, episode), hold);
        } else {
            cooldowns.releases.insert(sid, hold);
        }
    }
    Ok(cooldowns)
}

/// Requirements failures must not consume the general download failure budget.
pub(crate) async fn release_on_hold(db: &SqlitePool, sid: i64) -> Result<bool, sqlx::Error> {
    Ok(
        failure_cooldowns(db, &[sid], chrono::Utc::now().timestamp())
            .await?
            .releases
            .contains_key(&sid),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TrialIdentity {
    key: String,
    title: String,
    url: Option<String>,
}

/// Data for the source eligibility filter. Endpoint credentials never leave memory.
pub(crate) struct TrialSources {
    keys: HashSet<String>,
    titles: HashSet<String>,
    urls: HashSet<String>,
    indexers: HashMap<i64, (String, String)>,
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn title_key(title: &str) -> String {
    let title = title.trim().to_lowercase();
    let title = [".mkv", ".mp4", ".avi"]
        .into_iter()
        .find_map(|extension| title.strip_suffix(extension))
        .unwrap_or(&title);
    let words = title
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    digest(&words.join(" "))
}

fn url_key(url: &str) -> Option<String> {
    (!url.is_empty()).then(|| digest(url))
}

impl TrialSources {
    pub(crate) async fn load(db: &SqlitePool, sid: i64, ep: i32) -> Result<Self, String> {
        let mut sources = Self {
            keys: HashSet::new(),
            titles: HashSet::new(),
            urls: HashSet::new(),
            indexers: HashMap::new(),
        };
        let failures: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT g.hash,g.torrent_name,g.source_url,g.source_identity FROM grabbed_torrents g
             WHERE g.state='failed' AND (g.series_id=?1 OR EXISTS(
                 SELECT 1 FROM grabbed_torrent_series r JOIN json_each(CASE WHEN json_valid(r.episode_numbers) THEN r.episode_numbers ELSE '[]' END) e
                 WHERE r.grab_id=g.id AND r.series_id=?1 AND e.value=?2
             ))",
        )
        .bind(sid)
        .bind(ep)
        .fetch_all(db)
        .await
        .map_err(|error| error.to_string())?;
        for (hash, title, url, identity) in failures {
            sources.keys.extend(normalize_info_hash(&hash));
            sources.keys.extend(identity);
            sources.titles.insert(title_key(&title));
            sources.urls.extend(url_key(&url));
        }
        let leases: Vec<String> = sqlx::query_scalar(
            "SELECT a.detail FROM library_activity a JOIN series s ON s.id=a.series_id AND s.anilist_id=a.provider_id
             WHERE a.series_id=? AND a.action=? AND a.occurred_at>=datetime('now', ? || ' hours')
             AND json_extract(a.detail,'$.episode')=?",
        ).bind(sid).bind(TRIAL).bind(-redownload::WINDOW_HOURS).bind(ep)
            .fetch_all(db).await.map_err(|error| error.to_string())?;
        for detail in leases {
            let saved: TrialIdentity =
                serde_json::from_str(&detail).map_err(|error| error.to_string())?;
            sources.keys.insert(saved.key);
            sources.titles.insert(saved.title);
            sources.urls.extend(saved.url);
        }
        sources.indexers = indexers::list_all(db)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|row| (row.id, (row.kind, row.url)))
            .collect();
        Ok(sources)
    }

    fn identity(
        &self,
        hash: &str,
        title: &str,
        source: SourceRef<'_>,
    ) -> Result<TrialIdentity, String> {
        let indexer = source
            .indexer_id
            .and_then(|id| self.indexers.get(&id).map(|entry| (id, entry)));
        let key = match indexer {
            Some((id, (kind, url))) if kind == "newznab" => {
                source_replay::indexer_identity(id, kind, url, source.guid)
                    .map_err(|error| error.to_string())?
            }
            _ => normalize_info_hash(hash),
        }
        .ok_or("A different source could not be verified for the hourly retry")?;
        let title = title_key(title);
        let url = url_key(source.url);
        if self.keys.contains(&key)
            || self.titles.contains(&title)
            || url.as_ref().is_some_and(|key| self.urls.contains(key))
        {
            return Err("This source was already tried for the hourly retry".into());
        }
        Ok(TrialIdentity { key, title, url })
    }

    pub(crate) fn check(
        &self,
        hash: &str,
        title: &str,
        source: SourceRef<'_>,
    ) -> Result<(), String> {
        self.identity(hash, title, source).map(|_| ())
    }

    /// Call under the RSS/import/settings locks, after final source checks and
    /// just before client submission. A failed submission also spends the trial.
    pub(crate) async fn reserve(
        db: &SqlitePool,
        series: &Series,
        ep: i32,
        hash: &str,
        title: &str,
        source: SourceRef<'_>,
    ) -> Result<(), String> {
        let sources = Self::load(db, series.id, ep).await?;
        let identity = sources.identity(hash, title, source)?;
        let mut event = library_activity::Event::new(
            format!("source-trial:{}:{ep}:{}", series.id, rand::random::<u64>()),
            TRIAL,
            Some(series.id),
            &series.title,
            "Trying a different source after verified media failures",
        );
        event.provider_id = Some(series.anilist_id);
        event.detail = serde_json::json!({"episode":ep,"key":identity.key,"title":identity.title,"url":identity.url}).to_string();
        if !library_activity::try_append(db, event)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("The hourly retry could not be recorded".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};

    #[test]
    fn same_usenet_post_and_relabelled_mirror_do_not_qualify() {
        let original = "[Group] Test Series - 07 (1080p) [EDB4680B].mkv";
        let mut sources = TrialSources {
            keys: HashSet::from([source_replay::indexer_identity(
                1,
                "newznab",
                "https://one.example",
                "post-1",
            )
            .unwrap()
            .unwrap()]),
            titles: HashSet::from([title_key(original)]),
            urls: HashSet::new(),
            indexers: HashMap::from([
                (1, ("newznab".into(), "https://one.example".into())),
                (2, ("newznab".into(), "https://two.example".into())),
            ]),
        };
        let original_ref = SourceRef {
            indexer_id: Some(1),
            guid: "post-1",
            url: "https://one.example/fresh-proxy",
            ..Default::default()
        };
        let mirror = SourceRef {
            indexer_id: Some(2),
            guid: "mirror-1",
            url: "https://two.example/mirror",
            ..Default::default()
        };
        assert!(sources.check("", "Changed title", original_ref).is_err());
        assert!(
            sources
                .check("", "[GROUP] Test.Series - 07 (1080p)-Edb4680B", mirror)
                .is_err()
        );
        assert!(
            sources
                .check("", "[Other] Test Series - 07 (1080p)", mirror)
                .is_ok()
        );
        sources.urls.insert(url_key(mirror.url).unwrap());
        assert!(
            sources
                .check("", "[Other] Test Series - 07 (1080p)", mirror)
                .is_err()
        );
    }

    #[tokio::test]
    async fn verified_failures_follow_the_routed_slot() {
        let db = in_memory_pool().await;
        let parent = seed_series(&db, 1, "Parent").await;
        let owner = seed_series(&db, 2, "Owner").await;
        let now = chrono::Utc::now().timestamp();
        for n in 0..redownload::LOOP_BREAKER {
            let grab: i64 = sqlx::query_scalar(
                "INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason,grabbed_at)
                 VALUES(?,?,?,'[8]','failed',?,datetime(?,'unixepoch')) RETURNING id",
            ).bind(format!("{n:040x}")).bind(format!("Previous source {n}"))
                .bind(parent).bind(REQUIREMENTS_REJECTED_REASON).bind(now - 3600)
                .fetch_one(&db).await.unwrap();
            sqlx::query("INSERT INTO grabbed_torrent_series(grab_id,series_id,episode_numbers) VALUES(?,?,'[7]')")
                .bind(grab).bind(owner).execute(&db).await.unwrap();
        }
        let holds = failure_cooldowns(&db, &[parent, owner], now - 1)
            .await
            .unwrap();
        assert!(holds.needs_trial(owner, 7, now - 1).is_err());
        assert!(!holds.needs_trial(parent, 8, now).unwrap());
        let holds = failure_cooldowns(&db, &[parent, owner], now).await.unwrap();
        assert!(holds.needs_trial(owner, 7, now).unwrap());
        let sources = TrialSources::load(&db, owner, 7).await.unwrap();
        assert!(
            sources
                .check(
                    &format!("{:040x}", 1),
                    "Changed title",
                    SourceRef::default()
                )
                .is_err()
        );
    }
}

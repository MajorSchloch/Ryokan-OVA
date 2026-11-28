//! Latest scheduled upgrade result for each episode.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::models::grabbed_torrents::GrabbedTorrent;
use crate::models::series::Series;
use crate::services::media_requirements::{MINIMUM_SUBTITLE_REJECTION, TARGET_AUDIO_REJECTION};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    NoEligibleCandidate,
    NumberingHold,
    SourceHold,
    SourceMetadataRetry,
    Unavailable,
    PolicyHold,
    Duplicate,
    NoDownloadClient,
    ClientError,
    MetadataUnavailable,
    Queued(i64),
}

impl Outcome {
    fn storage(self) -> (&'static str, Option<i64>) {
        match self {
            Self::NoEligibleCandidate => ("no_candidate", None),
            Self::NumberingHold => ("numbering_hold", None),
            Self::SourceHold => ("source_hold", None),
            Self::SourceMetadataRetry => ("source_metadata_retry", None),
            Self::Unavailable => ("unavailable", None),
            Self::PolicyHold => ("policy_hold", None),
            Self::Duplicate => ("duplicate", None),
            Self::NoDownloadClient => ("no_client", None),
            Self::ClientError => ("client_error", None),
            Self::MetadataUnavailable => ("metadata_unavailable", None),
            Self::Queued(id) => ("queued", Some(id)),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Status {
    pub provider_id: i64,
    pub reason: &'static str,
    pub checked_at: DateTime<Utc>,
}

#[derive(Clone, Copy)]
pub(crate) enum ImportRejection {
    Subtitles,
    DialogueAudio,
    TargetAudio,
    MediaRequirements,
    Edition,
}

impl ImportRejection {
    pub(crate) fn from_probe_failure(reason: &str, content_rejection: bool) -> Option<Self> {
        if reason.starts_with("Media requirement: Replacement is substantially shorter (") {
            return Some(Self::Edition);
        }
        if !content_rejection {
            return None;
        }
        let reason = reason.strip_prefix("Media rejected: ").unwrap_or(reason);
        Some(if reason == MINIMUM_SUBTITLE_REJECTION {
            Self::Subtitles
        } else if reason == TARGET_AUDIO_REJECTION {
            Self::TargetAudio
        } else if reason.contains("Required dialogue audio is missing (") {
            Self::DialogueAudio
        } else {
            Self::MediaRequirements
        })
    }

    fn storage(self) -> &'static str {
        match self {
            Self::Subtitles => "subtitles",
            Self::DialogueAudio => "dialogue_audio",
            Self::TargetAudio => "target_audio",
            Self::MediaRequirements => "media_requirements",
            Self::Edition => "edition",
        }
    }
}

#[derive(sqlx::FromRow)]
struct Recorded {
    series_id: i64,
    provider_id: i64,
    episode: i32,
    checked_at: i64,
    outcome: String,
    grab_state: Option<String>,
    failure_reason: Option<String>,
    import_rejection: Option<String>,
    imported: bool,
}

impl Recorded {
    fn reason(&self) -> &'static str {
        if !self.imported {
            match self.import_rejection.as_deref() {
                Some("subtitles") => return "The replacement failed the selectable subtitle check.",
                Some("dialogue_audio") => return "The replacement failed the dialogue audio check.",
                Some("target_audio") => {
                    return "The replacement failed the good enough audio check.";
                }
                Some("media_requirements") => {
                    return "The replacement failed the file requirements.";
                }
                Some("edition") => {
                    return "The replacement was held because it is substantially shorter.";
                }
                _ => {}
            }
        }
        match self.outcome.as_str() {
            "numbering_hold" => "Could not verify episode numbering for the candidate releases.",
            "source_hold" => "Could not verify the source or its saved file selection.",
            "source_metadata_retry" => {
                "Numbering verified. Ryokan can retry loading the torrent's file list on a later upgrade check."
            }
            "unavailable" => "The source failed the availability check.",
            "policy_hold" => "The upgrade requirements held the replacement.",
            "duplicate" => "The client previously rejected the matching source as a duplicate.",
            "no_client" => "No download client is available for this source.",
            "client_error" => "The download client could not start the upgrade.",
            "metadata_unavailable" => "Could not verify release metadata for the upgrade.",
            "queued" => match (self.grab_state.as_deref(), self.failure_reason.as_deref()) {
                (_, Some("requirements_rejected")) => {
                    "The replacement failed the file requirements."
                }
                (_, Some("client_error")) => "The download client reported a failure.",
                (_, Some("nzbget_duplicate_copy")) => "The download client rejected a duplicate.",
                (Some("pending"), _) => "The upgrade is queued for download or import.",
                (Some("imported" | "replaced"), _) if self.imported => {
                    "A replacement imported. The current file is still below target."
                }
                (Some("imported" | "replaced"), _) => "The pack import skipped this episode.",
                (Some("failed"), _) => "The upgrade download or import failed.",
                (Some("removed" | "cancelled"), _) => "The upgrade download was removed.",
                _ => "The upgrade download record is unavailable.",
            },
            "no_candidate" => "No eligible replacement was found.",
            _ => "The upgrade result is unavailable.",
        }
    }
}

/// Store one latest result per checked slot, with an exact attempt receipt
/// for queued downloads. A removed or changed series cannot receive it.
pub(crate) async fn record(
    db: &SqlitePool,
    series: &Series,
    outcomes: &[(i32, Outcome)],
) -> Result<(), sqlx::Error> {
    if outcomes.is_empty() {
        return Ok(());
    }
    let slots: Vec<_> = outcomes
        .iter()
        .map(|(slot, outcome)| {
            let (kind, grab_id) = outcome.storage();
            serde_json::json!([slot, kind, grab_id])
        })
        .collect();
    sqlx::query(
        "INSERT INTO episode_upgrade_outcomes
             (series_id, provider_id, episode, outcome, checked_at, grab_id, grabbed_at)
         SELECT s.id, s.anilist_id, json_extract(slot.value, '$[0]'),
                json_extract(slot.value, '$[1]'), unixepoch('now'),
                json_extract(slot.value, '$[2]'), g.grabbed_at
         FROM json_each(?1) slot
         JOIN series s ON s.id=?2 AND s.anilist_id=?3
         LEFT JOIN grabbed_torrents g ON g.id=json_extract(slot.value, '$[2]') AND g.series_id=s.id
         WHERE json_extract(slot.value, '$[0]') > 0
         ON CONFLICT(series_id, episode) DO UPDATE SET
             provider_id=excluded.provider_id, outcome=excluded.outcome,
             checked_at=excluded.checked_at, grab_id=excluded.grab_id, grabbed_at=excluded.grabbed_at,
             import_rejection=CASE WHEN episode_upgrade_outcomes.provider_id=excluded.provider_id
                  AND episode_upgrade_outcomes.grab_id=excluded.grab_id
                  AND episode_upgrade_outcomes.grabbed_at=excluded.grabbed_at
                  AND excluded.outcome='queued' THEN episode_upgrade_outcomes.import_rejection END",
    )
    .bind(serde_json::Value::Array(slots).to_string())
    .bind(series.id)
    .bind(series.anilist_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Refine a recorded scheduled attempt only. Other imports and a newer grab
/// generation must leave the report alone.
pub(crate) async fn record_import_rejection(
    db: &SqlitePool,
    grab: &GrabbedTorrent,
    provider_id: i64,
    episodes: &[i32],
    rejection: ImportRejection,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE episode_upgrade_outcomes SET import_rejection=?1
         WHERE series_id=?2 AND provider_id=?3 AND grab_id=?4 AND grabbed_at=?5
           AND outcome='queued' AND episode IN (SELECT value FROM json_each(?6))
           AND EXISTS (SELECT 1 FROM grabbed_torrents g JOIN series s ON s.id=g.series_id
                       WHERE g.id=?4 AND g.series_id=?2 AND s.anilist_id=?3
                         AND g.grabbed_at=?5 AND g.state='pending')",
    )
    .bind(rejection.storage())
    .bind(grab.series_id)
    .bind(provider_id)
    .bind(grab.id)
    .bind(&grab.grabbed_at)
    .bind(serde_json::json!(episodes).to_string())
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn latest(
    db: &SqlitePool,
    series_ids: &[i64],
) -> Result<HashMap<(i64, i32), Status>, sqlx::Error> {
    if series_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, Recorded>(
        "SELECT o.series_id, o.provider_id, o.episode, o.checked_at, o.outcome,
                g.state AS grab_state, g.failure_reason,
                CASE WHEN g.id IS NOT NULL THEN o.import_rejection END AS import_rejection,
                EXISTS (SELECT 1 FROM json_each(
                    CASE WHEN json_valid(g.imported_episode_coverage)
                        THEN g.imported_episode_coverage ELSE '{}' END,
                    '$.\"' || o.series_id || '\"'
                 ) WHERE value=o.episode) AS imported
         FROM episode_upgrade_outcomes o
         JOIN series s ON s.id=o.series_id AND s.anilist_id=o.provider_id
         LEFT JOIN grabbed_torrents g
            ON g.id=o.grab_id AND g.series_id=o.series_id AND g.grabbed_at=o.grabbed_at
         WHERE o.series_id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::json!(series_ids).to_string())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some((
                (row.series_id, row.episode),
                Status {
                    provider_id: row.provider_id,
                    reason: row.reason(),
                    checked_at: DateTime::from_timestamp(row.checked_at, 0)?,
                },
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{grabbed_torrents, series};
    use crate::test_support::{in_memory_pool, seed_grabbed_torrent, seed_series};

    #[tokio::test]
    async fn latest_upgrade_outcome_tracks_partial_import_and_exact_attempt() {
        let db = in_memory_pool().await;
        let id = seed_series(&db, 2904, "Code Geass R2").await;
        let release = series::get_by_id(&db, id).await.unwrap().unwrap();
        let grab = seed_grabbed_torrent(&db, id, "test-hash", "Pack", &[1, 2, 3]).await;
        let attempt = grabbed_torrents::get_by_id(&db, grab)
            .await
            .unwrap()
            .unwrap();
        record(
            &db,
            &release,
            &[
                (1, Outcome::Queued(grab)),
                (2, Outcome::Queued(grab)),
                (3, Outcome::Queued(grab)),
            ],
        )
        .await
        .unwrap();
        let initial = latest(&db, &[id]).await.unwrap();
        assert_eq!(initial.len(), 3);
        assert!(initial[&(id, 1)].reason.contains("queued"));
        record_import_rejection(
            &db,
            &attempt,
            release.anilist_id,
            &[2],
            ImportRejection::Subtitles,
        )
        .await
        .unwrap();
        record(&db, &release, &[(2, Outcome::Queued(grab))])
            .await
            .unwrap();

        sqlx::query(
            "UPDATE grabbed_torrents SET state='imported', import_mode='partial',
                 imported_episode_coverage=?1 WHERE id=?2",
        )
        .bind(format!("{{\"{id}\":[1]}}"))
        .bind(grab)
        .execute(&db)
        .await
        .unwrap();
        let partial = latest(&db, &[id]).await.unwrap();
        assert!(partial[&(id, 1)].reason.contains("replacement imported"));
        assert!(
            partial[&(id, 2)]
                .reason
                .contains("failed the selectable subtitle check")
        );
        assert!(partial[&(id, 3)].reason.contains("skipped this episode"));

        sqlx::query("UPDATE grabbed_torrents SET grabbed_at=datetime(grabbed_at,'+1 day'), state='failed', failure_reason='requirements_rejected' WHERE id=?")
            .bind(grab)
            .execute(&db)
            .await
            .unwrap();
        let reused = latest(&db, &[id]).await.unwrap();
        assert!(reused[&(id, 2)].reason.contains("record is unavailable"));
        record(&db, &release, &[(2, Outcome::Queued(grab))])
            .await
            .unwrap();
        record_import_rejection(
            &db,
            &attempt,
            release.anilist_id,
            &[2],
            ImportRejection::Edition,
        )
        .await
        .unwrap();
        assert!(
            latest(&db, &[id]).await.unwrap()[&(id, 2)]
                .reason
                .contains("failed the file requirements")
        );
    }
}

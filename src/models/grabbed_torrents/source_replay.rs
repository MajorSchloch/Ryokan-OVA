//! A client job is not a release identity. Publish provenance and a reviewed
//! replay decision in the same transaction as the pending job.
use super::migration_replay::{self, ReplayPlan};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool};

#[derive(Clone, Copy, Default)]
pub struct SourceRef<'a> {
    pub url: &'a str,
    pub guid: &'a str,
    pub indexer_id: Option<i64>,
    pub client_id: Option<i64>,
    pub required_replay: bool,
}

pub struct SavedGrab {
    pub id: i64,
    pub episodes: Vec<i32>,
    pub replayed: bool,
}

fn fail(message: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("Episode mapping: {message}").into())
}

async fn identity(
    db: &mut SqliteConnection,
    source: SourceRef<'_>,
) -> Result<Option<String>, sqlx::Error> {
    if source.guid.trim().is_empty() {
        return Ok(None);
    }
    let Some(indexer) = source.indexer_id else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT kind,url FROM indexers WHERE id=?")
        .bind(indexer)
        .fetch_optional(&mut *db)
        .await?
        .ok_or_else(|| fail("the source indexer no longer exists"))?;
    // Preserve the opaque configured endpoint. Never strip credentials or
    // reconstruct a Prowlarr/Jackett URL. Only the digest is stored/exported.
    let input = serde_json::to_vec(&(
        1,
        indexer,
        row.get::<String, _>("kind"),
        row.get::<String, _>("url"),
        source.guid,
    ))
    .map_err(|e| sqlx::Error::Decode(e.into()))?;
    Ok(Some(format!(
        "indexer-guid-v1:{}",
        hex::encode(Sha256::digest(input))
    )))
}

fn exact_url(url: &str) -> &str {
    if reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "magnet")) {
        url
    } else {
        ""
    }
}

struct Reviewed {
    row: sqlx::sqlite::SqliteRow,
    plan: ReplayPlan,
}

async fn reviewed(
    db: &mut SqliteConnection,
    source: SourceRef<'_>,
    key: Option<&str>,
) -> Result<Option<Reviewed>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT * FROM grabbed_torrents WHERE library_migration_bridge IS NOT NULL
        AND ((?1 != '' AND source_url = ?1) OR (?2 IS NOT NULL AND source_identity = ?2)
             OR source_identity IN (SELECT source_identity FROM grabbed_torrents
                WHERE ?1 != '' AND source_url = ?1 AND library_migration_bridge IS NOT NULL))
        ORDER BY (state='imported') DESC, id DESC",
    )
    .bind(exact_url(source.url))
    .bind(key)
    .fetch_all(&mut *db)
    .await?;
    let mut selected: Option<Reviewed> = None;
    let mut established_key = key.map(str::to_owned);
    for row in rows {
        if let Some(old) = row.get::<Option<String>, _>("source_identity") {
            if established_key.as_ref().is_some_and(|new| *new != old) {
                return Err(fail(
                    "the source URL now names a different release identity",
                ));
            }
            established_key = Some(old);
        }
        if let (Some(old), Some(new)) = (row.get::<Option<i64>, _>("indexer_id"), source.indexer_id)
            && old != new
        {
            return Err(fail("the source URL belongs to another indexer"));
        }
        let plan = migration_replay::decode_bridge(row.get("library_migration_bridge"))
            .map_err(|e| sqlx::Error::Decode(e.into()))?;
        migration_replay::validate_owners(&mut *db, &plan)
            .await
            .map_err(|e| sqlx::Error::Decode(e.into()))?;
        if selected.as_ref().is_some_and(|old| old.plan != plan) {
            return Err(fail(
                "this source has conflicting reviewed file assignments",
            ));
        }
        if selected.is_none() {
            selected = Some(Reviewed { row, plan });
        }
    }
    Ok(selected)
}

/// Search/RSS may use this canonical projection before selecting a wanted slot.
/// Commit rechecks the full decision under the writer lock.
pub async fn episodes_for_source(
    db: &SqlitePool,
    source: SourceRef<'_>,
    anilist_id: i64,
) -> Result<Option<Vec<i32>>, String> {
    let mut conn = db.acquire().await.map_err(|e| e.to_string())?;
    let key = identity(&mut conn, source)
        .await
        .map_err(|e| e.to_string())?;
    let saved = reviewed(&mut conn, source, key.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    saved
        .map(|saved| {
            let sid = saved
                .plan
                .files
                .iter()
                .find(|f| f.anilist_id == anilist_id)
                .ok_or_else(|| {
                    "Episode mapping: reviewed source belongs to another entry".to_string()
                })?
                .series_id;
            Ok(saved.plan.episodes(sid))
        })
        .transpose()
}

#[allow(clippy::too_many_arguments)]
pub async fn record(
    db: &SqlitePool,
    hash: &str,
    title: &str,
    series_id: i64,
    episodes: &[i32],
    is_batch: bool,
    mapping: Option<&crate::services::release_numbering::ReleaseMapping>,
    replacement: Option<i32>,
    source: SourceRef<'_>,
) -> Result<Option<SavedGrab>, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let key = identity(&mut tx, source).await?;
    let prior = reviewed(&mut tx, source, key.as_deref()).await?;
    let finite_reviewed =
        crate::models::source_numbering::has_bound_history_in_connection(&mut tx, hash).await?;
    if finite_reviewed
        && (prior.is_some() || mapping.is_none_or(|mapping| mapping.bound_source().is_none()))
    {
        return Err(fail(
            "this exact source requires its saved finite numbering decision",
        ));
    }
    if prior.is_some() && mapping.is_some_and(|mapping| mapping.bound_source().is_some()) {
        return Err(fail(
            "the source URL already has library migration assignments",
        ));
    }
    // Restore may only know an old URL. Retain the identity learned from the
    // saved row when checking for another active job and recording this one.
    let key = key.or_else(|| {
        prior
            .as_ref()
            .and_then(|saved| saved.row.get::<Option<String>, _>("source_identity"))
    });
    if source.required_replay && prior.is_none() {
        return Err(fail(
            "the reviewed decision changed before the job could be saved",
        ));
    }
    let same_job: Option<(i64, Option<String>)> = sqlx::query_as("SELECT id,library_migration_bridge FROM grabbed_torrents WHERE hash=? COLLATE NOCASE AND hash!='' AND (state IN ('pending','imported') OR library_migration_bridge IS NOT NULL)")
        .bind(hash).fetch_optional(&mut *tx).await?;
    let has_reviewed_hash: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM grabbed_torrents WHERE hash=? COLLATE NOCASE AND hash!='' AND library_migration_bridge IS NOT NULL)")
        .bind(hash).fetch_one(&mut *tx).await?;
    if prior.is_none() && has_reviewed_hash {
        return Err(fail(
            "the client job's reviewed source identity does not match this request",
        ));
    }
    let (id, replayed, canonical) = if let Some(saved) = prior {
        let canonical = saved.plan.episodes(series_id);
        if hash.is_empty()
            || !saved.plan.files.iter().any(|f| f.series_id == series_id)
            || replacement.is_some_and(|ep| !canonical.contains(&ep))
            || ((source.required_replay || mapping.is_some())
                && episodes.iter().any(|ep| !canonical.contains(ep)))
        {
            return Err(fail(
                "the requested entry or replacement is outside the reviewed source",
            ));
        }
        let parent: i64 = saved.row.get("series_id");
        if replacement.is_some() && parent != series_id {
            return Err(fail(
                "replacement of a routed sibling requires a separate reviewed request",
            ));
        }
        // Never turn an unrelated/pending job into a replay by a late setter.
        if let Some((_, Some(bridge))) = &same_job {
            let plan = migration_replay::decode_bridge(bridge)
                .map_err(|e| sqlx::Error::Decode(e.into()))?;
            if plan != saved.plan {
                return Err(fail("the client job already has another reviewed decision"));
            }
        } else if same_job.is_some() {
            return Err(fail(
                "the client job already exists without this reviewed decision",
            ));
        }
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM grabbed_torrents WHERE state='pending' AND hash!=?1 COLLATE NOCASE AND library_migration_bridge IS NOT NULL AND ((?2!='' AND source_url=?2) OR (?3 IS NOT NULL AND source_identity=?3)))")
            .bind(hash).bind(exact_url(source.url)).bind(key.as_deref()).fetch_one(&mut *tx).await?;
        if busy {
            return Err(fail("this reviewed source already has an active download"));
        }
        let id = if same_job.is_some() {
            super::record_grab_in_connection(
                &mut tx,
                hash,
                title,
                series_id,
                &canonical,
                is_batch,
                None,
                replacement,
            )
            .await?
        } else {
            let status: String = saved.row.get("state");
            if !matches!(status.as_str(), "imported" | "removed" | "failed") {
                return Err(fail(
                    "the reviewed source cannot be replayed from its current state",
                ));
            }
            let bridge: String = saved.row.get("library_migration_bridge");
            let mut audit: serde_json::Value =
                serde_json::from_str(&bridge).map_err(|e| sqlx::Error::Decode(e.into()))?;
            audit["replay_parent_grab_id"] = serde_json::json!(saved.row.get::<i64, _>("id"));
            Some(sqlx::query_scalar("INSERT INTO grabbed_torrents
                (hash,torrent_name,series_id,episode_numbers,state,is_batch,library_migration_bridge,
                 imported_episode_coverage,imported_source_paths,source_identity,replacement_episode)
                SELECT ?,torrent_name,series_id,episode_numbers,'pending',is_batch,?,
                 imported_episode_coverage,imported_source_paths,source_identity,? FROM grabbed_torrents WHERE id=? RETURNING id")
                .bind(hash).bind(audit.to_string()).bind(replacement).bind(saved.row.get::<i64,_>("id"))
                .fetch_one(&mut *tx).await?)
        };
        (id, true, canonical)
    } else {
        if replacement.is_some_and(|ep| !(1..=5000).contains(&ep) || !episodes.contains(&ep)) {
            return Err(fail("replacement episode is outside the request"));
        }
        // A hash-bound replay must not acquire a different source identity.
        if same_job
            .as_ref()
            .is_some_and(|(_, bridge)| bridge.is_some())
        {
            return Err(fail(
                "the client job's reviewed source identity does not match this request",
            ));
        }
        let id = super::record_grab_in_connection(
            &mut tx,
            hash,
            title,
            series_id,
            episodes,
            is_batch,
            mapping,
            replacement,
        )
        .await?;
        let canonical = mapping
            .map(|m| m.episodes())
            .unwrap_or_else(|| episodes.to_vec());
        (id, false, canonical)
    };
    if let Some(id) = id {
        sqlx::query("UPDATE grabbed_torrents SET source_url=?,source_identity=COALESCE(?,source_identity),download_client_id=?,indexer_id=? WHERE id=?")
            .bind(source.url).bind(&key).bind(source.client_id).bind(source.indexer_id).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(id.map(|id| SavedGrab {
        id,
        episodes: canonical,
        replayed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};
    fn source(url: &str) -> SourceRef<'_> {
        SourceRef {
            url,
            guid: "release-123",
            indexer_id: Some(1),
            client_id: Some(1),
            required_replay: false,
        }
    }
    async fn fixture(db: &SqlitePool) -> (i64, i64) {
        sqlx::query("INSERT INTO indexers (id,name,kind,url) VALUES (1,'Example','newznab','https://indexer.example/api')").execute(db).await.unwrap();
        sqlx::query("INSERT INTO download_clients (id,name,kind,url) VALUES (1,'SAB','sabnzbd','http://sab.example')").execute(db).await.unwrap();
        let sid = seed_series(db, 9601, "Show").await;
        sqlx::query("UPDATE series SET episodes=2 WHERE id=?")
            .bind(sid)
            .execute(db)
            .await
            .unwrap();
        let saved = record(
            db,
            "SABnzbd_nzo_old",
            "Original pack",
            sid,
            &[91, 92],
            true,
            None,
            None,
            source("https://proxy.example/old"),
        )
        .await
        .unwrap()
        .unwrap();
        let files: Vec<_> = [0, 1]
            .into_iter()
            .map(|n| migration_replay::ReplayFile {
                name: format!("Pack/Show - {n:02}.mkv"),
                series_id: sid,
                anilist_id: 9601,
                season: 1,
                first: n + 1,
                last: n + 1,
                title: String::new(),
                resolution: "1080p".into(),
                source: "BluRay".into(),
                release_group: "Group".into(),
            })
            .collect();
        let bridge = serde_json::json!({"version":2,"replay_policy":"verified_files","replay": ReplayPlan{version:1, files}}).to_string();
        sqlx::query("UPDATE grabbed_torrents SET state='imported',library_migration_bridge=?,imported_episode_coverage=? WHERE id=?")
            .bind(bridge).bind(serde_json::json!({sid.to_string():[1,2]}).to_string()).bind(saved.id).execute(db).await.unwrap();
        (sid, saved.id)
    }

    #[tokio::test]
    async fn new_sab_jobs_follow_guid_across_proxy_urls_and_preserve_history() {
        let db = in_memory_pool().await;
        let (sid, old) = fixture(&db).await;
        for (job, url) in [
            ("SABnzbd_nzo_new", "https://proxy.example/new"),
            ("SABnzbd_nzo_again", "https://proxy.example/again"),
        ] {
            let saved = record(
                &db,
                job,
                "Changed title",
                sid,
                &[99],
                false,
                None,
                None,
                source(url),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(saved.replayed);
            assert_ne!(saved.id, old);
            assert_eq!(saved.episodes, vec![1, 2]);
            let row = sqlx::query("SELECT * FROM grabbed_torrents WHERE id=?")
                .bind(saved.id)
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(row.get::<String, _>("hash"), job);
            assert_eq!(row.get::<String, _>("episode_numbers"), "[91,92]");
            assert_eq!(row.get::<String, _>("torrent_name"), "Original pack");
            assert_eq!(row.get::<Option<i64>, _>("download_client_id"), Some(1));
            assert!(
                migration_replay::get_migration_replay(&db, saved.id)
                    .await
                    .unwrap()
                    .is_some()
            );
            sqlx::query("UPDATE grabbed_torrents SET state='imported' WHERE id=?")
                .bind(saved.id)
                .execute(&db)
                .await
                .unwrap();
        }
        let row = sqlx::query("SELECT hash,source_url,state FROM grabbed_torrents WHERE id=?")
            .bind(old)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("hash"), "SABnzbd_nzo_old");
        assert_eq!(
            row.get::<String, _>("source_url"),
            "https://proxy.example/old"
        );
        assert_eq!(row.get::<String, _>("state"), "imported");
    }

    #[tokio::test]
    async fn same_reviewed_job_reuses_removed_and_failed_records() {
        let db = in_memory_pool().await;
        let (sid, old) = fixture(&db).await;
        for state in ["failed", "removed", "imported"] {
            sqlx::query("UPDATE grabbed_torrents SET state=? WHERE id=?")
                .bind(state)
                .bind(old)
                .execute(&db)
                .await
                .unwrap();
            let saved = record(
                &db,
                "SABnzbd_nzo_old",
                "Changed title",
                sid,
                &[99],
                true,
                None,
                None,
                source("https://proxy.example/old"),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(saved.id, old);
            assert_eq!(saved.episodes, vec![1, 2]);
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[tokio::test]
    async fn legacy_exact_url_can_gain_a_guid_but_a_title_cannot_select_a_mapping() {
        let db = in_memory_pool().await;
        let (sid, old) = fixture(&db).await;
        sqlx::query("UPDATE grabbed_torrents SET source_identity=NULL WHERE id=?")
            .bind(old)
            .execute(&db)
            .await
            .unwrap();
        let saved = record(
            &db,
            "SABnzbd_nzo_legacy",
            "Same title is irrelevant",
            sid,
            &[0],
            true,
            None,
            None,
            source("https://proxy.example/old"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(saved.replayed);
        sqlx::query("UPDATE grabbed_torrents SET state='imported' WHERE id=?")
            .bind(saved.id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            episodes_for_source(&db, source("https://proxy.example/rotated"), 9601)
                .await
                .unwrap(),
            Some(vec![1, 2])
        );
        let unknown = SourceRef {
            guid: "different-release",
            ..source("https://proxy.example/other")
        };
        let other = record(
            &db,
            "SABnzbd_nzo_other",
            "Original pack",
            sid,
            &[1],
            false,
            None,
            None,
            unknown,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!other.replayed);
        assert!(
            migration_replay::get_migration_replay(&db, other.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn conflicting_identity_busy_source_and_late_setters_cannot_overwrite_a_decision() {
        let db = in_memory_pool().await;
        let (sid, old) = fixture(&db).await;
        let wrong = SourceRef {
            guid: "another-release",
            ..source("https://proxy.example/old")
        };
        assert!(
            record(
                &db,
                "SABnzbd_nzo_stale",
                "Pack",
                sid,
                &[1],
                true,
                None,
                None,
                SourceRef {
                    guid: "unknown",
                    required_replay: true,
                    ..source("https://proxy.example/new")
                }
            )
            .await
            .is_err()
        );
        assert!(
            record(
                &db,
                "SABnzbd_nzo_bad",
                "Original pack",
                sid,
                &[1],
                true,
                None,
                None,
                wrong
            )
            .await
            .is_err()
        );
        assert!(
            super::super::set_source_url(&db, old, "https://proxy.example/other")
                .await
                .is_err()
        );
        let first = record(
            &db,
            "SABnzbd_nzo_first",
            "Pack",
            sid,
            &[1],
            true,
            None,
            None,
            source("https://proxy.example/new"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            record(
                &db,
                "SABnzbd_nzo_second",
                "Pack",
                sid,
                &[1],
                true,
                None,
                None,
                source("https://proxy.example/second")
            )
            .await
            .is_err()
        );
        assert!(
            record(
                &db,
                "SABnzbd_nzo_restore",
                "Pack",
                sid,
                &[1],
                true,
                None,
                None,
                SourceRef {
                    guid: "",
                    ..source("https://proxy.example/old")
                }
            )
            .await
            .is_err(),
            "an old URL without a GUID must still detect the active job at a new URL"
        );
        assert!(
            record(
                &db,
                "SABnzbd_nzo_first",
                "Pack",
                sid,
                &[1],
                true,
                None,
                Some(1),
                source("https://proxy.example/new")
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(
            super::super::get_replacement_episode(&db, first.id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn corrupt_mapping_changed_owner_and_commit_failure_leave_no_pending_job() {
        for failure in ["owner", "bridge", "write"] {
            let db = in_memory_pool().await;
            let (sid, old) = fixture(&db).await;
            match failure {
                "owner" => {
                    sqlx::query("UPDATE series SET anilist_id=9602 WHERE id=?")
                        .bind(sid)
                        .execute(&db)
                        .await
                        .unwrap();
                }
                "bridge" => {
                    sqlx::query(
                        "UPDATE grabbed_torrents SET library_migration_bridge='broken' WHERE id=?",
                    )
                    .bind(old)
                    .execute(&db)
                    .await
                    .unwrap();
                }
                _ => {
                    sqlx::query("CREATE TRIGGER refuse_source BEFORE UPDATE OF source_url ON grabbed_torrents BEGIN SELECT RAISE(ABORT,'injected failure'); END").execute(&db).await.unwrap();
                }
            }
            assert!(
                record(
                    &db,
                    "SABnzbd_nzo_new",
                    "Pack",
                    sid,
                    &[1],
                    true,
                    None,
                    None,
                    source("https://proxy.example/new")
                )
                .await
                .is_err(),
                "{failure}"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
                    .fetch_one(&db)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT state FROM grabbed_torrents WHERE id=?")
                    .bind(old)
                    .fetch_one(&db)
                    .await
                    .unwrap(),
                "imported"
            );
        }
    }

    #[tokio::test]
    async fn indexer_scope_and_endpoint_changes_do_not_reuse_a_guid() {
        let db = in_memory_pool().await;
        fixture(&db).await;
        sqlx::query("INSERT INTO indexers (id,name,kind,url) VALUES (2,'Other','newznab','https://indexer.example/api')").execute(&db).await.unwrap();
        assert!(
            episodes_for_source(
                &db,
                SourceRef {
                    indexer_id: Some(2),
                    ..source("https://proxy.example/new")
                },
                9601
            )
            .await
            .unwrap()
            .is_none()
        );
        sqlx::query("UPDATE indexers SET url='https://other.example/api' WHERE id=1")
            .execute(&db)
            .await
            .unwrap();
        assert!(
            episodes_for_source(&db, source("https://proxy.example/new"), 9601)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            episodes_for_source(&db, source("https://proxy.example/old"), 9601)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn automatic_gate_uses_source_identity_before_the_requested_slot() {
        let db = in_memory_pool().await;
        fixture(&db).await;
        let detail = serde_json::from_value(serde_json::json!({
            "id":9601,"title_romaji":"Show","title_english":"Show","title_native":"",
            "cover_url":"","banner_url":"","format":"TV","status":"FINISHED","status_display":"Finished",
            "episodes":2,"season":"","description":"","genres":[],"score_is_ten_point":false,
            "score_class":"","synonyms":[],"streaming_episodes":[],"relations":[]
        })).unwrap();
        let candidate = crate::services::indexers::Release {
            indexer_id: 1,
            indexer_priority: 1,
            indexer_name: "Example".into(),
            title: "Show - 00".into(),
            guid: "release-123".into(),
            link: "https://proxy.example/new".into(),
            magnet: String::new(),
            publish_date: 0,
            size_bytes: 1,
            seeders: 1,
            leechers: 0,
            info_hash: String::new(),
            categories: vec![5070],
            download_volume_factor: None,
            upload_volume_factor: None,
            extra: Default::default(),
        }
        .into_search_result();
        use crate::services::{
            auto_search::SearchTarget, release_numbering::filter_automatic_candidates,
        };
        let accepted = filter_automatic_candidates(
            &db,
            &detail,
            999,
            &SearchTarget::Episode(1),
            vec![candidate.clone()],
        )
        .await;
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].replay_episodes, Some(vec![1, 2]));
        assert!(accepted[0].release_mapping.is_none());
        let mut stale = accepted[0].clone();
        assert!(
            filter_automatic_candidates(
                &db,
                &detail,
                999,
                &SearchTarget::Episode(3),
                vec![candidate.clone()]
            )
            .await
            .is_empty()
        );
        let mut other = candidate;
        other.source_guid = "another-release".into();
        assert!(
            filter_automatic_candidates(&db, &detail, 999, &SearchTarget::Episode(1), vec![other])
                .await
                .is_empty()
        );
        sqlx::query("UPDATE grabbed_torrents SET library_migration_bridge=NULL")
            .execute(&db)
            .await
            .unwrap();
        stale.title = "Show - 01".into();
        assert!(
            filter_automatic_candidates(&db, &detail, 0, &SearchTarget::Episode(1), vec![stale])
                .await
                .is_empty(),
            "a removed reviewed decision must not fall back to title numbering"
        );
    }
}

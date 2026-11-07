//! Replacement intent and actual import coverage are separate from the
//! numbering decision. Selecting a pack does not authorize every replacement,
//! and keeping a file already on disk does not make the new pack its owner.

use sqlx::SqlitePool;
use std::collections::BTreeMap;

/// Legacy or invalid bridge data stays held. A reviewed replay also checks
/// that the current entry IDs and counts still support its target episodes.
pub async fn ensure_migration_replay_allowed(db: &SqlitePool, grab_id: i64) -> Result<(), String> {
    super::migration_replay::get_migration_replay(db, grab_id)
        .await
        .map(|_| ())
}

pub async fn legacy_imported_name_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT g.torrent_name FROM grabbed_torrents g
        WHERE g.state = 'imported' AND g.imported_episode_coverage IS NULL
          AND (g.series_id = ? OR EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id = g.id AND r.series_id = ?))
        ORDER BY g.grabbed_at DESC, g.id DESC LIMIT 1")
        .bind(series_id).bind(series_id).fetch_optional(db).await
}

pub async fn get_replacement_episode(db: &SqlitePool, grab_id: i64) -> Result<Option<i32>, String> {
    let value: Option<i32> =
        sqlx::query_scalar("SELECT replacement_episode FROM grabbed_torrents WHERE id = ?")
            .bind(grab_id)
            .fetch_one(db)
            .await
            .map_err(|e| format!("Import protection: cannot read replacement request: {e}"))?;
    if value.is_some_and(|ep| !(1..=5000).contains(&ep)) {
        return Err("Import protection: invalid replacement request".into());
    }
    Ok(value)
}

/// NULL means a legacy grab whose original claims are the only evidence.
/// An empty map means this import deliberately kept every existing file.
/// Never change the saved numbering or route claims to describe this result.
pub async fn stamp_imported_episode_coverage(
    db: &SqlitePool,
    grab_id: i64,
    coverage: &BTreeMap<i64, Vec<i32>>,
) -> Result<(), sqlx::Error> {
    let json = serde_json::to_string(coverage).map_err(|e| sqlx::Error::Decode(e.into()))?;
    sqlx::query("UPDATE grabbed_torrents SET imported_episode_coverage = ? WHERE id = ?")
        .bind(json)
        .bind(grab_id)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::grabbed_torrents as grabs;
    use crate::test_support::{in_memory_pool, seed_series};

    #[tokio::test]
    async fn download_history_shows_canonical_ownership_and_mapped_old_attempts() {
        let db = in_memory_pool().await;
        let sid = seed_series(&db, 9408, "Mapped history").await;
        let gid = grabs::record_grab(&db, "history-map", "Pack", sid, &[1, 2], true)
            .await
            .unwrap()
            .unwrap();
        let bridge = serde_json::json!({"version":1,"identities":[
            {"from_series":sid,"from_episode":1,"to_series":sid,"to_episode":2},
            {"from_series":sid,"from_episode":2,"to_series":sid,"to_episode":3}
        ]})
        .to_string();
        sqlx::query(
            "UPDATE grabbed_torrents SET library_migration_bridge=?, state='failed' WHERE id=?",
        )
        .bind(bridge)
        .bind(gid)
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            grabs::get_blocked(&db, "english").await.unwrap()[0].episode_numbers,
            vec![2, 3]
        );
        assert_eq!(
            grabs::get_all_with_series(&db, 10, "english")
                .await
                .unwrap()[0]
                .episode_numbers,
            vec![2, 3]
        );
        grabs::mark_imported(&db, gid).await.unwrap();
        stamp_imported_episode_coverage(&db, gid, &BTreeMap::from([(sid, vec![1, 2, 3])]))
            .await
            .unwrap();
        assert_eq!(
            grabs::get_all_with_series(&db, 10, "english")
                .await
                .unwrap()[0]
                .episode_numbers,
            vec![1, 2, 3]
        );
        let original: String =
            sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents WHERE id=?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(original, "[1,2]");
        for invalid in [
            "not JSON",
            r#"{"identities":[]}"#,
            r#"{"version":2,"identities":[]}"#,
        ] {
            sqlx::query("UPDATE grabbed_torrents SET imported_episode_coverage=NULL, library_migration_bridge=? WHERE id=?")
                .bind(invalid).bind(gid).execute(&db).await.unwrap();
            assert!(
                grabs::get_all_with_series(&db, 10, "english")
                    .await
                    .unwrap()[0]
                    .episode_numbers
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn migrated_grab_cannot_reactivate_or_replay_even_with_corrupt_bridge() {
        let db = in_memory_pool().await;
        let sid = seed_series(&db, 9404, "Migrated").await;
        let gid = grabs::record_grab(&db, "migrated", "Pack", sid, &[1], true)
            .await
            .unwrap()
            .unwrap();
        ensure_migration_replay_allowed(&db, gid).await.unwrap();
        sqlx::query("UPDATE grabbed_torrents SET state='imported', library_migration_bridge='not valid JSON', imported_episode_coverage=? WHERE id=?")
            .bind(format!("{{\"{sid}\":[2]}}" )).bind(gid).execute(&db).await.unwrap();
        for state in ["imported", "removed", "pending", "failed"] {
            sqlx::query("UPDATE grabbed_torrents SET state=? WHERE id=?")
                .bind(state)
                .bind(gid)
                .execute(&db)
                .await
                .unwrap();
            assert!(
                ensure_migration_replay_allowed(&db, gid)
                    .await
                    .unwrap_err()
                    .starts_with("Episode mapping:")
            );
            assert!(
                grabs::record_grab(&db, "MIGRATED", "Pack", sid, &[1], true)
                    .await
                    .is_err()
            );
            let actual: String =
                sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id=?")
                    .bind(gid)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(actual, state);
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn canonical_ownership_can_move_to_an_entry_without_a_legacy_route() {
        let db = in_memory_pool().await;
        let old = seed_series(&db, 9405, "Old").await;
        let new = seed_series(&db, 9406, "New").await;
        let gid = grabs::record_grab(&db, "split", "Pack", old, &[1, 2], true)
            .await
            .unwrap()
            .unwrap();
        grabs::mark_imported(&db, gid).await.unwrap();
        stamp_imported_episode_coverage(&db, gid, &BTreeMap::from([(new, vec![1])]))
            .await
            .unwrap();
        let found = grabs::find_imported_for_episode(&db, new, 1).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].episode_numbers, vec![1]);
        assert!(
            grabs::find_imported_for_episode(&db, old, 1)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            grabs::imported_grabs_for_series(&db, new).await.unwrap(),
            vec![("Pack".into(), vec![1])]
        );
    }

    #[tokio::test]
    async fn replacement_permission_survives_migration_but_not_a_new_automatic_grab() {
        let db = in_memory_pool().await;
        let sid = seed_series(&db, 9401, "Show").await;
        let gid = grabs::record_manual_episode_grab(&db, "intent", "Show 01-02", sid, &[1, 2], 1)
            .await
            .unwrap()
            .unwrap();
        crate::models::migrate(&db).await.unwrap();
        assert_eq!(get_replacement_episode(&db, gid).await.unwrap(), Some(1));
        // A concurrent attempt cannot broaden an active request.
        assert!(
            grabs::record_manual_episode_grab(&db, "intent", "Show 01-02", sid, &[1, 2], 2)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(get_replacement_episode(&db, gid).await.unwrap(), Some(1));
        grabs::mark_imported(&db, gid).await.unwrap();
        stamp_imported_episode_coverage(&db, gid, &BTreeMap::from([(sid, vec![1])]))
            .await
            .unwrap();
        assert_eq!(
            grabs::record_grab(&db, "intent", "Show 01-02", sid, &[1, 2], true)
                .await
                .unwrap(),
            Some(gid)
        );
        assert_eq!(get_replacement_episode(&db, gid).await.unwrap(), None);
        let coverage: Option<String> = sqlx::query_scalar(
            "SELECT imported_episode_coverage FROM grabbed_torrents WHERE id = ?",
        )
        .bind(gid)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(coverage.is_none());
        assert!(
            grabs::record_manual_episode_grab(&db, "bad-intent", "Show", sid, &[1], 2)
                .await
                .is_err()
        );
        sqlx::query("UPDATE grabbed_torrents SET replacement_episode = 0 WHERE id = ?")
            .bind(gid)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            get_replacement_episode(&db, gid)
                .await
                .unwrap_err()
                .starts_with("Import protection:")
        );
    }

    #[tokio::test]
    async fn imported_coverage_excludes_protected_parent_and_sibling_episodes() {
        let db = in_memory_pool().await;
        let parent = seed_series(&db, 9402, "Parent").await;
        let sibling = seed_series(&db, 9403, "Sibling").await;
        let gid = grabs::record_grab(&db, "coverage", "Pack", parent, &[1, 2], true)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("INSERT INTO grabbed_torrent_series (grab_id, series_id, file_indices, episode_numbers, matched_subtitle, episode_offset) VALUES (?, ?, '[2,3]', '[1,2]', '', 0)")
            .bind(gid).bind(sibling).execute(&db).await.unwrap();
        grabs::mark_imported(&db, gid).await.unwrap();
        assert_eq!(
            grabs::find_imported_for_episode(&db, sibling, 1)
                .await
                .unwrap()
                .len(),
            1,
            "legacy claims still work"
        );
        stamp_imported_episode_coverage(
            &db,
            gid,
            &BTreeMap::from([(parent, vec![2]), (sibling, vec![1])]),
        )
        .await
        .unwrap();
        for (sid, present, absent) in [(parent, 2, 1), (sibling, 1, 2)] {
            assert_eq!(
                grabs::find_imported_for_episode(&db, sid, present)
                    .await
                    .unwrap()[0]
                    .id,
                gid
            );
            assert!(
                grabs::find_imported_for_episode(&db, sid, absent)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                grabs::imported_grabs_for_series(&db, sid).await.unwrap()[0].1,
                vec![present]
            );
            assert!(
                legacy_imported_name_for_series(&db, sid)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let claims: String =
            sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents WHERE id = ?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            claims, "[1,2]",
            "import results must not rewrite the numbering decision"
        );
    }
}

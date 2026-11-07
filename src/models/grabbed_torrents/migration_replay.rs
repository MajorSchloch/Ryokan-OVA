//! Reviewed source-file identities survive a library numbering migration.
//! A requested episode, source filename number or current offset cannot replace
//! this decision. Unknown/changed file sets require a new review.
use crate::services::post_processing;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayPlan {
    pub version: u32,
    pub files: Vec<ReplayFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayFile {
    pub name: String,
    pub series_id: i64,
    pub anilist_id: i64,
    pub season: i32,
    pub first: i32,
    pub last: i32,
    pub title: String,
    pub resolution: String,
    pub source: String,
    pub release_group: String,
}

fn fail(message: impl std::fmt::Display) -> String {
    format!("Episode mapping: {message}")
}

impl ReplayPlan {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.files.is_empty() {
            return Err(fail("unsupported or empty replay plan"));
        }
        let mut names = BTreeSet::new();
        let mut slots = BTreeSet::new();
        let mut owners = BTreeMap::new();
        for file in &self.files {
            post_processing::validate_relative_path_fragment(&file.name).map_err(fail)?;
            if !post_processing::is_video_file(&file.name)
                || !names.insert(&file.name)
                || file.series_id <= 0
                || file.anilist_id == 0
                || !matches!(file.season, 0 | 1)
                || file.first < 1
                || file.last < file.first
                || file.last > 5000
                || file.last - file.first >= crate::services::media::MAX_FILE_SPAN
                || (file.season == 0 && (file.first != file.last || file.title.trim().is_empty()))
            {
                return Err(fail("invalid reviewed source-file identity"));
            }
            if owners
                .insert(file.series_id, file.anilist_id)
                .is_some_and(|old| old != file.anilist_id)
            {
                return Err(fail("conflicting replay entry identities"));
            }
            for ep in file.first..=file.last {
                if !slots.insert((file.series_id, file.season, ep)) {
                    return Err(fail("two source files claim one replay slot"));
                }
            }
        }
        Ok(())
    }

    pub fn episodes(&self, series_id: i64) -> Vec<i32> {
        self.files
            .iter()
            .filter(|f| f.series_id == series_id && f.season == 1)
            .flat_map(|f| f.first..=f.last)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn bind_files<'a>(
        &'a self,
        files: &[(usize, &str, bool)],
    ) -> Result<HashMap<usize, &'a ReplayFile>, String> {
        self.validate()?;
        let saved: HashMap<_, _> = self.files.iter().map(|f| (f.name.as_str(), f)).collect();
        let mut seen = BTreeSet::new();
        let mut bound = HashMap::new();
        for &(index, name, wanted) in files {
            if !seen.insert(name) {
                return Err(fail("duplicate replay source path"));
            }
            if wanted {
                bound.insert(
                    index,
                    *saved.get(name).ok_or_else(|| {
                        fail(format!(
                            "source file '{name}' has no reviewed replay identity"
                        ))
                    })?,
                );
            }
        }
        if self.files.iter().any(|f| !seen.contains(f.name.as_str())) {
            return Err(fail("a reviewed source file is missing"));
        }
        Ok(bound)
    }
}

pub fn decode_bridge(json: &str) -> Result<ReplayPlan, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(fail)?;
    if value["version"] != 2 || value["replay_policy"] != "verified_files" {
        return Err(fail(
            "this download predates a library migration; its source-file mapping still needs review",
        ));
    }
    let plan: ReplayPlan = serde_json::from_value(value["replay"].clone()).map_err(fail)?;
    plan.validate()?;
    Ok(plan)
}

pub async fn get_migration_replay(
    db: &SqlitePool,
    grab_id: i64,
) -> Result<Option<ReplayPlan>, String> {
    let bridge: Option<String> =
        sqlx::query_scalar("SELECT library_migration_bridge FROM grabbed_torrents WHERE id=?")
            .bind(grab_id)
            .fetch_one(db)
            .await
            .map_err(fail)?;
    let plan = bridge.as_deref().map(decode_bridge).transpose()?;
    if let Some(plan) = &plan {
        validate_owners(&mut *db.acquire().await.map_err(fail)?, plan).await?;
    }

    Ok(plan)
}

pub(super) async fn validate_owners(
    db: &mut sqlx::SqliteConnection,
    plan: &ReplayPlan,
) -> Result<(), String> {
    let mut owners: BTreeMap<i64, (i64, i32)> = BTreeMap::new();
    for f in &plan.files {
        let owner = owners.entry(f.series_id).or_insert((f.anilist_id, 0));
        if f.season == 1 {
            owner.1 = owner.1.max(f.last);
        }
    }
    for (sid, (anilist_id, last)) in owners {
        let row = sqlx::query("SELECT anilist_id, episodes FROM series WHERE id=?")
            .bind(sid)
            .fetch_optional(&mut *db)
            .await
            .map_err(fail)?
            .ok_or_else(|| fail("replay entry no longer exists"))?;
        if row.get::<i64, _>("anilist_id") != anilist_id
            || (last > 0
                && row
                    .get::<Option<i32>, _>("episodes")
                    .is_none_or(|n| n < last))
        {
            return Err(fail("replay entry identity or episode count changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(name: &str, first: i32) -> ReplayFile {
        ReplayFile {
            name: name.into(),
            series_id: 1,
            anilist_id: 101,
            season: 1,
            first,
            last: first,
            title: String::new(),
            resolution: String::new(),
            source: String::new(),
            release_group: String::new(),
        }
    }
    #[test]
    fn exact_paths_override_source_numbers_and_order() {
        let plan = ReplayPlan {
            version: 1,
            files: vec![
                file("Specials/Show - S00E00.mkv", 1),
                file("Season 1/Show - 01.mkv", 2),
            ],
        };
        let bound = plan
            .bind_files(&[
                (9, "Season 1/Show - 01.mkv", true),
                (3, "Specials/Show - S00E00.mkv", true),
            ])
            .unwrap();
        assert_eq!(bound[&3].first, 1);
        assert_eq!(bound[&9].first, 2);
        assert!(plan.bind_files(&[(0, "Show - 01.mkv", true)]).is_err());
        assert!(
            plan.bind_files(&[(0, "Season 1/Show - 01.mkv", true)])
                .is_err()
        );
    }
    #[test]
    fn duplicate_targets_unsafe_paths_and_unknown_versions_are_rejected() {
        let mut plan = ReplayPlan {
            version: 1,
            files: vec![file("Show - 01.mkv", 1), file("Show - 02.mkv", 1)],
        };
        assert!(plan.validate().is_err());
        plan.files.pop();
        plan.files[0].name = "../Show - 01.mkv".into();
        assert!(plan.validate().is_err());
        for json in [
            "broken",
            r#"{"version":1}"#,
            r#"{"version":3,"replay_policy":"verified_files"}"#,
        ] {
            assert!(decode_bridge(json).is_err());
        }
    }

    #[tokio::test]
    async fn verified_reactivation_preserves_source_claims_and_limits_replacement_intent() {
        use crate::models::grabbed_torrents as grabs;
        use crate::test_support::{in_memory_pool, seed_series};
        let db = in_memory_pool().await;
        let sid = seed_series(&db, 101, "Show").await;
        sqlx::query("UPDATE series SET episodes=2 WHERE id=?")
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        let gid = grabs::record_grab(&db, "saved-source", "Pack", sid, &[99], true)
            .await
            .unwrap()
            .unwrap();
        let mut f = file("Show - 00.mkv", 1);
        f.series_id = sid;
        let plan = ReplayPlan {
            version: 1,
            files: vec![f],
        };
        let bridge =
            serde_json::json!({"version":2,"replay_policy":"verified_files","replay":plan})
                .to_string();
        sqlx::query("UPDATE grabbed_torrents SET state='imported', library_migration_bridge=?, imported_source_paths='[\"/source/Show - 00.mkv\"]' WHERE id=?")
            .bind(&bridge).bind(gid).execute(&db).await.unwrap();
        assert!(
            grabs::record_grab(&db, "saved-source", "Pack", sid, &[2], true)
                .await
                .is_err()
        );
        assert_eq!(
            grabs::record_manual_episode_grab(&db, "saved-source", "Pack", sid, &[1], 1)
                .await
                .unwrap(),
            Some(gid)
        );
        assert_eq!(
            grabs::get_replacement_episode(&db, gid).await.unwrap(),
            Some(1)
        );
        assert!(get_migration_replay(&db, gid).await.unwrap().is_some());
        let claims: String =
            sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents WHERE id=?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(claims, "[99]");
        assert!(
            grabs::record_grab(&db, "saved-source", "Pack", sid, &[1], true)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            grabs::get_replacement_episode(&db, gid).await.unwrap(),
            Some(1)
        );
        grabs::mark_imported(&db, gid).await.unwrap();
        assert_eq!(
            grabs::record_grab(&db, "saved-source", "Pack", sid, &[1], true)
                .await
                .unwrap(),
            Some(gid)
        );
        assert_eq!(
            grabs::get_replacement_episode(&db, gid).await.unwrap(),
            None
        );
        let still_saved: String =
            sqlx::query_scalar("SELECT library_migration_bridge FROM grabbed_torrents WHERE id=?")
                .bind(gid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(still_saved, bridge);
        sqlx::query("UPDATE series SET anilist_id=102 WHERE id=?")
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        assert!(get_migration_replay(&db, gid).await.is_err());
    }
}

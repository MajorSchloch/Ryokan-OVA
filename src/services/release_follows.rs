//! Follow AniList sequel edges independently of library and file ownership.
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

use crate::models::release_follows::{self, Follow};
use crate::services::anilist::{self, RelatedEntry};

pub static SETTINGS_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();

impl Follow {
    fn follows_edge(&self, entry: &RelatedEntry) -> bool {
        entry.id > 0
            && entry.media_type == "ANIME"
            && (entry.relation_type == "SEQUEL"
                || (self.alternatives && entry.relation_type == "ALTERNATIVE"))
    }

    fn accepts_release(&self, entry: &RelatedEntry) -> bool {
        let format_allowed = match entry.format.as_str() {
            "MOVIE" => self.movies,
            "OVA" => self.ovas,
            "TV" | "TV_SHORT" | "ONA" => self.tv,
            _ => false,
        };
        format_allowed
            && entry.status != "CANCELLED"
            && (entry.status == "NOT_YET_RELEASED"
                || entry
                    .start_date
                    .as_ref()
                    .is_some_and(|date| date >= &self.started_on))
    }
}

async fn check_follow(
    db: &SqlitePool,
    follow: &Follow,
    graph: &mut HashMap<i64, Vec<RelatedEntry>>,
) -> Result<usize, String> {
    let mut visited = HashSet::from([follow.anilist_id]);
    if follow.revision > 0 {
        let selected: Vec<i64> = sqlx::query_scalar(
            "SELECT anilist_id FROM franchise_choices WHERE follow_id=? AND selected=1",
        )
        .bind(follow.id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        visited.extend(selected);
    }
    let mut frontier: Vec<_> = visited.iter().copied().collect();
    let mut added = 0;
    let mut incomplete = false;
    while !frontier.is_empty() {
        let missing: Vec<_> = frontier
            .iter()
            .copied()
            .filter(|id| !graph.contains_key(id))
            .collect();
        if !missing.is_empty() {
            for (id, detail) in anilist::get_anime_details_batch(&missing).await? {
                graph.insert(id, detail.relations);
            }
        }
        let mut next = Vec::new();
        for id in frontier {
            let Some(entries) = graph.get(&id) else {
                incomplete = true;
                continue;
            };
            for entry in entries {
                if !follow.follows_edge(entry) || !visited.insert(entry.id) {
                    continue;
                }
                // A skipped movie or an older season can lead to a new season.
                next.push(entry.id);
                if follow.accepts_release(entry)
                    && release_follows::accept(db, follow, entry)
                        .await
                        .map_err(|e| e.to_string())?
                {
                    added += 1;
                }
            }
        }
        frontier = next;
    }
    if incomplete {
        return Err("AniList did not return all requested entries".into());
    }
    Ok(added)
}

pub async fn refresh(db: &SqlitePool) -> Result<usize, String> {
    let follows = release_follows::list(db).await.map_err(|e| e.to_string())?;
    let mut graph = HashMap::new();
    let mut added = 0;
    let mut errors = Vec::new();
    for follow in follows {
        let result = check_follow(db, &follow, &mut graph).await;
        if let Err(e) = release_follows::record_check(db, &follow, result.is_ok()).await {
            errors.push(format!(
                "Could not save check for AniList {}: {e}",
                follow.anilist_id
            ));
        }
        match result {
            Ok(n) => added += n,
            Err(e) => errors.push(format!("AniList {}: {e}", follow.anilist_id)),
        }
    }
    if errors.is_empty() {
        Ok(added)
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn franchise_follow_discovers_from_a_selected_side_story_once_and_respects_skips() {
        let db = crate::test_support::in_memory_pool().await;
        release_follows::start(&db, 1, "Franchise", true, false, false)
            .await
            .unwrap();
        sqlx::raw_sql(
            "UPDATE release_follows SET tv=0,revision=1;
            INSERT INTO franchise_choices SELECT 2,id,'Side story',1 FROM release_follows;
            INSERT INTO franchise_choices SELECT 3,id,'Skipped movie',0 FROM release_follows;
            INSERT INTO external_sync_exclusions(anilist_id,title) VALUES(6,'Excluded movie');",
        )
        .execute(&db)
        .await
        .unwrap();
        let follow = release_follows::list(&db).await.unwrap().remove(0);
        let sequel = |id, format| {
            serde_json::from_value::<RelatedEntry>(serde_json::json!({
            "id":id,"title_romaji":format!("Sequel {id}"),"title_english":"","title_native":"","cover_url":"",
            "format":format,"status":"NOT_YET_RELEASED","status_display":"","relation_type":"SEQUEL","media_type":"ANIME"
        })).unwrap()
        };
        let mut graph = HashMap::from([
            (1, vec![]),
            (
                2,
                vec![
                    sequel(3, "MOVIE"),
                    sequel(4, "TV"),
                    sequel(5, "MOVIE"),
                    sequel(6, "MOVIE"),
                ],
            ),
            (3, vec![]),
            (4, vec![]),
            (5, vec![]),
            (6, vec![]),
        ]);
        assert_eq!(check_follow(&db, &follow, &mut graph).await.unwrap(), 1);
        assert_eq!(check_follow(&db, &follow, &mut graph).await.unwrap(), 0);
        assert_eq!(crate::test_support::count_series(&db).await, 1);
        assert!(
            crate::models::series::get_by_anilist_id(&db, 5)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM followed_releases")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        sqlx::query("UPDATE release_follows SET revision=2")
            .execute(&db)
            .await
            .unwrap();
        assert!(
            !release_follows::accept(&db, &follow, &sequel(7, "MOVIE"))
                .await
                .unwrap()
        );
    }

    #[test]
    fn release_follows_format_and_relationship_filters_are_independent() {
        let mut follow = Follow {
            id: 1,
            anilist_id: 1,
            title: "Source".into(),
            started_on: "2026-05-10".into(),
            movies: false,
            ovas: false,
            alternatives: false,
            ..Follow::default()
        };
        let mut entry: RelatedEntry = serde_json::from_value(serde_json::json!({
            "id":2, "title_romaji":"Related", "title_english":"", "title_native":"", "cover_url":"",
            "format":"MOVIE", "status":"FINISHED", "status_display":"Finished", "relation_type":"ALTERNATIVE",
            "media_type":"ANIME", "start_date":"2026-05-10"
        })).unwrap();
        let allowed = |f: &Follow, e: &RelatedEntry| f.follows_edge(e) && f.accepts_release(e);
        follow.movies = true;
        assert!(!allowed(&follow, &entry));
        follow.movies = false;
        follow.alternatives = true;
        assert!(!allowed(&follow, &entry));
        follow.movies = true;
        assert!(allowed(&follow, &entry));
        entry.format = "OVA".into();
        assert!(!allowed(&follow, &entry));
        follow.ovas = true;
        assert!(allowed(&follow, &entry));
        follow.alternatives = false;
        entry.relation_type = "SEQUEL".into();
        assert!(allowed(&follow, &entry));
        entry.start_date = Some("2026-05-09".into());
        assert!(!allowed(&follow, &entry));
        entry.start_date = None;
        assert!(!allowed(&follow, &entry));
        entry.status = "NOT_YET_RELEASED".into();
        assert!(allowed(&follow, &entry));
        entry.relation_type = "PREQUEL".into();
        assert!(!allowed(&follow, &entry));
        entry.relation_type = "SEQUEL".into();
        entry.media_type = "MANGA".into();
        assert!(!allowed(&follow, &entry));
    }
}

use super::tests::{empty_anime_detail, related_entry};
use super::*;
use crate::test_support::{in_memory_pool, seed_series};

fn context() -> AutoExpandGrabContext {
    AutoExpandGrabContext {
        classification: ClassificationResult::unknown(),
        release_group: String::new(),
        size_bytes: 0,
    }
}

#[tokio::test]
async fn wanted_files_replace_broad_parent_claims_and_retire_only_unwanted_history() {
    let db = in_memory_pool().await;
    let sid = seed_series(&db, 17074, "Monogatari Second Season").await;
    let title = "Monogatari Second Season 01-26";
    let broad: Vec<_> = (1..=26).collect();
    let gid = grabbed_torrents::record_grab(&db, "selected-history", title, sid, &broad, true)
        .await
        .unwrap()
        .unwrap();
    for ep in &broad {
        episode_tags::record_grab(
            &db,
            sid,
            *ep,
            &ClassificationResult::unknown(),
            title,
            "Group",
            1,
            true,
        )
        .await
        .unwrap();
    }
    let names: Vec<_> = (1..=26)
        .map(|ep| format!("Monogatari Second Season - {ep:02}.mkv"))
        .collect();
    let selected: Vec<_> = (0..26).filter(|i| ![5, 10, 15].contains(i)).collect();
    let masked = crate::services::grab_commit::filenames_for_selection(&names, Some(&selected));
    let detail = empty_anime_detail(17074, "Monogatari Second Season", Some(26));
    expand_from_files(
        &db,
        &masked,
        &detail,
        sid,
        &broad,
        gid,
        "Monogatari Second Season display title",
        &context(),
    )
    .await;
    let expected: Vec<_> = (1..=26).filter(|ep| ![6, 11, 16].contains(ep)).collect();
    assert_eq!(
        grabbed_torrents::get_by_id(&db, gid)
            .await
            .unwrap()
            .unwrap()
            .episode_numbers,
        expected
    );
    let tags = episode_tags::get_for_series(&db, sid).await.unwrap();
    for ep in [6, 11, 16] {
        assert!(!tags.contains_key(&ep));
        let state: String = sqlx::query_scalar(
            "SELECT state FROM episode_grab_history WHERE series_id=? AND episode_number=?",
        )
        .bind(sid)
        .bind(ep)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(state, "removed");
    }
}

#[tokio::test]
async fn masked_sibling_routes_bind_original_client_indices() {
    let db = in_memory_pool().await;
    let parent = seed_series(&db, 999501, "Franchise: First Story").await;
    let sibling = seed_series(&db, 999502, "Franchise: Second Story").await;
    let mut detail = empty_anime_detail(999501, "Franchise: First Story", Some(2));
    let mut relation = related_entry(999502, "Franchise: Second Story", Some(2));
    relation.relation_type = "SUMMARY".into(); // No provider walk; existing sibling needs no hydration.
    detail.relations.push(relation);
    let title = "Franchise complete";
    let gid = grabbed_torrents::record_grab(&db, "selected-routes", title, parent, &[1, 2], true)
        .await
        .unwrap()
        .unwrap();
    let names: Vec<String> = [
        "Franchise First Story - 01.mkv",
        "Franchise Second Story - 01.mkv",
        "NCOP.mkv",
        "Franchise First Story - 02.mkv",
        "Franchise Second Story - 02.mkv",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let masked = crate::services::grab_commit::filenames_for_selection(&names, Some(&[0, 3, 4]));
    expand_from_files(
        &db,
        &masked,
        &detail,
        parent,
        &[1, 2],
        gid,
        title,
        &context(),
    )
    .await;
    let routes = grabbed_torrents::get_series_routes(&db, gid).await.unwrap();
    assert_eq!(
        routes
            .iter()
            .find(|r| r.series_id == parent)
            .unwrap()
            .file_indices,
        [0, 3]
    );
    let child = routes.iter().find(|r| r.series_id == sibling).unwrap();
    assert_eq!(child.file_indices, [4]);
    assert_eq!(child.episode_numbers, [2]);
    let snapshot = grabbed_torrents::get_numbering_snapshot(&db, gid)
        .await
        .unwrap()
        .unwrap();
    let bound = snapshot
        .bind_files(&[
            (0, names[0].as_str(), true),
            (1, names[1].as_str(), false),
            (2, names[2].as_str(), false),
            (3, names[3].as_str(), true),
            (4, names[4].as_str(), true),
        ])
        .unwrap();
    assert_eq!(bound.len(), 3);
    assert_eq!(bound[&4].series_id, sibling);
    assert_eq!(bound[&4].mapped_episode().unwrap().unwrap().first, 2);
}

#[tokio::test]
async fn special_only_tv_and_ova_or_movie_main_coverage_stay_distinct() {
    for (provider, format, name, offset, expected) in [
        (999601, "TV", "Show - OVA 01.mkv", 0, vec![]),
        (999602, "OVA", "Show - OVA 01.mkv", 0, vec![1]),
        (999603, "MOVIE", "Show - 02.mkv", 1, vec![1]),
        (999604, "TV", "Show - NCOP1.mkv", 0, vec![]),
        (999605, "TV", "Show - NCED1.mkv", 0, vec![]),
        (999606, "TV", "Show - OVA.mkv", 0, vec![]),
        (999607, "OVA", "Show - NCOP1.mkv", 0, vec![]),
        (999608, "TV", "unknown.mkv", 0, vec![1]),
    ] {
        let db = in_memory_pool().await;
        let sid = seed_series(&db, provider, "Show").await;
        sqlx::query("UPDATE series SET format=?,cumulative_prior_episodes=? WHERE id=?")
            .bind(format)
            .bind(offset)
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        let gid =
            grabbed_torrents::record_grab(&db, "selected-main-kind", "Show", sid, &[1], false)
                .await
                .unwrap()
                .unwrap();
        let mut detail = empty_anime_detail(provider, "Show", Some(1));
        detail.format = format.into();
        expand_from_files(
            &db,
            &[name.into()],
            &detail,
            sid,
            &[1],
            gid,
            "Show",
            &context(),
        )
        .await;
        let grab = grabbed_torrents::get_by_id(&db, gid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grab.episode_numbers, expected, "{format}: {name}");
        assert!(
            !grab.is_batch,
            "one selected main file must not become a batch"
        );
        let tags = episode_tags::get_for_series(&db, sid).await.unwrap();
        assert!(!tags.contains_key(&2));
    }
}

#[tokio::test]
async fn parent_coverage_uses_import_claimed_numbering_and_keeps_explicit_mapping() {
    for (provider, count, offset, source_episode, claim, explicit) in [
        (999801, 13, 1, 12, vec![12], false),
        (999802, 1, 11, 12, vec![1], false),
        (999803, 13, 1, 1, (1..=13).collect(), true),
    ] {
        let db = in_memory_pool().await;
        let sid = seed_series(&db, provider, "Show").await;
        sqlx::query(
            "UPDATE series SET format='TV',episodes=?,cumulative_prior_episodes=? WHERE id=?",
        )
        .bind(count)
        .bind(offset)
        .bind(sid)
        .execute(&db)
        .await
        .unwrap();
        let mapping: Option<crate::services::release_numbering::ReleaseMapping> =
            explicit.then(|| {
                serde_json::from_value(serde_json::json!({
                    "version":1,"anilist_id":provider,"tv_series":true,
                    "source_first":1,"source_last":13,"target_first":1,
                    "source_season":1,"basis":"season_relative"
                }))
                .unwrap()
            });
        let gid = grabbed_torrents::record_grab_with_release_mapping(
            &db,
            "numbered-parent",
            "Show",
            sid,
            &claim,
            true,
            mapping.as_ref(),
        )
        .await
        .unwrap()
        .unwrap();
        let names = if explicit {
            (1..=13).map(|ep| format!("Show - {ep:02}.mkv")).collect()
        } else {
            vec![format!("Show - {source_episode:02}.mkv")]
        };
        let detail = empty_anime_detail(provider, "Show", Some(count));
        expand_from_files(&db, &names, &detail, sid, &claim, gid, "Show", &context()).await;
        assert_eq!(
            grabbed_torrents::get_by_id(&db, gid)
                .await
                .unwrap()
                .unwrap()
                .episode_numbers,
            claim
        );
        if let Some(expected) = mapping {
            let saved = grabbed_torrents::get_release_mapping(&db, gid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved, expected);
            for ep in 1..=13 {
                let span =
                    media::parse_episode_span(&format!("Show - {ep:02}.mkv").to_ascii_lowercase())
                        .unwrap();
                assert_eq!(saved.resolve_file(span).unwrap().first, ep);
            }
        }
    }
}

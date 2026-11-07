use super::*;
use crate::models::grabbed_torrents::{self, record_grab};
use crate::test_support::{in_memory_pool, seed_series};

async fn fixture() -> (SqlitePool, i64, i64, Vec<GrabSeriesRoute>, Vec<String>) {
    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 9090, "Mapping Test").await;
    let grab_id = record_grab(&db, "saved-map", "Mapping Test", series_id, &[1], true)
        .await
        .unwrap()
        .unwrap();
    let routes = vec![GrabSeriesRoute {
        grab_id,
        series_id,
        file_indices: vec![0, 1],
        episode_numbers: vec![900], // the requested slot is not mapping evidence
        matched_subtitle: "selected test route".into(),
        episode_offset: -1,
    }];
    let names = vec![
        "Mapping Test - 00.mkv".into(),
        "Mapping Test - 01.mkv".into(),
    ];
    (db, series_id, grab_id, routes, names)
}

#[tokio::test]
async fn saved_choice_uses_paths_and_canonical_numbers_not_request_or_file_order() {
    let (db, series_id, grab_id, routes, names) = fixture().await;
    record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    let saved = get_numbering_snapshot(&db, grab_id).await.unwrap().unwrap();
    let bound = saved
        .bind_files(&[(0, &names[1], true), (1, &names[0], true)])
        .unwrap();
    assert_eq!(bound[&0].mapped_episode().unwrap().unwrap().first, 2);
    assert_eq!(bound[&1].mapped_episode().unwrap().unwrap().first, 1);
    assert_eq!(bound[&1].anilist_id, 9090);
    assert_eq!(bound[&1].series_id, series_id);
    let rows = grabbed_torrents::get_series_routes(&db, grab_id)
        .await
        .unwrap();
    assert_eq!(rows[0].episode_numbers, vec![1, 2]);
    assert_eq!(
        grabbed_torrents::get_by_id(&db, grab_id)
            .await
            .unwrap()
            .unwrap()
            .episode_numbers,
        vec![1, 2]
    );
    assert!(
        saved
            .bind_files(&[(0, "Mapping Test - 02.mkv", true)])
            .is_err()
    );
    assert!(
        saved
            .bind_files(&[(0, &names[0], true), (1, &names[0], true)])
            .is_err()
    );
    assert!(saved.bind_files(&[(4, &names[1], true)]).is_err());
    assert_eq!(
        saved
            .bind_files(&[(4, &names[1], true), (5, &names[0], false)])
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn unsupported_container_does_not_block_supported_files_in_the_same_pack() {
    let (db, _, grab_id, mut routes, mut names) = fixture().await;
    names.push("Mapping Test - 02.m2ts".into());
    routes[0].file_indices.push(2);
    record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    let saved = get_numbering_snapshot(&db, grab_id).await.unwrap().unwrap();
    assert_eq!(saved.files.len(), 2);
    assert_eq!(
        grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .unwrap()[0]
            .episode_numbers,
        vec![1, 2]
    );
}

#[tokio::test]
async fn conflicting_decisions_do_not_replace_a_saved_choice() {
    let (db, _, grab_id, mut routes, names) = fixture().await;
    record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    let original = get_numbering_snapshot(&db, grab_id).await.unwrap();
    routes[0].episode_offset = -2;
    let error = record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("different saved mapping"),
        "{error}"
    );
    assert!(
        grabbed_torrents::record_grab_series_routes(&db, &routes)
            .await
            .is_err()
    );
    assert_eq!(
        get_numbering_snapshot(&db, grab_id).await.unwrap(),
        original
    );
    assert_eq!(
        grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .unwrap()[0]
            .episode_offset,
        -1
    );
}

#[tokio::test]
async fn invalid_route_rolls_back_the_whole_write() {
    let (db, _, grab_id, mut routes, names) = fixture().await;
    let mut invalid = routes[0].clone();
    invalid.series_id = i64::MAX;
    routes.push(invalid);
    assert!(
        record_grab_series_routes_with_files(&db, &routes, &names)
            .await
            .is_err()
    );
    assert!(
        grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        get_numbering_snapshot(&db, grab_id)
            .await
            .unwrap()
            .is_none()
    );
    // The legacy writer must also be atomic; a valid first row cannot survive
    // a failing second row and later make a partial route set look complete.
    assert!(
        grabbed_torrents::record_grab_series_routes(&db, &routes)
            .await
            .is_err()
    );
    assert!(
        grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn missing_file_route_cannot_be_saved_as_a_complete_choice() {
    let (db, _, grab_id, mut routes, names) = fixture().await;
    routes[0].file_indices = vec![0];
    assert!(
        record_grab_series_routes_with_files(&db, &routes, &names)
            .await
            .is_err()
    );
    assert!(
        get_numbering_snapshot(&db, grab_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn retry_preserves_choice_but_new_owner_discards_stale_routes_atomically() {
    let (db, series_id, grab_id, routes, names) = fixture().await;
    record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    let original = get_numbering_snapshot(&db, grab_id).await.unwrap();
    assert_eq!(
        record_grab(&db, "saved-map", "ignored", series_id, &[77], true)
            .await
            .unwrap(),
        None
    );
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE id = ?")
        .bind(grab_id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        record_grab(&db, "saved-map", "retry", series_id, &[1], true)
            .await
            .unwrap(),
        Some(grab_id)
    );
    assert_eq!(
        get_numbering_snapshot(&db, grab_id).await.unwrap(),
        original
    );
    let other = seed_series(&db, 9091, "Other Entry").await;
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE id = ?")
        .bind(grab_id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        record_grab(&db, "saved-map", "new owner", other, &[1], true)
            .await
            .unwrap(),
        Some(grab_id)
    );
    assert!(
        get_numbering_snapshot(&db, grab_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        grabbed_torrents::get_by_id(&db, grab_id)
            .await
            .unwrap()
            .unwrap()
            .series_id,
        other
    );
}

#[tokio::test]
async fn malformed_saved_data_never_becomes_an_empty_legacy_route() {
    let (db, _, grab_id, routes, names) = fixture().await;
    record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    for bad in [
        "{",
        r#"{"version":2,"basis":"selected_pack_routes","files":[]}"#,
    ] {
        sqlx::query("UPDATE grabbed_torrent_numbering SET snapshot = ? WHERE grab_id = ?")
            .bind(bad)
            .bind(grab_id)
            .execute(&db)
            .await
            .unwrap();
        assert!(get_numbering_snapshot(&db, grab_id).await.is_err());
    }
    for bad in ["{", "[-1]", "[1.5]", "null"] {
        sqlx::query("UPDATE grabbed_torrent_series SET file_indices = ? WHERE grab_id = ?")
            .bind(bad)
            .bind(grab_id)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            grabbed_torrents::get_series_routes(&db, grab_id)
                .await
                .is_err()
        );
        assert!(
            grabbed_torrents::get_series_routes_for_grabs(&db, &[grab_id])
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn migrations_preserve_snapshots_and_grab_deletion_cascades() {
    let (db, _, grab_id, routes, names) = fixture().await;
    record_grab_series_routes_with_files(&db, &routes, &names)
        .await
        .unwrap();
    let original = get_numbering_snapshot(&db, grab_id).await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    assert_eq!(
        get_numbering_snapshot(&db, grab_id).await.unwrap(),
        original
    );
    sqlx::query("DELETE FROM grabbed_torrents WHERE id = ?")
        .bind(grab_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        get_numbering_snapshot(&db, grab_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn release_decision_is_atomic_and_survives_a_legacy_regrab() {
    let (db, series_id, _, _, _) = fixture().await;
    let context = crate::services::release_numbering::NumberingContext {
        anilist_id: 9090,
        tv_series: true,
        episode_count: Some(12),
        cumulative_offset: 1,
        expected_season: 2,
        provider_plan: None,
    };
    let mapping = context.choose("Mapping Test S02E06", false).unwrap();
    let id = grabbed_torrents::record_grab_with_release_mapping(
        &db,
        "release-choice",
        "Mapping Test S02E06",
        series_id,
        &[99],
        false,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        grabbed_torrents::get_by_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .episode_numbers,
        vec![6]
    );
    assert_eq!(
        grabbed_torrents::get_release_mapping(&db, id)
            .await
            .unwrap(),
        Some(mapping.clone())
    );
    let conflicting = context.choose("Mapping Test S02E07", false).unwrap();
    assert!(
        grabbed_torrents::record_grab_with_release_mapping(
            &db,
            "release-choice",
            "conflict while pending",
            series_id,
            &[7],
            false,
            Some(&conflicting),
        )
        .await
        .is_err()
    );
    crate::models::migrate(&db).await.unwrap();
    assert_eq!(
        grabbed_torrents::get_release_mapping(&db, id)
            .await
            .unwrap(),
        Some(mapping.clone())
    );
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE id = ?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE series SET cumulative_prior_episodes = 999 WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        grabbed_torrents::record_grab_with_release_mapping(
            &db,
            "release-choice",
            "conflict",
            series_id,
            &[7],
            false,
            Some(&conflicting),
        )
        .await
        .is_err()
    );
    record_grab(&db, "release-choice", "retry", series_id, &[99], false)
        .await
        .unwrap();
    assert_eq!(
        grabbed_torrents::get_by_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .episode_numbers,
        vec![6]
    );
    assert_eq!(
        grabbed_torrents::get_release_mapping(&db, id)
            .await
            .unwrap(),
        Some(mapping)
    );
}

#[tokio::test]
async fn release_decision_cannot_be_attached_to_another_entry() {
    let (db, series_id, _, _, _) = fixture().await;
    let mapping = crate::services::release_numbering::NumberingContext {
        anilist_id: 9091,
        tv_series: true,
        episode_count: Some(12),
        cumulative_offset: 0,
        expected_season: 1,
        provider_plan: None,
    }
    .choose("Other - 06", false)
    .unwrap();
    assert!(
        grabbed_torrents::record_grab_with_release_mapping(
            &db,
            "wrong-owner",
            "Other - 06",
            series_id,
            &[6],
            false,
            Some(&mapping),
        )
        .await
        .is_err()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = 'wrong-owner'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn concurrent_choices_cannot_overwrite_each_other() {
    let (db, _, grab_id, first, names) = fixture().await;
    let mut second = first.clone();
    second[0].episode_offset = -2;
    let (a, b) = tokio::join!(
        record_grab_series_routes_with_files(&db, &first, &names),
        record_grab_series_routes_with_files(&db, &second, &names)
    );
    assert_ne!(
        a.is_ok(),
        b.is_ok(),
        "exactly one choice must win: {a:?}, {b:?}"
    );
    let winner = if a.is_ok() { -1 } else { -2 };
    let saved = get_numbering_snapshot(&db, grab_id).await.unwrap().unwrap();
    assert!(saved.files.iter().all(|f| f.episode_offset == winner));
    assert_eq!(
        grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .unwrap()[0]
            .episode_offset,
        winner
    );
}

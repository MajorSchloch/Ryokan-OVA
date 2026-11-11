use super::*;

#[tokio::test]
async fn selected_pack_import_preserves_exact_history_coverage_without_recaps() {
    use crate::models::{episode_tags, metadata_cache};
    use crate::services::anilist::AnimeDetail;

    let _guard = POST_PROC_TEST_SERIALIZER.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let downloads = temp.path().join("downloads");
    let media = temp.path().join("media");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&media).unwrap();
    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let sid = seed_series(&db, 17074, "Monogatari Second Season").await;
    sqlx::query("UPDATE series SET format='TV',episodes=26 WHERE id=?")
        .bind(sid)
        .execute(&db)
        .await
        .unwrap();
    let detail = AnimeDetail {
        release_date: None,
        is_adult: false,
        id: 17074,
        id_mal: None,
        title_romaji: "Monogatari Second Season".into(),
        title_english: "Monogatari Second Season".into(),
        title_native: String::new(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(26),
        duration: Some(24),
        season: String::new(),
        season_year: Some(2013),
        end_year: Some(2013),
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    };
    metadata_cache::upsert(&db, sid, 17074, None, &detail)
        .await
        .unwrap();
    let selected: Vec<i32> = (1..=26).filter(|n| ![6, 11, 16].contains(n)).collect();
    let gid = grabbed_torrents::record_grab(
        &db,
        "selected-23-of-26",
        "[a-S_subs] Monogatari Second Season 1-26 (BDRip 1080p)",
        sid,
        &selected,
        true,
    )
    .await
    .unwrap()
    .unwrap();
    let files: Vec<_> = (1..=26)
        .map(|ep| {
            let name = format!("[a-s_subs]_monogatari_second_season_-_{ep:02}_-_arc_part_1.mkv");
            let mut file = complete_file(&name);
            file.wanted = ![6, 11, 16].contains(&ep);
            if file.wanted {
                std::fs::write(downloads.join(&name), format!("episode {ep}")).unwrap();
            } else {
                file.progress = 0.0;
            }
            file
        })
        .collect();
    let client = Arc::new(BatchClient::new(
        complete_torrent("selected-23-of-26", &downloads),
        files,
    ));
    let state = build_test_app_state(db.clone(), Some(client));
    post_processing::run_once(&state).await;
    assert_eq!(grab_state(&db, gid).await, "imported");
    let saved = grabbed_torrents::get_by_id(&db, gid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.episode_numbers, selected,
        "selected files own the batch history too"
    );
    assert_eq!(collect_files(&media, "mkv").len(), 23);
    let tags = episode_tags::get_for_series(&db, sid).await.unwrap();
    for ep in [6, 11, 16] {
        assert!(!tags.contains_key(&ep), "unwanted recap {ep} has no tag");
        let history: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM episode_grab_history WHERE series_id=? AND episode_number=?",
        )
        .bind(sid)
        .bind(ep)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(history, 0);
    }
    assert!(
        grabbed_torrents::get_series_routes(&db, gid)
            .await
            .unwrap()
            .is_empty()
    );
}

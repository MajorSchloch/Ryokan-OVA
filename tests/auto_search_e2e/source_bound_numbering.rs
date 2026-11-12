//! Reviewed source roots persist through real search dispatch and import.
use super::*;
use ryokan::models::{episode_tags, grabbed_torrents, source_numbering as decisions};
use ryokan::services::{download_client::DownloadItemState, source_numbering};

async fn cached(state: &AppState, id: i64, title: &str) -> i64 {
    let sid = seed_series_with_cache(state, id, title).await;
    sqlx::query("UPDATE series SET format='TV',status='FINISHED',episodes=12,monitor_mode='all',cumulative_offset_known=1,allow_upgrades=0 WHERE id=?")
        .bind(sid).execute(&state.db).await.unwrap();
    for episode in 1..=12 {
        ryokan::models::monitoring::set_episode_monitored(&state.db, sid, episode, true)
            .await
            .unwrap();
    }
    sid
}

async fn setup(state: &AppState, temp: &tempfile::TempDir) {
    let mut cfg = default_config();
    cfg.media_root = temp.path().join("library").to_string_lossy().into_owned();
    cfg.post_processing_enabled = true;
    cfg.post_processing_mode = "copy".into();
    cfg.required_subtitle_languages.clear();
    cfg.required_audio_languages.clear();
    cfg.media_requirements_enabled = false;
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
}

fn staged(
    hash: &str,
    title: &str,
    path: &std::path::Path,
    names: &[String],
) -> (DownloadItem, Vec<DownloadFile>) {
    (
        DownloadItem {
            hash: hash.into(),
            name: title.into(),
            size: 100,
            progress: 0.0,
            dlspeed: 0,
            state: "downloading".into(),
            category: "anime".into(),
            eta: 0,
            save_path: path.to_string_lossy().into_owned(),
            content_path: path.to_string_lossy().into_owned(),
            state_kind: DownloadItemState::Downloading,
            seeding_done: false,
        },
        names
            .iter()
            .map(|name| DownloadFile {
                name: name.clone(),
                size: 100,
                progress: 0.0,
                wanted: true,
            })
            .collect(),
    )
}

async fn finish(state: &AppState, client: &AutoSearchRecordingClient, temp: &tempfile::TempDir) {
    {
        let mut jobs = client.completed_for_import.lock().unwrap();
        for (item, files) in jobs.iter_mut() {
            item.progress = 1.0;
            item.state_kind = DownloadItemState::Seeding;
            item.state = "seeding".into();
            for file in files.iter_mut().filter(|file| file.wanted) {
                let path = std::path::Path::new(&item.save_path).join(&file.name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, format!("exact source {}", file.name)).unwrap();
                file.progress = 1.0;
            }
        }
    }
    let restarted = build_test_app_state(state.db.clone(), None);
    *restarted.download_clients.write().await = state.download_clients.read().await.clone();
    ryokan::services::post_processing::run_once(&restarted).await;
    assert!(
        !structural_season_numbering::library_videos(&temp.path().join("library")).is_empty(),
        "{}",
        dump_logs(&state.db).await
    );
}

#[tokio::test]
async fn source_bound_numbering_dr_stone_root_twelve_is_not_graph_offset_twelve() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let hash = "e4d62dcf1963a4a7022840c8e935c54fbd0cbcc3";
    let title =
        "[Erai-raws] Dr. Stone: Science Future - 12 [1080p CR WEB-DL AVC AAC][MultiSub][479E6A7A]";
    let name = "[Erai-raws] Dr Stone - Science Future - 12 [1080p CR WEB-DL AVC AAC][MultiSub][479E6A7A].mkv".to_string();
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                hash, 1952833, title, "1 GiB", 5,
            ))),
        )
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    setup(&state, &temp).await;
    let sid = cached(&state, 172019, "Dr. Stone: Science Future").await;
    cached(&state, 189117, "Dr. Stone: Science Future Part 2").await;
    let third = cached(&state, 199221, "Dr. Stone: Science Future Part 3").await;
    let mut third_detail = detail_for(199221, "Dr. Stone: Science Future Part 3");
    third_detail.episodes = Some(13);
    ryokan::models::metadata_cache::upsert(&state.db, third, 199221, None, &third_detail)
        .await
        .unwrap();
    sqlx::query("UPDATE series SET cumulative_prior_episodes=22 WHERE id=?")
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    let preview = source_numbering::prepare_review(
        source_numbering::load_domain(&state.db, 172019)
            .await
            .unwrap(),
        hash,
        std::slice::from_ref(&name),
        172019,
    )
    .unwrap();
    decisions::save_reviewed(
        &state.db,
        &preview.confirm(std::slice::from_ref(&name)).unwrap(),
    )
    .await
    .unwrap();
    ryokan::models::nyaa_description_cache::upsert(&state.db, hash, "Source: CR WEB-DL").await;
    let client = install_recording_default_torrent_client(&state).await;
    client
        .supports_selection
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *client.completed_for_import.lock().unwrap() = vec![staged(
        hash,
        title,
        &temp.path().join("downloads"),
        std::slice::from_ref(&name),
    )];
    let report = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, 12)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(report.grabbed.len(), 1, "{}", dump_logs(&state.db).await);
    let grab = grabbed_torrents::get_all_pending(&state.db)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(grab.episode_numbers, [12]);
    assert!(
        grabbed_torrents::get_release_mapping(&state.db, grab.id)
            .await
            .unwrap()
            .unwrap()
            .bound_source()
            .is_some()
    );
    finish(&state, &client, &temp).await;
    let videos = structural_season_numbering::library_videos(&temp.path().join("library"));
    assert_eq!(videos.len(), 1);
    assert!(
        videos[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("S01E12")
    );
    assert_eq!(
        episode_tags::get_for_series(&state.db, sid).await.unwrap()[&12].state,
        "completed"
    );
    let offset: i32 = sqlx::query_scalar("SELECT cumulative_prior_episodes FROM series WHERE id=?")
        .bind(sid)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(offset, 22);
    assert_eq!(client.add_calls().len(), 1);
    unset_nyaa_base();
}

#[tokio::test]
async fn source_bound_numbering_lagoon_pack_selects_only_second_barrage_and_imports_after_restart()
{
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let hash = "91a564c6d0f605344b67cef81eeab3199d61872b";
    let title = "[Legion] Black Lagoon (01-29) [BD 1080p Dual Audio]";
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                hash, 1428087, title, "83.8 GiB", 3,
            ))),
        )
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    setup(&state, &temp).await;
    cached(&state, 889, "Black Lagoon").await;
    let sid = cached(&state, 1519, "Black Lagoon: The Second Barrage").await;
    sqlx::query("UPDATE series SET cumulative_prior_episodes=12 WHERE id=?")
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    let names: Vec<_> = (1..=29)
        .map(|episode| {
            format!(
                "[Legion] Black Lagoon - {episode:02} [BD x264 1080p 10bit FLAC][Dual Audio].mkv"
            )
        })
        .collect();
    let preview = source_numbering::prepare_review(
        source_numbering::load_domain(&state.db, 889).await.unwrap(),
        hash,
        &names,
        1519,
    )
    .unwrap();
    let proof = preview.confirm(&names[12..24]).unwrap();
    decisions::save_reviewed(&state.db, &proof).await.unwrap();
    ryokan::models::nyaa_description_cache::upsert(&state.db, hash, "Source: BD").await;
    let client = install_recording_default_torrent_client(&state).await;
    client
        .supports_selection
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut reordered = names.clone();
    reordered.reverse();
    *client.completed_for_import.lock().unwrap() = vec![staged(
        hash,
        title,
        &temp.path().join("downloads"),
        &reordered,
    )];
    let report = auto_search_series(
        axum::extract::State(state.clone()),
        axum::extract::Path(sid),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(report.grabbed.len(), 1, "{}", dump_logs(&state.db).await);
    assert_eq!(client.add_calls().len(), 1);
    let pending = grabbed_torrents::get_all_pending(&state.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].episode_numbers, (1..=12).collect::<Vec<_>>());
    let selected: Vec<_> = client.completed_for_import.lock().unwrap()[0]
        .1
        .iter()
        .filter(|file| file.wanted)
        .map(|file| file.name.clone())
        .collect();
    assert_eq!(selected.len(), 12);
    assert!(selected.contains(&names[14]));
    assert!(!selected.contains(&names[24]));
    finish(&state, &client, &temp).await;
    let videos = structural_season_numbering::library_videos(&temp.path().join("library"));
    assert_eq!(videos.len(), 12, "{}", dump_logs(&state.db).await);
    assert_eq!(
        episode_tags::get_for_series(&state.db, sid)
            .await
            .unwrap()
            .len(),
        12
    );
    let history: Vec<i32> =
        sqlx::query_scalar("SELECT episode_number FROM episode_grab_history WHERE series_id=?")
            .bind(sid)
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert!(history.iter().all(|episode| (1..=12).contains(episode)));
    let coverage: Vec<i32> = episode_tags::get_for_series(&state.db, sid)
        .await
        .unwrap()
        .into_keys()
        .collect();
    assert!(coverage.contains(&3));
    assert!(
        episode_tags::get_for_series(&state.db, 1)
            .await
            .unwrap()
            .is_empty()
    );
    unset_nyaa_base();
}

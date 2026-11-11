//! Actual selection and rejected-content retry through private HTTP fixtures.

use super::*;

#[tokio::test]
async fn series_search_with_unknown_tv_count_grabs_each_missing_monitored_episode() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let first_hash = "3333333333333333333333333333333333333333";
    let second_hash = "4444444444444444444444444444444444444444";
    let title = "Devils' Crest";
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&format!(
                "{}{}",
                nyaa_row(
                    first_hash,
                    99401,
                "[ToonsHub] Devils Crest S01E01 1080p AMZN WEB-DL DUAL DDP2.0 H.264 (Demons' Crest, Dual-Audio, Multi-Subs)",
                    "500 MiB",
                    50
                ),
                nyaa_row(
                    second_hash,
                    99402,
                "[Erai-raws] Demons Crest - 02 [1080p AMZN WEBRip HEVC EAC3][MultiSub][292FEBD8]",
                    "500 MiB",
                    100
                ),
            ))),
        )
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    for present in [vec![], vec![2], vec![1, 2]] {
        let temp = tempfile::tempdir().unwrap();
        let state = build_state().await;
        let mut cfg = default_config();
        cfg.media_root = temp.path().to_string_lossy().to_string();
        ryokan::models::config::save_config(&state.db, &cfg)
            .await
            .unwrap();
        let provider_id = 9_900_991;
        let sid = ryokan::test_support::seed_series(&state.db, provider_id, title).await;
        sqlx::query("UPDATE series SET format='TV',status='RELEASING',episodes=NULL,monitor_mode='all',allow_upgrades=0 WHERE id=?")
            .bind(sid).execute(&state.db).await.unwrap();
        let mut detail = detail_for(provider_id, title);
        detail.title_romaji = "Demons' Crest".into();
        detail.format = "TV".into();
        detail.status = "RELEASING".into();
        detail.episodes = None;
        ryokan::models::metadata_cache::upsert(&state.db, sid, provider_id, None, &detail)
            .await
            .unwrap();
        // Both sources must stay unknown: a cached twelve-episode detail
        // would hydrate the series and conceal the original Single-target bug.
        let stored_count: Option<i64> =
            sqlx::query_scalar("SELECT episodes FROM series WHERE id=?")
                .bind(sid)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(stored_count, None);
        assert_eq!(
            ryokan::models::metadata_cache::get_by_series_id(&state.db, sid)
                .await
                .unwrap()
                .unwrap()
                .detail
                .episodes,
            None
        );
        for episode in [1, 2] {
            ryokan::models::monitoring::set_episode_monitored(&state.db, sid, episode, true)
                .await
                .unwrap();
        }
        let season = temp.path().join(title).join("Season 01");
        std::fs::create_dir_all(&season).unwrap();
        for episode in &present {
            std::fs::write(
                season.join(format!("{title} - S01E{episode:02} [1080p WEB].mkv")),
                b"existing episode",
            )
            .unwrap();
        }
        let client = install_recording_default_torrent_client(&state).await;
        let report = auto_search_series(
            axum::extract::State(state.clone()),
            axum::extract::Path(sid),
            axum::extract::Query(AutoSearchQuery::default()),
        )
        .await
        .unwrap()
        .0;
        let expected: Vec<i32> = [1, 2]
            .into_iter()
            .filter(|ep| !present.contains(ep))
            .collect();
        assert_eq!(
            report.grabbed.len(),
            expected.len(),
            "present={present:?}; report={report:?}\n{}",
            dump_logs(&state.db).await
        );
        let pending = ryokan::models::grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap();
        let mut episodes: Vec<i32> = pending
            .iter()
            .flat_map(|grab| grab.episode_numbers.iter().copied())
            .collect();
        episodes.sort_unstable();
        assert_eq!(
            episodes, expected,
            "each missing monitored episode needs its own numbered search"
        );
        let calls = client.add_calls();
        assert_eq!(calls.len(), expected.len());
        for (episode, hash) in [(1, first_hash), (2, second_hash)] {
            assert_eq!(
                calls.iter().any(|(_, grabbed_hash)| grabbed_hash == hash),
                expected.contains(&episode)
            );
        }
        assert!(
            pending
                .iter()
                .all(|grab| !grab.is_batch && grab.episode_numbers.len() == 1)
        );
        assert_eq!(
            ryokan::models::metadata_cache::get_by_series_id(&state.db, sid)
                .await
                .unwrap()
                .unwrap()
                .detail
                .episodes,
            None
        );
        let remaining_count: Option<i64> =
            sqlx::query_scalar("SELECT episodes FROM series WHERE id=?")
                .bind(sid)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(remaining_count, None);
    }
    unset_nyaa_base();
}

async fn requirement_retry_fixture(
    on_disk: bool,
) -> (
    tempfile::TempDir,
    AppState,
    std::sync::Arc<AutoSearchRecordingClient>,
    ryokan::models::grabbed_torrents::GrabbedTorrent,
) {
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    let mut cfg = default_config();
    cfg.media_root = temp.path().to_string_lossy().to_string();
    cfg.media_requirements_enabled = true;
    cfg.minimum_resolution = "720".into();
    cfg.target_resolution = "1080".into();
    cfg.required_subtitle_languages = "en".into();
    cfg.minimum_subtitle_check = true;
    cfg.target_subtitle_check = true;
    cfg.auto_redownload_failed = true;
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let sid = seed_series_with_cache(&state, 99881, "Research Show").await;
    sqlx::query("UPDATE series SET monitor_mode='future',allow_upgrades=1 WHERE id=?")
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    std::fs::create_dir_all(temp.path().join("Research Show")).unwrap();
    if on_disk {
        let old = temp
            .path()
            .join("Research Show/Season 01/Research Show - S01E03 [720p WEB].mkv");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"original video").unwrap();
        let identity = ryokan::services::media_info::FileIdentity::from_metadata(
            &std::fs::metadata(&old).unwrap(),
        )
        .unwrap();
        ryokan::models::media_probe_cache::upsert_identified(
            &state.db,
            std::fs::canonicalize(&old).unwrap().to_str().unwrap(),
            &identity.cache_key(),
            identity.size as i64,
            &serde_json::json!({"streams":[{
                "index":0,"codec_type":"video","codec_name":"h264","width":1280,"height":720
            }]})
            .to_string(),
        )
        .await;
    }
    let id = ryokan::models::grabbed_torrents::record_grab(
        &state.db,
        WRONG_HASH,
        "[Wrong] Research Show - 03 (1080p) [WEB].mkv",
        sid,
        &[3],
        false,
    )
    .await
    .unwrap()
    .unwrap();
    ryokan::models::grabbed_torrents::mark_failed_with_reason(
        &state.db,
        id,
        ryokan::services::post_processing::REQUIREMENTS_REJECTED_REASON,
    )
    .await
    .unwrap();
    let grab = ryokan::models::grabbed_torrents::get_by_id(&state.db, id)
        .await
        .unwrap()
        .unwrap();
    let client = install_recording_default_torrent_client(&state).await;
    (temp, state, client, grab)
}

#[tokio::test]
async fn automatic_upgrade_selects_target_when_release_omits_the_titles_apostrophe() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let target_hash = "1111111111111111111111111111111111111111";
    let lower_hash = "2222222222222222222222222222222222222222";
    let target = "[ToonsHub] My Girlfriends Friend S01E05 REPACK 1080p UNCENSORED ADN WEB-DL AAC2.0 H.264 (Kanojo no Tomodachi, Multi-Subs)";
    let lower =
        "[Doomdos] - Kanojo no Tomodachi - S01E05 [720p OV WEB-DL] [My Girlfriend's Friend]";
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&format!(
                "{}{}",
                nyaa_row(lower_hash, 99305, lower, "800 MiB", 500),
                nyaa_row(target_hash, 99306, target, "500 MiB", 5),
            ))),
        )
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let (temp, state, client, grab) = requirement_retry_fixture(true).await;
    let mut cfg = ryokan::models::config::get_config(&state.db)
        .await
        .unwrap()
        .unwrap();
    cfg.preferred_groups = "Doomdos".into();
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let mut detail = detail_for(99881, "Kanojo no Tomodachi");
    detail.title_english = "My Girlfriend’s Friend".into();
    detail.format = "TV_SHORT".into();
    ryokan::models::metadata_cache::upsert(&state.db, grab.series_id, 99881, None, &detail)
        .await
        .unwrap();
    sqlx::query("UPDATE series SET title=?,title_english=?,title_romaji=? WHERE id=?")
        .bind(&detail.title_english)
        .bind(&detail.title_english)
        .bind(&detail.title_romaji)
        .bind(grab.series_id)
        .execute(&state.db)
        .await
        .unwrap();
    let old = temp
        .path()
        .join("Research Show/Season 01/Show - S01E05 [720p WEB].mkv");
    std::fs::write(&old, b"existing720p").unwrap();
    let identity = ryokan::services::media_info::FileIdentity::from_metadata(
        &std::fs::metadata(&old).unwrap(),
    )
    .unwrap();
    ryokan::models::media_probe_cache::upsert_identified(&state.db,
        std::fs::canonicalize(&old).unwrap().to_str().unwrap(),&identity.cache_key(),identity.size as i64,
        &serde_json::json!({"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1280,"height":720}]}).to_string(),
    ).await;
    let report = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((grab.series_id, 5)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        report.grabbed.len(),
        1,
        "report={report:?}\n{}",
        dump_logs(&state.db).await
    );
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].1, target_hash,
        "target resolution must win after strict title matching"
    );
    let selected = ryokan::models::grabbed_torrents::get_all_pending(&state.db)
        .await
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].episode_numbers, vec![5]);
    assert!(
        ryokan::models::grabbed_torrents::get_replacement_episode(&state.db, selected[0].id)
            .await
            .unwrap()
            .is_none(),
        "this must be automatic, not an explicit replacement override"
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn requirements_rejection_researches_next_candidate_for_existing_future_or_missing_monitored()
{
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    for on_disk in [true, false] {
        let (_temp, state, client, grab) = requirement_retry_fixture(on_disk).await;
        if !on_disk {
            ryokan::models::monitoring::set_episode_monitored(&state.db, grab.series_id, 3, true)
                .await
                .unwrap();
        }
        assert!(ryokan::services::redownload::after_requirements_rejected(&state, &grab).await);
        let calls = wait_for_add_calls(&client, 1).await;
        assert_eq!(calls.len(), 1, "{}", dump_logs(&state.db).await);
        assert_eq!(
            calls[0].1, RIGHT_HASH,
            "the rejected source remains blocklisted"
        );
        // The detached retry's client add is acknowledged before the
        // subsequent log/progress writes and grab-row transaction. Wait for
        // the state this assertion actually needs, not just client receipt.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pending = loop {
            let pending = ryokan::models::grabbed_torrents::get_all_pending(&state.db)
                .await
                .unwrap();
            if pending.iter().any(|row| row.hash == RIGHT_HASH)
                || std::time::Instant::now() >= deadline
            {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(pending.len(), 1, "{}", dump_logs(&state.db).await);
        assert_eq!(pending[0].hash, RIGHT_HASH);
        assert_eq!(pending[0].episode_numbers, vec![3]);
        if on_disk {
            assert_eq!(
                std::fs::read(
                    _temp
                        .path()
                        .join("Research Show/Season 01/Research Show - S01E03 [720p WEB].mkv")
                )
                .unwrap(),
                b"original video"
            );
        }
    }
    unset_nyaa_base();
}

async fn prepared_rejected_import() -> (
    tempfile::TempDir,
    AppState,
    std::sync::Arc<AutoSearchRecordingClient>,
    ryokan::models::grabbed_torrents::GrabbedTorrent,
    std::path::PathBuf,
) {
    let (temp, state, client, grab) = requirement_retry_fixture(true).await;
    sqlx::query("UPDATE config SET post_processing_enabled=1,post_processing_mode='copy'")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET state='pending',failure_reason='' WHERE id=?")
        .bind(grab.id)
        .execute(&state.db)
        .await
        .unwrap();
    let source_dir = temp.path().join("download");
    std::fs::create_dir(&source_dir).unwrap();
    let name = "[Wrong] Research Show - 03 (1080p) [WEB].mkv";
    let source = source_dir.join(name);
    std::fs::write(&source, b"completed source without subtitles").unwrap();
    let identity = ryokan::services::media_info::FileIdentity::from_metadata(
        &std::fs::metadata(&source).unwrap(),
    )
    .unwrap();
    ryokan::models::media_probe_cache::upsert_identified(
        &state.db,
        std::fs::canonicalize(&source).unwrap().to_str().unwrap(),
        &identity.cache_key(),
        identity.size as i64,
        &serde_json::json!({"streams":[{
            "index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080
        }]})
        .to_string(),
    )
    .await;
    let classification = ryokan::services::source::classify_release_sync(name, None);
    ryokan::models::episode_tags::record_grab(
        &state.db,
        grab.series_id,
        3,
        &classification,
        &grab.torrent_name,
        "",
        identity.size as i64,
        false,
    )
    .await
    .unwrap();
    let path = source_dir.to_string_lossy().to_string();
    *client.completed_for_import.lock().unwrap() = vec![(
        DownloadItem {
            hash: WRONG_HASH.into(),
            name: grab.torrent_name.clone(),
            size: identity.size as i64,
            progress: 1.0,
            dlspeed: 0,
            state: "seeding".into(),
            category: "anime".into(),
            eta: 0,
            save_path: path.clone(),
            content_path: path,
            state_kind: ryokan::services::download_client::DownloadItemState::Seeding,
            seeding_done: false,
        },
        vec![DownloadFile {
            name: name.into(),
            size: identity.size as i64,
            progress: 1.0,
            wanted: true,
        }],
    )];
    (temp, state, client, grab, source)
}

#[tokio::test]
async fn verified_subtitle_import_rejection_automatically_grabs_the_next_release() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let (temp, state, client, grab, source) = prepared_rejected_import().await;
    ryokan::services::post_processing::run_once(&state).await;
    let calls = wait_for_add_calls(&client, 1).await;
    assert_eq!(calls.len(), 1, "{}", dump_logs(&state.db).await);
    assert_eq!(
        calls[0].1, RIGHT_HASH,
        "rejected source must be blocklisted before re-search"
    );
    let failure: (String, String) =
        sqlx::query_as("SELECT state,failure_reason FROM grabbed_torrents WHERE id=?")
            .bind(grab.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        failure,
        (
            "failed".into(),
            ryokan::services::post_processing::REQUIREMENTS_REJECTED_REASON.into()
        )
    );
    assert_eq!(
        std::fs::read(
            temp.path()
                .join("Research Show/Season 01/Research Show - S01E03 [720p WEB].mkv")
        )
        .unwrap(),
        b"original video"
    );
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"completed source without subtitles"
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn two_requirements_rejections_in_one_sweep_research_both_episode_slots() {
    let _gate = ENV_LOCK.lock().await;
    let second_wrong = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let second_right = "ffffffffffffffffffffffffffffffffffffffff";
    let server = MockServer::start().await;
    let html = nyaa_results_page(&format!(
        "{}{}{}{}",
        nyaa_row(
            WRONG_HASH,
            99301,
            "[Wrong] Research Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            500
        ),
        nyaa_row(
            RIGHT_HASH,
            99302,
            "[Right] Research Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            5
        ),
        nyaa_row(
            second_wrong,
            99303,
            "[Wrong] Research Show - 05 (1080p) [WEB].mkv",
            "1.4 GiB",
            500
        ),
        nyaa_row(
            second_right,
            99304,
            "[Right] Research Show - 05 (1080p) [WEB].mkv",
            "1.4 GiB",
            5
        ),
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let (temp, state, client, grab, first_source) = prepared_rejected_import().await;
    let name = "[Wrong] Research Show - 05 (1080p) [WEB].mkv";
    let second_source = first_source.parent().unwrap().join(name);
    std::fs::copy(&first_source, &second_source).unwrap();
    let identity = ryokan::services::media_info::FileIdentity::from_metadata(
        &std::fs::metadata(&second_source).unwrap(),
    )
    .unwrap();
    ryokan::models::media_probe_cache::upsert_identified(&state.db,
        std::fs::canonicalize(&second_source).unwrap().to_str().unwrap(),&identity.cache_key(),identity.size as i64,
        &serde_json::json!({"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080}]}).to_string(),
    ).await;
    let second_id = ryokan::models::grabbed_torrents::record_grab(
        &state.db,
        second_wrong,
        name,
        grab.series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();
    ryokan::models::monitoring::set_episode_monitored(&state.db, grab.series_id, 5, true)
        .await
        .unwrap();
    let mut item = client.completed_for_import.lock().unwrap()[0].0.clone();
    item.hash = second_wrong.into();
    item.name = name.into();
    client.completed_for_import.lock().unwrap().push((
        item,
        vec![DownloadFile {
            name: name.into(),
            size: identity.size as i64,
            progress: 1.0,
            wanted: true,
        }],
    ));
    ryokan::services::post_processing::run_once(&state).await;
    let calls = wait_for_add_calls(&client, 2).await;
    let got: std::collections::HashSet<_> = calls.iter().map(|(_, hash)| hash.as_str()).collect();
    assert_eq!(
        got,
        std::collections::HashSet::from([RIGHT_HASH, second_right]),
        "{}",
        dump_logs(&state.db).await
    );
    for id in [grab.id, second_id] {
        let reason: String =
            sqlx::query_scalar("SELECT failure_reason FROM grabbed_torrents WHERE id=?")
                .bind(id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(
            reason,
            ryokan::services::post_processing::REQUIREMENTS_REJECTED_REASON
        );
    }
    assert_eq!(
        std::fs::read(
            temp.path()
                .join("Research Show/Season 01/Research Show - S01E03 [720p WEB].mkv")
        )
        .unwrap(),
        b"original video"
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn requirement_rejection_does_not_revive_cancelled_or_reactivated_imports() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    for reactivated in [false, true] {
        let (temp, state, client, grab, _source) = prepared_rejected_import().await;
        let gate = std::sync::Arc::new(ImportFileGate {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *client.import_file_gate.lock().unwrap() = Some(gate.clone());
        let worker_state = state.clone();
        let worker = tokio::spawn(async move {
            ryokan::services::post_processing::run_once(&worker_state).await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), gate.started.notified())
            .await
            .unwrap();
        if reactivated {
            // Keep grabbed_at identical to exercise same-second reactivation;
            // every real reactivation clears its completed-seen stamp.
            sqlx::query("UPDATE grabbed_torrents SET completed_seen_at=NULL WHERE id=?")
                .bind(grab.id)
                .execute(&state.db)
                .await
                .unwrap();
        } else {
            ryokan::models::grabbed_torrents::mark_removed(&state.db, grab.id)
                .await
                .unwrap();
        }
        let before:(String,String,Option<String>)=sqlx::query_as(
            "SELECT state,COALESCE(failure_reason,''),completed_seen_at FROM grabbed_torrents WHERE id=?",
        ).bind(grab.id).fetch_one(&state.db).await.unwrap();
        let tags_before = ryokan::models::episode_tags::get_for_series(&state.db, grab.series_id)
            .await
            .unwrap();
        gate.release.notify_one();
        worker.await.unwrap();
        let after:(String,String,Option<String>)=sqlx::query_as(
            "SELECT state,COALESCE(failure_reason,''),completed_seen_at FROM grabbed_torrents WHERE id=?",
        ).bind(grab.id).fetch_one(&state.db).await.unwrap();
        assert_eq!(
            before, after,
            "a stale rejected import cannot change current user intent"
        );
        let tags_after = ryokan::models::episode_tags::get_for_series(&state.db, grab.series_id)
            .await
            .unwrap();
        assert_eq!(tags_before[&3].state, tags_after[&3].state);
        assert_eq!(tags_before[&3].release_title, tags_after[&3].release_title);
        assert!(client.add_calls().is_empty());
        assert_eq!(
            std::fs::read(
                temp.path()
                    .join("Research Show/Season 01/Research Show - S01E03 [720p WEB].mkv")
            )
            .unwrap(),
            b"original video"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    unset_nyaa_base();
}

#[tokio::test]
async fn requirements_rejection_retry_honors_intent_and_failure_budget() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    for case in [
        "disabled",
        "policy_disabled",
        "budget",
        "manual",
        "batch",
        "opt_out",
        "missing_unmonitored",
        "pin",
        "successor",
        "manual_picker",
        "io_failure",
        "cancelled",
        "missing_folder",
        "invalid_root",
    ] {
        let (_temp, state, client, mut grab) =
            requirement_retry_fixture(!matches!(case, "missing_unmonitored" | "missing_folder"))
                .await;
        match case {
            "disabled" => {
                sqlx::query("UPDATE config SET auto_redownload_failed=0")
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            "policy_disabled" => {
                sqlx::query("UPDATE config SET media_requirements_enabled=0")
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            "missing_folder" => {
                ryokan::models::monitoring::set_episode_monitored(
                    &state.db,
                    grab.series_id,
                    3,
                    true,
                )
                .await
                .unwrap();
                std::fs::remove_dir(_temp.path().join("Research Show")).unwrap();
            }
            "invalid_root" => {
                ryokan::models::monitoring::set_episode_monitored(
                    &state.db,
                    grab.series_id,
                    3,
                    true,
                )
                .await
                .unwrap();
                sqlx::query("UPDATE config SET media_root='' ")
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            "budget" => {
                for index in 0..2 {
                    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state) VALUES(?,?,?,'[3]','failed')")
                        .bind(format!("budget-{index}")).bind(format!("failed-{index}")).bind(grab.series_id)
                        .execute(&state.db).await.unwrap();
                }
            }
            "manual" => {
                sqlx::query("UPDATE grabbed_torrents SET replacement_episode=3 WHERE id=?")
                    .bind(grab.id)
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            "batch" => {
                grab.is_batch = true;
            }
            "opt_out" => {
                sqlx::query("UPDATE series SET allow_upgrades=0 WHERE id=?")
                    .bind(grab.series_id)
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            "missing_unmonitored" => {}
            "manual_picker" => {
                ryokan::models::pending_grabs::create(
                    &state.db,
                    "manual-choice",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "qbittorrent",
                    None,
                    Some(grab.series_id),
                    "{}",
                    true,
                    Some(1),
                )
                .await
                .unwrap();
            }
            "pin" => {
                let classification = ryokan::services::source::classify_release_sync(
                    "[Old] Research Show - 03 (720p WEB)",
                    None,
                );
                ryokan::models::episode_tags::record_grab(
                    &state.db,
                    grab.series_id,
                    3,
                    &classification,
                    "old",
                    "",
                    12,
                    false,
                )
                .await
                .unwrap();
                sqlx::query("UPDATE episode_quality_tags SET manual_override=1,state='completed' WHERE series_id=?").bind(grab.series_id).execute(&state.db).await.unwrap();
            }
            "successor" => {
                ryokan::models::grabbed_torrents::record_grab(
                    &state.db,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "successor",
                    grab.series_id,
                    &[3],
                    false,
                )
                .await
                .unwrap();
            }
            "io_failure" => {
                sqlx::query("UPDATE grabbed_torrents SET failure_reason='' WHERE id=?")
                    .bind(grab.id)
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            "cancelled" => {
                sqlx::query("UPDATE grabbed_torrents SET state='removed' WHERE id=?")
                    .bind(grab.id)
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !ryokan::services::redownload::after_requirements_rejected(&state, &grab).await,
            "case={case}"
        );
        assert!(client.add_calls().is_empty(), "case={case}");
    }
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "skip paths must not start a search"
    );
    unset_nyaa_base();
}

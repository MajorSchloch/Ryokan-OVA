//! A structural season heading must survive dispatch, saved ownership and import.
//! Provider episode bounds remain authoritative when the total count is unknown.

use super::*;
use ryokan::models::{episode_tags, grabbed_torrents};
use ryokan::services::download_client::DownloadItemState;

const ANILIST_ID: i64 = 172192;
const MAL_ID: i64 = 57612;
const ROMAJI: &str = "Kikansha no Mahou wa Tokubetsu desu 2nd Season";
const ENGLISH: &str = "A Returner's Magic Should be Special Season 2";
// The saved incident records the exact title/CRC, not a full torrent hash.
// This deliberately synthetic identity is confined to the local HTTP fixture.
const HASH: &str = "1234123412341234123412341234123412341234";
const REAL_TITLE: &str = "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu 2nd Season - 03 [1080p CR WEB-DL AVC AAC][MultiSub][B459EA91]";

async fn fixture() -> (
    tempfile::TempDir,
    AppState,
    i64,
    std::sync::Arc<AutoSearchRecordingClient>,
) {
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    let mut cfg = default_config();
    cfg.media_root = temp.path().join("library").to_string_lossy().into_owned();
    cfg.post_processing_enabled = true;
    cfg.post_processing_mode = "copy".into();
    cfg.media_requirements_enabled = true;
    cfg.minimum_resolution = "720".into();
    cfg.target_resolution = "1080".into();
    cfg.required_subtitle_languages = "en".into();
    cfg.minimum_subtitle_check = true;
    cfg.target_subtitle_check = true;
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let sid = ryokan::test_support::seed_series(&state.db, ANILIST_ID, ENGLISH).await;
    sqlx::query("UPDATE series SET mal_id=?,title_romaji=?,title_english=?,format='TV',status='RELEASING',episodes=NULL,cumulative_prior_episodes=12,cumulative_offset_known=1,monitor_mode='all',allow_upgrades=0 WHERE id=?")
        .bind(MAL_ID).bind(ROMAJI).bind(ENGLISH).bind(sid)
        .execute(&state.db).await.unwrap();
    let mut detail = detail_for(ANILIST_ID, ROMAJI);
    detail.id_mal = Some(MAL_ID);
    detail.title_english = ENGLISH.into();
    detail.title_native.clear();
    detail.status = "RELEASING".into();
    detail.episodes = None;
    detail.season_year = Some(2026);
    detail.end_year = None;
    ryokan::models::metadata_cache::upsert(&state.db, sid, ANILIST_ID, Some(MAL_ID), &detail)
        .await
        .unwrap();
    ryokan::services::anibridge::seed_mapping_data_for_tests(&serde_json::json!({
        "mal:57612": {"anilist:172192": {"1-4":"1-4"}}
    }))
    .await;
    ryokan::models::nyaa_description_cache::upsert(&state.db, HASH, "Source: CR WEB-DL").await;
    let client = install_recording_default_torrent_client(&state).await;
    (temp, state, sid, client)
}

async fn serve(server: &MockServer, title: &str) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                HASH, 99981, title, "500 MiB", 50,
            ))),
        )
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

async fn search(state: &AppState, sid: i64, episode: i32) -> usize {
    let report = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, episode)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0;
    report.grabbed.len()
}

async fn complete_file(
    temp: &tempfile::TempDir,
    state: &AppState,
    client: &AutoSearchRecordingClient,
    filename: &str,
) -> std::path::PathBuf {
    let downloads = temp.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let source = downloads.join(filename);
    std::fs::write(&source, b"the actual downloaded third episode").unwrap();
    let identity = ryokan::services::media_info::FileIdentity::from_metadata(
        &std::fs::metadata(&source).unwrap(),
    )
    .unwrap();
    ryokan::models::media_probe_cache::upsert_identified(
        &state.db,
        std::fs::canonicalize(&source).unwrap().to_str().unwrap(),
        &identity.cache_key(),
        identity.size as i64,
        &serde_json::json!({
            "format":{"duration":"1440.0"},
            "streams":[
                {"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
                {"index":1,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng","title":"English"},"disposition":{"default":1,"forced":0}}
            ]
        }).to_string(),
    )
    .await;
    let path = downloads.to_string_lossy().into_owned();
    *client.completed_for_import.lock().unwrap() = vec![(
        DownloadItem {
            hash: HASH.into(),
            name: REAL_TITLE.into(),
            size: identity.size as i64,
            progress: 1.0,
            dlspeed: 0,
            state: "seeding".into(),
            category: "anime".into(),
            eta: 0,
            save_path: path.clone(),
            content_path: path,
            state_kind: DownloadItemState::Seeding,
            seeding_done: false,
        },
        vec![DownloadFile {
            name: filename.into(),
            size: identity.size as i64,
            progress: 1.0,
            wanted: true,
        }],
    )];
    source
}

pub(super) fn library_videos(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut videos = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return videos;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            videos.extend(library_videos(&path));
        } else if path.extension().is_some_and(|ext| ext == "mkv") {
            videos.push(path);
        }
    }
    videos
}

#[tokio::test]
async fn structural_season_numbering_returner_auto_grab_persists_and_imports_own_three() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    serve(&server, REAL_TITLE).await;
    let (temp, state, sid, client) = fixture().await;

    assert_eq!(
        search(&state, sid, 3).await,
        1,
        "{}",
        dump_logs(&state.db).await
    );
    assert_eq!(client.add_calls().len(), 1);
    assert_eq!(client.add_calls()[0].1, HASH);
    let pending = grabbed_torrents::get_all_pending(&state.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    let grab = &pending[0];
    assert_eq!(grab.series_id, sid);
    assert_eq!(grab.episode_numbers, vec![3]);
    assert!(!grab.is_batch);
    assert_eq!(grab.download_client_id, Some(1));
    let mapping = grabbed_torrents::get_release_mapping(&state.db, grab.id)
        .await
        .unwrap()
        .expect("automatic dispatch must save the source decision");
    assert_eq!(mapping.anilist_id, ANILIST_ID);
    assert_eq!(mapping.episodes(), vec![3]);
    assert_eq!(mapping.offset(), 0);
    let saved = serde_json::to_value(&mapping).unwrap();
    assert_eq!(saved["source_season"], 2);
    assert_eq!(saved["basis"], "season_relative");
    let source_url: String =
        sqlx::query_scalar("SELECT source_url FROM grabbed_torrents WHERE id=?")
            .bind(grab.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(
        source_url.contains(HASH),
        "the exact source identity must survive dispatch"
    );
    let tags = episode_tags::get_for_series(&state.db, sid).await.unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[&3].state, "grabbed");

    let source = complete_file(&temp, &state, &client, &format!("{REAL_TITLE}.mkv")).await;
    // A fresh application state must import the persisted decision, rather
    // than retaining an in-memory search result or subtracting the saved12.
    let restarted = build_test_app_state(state.db.clone(), None);
    *restarted.download_clients.write().await = state.download_clients.read().await.clone();
    ryokan::services::post_processing::run_once(&restarted).await;
    let videos = library_videos(&temp.path().join("library"));
    assert_eq!(videos.len(), 1, "{}", dump_logs(&state.db).await);
    assert!(
        videos[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("S01E03")
    );
    assert_eq!(
        std::fs::read(&videos[0]).unwrap(),
        std::fs::read(&source).unwrap()
    );
    assert_eq!(
        client.add_calls().len(),
        1,
        "import must not add a second client job"
    );
    let imported: (String, Option<String>) =
        sqlx::query_as("SELECT state,imported_at FROM grabbed_torrents WHERE id=?")
            .bind(grab.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(imported.0, "imported");
    assert!(imported.1.is_some());
    let tags = episode_tags::get_for_series(&state.db, sid).await.unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[&3].state, "completed");
    let history: Vec<(i32, String)> =
        sqlx::query_as("SELECT episode_number,state FROM episode_grab_history WHERE series_id=?")
            .bind(sid)
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert!(!history.is_empty());
    assert!(
        history
            .iter()
            .all(|(episode, state)| *episode == 3 && state == "completed")
    );
    let offset: i32 = sqlx::query_scalar("SELECT cumulative_prior_episodes FROM series WHERE id=?")
        .bind(sid)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        offset, 12,
        "source-relative evidence must not rewrite library numbering"
    );
    assert_eq!(
        grabbed_torrents::get_release_mapping(&state.db, grab.id)
            .await
            .unwrap()
            .unwrap(),
        mapping
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn structural_season_numbering_preserves_wrong_season_part_cour_and_provider_gap_holds() {
    let _gate = ENV_LOCK.lock().await;
    for (title, episode) in [
        (
            "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu 3rd Season - 03 [1080p CR WEB-DL AVC AAC][MultiSub]",
            3,
        ),
        (
            "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu Part 2 - 03 [1080p CR WEB-DL AVC AAC][MultiSub]",
            3,
        ),
        (
            "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu Cour II - 03 [1080p CR WEB-DL AVC AAC][MultiSub]",
            3,
        ),
        (
            "[ASW] Kikansha no Mahou wa Tokubetsu desu - 15 [1080p HEVC x265 10Bit][AAC]",
            3,
        ),
        (
            "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu 2nd Season - 05 [1080p CR WEB-DL AVC AAC][MultiSub]",
            5,
        ),
    ] {
        let server = MockServer::start().await;
        serve(&server, title).await;
        let (temp, state, sid, client) = fixture().await;
        assert_eq!(
            search(&state, sid, episode).await,
            0,
            "{title}\n{}",
            dump_logs(&state.db).await
        );
        assert!(client.add_calls().is_empty(), "{title}");
        assert!(
            grabbed_torrents::get_all_pending(&state.db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            episode_tags::get_for_series(&state.db, sid)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(library_videos(&temp.path().join("library")).is_empty());
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn structural_season_numbering_import_refuses_conflicting_actual_file_seasons() {
    let _gate = ENV_LOCK.lock().await;
    for filename in [
        "Kikansha no Mahou wa Tokubetsu desu - S01E03.mkv",
        "Kikansha no Mahou wa Tokubetsu desu 3rd Season - 03.mkv",
    ] {
        let server = MockServer::start().await;
        serve(&server, REAL_TITLE).await;
        let (temp, state, sid, client) = fixture().await;
        assert_eq!(
            search(&state, sid, 3).await,
            1,
            "{}",
            dump_logs(&state.db).await
        );
        let pending = grabbed_torrents::get_all_pending(&state.db).await.unwrap();
        let grab = &pending[0];
        let mapping_before = grabbed_torrents::get_release_mapping(&state.db, grab.id)
            .await
            .unwrap();
        let tags_before = episode_tags::get_for_series(&state.db, sid).await.unwrap();
        let claims_before = grab.episode_numbers.clone();
        let source = complete_file(&temp, &state, &client, filename).await;
        ryokan::services::post_processing::run_once(&state).await;
        assert!(
            library_videos(&temp.path().join("library")).is_empty(),
            "{filename}\n{}",
            dump_logs(&state.db).await
        );
        assert_eq!(
            std::fs::read(source).unwrap(),
            b"the actual downloaded third episode"
        );
        let status: (String, Option<String>, String) = sqlx::query_as(
            "SELECT state,imported_at,episode_numbers FROM grabbed_torrents WHERE id=?",
        )
        .bind(grab.id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_ne!(status.0, "imported", "{filename}");
        assert!(status.1.is_none());
        assert_eq!(
            serde_json::from_str::<Vec<i32>>(&status.2).unwrap(),
            claims_before
        );
        assert_eq!(
            grabbed_torrents::get_release_mapping(&state.db, grab.id)
                .await
                .unwrap(),
            mapping_before
        );
        let tags_after = episode_tags::get_for_series(&state.db, sid).await.unwrap();
        assert_eq!(tags_after.len(), tags_before.len());
        assert_eq!(tags_after[&3].release_title, tags_before[&3].release_title);
        assert_ne!(tags_after[&3].state, "completed");
        assert_eq!(client.add_calls().len(), 1);
    }
    unset_nyaa_base();
}

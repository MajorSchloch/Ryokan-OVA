//! Actual daily-sweep searches use media format without losing the library slot.

use super::*;

const GOOD_HASH: &str = "1234512345123451234512345123451234512345";

async fn fixture(
    format: &str,
    episodes: i32,
    width: u32,
    height: u32,
    requirements: bool,
    ranged_file: bool,
) -> (
    tempfile::TempDir,
    AppState,
    std::sync::Arc<AutoSearchRecordingClient>,
    i64,
    std::path::PathBuf,
) {
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    let mut cfg = default_config();
    cfg.media_root = temp.path().to_string_lossy().into_owned();
    cfg.media_requirements_enabled = requirements;
    cfg.minimum_resolution = "720".into();
    cfg.target_resolution = "1080".into();
    cfg.cutoff_resolution = "1080".into();
    cfg.required_subtitle_languages = "en".into();
    cfg.minimum_subtitle_check = true;
    cfg.target_subtitle_check = true;
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let sid = seed_series_with_cache(&state, 99882, "Feature Story").await;
    sqlx::query(
        "UPDATE series SET format=?,episodes=?,monitor_mode='future',allow_upgrades=1 WHERE id=?",
    )
    .bind(format)
    .bind(episodes)
    .bind(sid)
    .execute(&state.db)
    .await
    .unwrap();
    let mut detail = detail_for(99882, "Feature Story");
    detail.format = format.into();
    detail.episodes = Some(episodes);
    ryokan::models::metadata_cache::upsert(&state.db, sid, 99882, None, &detail)
        .await
        .unwrap();
    let slot = if ranged_file { "S01E01-E02" } else { "S01E01" };
    let old = temp.path().join(format!(
        "Feature Story/Season 01/Feature Story - {slot} [720p WEB].mkv"
    ));
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"existing full-length video").unwrap();
    let canonical = std::fs::canonicalize(&old).unwrap();
    let identity = ryokan::services::media_info::FileIdentity::from_metadata(
        &std::fs::metadata(&canonical).unwrap(),
    )
    .unwrap();
    ryokan::models::media_probe_cache::upsert_identified(
        &state.db,
        canonical.to_str().unwrap(),
        &identity.cache_key(),
        identity.size as i64,
        &serde_json::json!({
            "format":{"duration":"1400"},
            "streams":[
                {"index":0,"codec_type":"video","codec_name":"h264","width":width,"height":height},
                {"index":1,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng"}}
            ]
        })
        .to_string(),
    )
    .await;
    let client = install_recording_default_torrent_client(&state).await;
    (temp, state, client, sid, old)
}

async fn assert_logical_grab(state: &AppState, client: &AutoSearchRecordingClient, sid: i64) {
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1, "{}", dump_logs(&state.db).await);
    assert_eq!(calls[0].1, GOOD_HASH);
    let grabs = ryokan::models::grabbed_torrents::get_all_pending(&state.db)
        .await
        .unwrap();
    assert_eq!(grabs.len(), 1);
    assert_eq!(grabs[0].series_id, sid);
    assert_eq!(grabs[0].episode_numbers, [1]);
    assert!(!grabs[0].is_batch);
    assert_eq!(
        ryokan::models::grabbed_torrents::get_replacement_episode(&state.db, grabs[0].id)
            .await
            .unwrap(),
        None,
        "daily upgrades must keep automatic import/runtime protection"
    );
    let history: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT episode_number,match_kind,matched_alias FROM episode_grab_history WHERE series_id=?",
    )
    .bind(sid)
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(history, [(1, "verbatim".into(), "Feature Story".into())]);
    let tags = ryokan::models::episode_tags::get_for_series(&state.db, sid)
        .await
        .unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[&1].state, "grabbed");
    assert!(!tags[&1].manual_override);
}

#[tokio::test]
async fn daily_sweep_grabs_numberless_single_entry_releases_for_logical_episode_one() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let html = nyaa_results_page(&format!(
        "{}{}{}",
        nyaa_row(
            "2222222222222222222222222222222222222222",
            998821,
            "[Candidate] Feature Story Another Journey [1080p WEB-DL AAC]",
            "500 MiB",
            500,
        ),
        nyaa_row(
            "3333333333333333333333333333333333333333",
            998822,
            "[Candidate] Feature Story - 01-02 [1080p WEB-DL AAC]",
            "500 MiB",
            500,
        ),
        nyaa_row(
            GOOD_HASH,
            998823,
            "[Candidate] Feature Story [1080p WEB-DL AAC]",
            "500 MiB",
            5,
        ),
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    for format in ["MOVIE", "OVA", "ONA", "SPECIAL"] {
        let (_temp, state, client, sid, old) = fixture(format, 1, 1280, 720, true, false).await;
        let summary = ryokan::services::upgrade::run_once(&state).await.unwrap();
        assert_eq!(
            summary.upgrades_grabbed,
            1,
            "format={format}; {}",
            dump_logs(&state.db).await
        );
        assert_eq!(summary.episodes_checked, 1);
        assert_logical_grab(&state, &client, sid).await;
        assert_eq!(std::fs::read(&old).unwrap(), b"existing full-length video");
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn daily_sweep_numberless_movie_keeps_legacy_upgrade_policy() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                GOOD_HASH,
                998840,
                "[Candidate] Feature Story [1080p WEB-DL AAC]",
                "500 MiB",
                5,
            ))),
        )
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let (_temp, state, client, sid, old) = fixture("MOVIE", 1, 1280, 720, false, false).await;
    let summary = ryokan::services::upgrade::run_once(&state).await.unwrap();
    assert_eq!(
        summary.upgrades_grabbed,
        1,
        "{}",
        dump_logs(&state.db).await
    );
    assert_logical_grab(&state, &client, sid).await;
    assert_eq!(std::fs::read(&old).unwrap(), b"existing full-length video");
    unset_nyaa_base();
}

#[tokio::test]
async fn daily_sweep_keeps_numbered_targets_for_tv_and_multi_entry_releases() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let titles = [
        "[Candidate] Feature Story [1080p WEB-DL AAC]",
        "[Candidate] Feature Story S02E01 [1080p WEB-DL AAC]",
        "[Candidate] Feature Story S01E02 [1080p WEB-DL AAC]",
        "[Candidate] Feature Story S01E01-E02 [1080p WEB-DL AAC]",
        "[Candidate] Feature Story Another Journey S01E01 [1080p WEB-DL AAC]",
    ];
    let mut html = String::new();
    for (index, title) in titles.iter().enumerate() {
        html.push_str(&nyaa_row(
            &format!("{:040x}", index + 10),
            998830 + index as u64,
            title,
            "500 MiB",
            500,
        ));
    }
    html.push_str(&nyaa_row(
        GOOD_HASH,
        998839,
        "[Candidate] Feature Story S01E01 [1080p WEB-DL AAC]",
        "500 MiB",
        5,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&html)))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    for format in ["TV", "TV_SHORT", "OVA"] {
        let (_temp, state, client, sid, old) = fixture(format, 3, 1280, 720, true, false).await;
        let summary = ryokan::services::upgrade::run_once(&state).await.unwrap();
        assert_eq!(
            summary.upgrades_grabbed,
            1,
            "format={format}; {}",
            dump_logs(&state.db).await
        );
        assert_logical_grab(&state, &client, sid).await;
        assert_eq!(std::fs::read(&old).unwrap(), b"existing full-length video");
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn daily_sweep_single_entry_search_keeps_target_pin_pending_and_span_protection() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    set_nyaa_base(&server.uri());
    for case in ["target_met", "pin", "pending", "multi_file", "unverified"] {
        let (width, height) = match case {
            "target_met" => (1920, 1080),
            "unverified" => (0, 0),
            _ => (1280, 720),
        };
        let (_temp, state, client, sid, old) =
            fixture("MOVIE", 1, width, height, true, case == "multi_file").await;
        if matches!(case, "pin" | "pending") {
            sqlx::query("INSERT INTO episode_quality_tags(series_id,episode_number,state,manual_override) VALUES(?,1,?,?)")
                .bind(sid).bind(if case == "pending" { "grabbed" } else { "completed" })
                .bind(i64::from(case == "pin")).execute(&state.db).await.unwrap();
        }
        let tags_before: Vec<(i32, String, i64)> = sqlx::query_as(
            "SELECT episode_number,state,manual_override FROM episode_quality_tags WHERE series_id=? ORDER BY episode_number",
        )
        .bind(sid)
        .fetch_all(&state.db)
        .await
        .unwrap();
        let summary = ryokan::services::upgrade::run_once(&state).await.unwrap();
        assert_eq!(summary.episodes_checked, 0, "{case}");
        assert_eq!(summary.upgrades_grabbed, 0, "{case}");
        assert!(client.add_calls().is_empty(), "{case}");
        let tags_after: Vec<(i32, String, i64)> = sqlx::query_as(
            "SELECT episode_number,state,manual_override FROM episode_quality_tags WHERE series_id=? ORDER BY episode_number",
        )
        .bind(sid)
        .fetch_all(&state.db)
        .await
        .unwrap();
        assert_eq!(tags_before, tags_after, "{case}");
        assert_eq!(std::fs::read(&old).unwrap(), b"existing full-length video");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    unset_nyaa_base();
}

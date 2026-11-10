//! A numeric provider title cannot borrow its identity from the episode field.
use super::*;

const ACTUAL_WRONG_HASH: &str = "00c1fb5528b441572e5e30054b3e4ae9e73d6b3a";
const GOOD_HASH: &str = "726bc7daf80a5de5c3f81ee9d022334455667788";
const WRONG_TITLE: &str =
    "[YakuboEncodes] Digimon Adventure (2020) - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]";
const GOOD_TITLE: &str =
    "[Keyword] Digimon Adventure 02 - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]";

async fn seed_digimon(state: &AppState) -> i64 {
    let sid = seed_series_with_cache(state, 1313, "Digimon Adventure 02").await;
    let mut detail = detail_for(1313, "Digimon Adventure 02");
    detail.title_english = "Digimon: Digital Monsters 02".into();
    detail.title_native = "デジモンアドベンチャー０２".into();
    detail.episodes = Some(50);
    detail.season_year = Some(2000);
    detail.end_year = Some(2001);
    sqlx::query("UPDATE series SET title_english=?, episodes=50, season_year=2000 WHERE id=?")
        .bind(&detail.title_english)
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert(&state.db, sid, 1313, None, &detail)
        .await
        .unwrap();
    for hash in [ACTUAL_WRONG_HASH, GOOD_HASH] {
        ryokan::models::nyaa_description_cache::upsert(&state.db, hash, "Source: WEB-DL").await;
    }
    sid
}

async fn serve_numeric_rows(server: &MockServer, with_correct_release: bool) {
    let mut rows = nyaa_row(ACTUAL_WRONG_HASH, 99010, WRONG_TITLE, "353 MiB", 500);
    if with_correct_release {
        rows.push_str(&nyaa_row(GOOD_HASH, 99011, GOOD_TITLE, "353 MiB", 5));
    }
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn numeric_title_identity_actual_digimon_reboot_is_never_automatically_grabbed() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    serve_numeric_rows(&server, false).await;
    set_nyaa_base(&server.uri());
    let state = build_state().await;
    seed_digimon(&state).await;
    let client = install_recording_default_torrent_client(&state).await;
    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((1313, 2)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap();
    assert!(
        report.grabbed.is_empty(),
        "episode 02 cannot make the reboot into Adventure 02: {report:?}"
    );
    assert!(client.add_calls().is_empty());
    let grabs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(grabs, 0);
    unset_nyaa_base();
}

#[tokio::test]
async fn numeric_title_identity_selects_correct_02_release_despite_reboot_having_more_seeders() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    serve_numeric_rows(&server, true).await;
    set_nyaa_base(&server.uri());
    let state = build_state().await;
    let sid = seed_digimon(&state).await;
    let client = install_recording_default_torrent_client(&state).await;
    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((1313, 2)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap();
    assert_eq!(report.grabbed.len(), 1, "{report:?}");
    assert_eq!(report.grabbed[0].release_title, GOOD_TITLE);
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, GOOD_HASH);
    let row: (String, String, String, f64) = sqlx::query_as("SELECT match_kind, match_phase, matched_alias, match_ratio FROM episode_grab_history WHERE series_id=? AND episode_number=2")
        .bind(sid).fetch_one(&state.db).await.unwrap();
    assert_eq!(
        row,
        (
            "verbatim".into(),
            "primary".into(),
            "Digimon Adventure 02".into(),
            1.0
        )
    );
    let manual_override: i64 = sqlx::query_scalar("SELECT COALESCE(manual_override,0) FROM episode_quality_tags WHERE series_id=? AND episode_number=2")
        .bind(sid).fetch_one(&state.db).await.unwrap();
    assert_eq!(manual_override, 0);
    unset_nyaa_base();
}

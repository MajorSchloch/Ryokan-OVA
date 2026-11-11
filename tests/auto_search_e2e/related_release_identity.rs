//! A known related movie cannot borrow the parent's short franchise alias.
use super::*;
use ryokan::services::anilist::RelatedEntry;

const AKITO_HASH: &str = "550a813a4f1f104de2d2dd5d03a45a7742e5610c";
const TV_HASH: &str = "2ac2ac2ac2ac2ac2ac2ac2ac2ac2ac2ac2ac2ac2";
const AKITO_RELEASE: &str =
    "[LNS] Code Geass - Boukoku no Akito (Akito the Exiled) - 01 [BD 1080p]";
const TV_RELEASE: &str = "[Group] Code Geass - 01 [WEB 1080p]";

fn relation(detail: &AnimeDetail, kind: &str) -> RelatedEntry {
    RelatedEntry {
        release_date: detail.release_date.clone(),
        id: detail.id,
        id_mal: None,
        title_romaji: detail.title_romaji.clone(),
        title_english: detail.title_english.clone(),
        title_native: detail.title_native.clone(),
        cover_url: String::new(),
        format: detail.format.clone(),
        status: detail.status.clone(),
        status_display: detail.status_display.clone(),
        episodes: detail.episodes,
        relation_type: kind.into(),
        season_year: detail.season_year,
        start_date: detail.release_date.clone(),
        media_type: "ANIME".into(),
    }
}

fn movie() -> AnimeDetail {
    let mut detail = detail_for(
        8888,
        "Code Geass: Boukoku no Akito 1 - Yokuryuu wa Maiorita",
    );
    detail.title_english = "Code Geass: Akito the Exiled - The Wyvern Arrives".into();
    detail.title_native = "コードギアス 亡国のアキト 第1章 翼竜は舞い降りた".into();
    detail.format = "MOVIE".into();
    detail.episodes = Some(1);
    detail.duration = Some(50);
    detail.season_year = Some(2012);
    detail.end_year = Some(2012);
    detail
}

fn television() -> AnimeDetail {
    let mut detail = detail_for(1575, "Code Geass: Hangyaku no Lelouch");
    detail.title_english = "Code Geass: Lelouch of the Rebellion".into();
    detail.title_native = "コードギアス 反逆のルルーシュ".into();
    detail.episodes = Some(25);
    detail.season_year = Some(2006);
    detail.end_year = Some(2007);
    detail.relations = vec![relation(&movie(), "OTHER")];
    detail
}

async fn seed_detail(state: &AppState, detail: &AnimeDetail) -> i64 {
    let sid = ryokan::test_support::seed_series(&state.db, detail.id, &detail.title_romaji).await;
    sqlx::query("UPDATE series SET episodes=?,format=?,title_english=? WHERE id=?")
        .bind(detail.episodes)
        .bind(&detail.format)
        .bind(&detail.title_english)
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert(&state.db, sid, detail.id, None, detail)
        .await
        .unwrap();
    sid
}

async fn serve(server: &MockServer, state: &AppState, rows: &[(&str, &str, i32)]) {
    let mut body = String::new();
    for (index, (hash, title, seeders)) in rows.iter().enumerate() {
        body.push_str(&nyaa_row(
            hash,
            99800 + index as u64,
            title,
            "1.0 GiB",
            *seeders,
        ));
        ryokan::models::nyaa_description_cache::upsert(&state.db, hash, "Source: Blu-ray").await;
    }
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&body)))
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

#[tokio::test]
async fn related_identity_actual_akito_movie_is_not_grabbed_for_parent_tv() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let state = build_state().await;
    let sid = seed_detail(&state, &television()).await;
    let client = install_recording_default_torrent_client(&state).await;
    serve(&server, &state, &[(AKITO_HASH, AKITO_RELEASE, 500)]).await;
    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, 1)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap();
    unset_nyaa_base();
    assert!(
        report.grabbed.is_empty(),
        "a known OTHER movie is a different release: {report:?}"
    );
    assert!(client.add_calls().is_empty());
    let grabs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(grabs, 0);
}

#[tokio::test]
async fn related_identity_valid_lower_seeder_parent_wins_over_known_movie() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let state = build_state().await;
    let sid = seed_detail(&state, &television()).await;
    let client = install_recording_default_torrent_client(&state).await;
    serve(
        &server,
        &state,
        &[(AKITO_HASH, AKITO_RELEASE, 500), (TV_HASH, TV_RELEASE, 2)],
    )
    .await;
    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, 1)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap();
    unset_nyaa_base();
    assert_eq!(report.grabbed.len(), 1, "{report:?}");
    assert_eq!(report.grabbed[0].release_title, TV_RELEASE);
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, TV_HASH);
}

#[tokio::test]
async fn related_identity_akito_stays_selectable_for_its_own_movie_entry() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let state = build_state().await;
    let mut detail = movie();
    detail.relations = vec![relation(&television(), "OTHER")];
    let sid = seed_detail(&state, &detail).await;
    let client = install_recording_default_torrent_client(&state).await;
    serve(&server, &state, &[(AKITO_HASH, AKITO_RELEASE, 500)]).await;
    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, 1)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap();
    unset_nyaa_base();
    assert_eq!(report.grabbed.len(), 1, "{report:?}");
    assert_eq!(client.add_calls()[0].1, AKITO_HASH);
    let saved_series: i64 = sqlx::query_scalar("SELECT series_id FROM grabbed_torrents")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(saved_series, sid);
}

#[tokio::test]
async fn related_identity_keeps_legitimate_episode_title_before_number() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let state = build_state().await;
    let mut detail = detail_for(101922, "Kimetsu no Yaiba");
    detail.episodes = Some(26);
    let related = detail_for(112151, "Kimetsu no Yaiba: Mugen Ressha-hen - Main Story");
    detail.relations = vec![relation(&related, "OTHER")];
    let sid = seed_detail(&state, &detail).await;
    let client = install_recording_default_torrent_client(&state).await;
    let title = "[Group] Kimetsu no Yaiba - The Hand Demon - 05 [BD 1080p]";
    serve(&server, &state, &[(TV_HASH, title, 2)]).await;
    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, 5)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap();
    unset_nyaa_base();
    assert_eq!(report.grabbed.len(), 1, "{report:?}");
    assert_eq!(client.add_calls()[0].1, TV_HASH);
}

#[test]
fn related_identity_ignores_manga_and_generic_related_parent_names() {
    let mut detail = television();
    let aliases = ryokan::services::auto_search::collect_aliases(&detail);
    detail.relations[0].media_type = "MANGA".into();
    assert!(
        ryokan::services::auto_search::collect_related_identity_aliases(&detail, &aliases)
            .is_empty()
    );
    detail.relations[0].media_type = "ANIME".into();
    detail.relations[0].title_romaji = "Code Geass".into();
    detail.relations[0].title_english = "Code Geass".into();
    detail.relations[0].title_native.clear();
    assert!(
        ryokan::services::auto_search::collect_related_identity_aliases(&detail, &aliases)
            .is_empty()
    );
}

#[test]
fn related_identity_keeps_positive_file_verification_aliases_unchanged() {
    let mut detail = television();
    let own = ryokan::services::auto_search::collect_aliases(&detail);
    // OTHER is negative identity evidence, never a new positive allowlist.
    assert!(ryokan::services::auto_search::collect_sibling_aliases(&detail, &own).is_empty());
    assert!(
        !ryokan::services::auto_search::collect_related_identity_aliases(&detail, &own).is_empty()
    );
    detail.relations[0].relation_type = "SEQUEL".into();
    let verification = ryokan::services::auto_search::collect_sibling_aliases(&detail, &own);
    assert!(verification.contains(&detail.relations[0].title_romaji));
    assert!(!verification.contains(&"Code Geass: Boukoku no Akito".into()));
    assert!(!verification.contains(&"Code Geass: Akito the Exiled".into()));
}

#[tokio::test]
async fn related_identity_absolute_franchise_fallback_rejects_other_movie_keeps_valid_title() {
    use ryokan::services::auto_search::{MatchPhase, find_all_for_target, find_best_for_target};

    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let mut detail = detail_for(2904, "Code Geass: Hangyaku no Lelouch R2");
    detail.title_english = "Code Geass: Lelouch of the Rebellion R2".into();
    detail.title_native.clear();
    detail.episodes = Some(25);
    detail.relations = vec![relation(&movie(), "OTHER")];
    // The short root title represents the fallback alias. A synthetic
    // source number deliberately collides with its absolute episode 26.
    let sid = seed_franchise_chain(
        &state.db,
        detail.id,
        &detail.title_romaji,
        1575,
        "Code Geass",
        25,
    )
    .await;
    ryokan::models::metadata_cache::upsert(&state.db, sid, detail.id, None, &detail)
        .await
        .unwrap();
    let wrong = "[LNS] Code Geass - Boukoku no Akito (Akito the Exiled) - 26 [BD 1080p]";
    let valid = "[Group] Code Geass - 26 [BD 1080p]";
    let rows = format!(
        "{}{}",
        nyaa_row(AKITO_HASH, 99901, wrong, "1.0 GiB", 500),
        nyaa_row(TV_HASH, 99902, valid, "1.0 GiB", 2),
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("q", "Code Geass 26"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .with_priority(1)
        .expect(1..)
        .mount(&server)
        .await;
    for hash in [AKITO_HASH, TV_HASH] {
        ryokan::models::nyaa_description_cache::upsert(&state.db, hash, "Source: Blu-ray").await;
    }
    set_nyaa_base(&server.uri());
    let hits = find_all_for_target(
        &state.db,
        &detail,
        &default_config(),
        &SearchTarget::Episode(1),
        false,
        &[],
        &state.indexers,
    )
    .await;
    assert_eq!(
        hits.len(),
        1,
        "known OTHER movie must not enter the fallback: {hits:?}"
    );
    assert_eq!(hits[0].info_hash, TV_HASH);
    assert_eq!(
        hits[0].match_provenance.as_ref().unwrap().phase,
        MatchPhase::Franchise
    );

    assert!(
        find_best_for_target(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Episode(1),
            false,
            false,
            &[],
            &state.indexers
        )
        .await
        .is_none()
    );
    unset_nyaa_base();
    let mapping_holds: Vec<String> =
        sqlx::query_scalar("SELECT message FROM logs WHERE detail LIKE 'Episode mapping:%'")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert!(
        mapping_holds.contains(&format!("Automatic grab held: {valid}")),
        "valid absolute identity must reach the existing numbering review gate: {mapping_holds:?}"
    );
    assert!(!mapping_holds.contains(&format!("Automatic grab held: {wrong}")));
}

//! Provider coordinates must survive automatic search and saved-file preflight.
use super::*;
use ryokan::services::auto_search::{
    find_best_for_target, find_best_for_target_with_filter, infer_season_from_detail,
};
use ryokan::services::indexers::torznab::TorznabIndexer;
use ryokan::services::release_numbering::ReleaseMapping;
use std::sync::Arc;

const OWNER: i64 = 690108;
const TITLE: &str = "Provider Coordinate Show";
const GOOD_HASH: &str = "0909090909090909090909090909090909090909";
const NEIGHBOR_HASH: &str = "0808080808080808080808080808080808080808";
const LOW_HASH: &str = "4848484848484848484848484848484848484848";
const GOOD: &str = "[Fixture] Provider Coordinate Show S06E09 [1080p WEB-DL] [English]";
const NEIGHBOR: &str = "[Fixture] Provider Coordinate Show S06E08 [1080p WEB-DL] [English]";
const LOW: &str = "[Fixture] Provider Coordinate Show S06E09 [480p WEB-DL] [English]";

fn item(server: &MockServer, hash: &str, title: &str, seeds: i32) -> String {
    format!(
        r#"<item><title>{title}</title><guid>provider-singleton-{hash}</guid>
<link>{url}/{hash}.torrent</link><pubDate>Wed, 18 Nov 2026 12:00:00 +0000</pubDate>
<enclosure url="{url}/{hash}.torrent" length="419430400" type="application/x-bittorrent"/>
<torznab:attr name="category" value="5070"/><torznab:attr name="seeders" value="{seeds}"/>
<torznab:attr name="infohash" value="{hash}"/></item>"#,
        url = server.uri()
    )
}

async fn fixture(
    server: &MockServer,
    releases: &[(&str, &str, i32)],
    minimum: &str,
) -> (AppState, AnimeDetail, Config) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<rss><channel/></rss>"))
        .with_priority(10)
        .mount(server)
        .await;
    let items = releases
        .iter()
        .map(|(hash, title, seeds)| item(server, hash, title, *seeds))
        .collect::<String>();
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(|request: &wiremock::Request| {
            request.url.query_pairs().any(|(key, value)| {
                key == "q" && value.eq_ignore_ascii_case(&format!("{TITLE} S06E09"))
            })
        })
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>{items}</channel></rss>"#
        )))
        .with_priority(1)
        .expect(1..)
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let sid = ryokan::test_support::seed_series(&state.db, OWNER, TITLE).await;
    let mut owner = detail_for(OWNER, TITLE);
    owner.episodes = Some(11);
    owner.release_date = Some("2026-09-15".into());
    owner.season_year = Some(2026);
    owner.end_year = Some(2026);
    ryokan::models::metadata_cache::upsert(&state.db, sid, OWNER, None, &owner)
        .await
        .unwrap();
    // The catalogue owner has no season marker: its canonical import fallback
    // is Season1. The provider season contains one preceding local entry.
    assert_eq!(infer_season_from_detail(&owner), 0);
    ryokan::services::anibridge::seed_tmdb_episode_spans_for_tests(&[(406009, 6, 2, 12, OWNER, 1)])
        .await;

    let cfg = Config {
        media_requirements_enabled: true,
        minimum_resolution: minimum.into(),
        target_resolution: if minimum == "2160" { "2160" } else { "1080" }.into(),
        required_subtitle_languages: "en".into(),
        subtitle_language_match: "all".into(),
        minimum_subtitle_check: true,
        target_subtitle_check: true,
        ..default_config()
    };
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO indexers(id,name,kind,url,min_seeders) VALUES(194,'Provider singleton fixture','torznab',?,0)",
    )
    .bind(format!("{}/api", server.uri()))
    .execute(&state.db)
    .await
    .unwrap();
    let row = ryokan::models::indexers::get_by_id(&state.db, 194)
        .await
        .unwrap()
        .unwrap();
    *state.indexers.write().await = Arc::new(vec![TorznabIndexer::from_row_arc(&row).unwrap()]);
    (state, owner, cfg)
}

async fn assert_search_did_not_change_requirements(state: &AppState, expected: &Config) {
    let cfg = ryokan::models::config::get_config(&state.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cfg.media_requirements_enabled,
        expected.media_requirements_enabled
    );
    assert_eq!(cfg.minimum_resolution, expected.minimum_resolution);
    assert_eq!(cfg.target_resolution, expected.target_resolution);
    assert_eq!(cfg.required_subtitle_languages, "en");
    assert_eq!(cfg.subtitle_language_match, "all");
    assert!(cfg.minimum_subtitle_check && cfg.target_subtitle_check);
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM grabbed_torrents),(SELECT COUNT(*) FROM episode_quality_tags),(SELECT COUNT(*) FROM source_numbering_decisions)",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(
        counts,
        (0, 0, 0),
        "search must not create grabs, tags, or reviewed source receipts"
    );
}

#[tokio::test]
async fn provider_singleton_auto_selection_and_saved_preflight_map_s06e09_to_local_e08() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let (state, owner, cfg) = fixture(
        &server,
        &[
            (NEIGHBOR_HASH, NEIGHBOR, 500),
            (LOW_HASH, LOW, 900),
            (GOOD_HASH, GOOD, 20),
        ],
        "720",
    )
    .await;
    let result = find_best_for_target(
        &state.db,
        &owner,
        &cfg,
        &SearchTarget::Episode(8),
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
    .expect("provider S06E09 must be discoverable for catalogue-local E8");
    assert_eq!(result.info_hash, GOOD_HASH);
    assert_eq!(result.indexer_id, Some(194));
    assert!(!result.is_batch);
    assert_eq!(
        result.match_provenance.as_ref().unwrap().kind.as_str(),
        "provider_season"
    );
    let mapping = result
        .release_mapping
        .expect("provider numbering must be saved");
    assert_eq!(mapping.anilist_id, OWNER);
    assert_eq!(mapping.episodes(), [8]);
    assert_eq!(mapping.offset(), 1);
    assert!(
        mapping.bound_source().is_none(),
        "this is an ordinary singleton, not a pack approval"
    );
    let saved = serde_json::to_value(&mapping).unwrap();
    assert_eq!(saved["source_season"], 6);
    let restored: ReleaseMapping = serde_json::from_value(saved).unwrap();
    let resolved = restored.resolve_named_file(&format!("{GOOD}.mkv")).unwrap();
    assert_eq!(
        (resolved.entry, resolved.first, resolved.last),
        (OWNER, 8, 8)
    );
    for invalid in [
        format!("{NEIGHBOR}.mkv"),
        "[Fixture] Provider Coordinate Show S05E09 [1080p WEB-DL].mkv".into(),
        "[Fixture] Another Show S06E09 [1080p WEB-DL].mkv".into(),
    ] {
        assert!(restored.resolve_named_file(&invalid).is_err(), "{invalid}");
    }
    assert_search_did_not_change_requirements(&state, &cfg).await;
    unset_nyaa_base();
}

#[tokio::test]
async fn provider_singleton_auto_selection_rejects_wrong_episode_and_below_minimum() {
    let _gate = ENV_LOCK.lock().await;
    for (title, hash, minimum) in [
        (NEIGHBOR, NEIGHBOR_HASH, "720"),
        (LOW, LOW_HASH, "720"),
        (GOOD, GOOD_HASH, "2160"),
    ] {
        let server = MockServer::start().await;
        let (state, owner, cfg) = fixture(&server, &[(hash, title, 100)], minimum).await;
        assert!(
            find_best_for_target(
                &state.db,
                &owner,
                &cfg,
                &SearchTarget::Episode(8),
                false,
                false,
                &[],
                &state.indexers,
            )
            .await
            .is_none(),
            "provider evidence cannot admit title={title} under minimum={minimum}",
        );
        assert_search_did_not_change_requirements(&state, &cfg).await;
        unset_nyaa_base();
    }

    // Provider identity must also retain the caller's capability/upgrade gate.
    // A saved numbering decision authorizes a coordinate, not a file quality.
    let server = MockServer::start().await;
    let (state, owner, cfg) = fixture(&server, &[(GOOD_HASH, GOOD, 100)], "720").await;
    let gate_called = std::sync::atomic::AtomicBool::new(false);
    let reject = |candidate: &ryokan::services::nyaa::SearchResult,
                  _: &ryokan::services::source::ClassificationResult| {
        assert_eq!(candidate.release_mapping.as_ref().unwrap().episodes(), [8]);
        gate_called.store(true, std::sync::atomic::Ordering::SeqCst);
        false
    };
    let (selected, _) = find_best_for_target_with_filter(
        &state.db,
        &owner,
        &cfg,
        &SearchTarget::Episode(8),
        false,
        false,
        &[],
        &state.indexers,
        Some(&reject),
    )
    .await;
    assert!(gate_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        selected.is_none(),
        "provider numbering cannot bypass eligibility"
    );
    assert_search_did_not_change_requirements(&state, &cfg).await;
    unset_nyaa_base();
}

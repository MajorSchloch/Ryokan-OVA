//! Finite-source verification must preserve ordinary acquisition paths.
use super::*;
use ryokan::services::indexers::{Indexer, IndexerCaps, Release, SearchQuery};
use std::sync::Arc;

const HASH: &str = "caca00000000000000000000000000000000caca";

async fn cached_lagoon(state: &AppState, track_root: bool) -> AnimeDetail {
    let root = detail_for(889, "Black Lagoon");
    let sequel = detail_for(1519, "Black Lagoon: The Second Barrage");
    for detail in [&root, &sequel] {
        ryokan::models::metadata_cache::upsert_provider(&state.db, detail.id, None, detail)
            .await
            .unwrap();
    }
    if track_root {
        seed_series_with_cache(state, root.id, &root.title_english).await;
    }
    root
}

async fn assert_no_numbering_decisions(state: &AppState) {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_numbering_decisions")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "ordinary matching must not invent a source receipt"
    );
}

#[tokio::test]
async fn automatic_numbering_compat_explicit_root_single_keeps_ordinary_season_mapping() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let title = "[Fixture] Black Lagoon S01E03 [1080p WEB-DL AAC]";
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                HASH, 990003, title, "400 MiB", 20,
            ))),
        )
        .mount(&server)
        .await;
    // Explicit ordinary numbering needs no external file-list proof. An
    // attempted fetch would also fail, so it cannot accidentally pass here.
    Mock::given(method("GET"))
        .and(path("/view/990003"))
        .respond_with(ResponseTemplate::new(503))
        .expect(0)
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let state = build_state().await;
    let root = cached_lagoon(&state, true).await;
    let result = ryokan::services::auto_search::find_best_for_target(
        &state.db,
        &root,
        &default_config(),
        &SearchTarget::Episode(3),
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
    .expect("a canonical S01E03 remains eligible despite finite sequel redirects");
    let mapping = result.release_mapping.unwrap();
    assert_eq!(mapping.episodes(), [3]);
    assert!(mapping.bound_source().is_none());
    assert_eq!(serde_json::to_value(mapping).unwrap()["source_season"], 1);
    assert_no_numbering_decisions(&state).await;
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_numbering_compat_numberless_movie_keeps_ordinary_identity() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let canonical = "Kizumonogatari Part 1: Tekketsu";
    let release = "[Fixture] Kizumonogatari Part 1 - Tekketsu (BD 1080p HEVC FLAC)";
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                HASH, 990005, release, "2 GiB", 20,
            ))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/view/990005"))
        .respond_with(ResponseTemplate::new(503))
        .expect(0)
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let state = build_state().await;
    let sid = seed_series_with_cache(&state, 9260, canonical).await;
    let mut movie = detail_for(9260, canonical);
    movie.format = "MOVIE".into();
    movie.episodes = Some(1);
    sqlx::query("UPDATE series SET format='MOVIE',episodes=1 WHERE id=?")
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert(&state.db, sid, movie.id, None, &movie)
        .await
        .unwrap();
    // This real source namespace redirects raw2/raw3 to the other films.
    // Those rules do not require a numberless standalone movie to become an
    // episodic source or fetch an otherwise unnecessary file manifest.
    assert!(ryokan::services::anime_relations::has_source_namespace(
        9260
    ));
    let result = ryokan::services::auto_search::find_best_for_target(
        &state.db,
        &movie,
        &default_config(),
        &SearchTarget::Single,
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
    .expect("a numberless canonical movie remains selectable despite source redirect rules");
    assert_eq!(result.info_hash, HASH);
    assert!(result.release_mapping.is_none());
    assert_no_numbering_decisions(&state).await;
    unset_nyaa_base();
}

struct OrdinaryNzb {
    title: &'static str,
}

#[async_trait]
impl Indexer for OrdinaryNzb {
    fn id(&self) -> i64 {
        91
    }
    fn name(&self) -> &str {
        "Ordinary NZB"
    }
    fn priority(&self) -> i32 {
        1
    }
    fn is_private_tracker(&self) -> bool {
        false
    }
    fn kind(&self) -> &str {
        "newznab"
    }
    async fn caps(&self) -> Result<IndexerCaps, String> {
        Ok(IndexerCaps::default())
    }
    async fn search(&self, _: &SearchQuery) -> Result<Vec<Release>, String> {
        Ok(vec![Release {
            indexer_id: self.id(),
            indexer_priority: 1,
            indexer_name: self.name().into(),
            title: self.title.into(),
            guid: "ordinary-nzb-episode-three".into(),
            link: "https://indexer.example/episode-three.nzb".into(),
            magnet: String::new(),
            publish_date: 0,
            size_bytes: 400 * 1024 * 1024,
            seeders: 0,
            leechers: 0,
            info_hash: String::new(),
            categories: vec![5070],
            download_volume_factor: None,
            upload_volume_factor: None,
            extra: Default::default(),
        }])
    }
}

#[tokio::test]
async fn automatic_numbering_compat_nzb_preserves_explicit_season_but_holds_ambiguous_root() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());
    let state = build_state().await;
    let root = cached_lagoon(&state, true).await;
    sqlx::query("INSERT INTO indexers(id,name,kind,url,min_seeders) VALUES(91,'Ordinary NZB','newznab','https://indexer.example',0)")
        .execute(&state.db).await.unwrap();
    *state.indexers.write().await = Arc::new(vec![Arc::new(OrdinaryNzb {
        title: "[Fixture] Black Lagoon.S01E03 [1080p WEB-DL AAC]",
    }) as Arc<dyn Indexer>]);
    let cfg = default_config();
    assert_eq!(cfg.minimum_torrent_seeders, 1);
    let result = ryokan::services::auto_search::find_best_for_target(
        &state.db,
        &root,
        &cfg,
        &SearchTarget::Episode(3),
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
    .expect("NZB must not need a BT hash or a Nyaa manifest");
    assert_eq!(result.indexer_id, Some(91));
    assert!(result.info_hash.is_empty());
    assert_eq!(result.seeders, 0);
    let mapping = result.release_mapping.unwrap();
    assert_eq!(mapping.episodes(), [3]);
    assert!(mapping.bound_source().is_none());
    assert_eq!(serde_json::to_value(&mapping).unwrap()["source_season"], 1);
    // The absence of a BT manifest must not invent an interpretation for
    // a namespace that could mean root-relative or a later reset cour.
    *state.indexers.write().await = Arc::new(vec![Arc::new(OrdinaryNzb {
        title: "[Fixture] Black Lagoon - 03 [1080p WEB-DL AAC]",
    }) as Arc<dyn Indexer>]);
    assert!(
        ryokan::services::auto_search::find_best_for_target(
            &state.db,
            &root,
            &cfg,
            &SearchTarget::Episode(3),
            false,
            false,
            &[],
            &state.indexers,
        )
        .await
        .is_none(),
        "an unqualified curated root cannot use offset zero to bypass source ambiguity"
    );
    assert_no_numbering_decisions(&state).await;
    unset_nyaa_base();
}

#[tokio::test]
async fn automatic_numbering_compat_rss_saved_human_owner_missing_never_falls_back() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    set_nyaa_base(&server.uri());
    let state = build_state().await;
    cached_lagoon(&state, false).await;
    seed_series_with_cache(&state, 1519, "Black Lagoon: The Second Barrage").await;
    let client = install_recording_default_torrent_client(&state).await;
    let domain = ryokan::services::source_numbering::load_domain(&state.db, 889)
        .await
        .unwrap();
    let files = vec!["[Fixture] Black Lagoon - 01 [1080p].mkv".into()];
    let human = ryokan::services::source_numbering::prepare_review(domain, HASH, &files, 889)
        .unwrap()
        .confirm(&files)
        .unwrap();
    let saved = ryokan::models::source_numbering::save_reviewed(&state.db, &human)
        .await
        .unwrap();
    let feed = format!(
        "<rss version=\"2.0\" xmlns:nyaa=\"https://nyaa.si/xmlns/nyaa\"><channel><item><title>[Fixture] Black Lagoon: The Second Barrage - 03 [1080p WEB-DL AAC]</title><guid>known-human-owner-missing</guid><link>{}/view/990004</link><nyaa:magneturi>magnet:?xt=urn:btih:{HASH}</nyaa:magneturi><nyaa:infohash>{HASH}</nyaa:infohash><nyaa:seeders>20</nyaa:seeders></item></channel></rss>",
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/known-human-feed"))
        .respond_with(ResponseTemplate::new(200).set_body_string(feed))
        .mount(&server)
        .await;
    let feed_url = format!("{}/known-human-feed", server.uri());
    ryokan::models::direct_rss_feeds::insert(
        &state.db,
        ryokan::models::direct_rss_feeds::DirectRssFeedForm {
            name: "Known human source",
            url: &feed_url,
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        },
    )
    .await
    .unwrap();
    let mut cfg = default_config();
    cfg.rss_master_enabled = true;
    cfg.rss_enabled = false;
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    let result = ryokan::services::rss::sync_once(&state, "scheduled")
        .await
        .unwrap();
    assert_eq!(result.items_seen, 1);
    assert_eq!(result.grabbed, 0);
    assert!(
        client.add_calls().is_empty(),
        "known source ownership must hold before any add"
    );
    let reasons: Vec<String> =
        sqlx::query_scalar("SELECT reason FROM rss_seen WHERE decision='rejected'")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("no tracked owner")),
        "{reasons:?}"
    );
    let unchanged = ryokan::models::source_numbering::lookup_exact(&state.db, HASH, 889)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.fingerprint, saved.fingerprint);
    assert_eq!(unchanged.proof, human);
    let grabs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(grabs, 0);
    unset_nyaa_base();
}

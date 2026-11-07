//! Wiremock coverage for `services::jikan`. Inline tests already pin
//! the small pure helpers (URL parsing, label normalization, etc.); a
//! handful of network-bound endpoints make up the rest of the surface.
//! This file fills them in via a wiremock server pointed at by
//! `JIKAN_API_BASE`.
//!
//! Same env-var serializer pattern as `tests/external_sync_e2e.rs` and
//! `tests/metadata_sync_e2e.rs` — each integration test crate is its
//! own process, so cross-binary races are impossible; within-binary
//! tests share an `ENV_LOCK` mutex so the env-var write doesn't race.
//!
//! What's covered:
//!   * `search_anime(query)` — happy 200 + a mapped `AnimeEntry`,
//!     score round-trip from MAL's 0-10 scale to AL's 0-100.
//!   * `search_anime` empty-query short-circuit (no HTTP call).
//!   * `search_anime` 429 → cooldown set + Err.
//!   * `get_anime_detail(mal_id)` — `/anime/{id}/full` happy path.
//!   * `get_anime_detail` 5xx → Err.
//!   * `get_anime_detail_cached` cache hit short-circuits a second
//!     network request.

use ryokan::services::jikan;
use serde_json::json;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn episode_detail(id: i64, mal_id: i64, episodes: i32) -> ryokan::services::anilist::AnimeDetail {
    serde_json::from_value(json!({
        "id": id, "id_mal": mal_id, "episodes": episodes,
        "title_romaji": "Test", "title_english": "Test", "title_native": "",
        "cover_url": "", "banner_url": "", "format": "TV", "status": "FINISHED",
        "status_display": "Finished", "season": "", "description": "", "genres": [],
        "score_is_ten_point": false, "score_class": "", "synonyms": [],
        "streaming_episodes": [], "relations": []
    }))
    .unwrap()
}

#[tokio::test]
async fn split_entry_without_an_explicit_map_does_not_borrow_its_siblings_numbering() {
    let _gate = ENV_LOCK.lock().await;
    ryokan::services::anibridge::seed_mapping_data_for_tests(&json!({
        "mal:61469": {"anilist:190327": {"1": "1"}}
    }))
    .await;
    let db = ryokan::test_support::in_memory_pool().await;
    let result =
        jikan::fetch_episode_titles_with_mapping(&db, &episode_detail(210482, 61469, 11)).await;
    assert!(
        result
            .mapping_error
            .unwrap()
            .contains("none for AniList 210482")
    );
    assert!(result.episodes.is_empty());
    assert!(!result.allow_kitsu_fallback);
}

#[tokio::test]
async fn fallback_provider_id_does_not_select_an_unrelated_anilist_map() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    ryokan::services::anibridge::seed_mapping_data_for_tests(&json!({
        "mal:701": {"anilist:700": {"1-3": "1-3"}},
        "mal:801": {"anilist:800": {"1-3": "1-3"}}
    }))
    .await;
    let db = ryokan::test_support::in_memory_pool().await;
    for number in 1..=3 {
        sqlx::query("INSERT INTO episode_cache (mal_id, episode_number, title) VALUES (701, ?, 'Correct entry'), (801, ?, 'Wrong entry')")
            .bind(number).bind(number).execute(&db).await.unwrap();
    }
    // The fallback detail's id is Kitsu 800, not AniList 800. The tracked
    // identity remains AniList 700, associated with MAL 701.
    let detail = episode_detail(800, 701, 3);
    let result = jikan::fetch_episode_titles_for_entry(&db, 700, &detail).await;
    assert!(result.mapping_error.is_none());
    assert_eq!(result.episodes.len(), 3);
    assert!(
        result
            .episodes
            .values()
            .all(|episode| episode.title == "Correct entry")
    );
    // A provider with different numbering cannot also supply the canonical
    // entry's count/order during an AniList outage.
    ryokan::services::anibridge::seed_mapping_data_for_tests(&json!({
        "mal:701": {"anilist:700": {"1-3": "2-4"}}
    }))
    .await;
    let blocked = jikan::fetch_episode_titles_for_entry(&db, 700, &detail).await;
    assert!(blocked.mapping_error.is_some());
    assert!(blocked.episodes.is_empty());
    assert!(!blocked.allow_kitsu_fallback);
}

#[tokio::test]
async fn short_provider_list_never_borrows_a_sequel_title() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    ryokan::services::anibridge::seed_mapping_data_for_tests(&json!({})).await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/123/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"mal_id": 1, "title": "First Entry"}]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/123/relations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "relation": "Sequel", "entry": [{"mal_id": 124, "type": "anime", "name": "Sequel"}]
        }]})))
        .expect(0)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/124/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"mal_id": 1, "title": "Wrong Entry"}]
        })))
        .expect(0)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }
    let db = ryokan::test_support::in_memory_pool().await;
    let episodes = jikan::fetch_episode_titles_for_detail(&db, &episode_detail(999, 123, 2)).await;
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[&1].title, "First Entry");
    assert_eq!(
        episode_cache_rows(&db, 123).await,
        vec![(1, "First Entry".into())]
    );
    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
}

#[tokio::test]
async fn mapped_titles_keep_provider_identity_and_survive_cache_reload() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    // Direct mappings read from AniBridge's published dataset on 2026-11-06.
    // The application uses the same parser for every title; these IDs occur
    // only in this regression fixture.
    let mappings = json!({
        "mal:22297": {"anilist:19603": {"1-12": "2-13"}},
        "mal:27821": {"anilist:19603": {"1": "1"}}
    });
    ryokan::services::anibridge::seed_mapping_data_for_tests(&mappings).await;
    let mock = MockServer::start().await;
    let data: Vec<_> = (1..=12)
        .map(|n| {
            json!({
                "mal_id": n, "title": format!("Regular {n}"), "aired": "2014-10-11"
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/anime/22297/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": data})))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/27821/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"mal_id": 1, "title": "Prologue", "aired": "2014-10-04"}
        ]})))
        .expect(1)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }
    let db = ryokan::test_support::in_memory_pool().await;
    for _ in 0..2 {
        let result =
            jikan::fetch_episode_titles_with_mapping(&db, &episode_detail(19603, 22297, 13)).await;
        assert!(!result.allow_kitsu_fallback);
        assert_eq!(result.episodes.len(), 13);
        assert_eq!(result.episodes[&1].title, "Prologue");
        assert_eq!(result.episodes[&1].aired, "2014-10-04");
        assert_eq!(result.episodes[&2].title, "Regular 1");
        assert_eq!(result.episodes[&13].title, "Regular 12");
        // Drop process caches, then reuse only database rows and parsed maps.
        jikan::reset_state_for_tests().await;
        ryokan::services::anibridge::clear_cache_for_tests().await;
        ryokan::services::anibridge::seed_mapping_data_for_tests(&mappings).await;
    }
    let raw = jikan::fetch_episode_titles(&db, 22297).await;
    assert_eq!(raw.len(), 12);
    assert_eq!(raw[&1].title, "Regular 1");
    assert!(!raw.contains_key(&13));
    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
}

#[tokio::test]
async fn missing_mapped_episode_stays_empty_and_blocks_unmapped_fallback() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    ryokan::services::anibridge::seed_mapping_data_for_tests(&json!({
        "mal:901": {"anilist:902": {"1-2": "2-3"}}
    }))
    .await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/901/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"mal_id": 1, "title": "Second Video"}
        ]})))
        .expect(1)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }
    let db = ryokan::test_support::in_memory_pool().await;
    let result = jikan::fetch_episode_titles_with_mapping(&db, &episode_detail(902, 901, 3)).await;
    assert!(!result.allow_kitsu_fallback);
    assert_eq!(result.episodes.len(), 1);
    assert_eq!(result.episodes[&2].title, "Second Video");
    assert!(!result.episodes.contains_key(&1));
    assert!(!result.episodes.contains_key(&3));
    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
}

/// Minimal Jikan v4 search response — only the fields the parser
/// reads. Matches `services::jikan::AnimeSearchItem` shape.
fn search_response(mal_id: i64, title: &str, score: f64) -> serde_json::Value {
    json!({
        "data": [{
            "mal_id": mal_id,
            "title": title,
            "title_english": title,
            "title_japanese": null,
            "type": "TV",
            "status": "Finished Airing",
            "episodes": 12,
            "score": score,
            "year": 2020,
            "images": {
                "jpg": {
                    "image_url": "https://example/jpg.jpg",
                    "small_image_url": "https://example/small.jpg",
                    "large_image_url": "https://example/large.jpg",
                }
            }
        }]
    })
}

/// Minimal Jikan v4 `/anime/{id}/full` response.
fn full_response(mal_id: i64, title: &str) -> serde_json::Value {
    json!({
        "data": {
            "mal_id": mal_id,
            "title": title,
            "title_english": title,
            "title_japanese": null,
            "type": "TV",
            "status": "Finished Airing",
            "episodes": 12,
            "score": 8.5,
            "year": 2020,
            "season": "spring",
            "duration": "24 min per ep",
            "images": {
                "jpg": {
                    "image_url": "https://example/jpg.jpg",
                    "small_image_url": "https://example/small.jpg",
                    "large_image_url": "https://example/large.jpg",
                }
            },
            "synopsis": "A test synopsis.",
            "background": null,
            "trailer": null,
            "aired": { "from": "2020-04-01T00:00:00+00:00", "to": "2020-06-21T00:00:00+00:00" },
            "genres": [{"name": "Action"}],
            "themes": [],
            "demographics": [],
            "relations": []
        }
    })
}

#[tokio::test]
async fn search_anime_returns_mapped_entries_on_200() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .and(query_param("q", "naruto"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(20, "Naruto", 7.95)))
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let results = jikan::search_anime("naruto")
        .await
        .expect("search should succeed");
    assert_eq!(results.len(), 1);
    let entry = &results[0];
    // MAL id is stored as a NEGATIVE id_anilist sentinel for
    // Jikan-fallback entries — guards against the AL-keyed query
    // paths accidentally pulling them in. CLAUDE.md: "When a series
    // is added via the Jikan fallback ... `series.anilist_id =
    // -mal_id`."
    assert_eq!(entry.id, -20);
    assert_eq!(entry.id_mal, Some(20));
    assert_eq!(entry.title_english, "Naruto");
    assert_eq!(entry.format, "TV");
    // 7.95 × 10 → round → 80 (AL's 0-100 scale).
    assert_eq!(entry.average_score, Some(80));
    // Source label distinguishes Jikan-sourced rows in callers.
    assert_eq!(entry.source, "mal");

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn search_anime_empty_query_returns_empty_without_http_call() {
    // No mock; if the handler tried to make a request it would
    // resolve to whatever JIKAN_API_BASE is set to (could leak from
    // another test) and probably fail. The `if query.is_empty()`
    // guard at the top must short-circuit before the URL is built.
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;

    let results = jikan::search_anime("   ").await.expect("ok empty");
    assert!(results.is_empty());

    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn search_anime_429_sets_cooldown_and_subsequent_call_short_circuits() {
    // Pin the rate-limit cascade guard: after a 429, the next call
    // must not re-hit Jikan. CLAUDE.md: "Prevents the 'AL 429 →
    // Jikan 429' cascade that drains Jikan's budget whenever
    // AniList enters its cooldown window."
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .and(query_param("q", "throttle"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_string("rate limited"),
        )
        .expect(1) // exactly one — the second call short-circuits
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let first = jikan::search_anime("throttle").await;
    assert!(first.is_err(), "429 must surface as Err");

    let second = jikan::search_anime("throttle").await;
    let err = second.expect_err("cooldown must short-circuit");
    assert!(
        err.to_lowercase().contains("rate-limited"),
        "expected rate-limit message, got: {err}"
    );

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn get_anime_detail_returns_animedetail_on_200() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/1234/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(full_response(1234, "Detail Show")))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let detail = jikan::get_anime_detail(1234).await.expect("ok");
    assert_eq!(detail.id, -1234, "Jikan path uses the negated-id sentinel");
    assert_eq!(detail.id_mal, Some(1234));
    assert_eq!(detail.title_english, "Detail Show");
    assert_eq!(detail.format, "TV");
    assert_eq!(detail.episodes, Some(12));
    // 8.5 → round → 9 (AL's 0-100 column on `AnimeDetail` stores
    // the rounded MAL score, distinct from search_anime's ×10
    // mapping). The struct's `score_is_ten_point` flag flips true
    // so renderers downstream pick the right divisor.
    assert_eq!(detail.average_score, Some(9));
    assert!(detail.score_is_ten_point);
    assert_eq!(detail.season, "SPRING");
    assert_eq!(detail.season_year, Some(2020));
    assert_eq!(detail.end_year, Some(2020));

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn get_anime_detail_returns_err_on_5xx() {
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/9999/full"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let result = jikan::get_anime_detail(9999).await;
    assert!(result.is_err(), "503 must surface as Err");

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn get_anime_detail_cached_short_circuits_second_call() {
    // Pin the in-memory cache: the second get_anime_detail_cached
    // call must hit the cache and not the wiremock. We assert this
    // by `expect(1)` on the mock — wiremock fails the test if a
    // second request lands.
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/777/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(full_response(777, "Cached")))
        .expect(1)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let first = jikan::get_anime_detail_cached(777).await.expect("first");
    assert_eq!(first.title_english, "Cached");
    let second = jikan::get_anime_detail_cached(777).await.expect("second");
    // Same payload — proves the cache served it.
    assert_eq!(second.id_mal, Some(777));

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

// ── episode cache: failures are not negative-cached (#235) ────────

use ryokan::test_support::in_memory_pool;

async fn episode_cache_rows(db: &sqlx::SqlitePool, mal_id: i64) -> Vec<(i32, String)> {
    sqlx::query_as("SELECT episode_number, title FROM episode_cache WHERE mal_id = ? ORDER BY 1")
        .bind(mal_id)
        .fetch_all(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn fetch_episode_titles_does_not_cache_a_failed_fetch() {
    // A 5xx used to be cached as the 7-day `__RYOKAN_EMPTY__` sentinel,
    // which handed the series to the Kitsu title fallback until the
    // TTL lapsed (#235). The failure must leave no row so the next
    // call asks again.
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/123/episodes"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    let eps = jikan::fetch_episode_titles(&db, 123).await;
    assert!(eps.is_empty());
    assert!(
        episode_cache_rows(&db, 123).await.is_empty(),
        "a failed fetch must not write the negative-cache sentinel"
    );
    // ...and it must say so where the user looks (System → Logs), not
    // only on the console.
    let logged: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT level, message, detail FROM logs WHERE category = 'jikan' ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert_eq!(logged[0].0, "warn");
    assert_eq!(logged[0].1, "Episode list fetch failed for MAL 123");
    assert!(logged[0].2.contains("503"), "{}", logged[0].2);

    // Tenrai recovers: the next call fetches and caches for real.
    mock.reset().await;
    Mock::given(method("GET"))
        .and(path("/anime/123/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "mal_id": 1, "title": "Back Online", "aired": "2024-01-01T00:00:00+00:00" }]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let eps = jikan::fetch_episode_titles(&db, 123).await;
    assert_eq!(eps.get(&1).map(|e| e.title.as_str()), Some("Back Online"));
    assert_eq!(
        episode_cache_rows(&db, 123).await,
        vec![(1, "Back Online".to_string())]
    );

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn fetch_episode_titles_still_negative_caches_a_real_empty_list() {
    // The sentinel keeps its job for the case it was built for: MAL
    // has no episode list. One request, then the cache answers.
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/124/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .expect(1)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    assert!(jikan::fetch_episode_titles(&db, 124).await.is_empty());
    assert_eq!(
        episode_cache_rows(&db, 124).await,
        vec![(0, "__RYOKAN_EMPTY__".to_string())]
    );
    assert!(jikan::fetch_episode_titles(&db, 124).await.is_empty());

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

#[tokio::test]
async fn fetch_episode_titles_skips_the_request_during_cooldown_without_caching() {
    // A 429 elsewhere sets the process-wide cooldown; the episode
    // fetch honors it like `search_anime` does, and the skipped call
    // leaves no sentinel behind.
    let _gate = ENV_LOCK.lock().await;
    jikan::reset_state_for_tests().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limit"))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/125/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "mal_id": 1, "title": "Unreached", "aired": null }]
        })))
        .expect(0)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    jikan::search_anime("anything")
        .await
        .expect_err("429 must surface as Err and arm the cooldown");

    assert!(jikan::fetch_episode_titles(&db, 125).await.is_empty());
    assert!(episode_cache_rows(&db, 125).await.is_empty());
    // The cooldown skip stays off System → Logs: the 429 that armed it
    // was the event, one line per series for the rest of the window is
    // noise.
    let jikan_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE category = 'jikan'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(jikan_rows, 0);

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    jikan::reset_state_for_tests().await;
}

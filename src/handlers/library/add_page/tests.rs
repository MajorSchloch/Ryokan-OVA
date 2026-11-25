use super::*;
use crate::{
    models::monitoring,
    test_support::{build_test_app_state, in_memory_pool, seed_series},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, header},
};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, body_string_contains, method},
};

const NEW: i64 = 910_021;
const TRACKED: i64 = 910_022;
const OTHER: i64 = 910_023;
const UPCOMING: i64 = 910_024;
const QUERY: &str = "Fixture choices";
const ORIGINAL: &str = "Ada Orbit";
const SEQUEL: &str = "Ada Orbit Season 2";
const UNRELATED: &str = "Separate fixture";
const SHARED_MAL_ID: i64 = 610_024;

fn provider_entry(id: i64, title: &str) -> Value {
    json!({
        "id": id,
        "title": {"english": title, "romaji": title},
        "type": "ANIME",
        "format": "TV",
        "status": "FINISHED",
        "episodes": 2,
        "seasonYear": 2022
    })
}

async fn cache(db: &SqlitePool, id: i64, title: &str) {
    let detail = anilist::parse_media_node(&provider_entry(id, title)).unwrap();
    metadata_cache::upsert_provider(db, id, None, &detail)
        .await
        .unwrap();
}

async fn request(state: &AppState, uri: &str, form: Option<&str>) -> Response {
    let request = Request::builder().uri(uri);
    let request = if let Some(form) = form {
        request
            .method("POST")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_owned()))
    } else {
        request.body(Body::empty())
    }
    .unwrap();
    detail::routes()
        .with_state(state.clone())
        .oneshot(request)
        .await
        .unwrap()
}

async fn html(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn add_links(page: &str, selector: &str) -> Vec<reqwest::Url> {
    let document = scraper::Html::parse_document(page);
    let links = scraper::Selector::parse(selector).unwrap();
    let base = reqwest::Url::parse("http://localhost").unwrap();
    document
        .select(&links)
        .map(|link| base.join(link.value().attr("href").unwrap()).unwrap())
        .collect()
}

fn parameter<'a>(link: &'a reqwest::Url, name: &str) -> Option<std::borrow::Cow<'a, str>> {
    link.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn link_id(link: &reqwest::Url, name: &str) -> Option<i64> {
    parameter(link, name).map(|id| id.parse().unwrap())
}

fn request_uri(link: &reqwest::Url) -> String {
    format!("{}?{}", link.path(), link.query().unwrap())
}

async fn assert_no_additions(db: &SqlitePool) {
    for table in [
        "series",
        "episode_monitor_state",
        "release_follows",
        "grabbed_torrents",
    ] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(db)
                .await
                .unwrap();
        assert_eq!(count, 0, "unexpected rows in {table}");
    }
}

#[tokio::test]
async fn setup_from_cached_provider_does_not_add_or_monitor() {
    let db = in_memory_pool().await;
    cache(&db, NEW, "New fixture").await;
    let state = build_test_app_state(db.clone(), None);

    let page = html(
        request(
            &state,
            &format!("/library/add?provider_id={NEW}&q=Fixture%20choices&source=al"),
            None,
        )
        .await,
    )
    .await;

    assert!(page.contains("Set up release"));
    assert!(page.contains("New fixture"));
    assert!(page.contains(&format!("href=\"/anime/anilist/{NEW}\"")));
    assert!(page.contains("name=\"monitor_mode\""));
    assert!(page.contains("Search after adding"));
    assert!(page.contains("Back to search"));
    assert_no_additions(&db).await;
}

#[tokio::test]
async fn save_existing_release_redirects_without_changing_monitoring() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, TRACKED, "Tracked fixture").await;
    series::update_monitor_mode_with_override(&db, id, "future", true)
        .await
        .unwrap();
    monitoring::replace_series_states(
        &db,
        id,
        &[monitoring::EpisodeMonitorState {
            episode_number: 2,
            monitored: true,
        }],
    )
    .await
    .unwrap();
    let state = build_test_app_state(db.clone(), None);
    let form =
        format!("provider_id={TRACKED}&q=Fixture&source=al&monitor_mode=all&search_now=true");

    let response = request(&state, "/library/add", Some(&form)).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], detail::path(TRACKED));
    let row = series::get_by_id(&db, id).await.unwrap().unwrap();
    assert_eq!(row.monitor_mode_enum(), MonitorMode::Future);
    assert!(row.monitor_mode_manual_override);
    assert_eq!(
        monitoring::get_monitored_episode_numbers(&db, id)
            .await
            .unwrap(),
        [2]
    );
    assert_eq!(crate::test_support::count_series(&db).await, 1);
}

#[tokio::test]
async fn invalid_monitoring_mode_does_not_add_the_release() {
    let db = in_memory_pool().await;
    cache(&db, NEW, "New fixture").await;
    let state = build_test_app_state(db.clone(), None);
    let form = format!("provider_id={NEW}&q=Fixture&source=al&monitor_mode=invalid");

    let response = request(&state, "/library/add", Some(&form)).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_no_additions(&db).await;
}

#[tokio::test]
async fn discover_keeps_search_available_without_a_calendar_snapshot() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let page = html(request(&state, "/discover", None).await).await;
    assert!(page.contains("Upcoming anime"));
    assert!(page.contains("No dated premieres are available"));
    assert!(page.contains("action=\"/discover\" method=\"get\""));
    assert_no_additions(&db).await;
}

#[tokio::test]
async fn orders_releases_across_formats_and_keeps_unknown_watch_positions_separate() {
    let db = in_memory_pool().await;
    let root = provider_entry(TRACKED, ORIGINAL);
    let mut film = provider_entry(OTHER, "Ada Orbit Compilation");
    film["format"] = json!("MOVIE");
    film["startDate"] = json!({"year":2023,"month":4});
    film["relations"]["edges"] = json!([{"relationType":"PARENT","node":root.clone()}]);
    let mut prequel = provider_entry(NEW, "Ada Orbit Origins");
    prequel["startDate"] = json!({"year":2024,"month":2,"day":1});
    prequel["relations"]["edges"] = json!([{"relationType":"SEQUEL","node":root.clone()}]);
    for item in [root, film, prequel] {
        let detail = anilist::parse_media_node(&item).unwrap();
        metadata_cache::upsert_provider(&db, detail.id, None, &detail)
            .await
            .unwrap();
    }
    let state = build_test_app_state(db, None);
    let path = format!("/discover?franchise_id={TRACKED}");
    let premiere = html(request(&state, &format!("{path}&order=premiere"), None).await).await;
    let release_ids = |page: &str, selector: &str| {
        add_links(page, selector)
            .iter()
            .map(|url| link_id(url, "provider_id").unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        release_ids(&premiere, ".add-anime-results .anilist-title-link"),
        [TRACKED, OTHER, NEW]
    );
    assert!(premiere.contains("Related to <a"));
    assert!(premiere.contains("Premiere · April 2023 (day TBA)"));

    let watch = html(request(&state, &format!("{path}&order=watch"), None).await).await;
    assert_eq!(
        release_ids(
            &watch,
            "[aria-labelledby='discover-releases-watch'] .anilist-title-link"
        ),
        [NEW, TRACKED]
    );
    assert_eq!(
        release_ids(
            &watch,
            "[aria-labelledby='discover-releases-other'] .anilist-title-link"
        ),
        [OTHER]
    );
    let selection =
        html(request(&state, &format!("{path}&order=watch&select=true"), None).await).await;
    assert!(selection.contains("name=\"order\" value=\"watch\""));
    assert!(selection.contains("No known watch position"));
    assert!(!selection.contains("aria-label=\"Release order\""));

    let mut branch = provider_entry(UPCOMING, "Ada Orbit Branch");
    branch["relations"]["edges"] = json!([{
        "relationType": "PREQUEL", "node": provider_entry(NEW, "Ada Orbit Origins")
    }]);
    let detail = anilist::parse_media_node(&branch).unwrap();
    metadata_cache::upsert_provider(&state.db, detail.id, None, &detail)
        .await
        .unwrap();
    let ambiguous = html(request(&state, &format!("{path}&order=watch"), None).await).await;
    assert!(ambiguous.contains("A single watch sequence is unavailable."));
    assert!(!ambiguous.contains("Known watch sequence"));
    assert_eq!(
        release_ids(&ambiguous, ".add-anime-results .anilist-title-link").len(),
        4
    );
}

#[tokio::test]
async fn cold_search_groups_releases_and_saves_the_selected_mode() {
    let server = MockServer::start().await;
    let mut original = provider_entry(TRACKED, ORIGINAL);
    original["title"]["romaji"] = json!("Ada no Kidou");
    original["averageScore"] = json!(86);
    original["synonyms"] = json!(["Orbit of Ada", " orbit of ada ", ORIGINAL]);
    original["description"] = json!(
        "<p>The <em>original story</em> begins here.</p><script>unsafe_fixture_markup</script>"
    );
    let mut sequel = provider_entry(NEW, SEQUEL);
    sequel["description"] = json!("The sequel's own synopsis.");
    sequel["averageScore"] = json!(81);
    sequel["seasonYear"] = json!(2023);
    sequel["idMal"] = json!(SHARED_MAL_ID);
    let mut unrelated = provider_entry(OTHER, UNRELATED);
    unrelated["idMal"] = json!(SHARED_MAL_ID);
    sequel["relations"] = json!({
        "edges": [{"relationType": "PREQUEL", "node": original.clone()}]
    });

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables": {"search": QUERY}})))
        .and(body_string_contains("relations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"Page": {"media": [sequel.clone(), original.clone(), unrelated.clone()]}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    for (id, detail) in [(NEW, sequel), (TRACKED, original), (OTHER, unrelated)] {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"variables": {"id": id}})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"Media": detail}})),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables": {"id": UPCOMING}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"Media": null}})))
        .expect(1)
        .mount(&server)
        .await;

    // A subprocess keeps the provider overrides local to this fixture.
    // The add endpoint starts metadata refreshes in background tasks.
    let (_, module) = module_path!().split_once("::").unwrap();
    let test = format!("{module}::mocked_provider_results_and_save");
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", &test, "--nocapture"])
        .env("RYOKAN_ANILIST_API_BASE", server.uri())
        .env("JIKAN_API_BASE", server.uri())
        .env("RYOKAN_KITSU_API_BASE", server.uri())
        .env("RYOKAN_ANIBRIDGE_MAPPINGS_URL", server.uri())
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[ignore = "run by cold_search_groups_releases_and_saves_the_selected_mode"]
#[tokio::test]
async fn mocked_provider_results_and_save() {
    let db = in_memory_pool().await;
    sqlx::query("UPDATE library_path_policy SET enabled=1 WHERE id=1")
        .execute(&db)
        .await
        .unwrap();
    let tracked = seed_series(&db, TRACKED, ORIGINAL).await;
    sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
        .bind(format!("{ORIGINAL}/{ORIGINAL}"))
        .bind(tracked)
        .execute(&db)
        .await
        .unwrap();
    let state = build_test_app_state(db.clone(), None);

    let entries = json!([{
        "entry": {
            "id": UPCOMING, "title_english": "Tomorrow's Film", "title_romaji": "Ashita no Eiga",
            "title_native": "", "cover_url": "", "format": "MOVIE", "status": "NOT_YET_RELEASED",
            "status_display": "Not yet released", "source": "anilist", "average_score": 79
        },
        "date": chrono::Utc::now().date_naive().succ_opt().unwrap().to_string(), "season": ""
    }]);
    sqlx::query(
        "INSERT INTO premiere_discovery(id,entries,refreshed_at,failed) VALUES(1,?,unixepoch(),1)",
    )
    .bind(entries.to_string())
    .execute(&db)
    .await
    .unwrap();
    let landing = html(request(&state, "/discover", None).await).await;
    let link = add_links(
        &landing,
        "[aria-labelledby='discover-upcoming-title'] a.discover-poster",
    );
    assert_eq!(link.len(), 1);
    assert!(landing.contains("79%</span>"));
    assert_eq!(link_id(&link[0], "franchise_id"), Some(UPCOMING));
    let saved_preview = html(request(&state, &request_uri(&link[0]), None).await).await;
    let saved_doc = scraper::Html::parse_document(&saved_preview);
    let heading = scraper::Selector::parse("#add-anime-title").unwrap();
    assert_eq!(
        saved_doc
            .select(&heading)
            .next()
            .unwrap()
            .text()
            .collect::<String>(),
        "Tomorrow's Film"
    );
    assert_eq!(
        link_id(
            &add_links(&saved_preview, "a.btn[href^='/library/add?']")[0],
            "provider_id"
        ),
        Some(UPCOMING)
    );
    assert_eq!(crate::test_support::count_series(&db).await, 1);

    let results =
        html(request(&state, "/discover?q=Fixture%20choices&source=al", None).await).await;
    let result_links = add_links(
        &results,
        ".discover-poster-grid[aria-labelledby='discover-results-title'] a.discover-poster[href^='/discover?']",
    );
    let franchise_links: Vec<_> = result_links
        .iter()
        .filter(|link| link_id(link, "franchise_id").is_some())
        .collect();
    assert_eq!(
        franchise_links
            .iter()
            .filter_map(|link| link_id(link, "franchise_id"))
            .collect::<Vec<_>>(),
        [NEW, OTHER]
    );
    assert!(
        result_links
            .iter()
            .all(|link| link_id(link, "provider_id").is_none())
    );
    assert!(results.contains(ORIGINAL));
    assert!(results.contains(UNRELATED));
    assert!(results.contains("86%</span>"));
    assert!(!results.contains("81%</span>"));
    let franchise_link = franchise_links[0];
    assert_eq!(parameter(franchise_link, "q").as_deref(), Some(QUERY));
    assert_eq!(parameter(franchise_link, "source").as_deref(), Some("al"));

    let franchise = html(request(&state, &request_uri(franchise_link), None).await).await;
    assert!(franchise.contains(&format!("<h2 id=\"add-anime-title\">{ORIGINAL}</h2>")));
    assert!(franchise.contains("The <em>original story</em> begins here."));
    assert!(!franchise.contains("unsafe_fixture_markup"));
    assert!(!franchise.contains("The sequel's own synopsis"));
    let franchise_doc = scraper::Html::parse_document(&franchise);
    let aliases = scraper::Selector::parse(".discover-alternate-titles li").unwrap();
    assert_eq!(
        franchise_doc
            .select(&aliases)
            .map(|alias| alias.text().collect::<String>())
            .collect::<Vec<_>>(),
        ["Ada no Kidou", "Orbit of Ada"]
    );
    assert!(franchise.contains(SEQUEL));
    assert!(franchise.contains("81%</span>"));
    assert!(franchise.contains(&format!(
        "Sequel to <a href=\"#discover-release-{TRACKED}\">{ORIGINAL}"
    )));
    assert!(!franchise.contains(UNRELATED));
    assert!(franchise.contains("In library"));
    assert!(franchise.contains("Open release"));
    assert!(franchise.contains(&format!("href=\"/anime/anilist/{TRACKED}\"")));
    let setup_links: Vec<_> = add_links(&franchise, "a.btn[href^='/library/add?']")
        .into_iter()
        .filter(|link| link_id(link, "provider_id").is_some())
        .collect();
    assert_eq!(setup_links.len(), 1);
    let setup_link = &setup_links[0];
    assert_eq!(link_id(setup_link, "provider_id"), Some(NEW));
    assert_eq!(link_id(setup_link, "franchise_id"), Some(NEW));
    assert_eq!(parameter(setup_link, "q").as_deref(), Some(QUERY));
    assert_eq!(parameter(setup_link, "source").as_deref(), Some("al"));

    let setup = html(request(&state, &request_uri(setup_link), None).await).await;
    assert!(setup.contains("Set up release"));
    assert!(setup.contains(SEQUEL));
    let back_links = add_links(&setup, "a[href^='/discover?']");
    assert!(back_links.iter().any(|link| link == franchise_link));
    assert_eq!(crate::test_support::count_series(&db).await, 1);

    for (provider, requested, expected) in [
        (NEW, "none", MonitorMode::None),
        (OTHER, "future", MonitorMode::Future),
    ] {
        let form = format!("provider_id={provider}&q=Fixture&source=al&monitor_mode={requested}");
        let response = request(&state, "/library/add", Some(&form)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let row = series::get_by_anilist_id(&db, provider)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.headers()[header::LOCATION],
            format!("/series/{}", row.id)
        );
        assert_eq!(row.monitor_mode_enum(), expected);
        assert!(row.monitor_mode_manual_override);
        if provider == NEW {
            assert_eq!(row.folder_name, format!("{ORIGINAL}/{SEQUEL}"));
        }
        let states = monitoring::get_series_states(&db, row.id).await.unwrap();
        assert_eq!(states.len(), 2);
        assert!(states.iter().all(|state| !state.monitored));
    }
    assert_eq!(crate::test_support::count_series(&db).await, 3);
}

#[tokio::test]
async fn split_mal_id_requires_exact_confirmation_to_open_legacy_franchise() {
    let db = in_memory_pool().await;
    let legacy = -SHARED_MAL_ID;
    let series_id = seed_series(&db, legacy, ORIGINAL).await;
    sqlx::query("UPDATE series SET mal_id=? WHERE id=?")
        .bind(SHARED_MAL_ID)
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO discovery_media_ids (anilist_id, mal_id) VALUES (?, ?)")
        .bind(OTHER)
        .bind(SHARED_MAL_ID)
        .execute(&db)
        .await
        .unwrap();

    let mut original = provider_entry(TRACKED, ORIGINAL);
    original["idMal"] = json!(SHARED_MAL_ID);
    let mut sequel = provider_entry(NEW, SEQUEL);
    sequel["seasonYear"] = json!(2023);
    original["relations"] = json!({
        "edges": [{"relationType": "SEQUEL", "node": sequel}]
    });
    let original = anilist::parse_media_node(&original).unwrap();
    metadata_cache::upsert_provider(&db, TRACKED, Some(SHARED_MAL_ID), &original)
        .await
        .unwrap();
    let state = build_test_app_state(db.clone(), None);
    let view = AddPage::new(&config::Config::default(), String::new(), "al", None);

    let group = discovery::open(&state, &view, TRACKED).await.unwrap();
    assert_eq!(group.title, ORIGINAL);
    let releases = &group.sections[0].releases;
    assert_eq!(releases.len(), 2);
    assert!(releases.iter().all(|release| !release.row.existing));
    assert!(
        releases[0]
            .row
            .href
            .ends_with(&format!("&provider_id={TRACKED}"))
    );

    metadata_cache::upsert(&db, series_id, TRACKED, Some(SHARED_MAL_ID), &original)
        .await
        .unwrap();
    for seed in [TRACKED, legacy] {
        let group = discovery::open(&state, &view, seed).await.unwrap();
        assert_eq!(group.title, ORIGINAL);
        let releases = &group.sections[0].releases;
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].row.title, ORIGINAL);
        assert!(releases[0].row.existing);
        assert_eq!(releases[0].row.href, detail::path(legacy));
        assert_eq!(releases[1].row.title, SEQUEL);
        assert!(!releases[1].row.existing);
    }
    let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    assert_eq!(row.anilist_id, legacy);
}

#[tokio::test]
async fn concurrent_confirmation_pins_only_the_created_releases_monitoring() {
    let db = in_memory_pool().await;
    let core = || series::SeriesCore {
        anilist_id: NEW,
        mal_id: None,
        title: "One release",
        title_romaji: "One release",
        title_english: "One release",
        title_native: "",
        cover_url: "",
        format: "TV",
        status: "FINISHED",
        episodes: Some(2),
        season_year: None,
        end_year: None,
    };
    let (all, none) = tokio::join!(
        series::add_with_monitoring(&db, core(), false, MonitorMode::All, None),
        series::add_with_monitoring(&db, core(), false, MonitorMode::None, None),
    );
    let (all, none) = (all.unwrap(), none.unwrap());
    assert_eq!(all.0, none.0);
    assert_ne!(all.1, none.1);
    let row = series::get_by_id(&db, all.0).await.unwrap().unwrap();
    assert!(row.monitor_mode_manual_override);
    assert_eq!(
        row.monitor_mode_enum(),
        if all.1 {
            MonitorMode::All
        } else {
            MonitorMode::None
        }
    );
}

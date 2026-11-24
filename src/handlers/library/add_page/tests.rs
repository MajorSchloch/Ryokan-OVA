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
    matchers::{body_partial_json, method},
};

const NEW: i64 = 910_021;
const TRACKED: i64 = 910_022;
const OTHER: i64 = 910_023;
const QUERY: &str = "Fixture choices";

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
async fn results_distinguish_tracked_titles_and_save_the_selected_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables": {"search": QUERY}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"Page": {"media": [
                provider_entry(TRACKED, "Tracked fixture"),
                provider_entry(NEW, "New fixture")
            ]}}
        })))
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

#[ignore = "run by results_distinguish_tracked_titles_and_save_the_selected_mode"]
#[tokio::test]
async fn mocked_provider_results_and_save() {
    let db = in_memory_pool().await;
    seed_series(&db, TRACKED, "Tracked fixture").await;
    cache(&db, NEW, "New fixture").await;
    cache(&db, OTHER, "Other fixture").await;
    let state = build_test_app_state(db.clone(), None);

    let page =
        html(request(&state, "/library/add?q=Fixture%20choices&source=al", None).await).await;
    assert!(page.contains("In library"));
    assert!(page.contains(&format!("href=\"/anime/anilist/{TRACKED}\"")));
    assert!(page.contains(&format!("provider_id={NEW}\"")));
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
        let states = monitoring::get_series_states(&db, row.id).await.unwrap();
        assert_eq!(states.len(), 2);
        assert!(states.iter().all(|state| !state.monitored));
    }
    assert_eq!(crate::test_support::count_series(&db).await, 3);
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
        series::add_with_monitoring(&db, core(), false, MonitorMode::All),
        series::add_with_monitoring(&db, core(), false, MonitorMode::None),
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

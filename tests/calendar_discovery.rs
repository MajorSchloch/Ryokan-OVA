//! The only network endpoint this integration binary uses is its fixture server.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
    routing::{get, post},
};
use ryokan::{
    handlers::{calendar, library::crud},
    models::{
        config::{self, Config},
        series,
    },
    services::calendar::discovery,
    test_support,
};
use scraper::{Html, Selector};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method},
};

fn media(id: i64, format: &str, start: Value) -> Value {
    json!({"id":id,"idMal":800,"format":format,"status":"NOT_YET_RELEASED",
        "title":{"english":format!("English {id}"),"native":format!("Native {id}")},"startDate":start})
}

async fn fetch(app: &Router, path: &str, json: Option<&Value>) -> String {
    let request = match json {
        Some(v) => Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(v.to_string())),
        None => Request::get(path).body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    String::from_utf8(
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn mock_page(server: &MockServer, page: i32, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"variables":{"page":page,"perPage":50}}),
        ))
        .respond_with(response)
        .mount(server)
        .await;
}

fn page(more: bool, media: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"data":{"Page":{"pageInfo":{"hasNextPage":more},"media":media}}}))
}

#[tokio::test]
async fn discover_filter_add_and_serve_the_last_complete_snapshot_on_429() {
    let server = MockServer::start().await;
    // A single test owns the process. Background metadata tasks from the real
    // Add endpoint use these fixtures as well.
    for key in [
        "RYOKAN_ANILIST_API_BASE",
        "JIKAN_API_BASE",
        "RYOKAN_KITSU_API_BASE",
        "RYOKAN_ANIBRIDGE_MAPPINGS_URL",
    ] {
        unsafe {
            std::env::set_var(key, server.uri());
        }
    }
    let db = test_support::in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    config::save_config(
        &db,
        &Config {
            media_root: temp.path().display().to_string(),
            title_language: "native".into(),
            default_allow_upgrades: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let app = Router::new()
        .route("/calendar/upcoming", get(calendar::discovery::page))
        .route("/calendar", get(calendar::page))
        .route("/api/calendar.ics", get(calendar::ical_feed))
        .route("/api/library/add", post(crud::add_series))
        .with_state(test_support::build_test_app_state(db.clone(), None));
    let path = "/calendar/upcoming";
    assert!(
        fetch(&app, path, None)
            .await
            .contains("temporarily unavailable")
    );
    let tv = media(12, "TV", json!({"year":2099,"month":4,"day":6}));
    let ova = media(13, "OVA", json!({"year":2099}));
    let mut season = media(14, "MOVIE", Value::Null);
    season["season"] = json!("SPRING");
    season["seasonYear"] = json!(2099);
    mock_page(&server, 1, page(true, json!([tv, ova]))).await;
    mock_page(
        &server,
        2,
        page(
            true,
            json!([
                tv,
                season,
                media(15, "ONA", Value::Null),
                media(16, "SPECIAL", json!({"year":2099,"month":9}))
            ]),
        ),
    )
    .await;
    // Pages overlap. Even if AniList continues to advertise more results,
    // discovery stops at its four-page budget.
    mock_page(&server, 3, page(true, json!([tv]))).await;
    mock_page(&server, 4, page(true, json!([ova]))).await;
    discovery::refresh(&db).await.unwrap();
    let html = fetch(&app, path, None).await;
    let doc = Html::parse_document(&html);
    let select = |q: &str| Selector::parse(q).unwrap();
    assert_eq!(doc.select(&select(".discovery-card")).count(), 5);
    assert_eq!(
        doc.select(&select("time[datetime='2099-04-06']")).count(),
        1
    );
    let announced = doc
        .select(&select(".discovery-announced"))
        .next()
        .unwrap()
        .text()
        .collect::<String>();
    for label in [
        "Native 13",
        "OVA · 2099",
        "Spring 2099",
        "Date TBA",
        "2099-09",
    ] {
        assert!(announced.contains(label), "{label}");
    }
    assert!(!html.contains("2099-01-01"));
    assert_eq!(
        doc.select(&select("[data-anilist-id='12'] button")).count(),
        1
    );
    let filtered = fetch(&app, "/calendar/upcoming?format=OVA", None).await;
    assert!(filtered.contains("Native 13") && !filtered.contains("Native 12"));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    for table in ["series", "release_follows", "grabbed_torrents"] {
        let q = format!("SELECT count(*) FROM {table}");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q))
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
    }

    // A partial refresh must not publish page one if page two fails.
    let refreshed: i64 = sqlx::query_scalar("SELECT refreshed_at FROM premiere_discovery")
        .fetch_one(&db)
        .await
        .unwrap();
    server.reset().await;
    mock_page(
        &server,
        1,
        page(true, json!([media(99, "TV", Value::Null)])),
    )
    .await;
    mock_page(
        &server,
        2,
        ResponseTemplate::new(429).insert_header("Retry-After", "1"),
    )
    .await;
    assert!(discovery::refresh(&db).await.is_err());
    let stale = fetch(&app, path, None).await;
    assert!(stale.contains("Last refreshed") && stale.contains("last saved premieres"));
    assert!(stale.contains("Native 13") && !stale.contains("Native 99"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT refreshed_at FROM premiere_discovery")
            .fetch_one(&db)
            .await
            .unwrap(),
        refreshed
    );

    let form = json!({"anilist_id":12,"mal_id":800,"title":"English 12","title_native":"Native 12","title_english":"English 12","title_romaji":"","format":"TV","status":"NOT_YET_RELEASED","cover_url":""});
    let (a, b) = tokio::join!(
        fetch(&app, "/api/library/add", Some(&form)),
        fetch(&app, "/api/library/add", Some(&form))
    );
    let added: Value = serde_json::from_str(&a).unwrap();
    let repeat: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(added["id"], repeat["id"]);
    assert_ne!(added["created"], repeat["created"]);
    let library = series::get_all(&db).await.unwrap();
    assert_eq!(library.len(), 1);
    assert!(!library[0].allow_upgrades);
    let html = fetch(&app, path, None).await;
    let doc = Html::parse_document(&html);
    let link = doc
        .select(&select("[data-anilist-id='12'] a"))
        .next()
        .unwrap();
    assert_eq!(link.text().collect::<String>(), "Open in library");
    assert_eq!(
        link.value().attr("href"),
        Some(format!("/series/{}", library[0].id).as_str())
    );
    assert_eq!(
        doc.select(&select("[data-anilist-id='13'] button")).count(),
        1
    );
    let filtered = fetch(&app, "/calendar/upcoming?hide_tracked=true", None).await;
    assert!(!filtered.contains("Native 12") && filtered.contains("Native 13"));
    let mut other = form.clone();
    other["anilist_id"] = json!(13);
    fetch(&app, "/api/library/add", Some(&other)).await;
    assert_eq!(series::get_all(&db).await.unwrap().len(), 2);

    sqlx::query(
        "INSERT INTO episode_airings(series_id,episode,airing_at) VALUES(?,1,unixepoch()+3600)",
    )
    .bind(library[0].id)
    .execute(&db)
    .await
    .unwrap();
    let library_calendar = fetch(&app, "/calendar", None).await;
    assert!(library_calendar.contains("Native 12"));
    let feed = fetch(&app, "/api/calendar.ics", None).await;
    assert!(feed.contains("BEGIN:VEVENT") && feed.contains("Native 12"));
    assert!(!feed.contains("20990406"));
}

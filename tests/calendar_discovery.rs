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

static PROVIDER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn use_fixture_providers(server: &MockServer) {
    ryokan::services::anilist::reset_state_for_tests();
    // The tests hold PROVIDER_LOCK for the lifetime of their fixture.
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
}

#[tokio::test]
async fn discover_filter_add_and_serve_the_last_complete_snapshot_on_429() {
    let _gate = PROVIDER_LOCK.lock().await;
    let server = MockServer::start().await;
    use_fixture_providers(&server);
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
        .select(&select("[data-anilist-id='12'] a.btn"))
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

// Use the entry payload rendered on the calendar button, just as library_add.js does.
fn add_form(html: &str, id: i64) -> Value {
    let doc = Html::parse_document(html);
    let selector =
        Selector::parse(&format!("[data-anilist-id='{id}'] button[data-entry]")).unwrap();
    let button = doc.select(&selector).next().expect("Add to library button");
    let entry: Value = serde_json::from_str(button.value().attr("data-entry").unwrap()).unwrap();
    json!({"anilist_id":entry["id"],"mal_id":entry["id_mal"],"title":entry["title_english"],
        "title_english":entry["title_english"],"title_romaji":entry["title_romaji"],
        "title_native":entry["title_native"],"cover_url":entry["cover_url"],"format":entry["format"],
        "status":entry["status"],"episodes":entry["episodes"],"season_year":entry["season_year"]})
}

async fn ambiguous_calendar_add(
    app: &Router,
    db: &sqlx::SqlitePool,
    server: &MockServer,
    stale: bool,
) {
    let original = sqlx::query_scalar::<_,i64>(
        "INSERT INTO series(anilist_id,mal_id,title,folder_name,monitor_mode,monitor_mode_manual_override)
         VALUES(-800,800,'Existing part','Existing folder','missing',1) RETURNING id"
    ).fetch_one(db).await.unwrap();
    sqlx::query("INSERT INTO episode_monitor_state(series_id,episode_number,monitored) VALUES(?,3,0),(?,4,1)")
        .bind(original).bind(original).execute(db).await.unwrap();
    sqlx::query("INSERT INTO episode_grab_history(series_id,episode_number,file_name,state) VALUES(?,4,'Existing part - 04.mkv','completed')")
        .bind(original).execute(db).await.unwrap();
    let path = "/calendar/upcoming";
    let html = fetch(app, path, None).await;
    let form = add_form(&html, 13);
    if stale {
        // Entry 12 leaves the bounded snapshot between rendering and clicking.
        server.reset().await;
        mock_page(
            server,
            1,
            page(false, json!([media(13, "OVA", Value::Null)])),
        )
        .await;
        discovery::refresh(db).await.unwrap();
        assert_eq!(add_form(&fetch(app, path, None).await, 13), form);
    }
    // Known siblings settle the match locally even after one leaves the page.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables":{"idMal":800}})))
        .respond_with(page(false, json!([media(13, "OVA", Value::Null)])))
        .expect(0)
        .mount(server)
        .await;
    let add: Value =
        serde_json::from_str(&fetch(app, "/api/library/add", Some(&form)).await).unwrap();
    assert_eq!(add["created"], true);
    let id = add["id"].as_i64().unwrap();
    assert_ne!(id, original);
    let new = series::get_by_id(db, id).await.unwrap().unwrap();
    assert_eq!((new.anilist_id, new.mal_id), (13, Some(800)));
    assert!(!new.folder_name.is_empty());
    assert_ne!(new.folder_name, "Existing folder");
    let retry: Value =
        serde_json::from_str(&fetch(app, "/api/library/add", Some(&form)).await).unwrap();
    assert_eq!(
        (retry["id"].as_i64(), retry["created"].as_bool()),
        (Some(id), Some(false))
    );

    let doc = Html::parse_document(&fetch(app, path, None).await);
    let link = doc
        .select(&Selector::parse("[data-anilist-id='13'] a.btn").unwrap())
        .next()
        .unwrap();
    assert_eq!(
        link.value().attr("href"),
        Some(format!("/series/{id}").as_str())
    );
    assert_eq!(link.text().collect::<String>(), "Open in library");
    let old = series::get_by_id(db, original).await.unwrap().unwrap();
    assert_eq!(
        (
            old.anilist_id,
            old.mal_id,
            old.title.as_str(),
            old.folder_name.as_str()
        ),
        (-800, Some(800), "Existing part", "Existing folder")
    );
    assert_eq!(old.monitor_mode, "missing");
    assert!(old.monitor_mode_manual_override);
    let slots = sqlx::query_as::<_,(i32,bool)>("SELECT episode_number,monitored FROM episode_monitor_state WHERE series_id=? ORDER BY episode_number")
        .bind(original).fetch_all(db).await.unwrap();
    assert_eq!(slots, [(3, false), (4, true)]);
    let assigned = sqlx::query_as::<_,(i64,i32)>("SELECT series_id,episode_number FROM episode_grab_history WHERE file_name='Existing part - 04.mkv' AND state='completed'")
        .fetch_all(db).await.unwrap();
    assert_eq!(assigned, [(original, 4)]);
    assert_eq!(series::get_all(db).await.unwrap().len(), 2);
}

#[tokio::test]
async fn calendar_add_preserves_the_mal_only_row_for_shared_ids_even_from_a_stale_tab() {
    let _gate = PROVIDER_LOCK.lock().await;
    let server = MockServer::start().await;
    use_fixture_providers(&server);
    // Both cards share MAL 800. All subsequent metadata hydration stays local.
    let tv = media(12, "TV", Value::Null);
    let ova = media(13, "OVA", Value::Null);
    mock_page(&server, 1, page(false, json!([tv, ova]))).await;
    for stale in [false, true] {
        let db = test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        config::save_config(
            &db,
            &Config {
                media_root: temp.path().display().to_string(),
                ..Config::default()
            },
        )
        .await
        .unwrap();
        discovery::refresh(&db).await.unwrap();
        let app = Router::new()
            .route("/calendar/upcoming", get(calendar::discovery::page))
            .route("/api/library/add", post(crud::add_series))
            .with_state(test_support::build_test_app_state(db.clone(), None));
        let folder = temp.path().join("Existing folder");
        std::fs::create_dir(&folder).unwrap();
        let episode = folder.join("Existing part - 04.mkv");
        std::fs::write(&episode, b"fixture episode").unwrap();
        ambiguous_calendar_add(&app, &db, &server, stale).await;
        assert_eq!(std::fs::read(episode).unwrap(), b"fixture episode");
    }
}

#[tokio::test]
async fn manual_add_promotes_a_verified_unique_mal_match() {
    let _gate = PROVIDER_LOCK.lock().await;
    let server = MockServer::start().await;
    use_fixture_providers(&server);
    let mut unique = media(14, "TV", Value::Null);
    unique["idMal"] = json!(801);
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables":{"idMal":801}})))
        .respond_with(page(false, json!([unique])))
        .expect(1)
        .mount(&server)
        .await;
    let db = test_support::in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    config::save_config(
        &db,
        &Config {
            media_root: temp.path().display().to_string(),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let original = sqlx::query_scalar::<_, i64>(
        "INSERT INTO series(anilist_id,mal_id,title,folder_name,monitor_mode,monitor_mode_manual_override)
         VALUES(-801,801,'Unique part','Original folder','none',1) RETURNING id",
    ).fetch_one(&db).await.unwrap();
    mock_page(&server, 1, page(false, json!([unique]))).await;
    discovery::refresh(&db).await.unwrap();
    let app = Router::new()
        .route("/calendar/upcoming", get(calendar::discovery::page))
        .route("/api/library/add", post(crud::add_series))
        .with_state(test_support::build_test_app_state(db.clone(), None));
    // A single result on a bounded discovery page still needs verification.
    let form = add_form(&fetch(&app, "/calendar/upcoming", None).await, 14);
    let response: Value =
        serde_json::from_str(&fetch(&app, "/api/library/add", Some(&form)).await).unwrap();
    assert_eq!(response["id"], original);
    assert_eq!(response["created"], false);
    let rows = series::get_all(&db).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].anilist_id, 14);
    assert_eq!(rows[0].folder_name, "Original folder");
    assert_eq!(rows[0].monitor_mode, "none");
    assert!(rows[0].monitor_mode_manual_override);
}

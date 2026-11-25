//! Local calendar navigation: stored airings, real handler and optional browser.
use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{Request, StatusCode},
    routing::get,
};
use chrono::{TimeZone, Utc};
use ryokan::{
    handlers::calendar,
    test_support::{build_test_app_state, in_memory_pool, seed_series},
};
use tower::ServiceExt;

async fn app() -> Router {
    let db = in_memory_pool().await;
    let root = std::sync::Arc::new(tempfile::tempdir().unwrap());
    std::fs::create_dir(root.path().join("Calendar feature")).unwrap();
    std::fs::write(
        root.path()
            .join("Calendar feature/Calendar feature - S01E31.mkv"),
        b"file",
    )
    .unwrap();
    ryokan::models::config::save_config(
        &db,
        &ryokan::models::config::Config {
            media_root: root.path().to_string_lossy().into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let series = seed_series(&db, 100, "Calendar feature").await;
    sqlx::query("UPDATE series SET monitor_mode='all',folder_name='Calendar feature',alternate_titles='Star path' WHERE id=?")
        .bind(series)
        .execute(&db)
        .await
        .unwrap();
    for (year, month, day) in [(2025, 12, 31), (2026, 1, 1), (2026, 1, 7)] {
        let at = Utc
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .unwrap()
            .timestamp();
        sqlx::query("INSERT INTO episode_airings(series_id,episode,airing_at,duration_minutes) VALUES(?,?,?,24)")
            .bind(series).bind(day as i32).bind(at).execute(&db).await.unwrap();
    }
    let same_day = Utc
        .with_ymd_and_hms(2026, 1, 1, 15, 0, 0)
        .unwrap()
        .timestamp();
    sqlx::query("INSERT INTO episode_airings(series_id,episode,airing_at,duration_minutes) VALUES(?,42,?,24)")
        .bind(series).bind(same_day).execute(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO episode_monitor_state(series_id,episode_number,monitored) VALUES(?,42,0)",
    )
    .bind(series)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state) VALUES('pending','fixture',?,'[7]','pending')").bind(series).execute(&db).await.unwrap();
    Router::new()
        .layer(axum::Extension(root))
        .route("/calendar", get(calendar::page))
        .route("/static/{*path}", get(asset))
        .with_state(build_test_app_state(db, None))
}

async fn asset(Path(path): Path<String>) -> Result<impl axum::response::IntoResponse, StatusCode> {
    if path.split('/').any(|part| part == "..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mime = if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "text/javascript"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    };
    let bytes = tokio::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("static")
            .join(path),
    )
    .await
    .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(([("content-type", mime)], bytes))
}

#[tokio::test]
async fn earlier_day_partial_preserves_filters_and_full_page_context() {
    let app = app().await;
    let url = "/calendar?range=day&date=2025-12-31&monitored=true&search=Calendar&external=true&timezone=Europe%2FVienna";
    for partial in [false, true] {
        let mut request = Request::builder().uri(url);
        if partial {
            request = request.header("HX-Request", "true");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("Calendar feature"), "{body}");
        assert!(body.contains("data-date=\"2025-12-31\""));
        assert!(body.contains("date=2025-12-30"));
        assert!(body.contains("monitored=true"));
        assert!(body.contains("external=true"));
        assert!(body.contains("timezone=Europe%2FVienna"));
        assert_eq!(body.contains("<!DOCTYPE html>"), !partial);
    }
}

#[tokio::test]
async fn unavailable_schedules_do_not_claim_an_empty_calendar() {
    let db = in_memory_pool().await;
    sqlx::query("DROP TABLE episode_airings")
        .execute(&db)
        .await
        .unwrap();
    let app = Router::new()
        .route("/calendar", get(calendar::page))
        .with_state(build_test_app_state(db, None));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/calendar?date=2026-01-01")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("Could not load saved episode schedules"));
    assert!(!body.contains("No saved episodes"));
}

/// Run with RYOKAN_PLAYWRIGHT_MODULE pointing to playwright-core's index.mjs.
#[tokio::test]
#[ignore = "requires Node, Playwright and headless Chrome"]
async fn calendar_navigation_browser() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app().await;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let result = tokio::process::Command::new("node")
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/calendar_navigation_browser.mjs"),
        )
        .arg(format!("http://{address}"))
        .status()
        .await
        .unwrap();
    server.abort();
    assert!(result.success());
}

#[tokio::test]
async fn availability_filter_is_shared_by_full_partial_list_and_grid_and_preserves_dates() {
    let app = app().await;
    for (range, status, expected) in [
        ("day", "missing", true),
        ("day", "available", false),
        ("month", "pending", true),
    ] {
        for partial in [false, true] {
            let url = format!(
                "/calendar?range={range}&date=2026-01-01&status={status}&monitored=true&search=Calendar"
            );
            let mut request = Request::builder().uri(&url);
            if partial {
                request = request.header("HX-Request", "true");
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let body = String::from_utf8(
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert_eq!(
                body.contains(&format!(
                    "class=\"calendar-file-status calendar-file-status-{status}\""
                )),
                expected,
                "{body}"
            );
            assert!(body.contains(&format!("status={status}")));
            assert!(body.contains("monitored=true"));
            assert!(body.contains("search=Calendar"));
            assert!(!body.contains("calendar-file-status-unknown"));
        }
    }
}

#[tokio::test]
async fn title_alias_monitoring_and_status_compose_in_every_range() {
    let app = app().await;
    for range in ["day", "this_week", "month"] {
        let url = format!(
            "/calendar?range={range}&date=2026-01-01&search=star%20path&monitoring=unmonitored&status=unmonitored"
        );
        let response = app
            .clone()
            .oneshot(Request::get(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("data-episode=\"42\""));
        assert!(!body.contains("data-episode=\"1\""));
        assert!(body.contains("calendar-file-status-unmonitored"));
        assert!(body.contains("monitoring=unmonitored"));
        assert!(body.contains("search=star%20path"));
    }

    let response = app
        .oneshot(
            Request::get("/calendar?range=day&date=2026-01-01&search=unrelated")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("No saved episodes match these filters")
    );
}

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
    let series = seed_series(&db, 100, "Calendar feature").await;
    sqlx::query("UPDATE series SET monitor_mode='all' WHERE id=?")
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
    Router::new()
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

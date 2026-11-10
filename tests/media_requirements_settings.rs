//! Quality requirements through the real settings routes, with optional browser QA.
use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{Request, StatusCode},
    routing::{get, post},
};
use ryokan::{
    handlers::settings,
    models::config,
    test_support::{build_test_app_state, in_memory_pool},
};
use tower::ServiceExt;

async fn app() -> Router {
    let db = in_memory_pool().await;
    config::save_config(
        &db,
        &config::Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Router::new()
        .route("/settings", get(settings::settings_page))
        .route("/settings/quality", post(settings::settings_quality_submit))
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
async fn invalid_requirements_preserve_the_draft_and_do_not_save() {
    let app = app().await;
    let response=app.clone().oneshot(Request::builder().method("POST").uri("/settings/quality").header("content-type","application/x-www-form-urlencoded").header("HX-Request","true").body(Body::from("preferred_groups=Keep&blocked_groups=&preferred_source=web&preferred_resolution=1080&cutoff_source=bluray&cutoff_resolution=1080&finished_series_quality=same&prefer_subs=1&media_requirements_enabled=1&minimum_resolution=1080&target_resolution=720&minimum_subtitle_check=0&target_subtitle_check=1&required_subtitle_languages=Spanish&subtitle_language_match=any")).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("must be at least the minimum"));
    assert!(text.contains("value=\"es\""));
    assert!(text.contains("value=\"720\" selected"));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/settings?tab=quality")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("value=\"en\""));
    assert!(!text.contains("data-ryokan-toast=\"error\""));
}
#[tokio::test]
#[ignore = "requires Node, Playwright and headless Chrome"]
async fn media_requirements_settings_browser() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app().await;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let result = tokio::process::Command::new("node")
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/media_requirements_settings_browser.mjs"),
        )
        .arg(format!("http://{address}"))
        .status()
        .await
        .unwrap();
    server.abort();
    assert!(result.success());
}

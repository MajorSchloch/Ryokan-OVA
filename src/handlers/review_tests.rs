use super::*;
use crate::{
    handlers::library::misgrabs,
    models::series,
    services::upgrade::outcomes::{self, Outcome},
    test_support::{build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series},
};
use axum::http::header;
use axum_htmx::HxRequest;
use sqlx::SqlitePool;

fn no_feedback() -> Query<Feedback> {
    Query(Feedback {
        message: None,
        error: None,
    })
}

async fn html(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

async fn seed_edition(db: &SqlitePool, owner: i64, hash: &str, episode: i32) -> i64 {
    let grab = seed_grabbed_torrent(db, owner, hash, "Candidate", &[episode]).await;
    sqlx::query("UPDATE grabbed_torrents SET state='failed' WHERE id=?")
        .bind(grab)
        .execute(db)
        .await
        .unwrap();
    let grabbed_at = grabbed_torrents::get_by_id(db, grab)
        .await
        .unwrap()
        .unwrap()
        .grabbed_at;
    edition_review::save(
        db,
        &edition_review::Review {
            id: 0,
            grab_id: grab,
            grabbed_at,
            series_id: owner,
            provider_id: 10,
            episode,
            source_root: "/fixture/downloads".into(),
            source_path: format!("/fixture/downloads/{hash}.mkv"),
            source_identity: "candidate".into(),
            current_path: format!("/fixture/library/{episode}.mkv"),
            current_identity: "library".into(),
            reason: "The replacement is substantially shorter".into(),
        },
    )
    .await
    .unwrap();
    grab
}

async fn seed_misgrab(db: &SqlitePool, series_id: i64, hash: &str, action: &str) -> i64 {
    let id = seed_grabbed_torrent(db, series_id, hash, "Held release", &[2, 4]).await;
    grabbed_torrents::stamp_verification(db, id, "misgrab", r#"{"files":["Another show.mkv"]}"#)
        .await
        .unwrap();
    grabbed_torrents::set_misgrab_action(db, id, action)
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn review_queue_groups_open_editions_and_excludes_background_work() {
    let db = in_memory_pool().await;
    let edition_owner = seed_series(&db, 10, "Shorter Edition").await;
    seed_edition(&db, edition_owner, "edition-1", 1).await;
    seed_edition(&db, edition_owner, "edition-2", 1).await;
    let closed = seed_edition(&db, edition_owner, "edition-3", 2).await;
    sqlx::query("UPDATE episode_edition_reviews SET decision='kept' WHERE grab_id=?")
        .bind(closed)
        .execute(&db)
        .await
        .unwrap();

    let held_owner = seed_series(&db, 11, "Held For Review").await;
    let held = seed_misgrab(&db, held_owner, "held", "flagged").await;
    let historical = seed_series(&db, 12, "Remediated Download").await;
    seed_misgrab(&db, historical, "remediated", "removed").await;
    let running = seed_series(&db, 13, "Ordinary Download").await;
    seed_grabbed_torrent(&db, running, "pending", "An active download", &[1]).await;
    let retry = seed_series(&db, 14, "Background Retry").await;
    let retry = series::get_by_id(&db, retry).await.unwrap().unwrap();
    outcomes::record(&db, &retry, &[(1, Outcome::SourceHold)])
        .await
        .unwrap();

    let state = build_test_app_state(db, None);
    let body = html(page(State(state.clone()), HeaderMap::new(), no_feedback()).await).await;
    assert_eq!(body.matches("data-edition-case=").count(), 1);
    assert_eq!(body.matches("data-misgrab-case=").count(), 1);
    assert!(body.contains("Shorter Edition · Episode 1"));
    assert!(body.contains("return_to=%2Freview"));
    assert!(body.contains(&format!("/review/misgrabs/{held}")));
    for excluded in [
        "Shorter Edition · Episode 2",
        "Remediated Download",
        "Ordinary Download",
        "Background Retry",
    ] {
        assert!(
            !body.contains(excluded),
            "case unexpectedly shown: {excluded}"
        );
    }
    let body = html(misgrab(State(state), Path(held), HeaderMap::new(), no_feedback()).await).await;
    assert!(body.contains("Another show.mkv"));
    assert!(body.contains("Back to review queue"));
    assert!(body.contains(&format!(
        "/api/library/misgrabs/{held}/dismiss?return_to=%2Freview"
    )));
}

#[tokio::test]
async fn accepting_a_held_download_returns_to_the_queue_and_closes_the_case() {
    let db = in_memory_pool().await;
    let owner = seed_series(&db, 24, "Held For Review").await;
    let held = seed_misgrab(&db, owner, "held", "flagged").await;
    let state = build_test_app_state(db, None);
    let response = misgrabs::restore_misgrab(
        State(state.clone()),
        HxRequest(false),
        Path(held),
        Query::try_from_uri(&"/review?return_to=%2Freview".parse().unwrap()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(
        response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .starts_with("/review?message=")
    );
    let body = html(page(State(state), HeaderMap::new(), no_feedback()).await).await;
    assert!(body.contains("No cases are waiting for your decision."));
    assert!(!body.contains("data-misgrab-case="));
}

#[tokio::test]
async fn review_queue_distinguishes_a_read_error_from_an_empty_queue() {
    let db = in_memory_pool().await;
    sqlx::query("DROP TABLE episode_edition_reviews")
        .execute(&db)
        .await
        .unwrap();
    let state = build_test_app_state(db, None);
    let body = html(page(State(state), HeaderMap::new(), no_feedback()).await).await;
    assert!(body.contains("Some cases could not be loaded"));
    assert!(!body.contains("No cases are waiting for your decision."));
}

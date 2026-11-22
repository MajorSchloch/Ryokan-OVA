use super::super::*;
use crate::models::indexers;
use crate::services::indexers::torznab::TorznabIndexer;
use crate::test_support::{build_test_app_state, in_memory_pool};
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn indexer_rss_records_the_429_and_preserves_poll_metrics_during_deferrals() {
    let db = in_memory_pool().await;
    let server = MockServer::start().await;
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO indexers (name, kind, url, enabled, rss_enabled)
         VALUES ('Quota fixture', 'newznab', ?, 1, 1) RETURNING id",
    )
    .bind(format!("{}/api", server.uri()))
    .fetch_one(&db)
    .await
    .unwrap();
    let row = indexers::get_by_id(&db, id).await.unwrap().unwrap();
    let state = build_test_app_state(db.clone(), None);
    *state.indexers.write().await = Arc::new(vec![TorznabIndexer::from_row_arc(&row).unwrap()]);
    let cfg = config::Config {
        nyaa_enabled: false,
        ..Default::default()
    };
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "71018")
                .set_body_string("provider body marker"),
        )
        .expect(1)
        .mount(&server)
        .await;

    assert!(
        fetch_all_sources(&state, &cfg, false, &[5070])
            .await
            .is_empty()
    );
    let actual = indexers::get_by_id(&db, id).await.unwrap().unwrap();
    assert!(actual.rss_last_polled_at.is_some());
    assert!(cooldown::is_rate_limit(&actual.rss_last_poll_error));
    assert!(!actual.rss_last_poll_error.contains("provider body marker"));
    let retry_at = actual
        .rss_last_poll_error
        .split_once("Retry after ")
        .and_then(|(_, retry)| retry.split_once(". "))
        .map(|(stamp, _)| stamp)
        .unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(retry_at).is_ok());
    // Use a distinct earlier timestamp to detect writes within one second.
    sqlx::query("UPDATE indexers SET rss_last_polled_at = 1 WHERE id = ?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();

    for _ in 0..2 {
        assert!(
            fetch_all_sources(&state, &cfg, false, &[5070])
                .await
                .is_empty()
        );
    }
    let deferred = indexers::get_by_id(&db, id).await.unwrap().unwrap();
    assert_eq!(deferred.rss_last_polled_at, Some(1));
    assert_eq!(deferred.rss_last_poll_error, actual.rss_last_poll_error);
    assert_eq!(deferred.rss_last_item_count, actual.rss_last_item_count);

    let logs: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT level, message, detail FROM logs WHERE category = 'rss' ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(logs.len(), 2, "{logs:?}");
    assert_eq!(logs[0].0, "warn");
    assert_eq!(logs[0].1, "Indexer-RSS poll failed: Quota fixture");
    assert!(cooldown::is_rate_limit(&logs[0].2));
    assert_eq!(logs[1].0, "info");
    assert_eq!(logs[1].1, "Indexer-RSS poll deferred: Quota fixture");
    assert!(cooldown::is_deferred(&logs[1].2));
    assert!(logs[1].2.ends_with(&format!("Retry after {retry_at}.")));
}

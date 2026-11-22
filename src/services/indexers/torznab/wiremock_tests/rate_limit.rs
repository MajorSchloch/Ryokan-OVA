use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::fixture::{new_fixture_with_row, sample_row_for};
use crate::models::indexers::KIND_NEWZNAB;
use crate::services::indexers::torznab::TorznabIndexer;
use crate::services::indexers::{Indexer, SearchQuery, cooldown};

#[tokio::test]
async fn http_429_defers_search_and_caps_for_rebuilt_clients_on_the_same_endpoint() {
    let server = MockServer::start().await;
    let mut row = sample_row_for(&server, KIND_NEWZNAB, "Rate limit fixture");
    row.api_key = "rate-limited-account".into();
    let client = TorznabIndexer::from_row(&row).unwrap();
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("apikey", &row.api_key))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "71018")
                .set_body_string("Non-public proxy detail"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let err = client.search(&SearchQuery::default()).await.unwrap_err();
    assert!(cooldown::is_rate_limit(&err) && !cooldown::is_deferred(&err));
    assert!(err.contains("Retry-After requested 71018s"), "{err}");
    assert!(!err.contains("Non-public proxy detail"), "{err}");
    assert!(!err.contains(&row.api_key), "{err}");
    let retry = err
        .split_once("Retry after ")
        .unwrap()
        .1
        .split('.')
        .next()
        .unwrap();
    assert!(retry.ends_with('Z'), "{retry}");

    row.id = 0; // An otherwise identical Settings test client.
    let test_client = TorznabIndexer::from_row(&row).unwrap();
    let search = test_client
        .search(&SearchQuery::default())
        .await
        .unwrap_err();
    let caps = test_client.caps().await.unwrap_err();
    assert_eq!(search, caps);
    assert!(
        cooldown::is_deferred(&search) && search.contains(retry),
        "{search}"
    );

    row.api_key = "different-account".into();
    let other_client = TorznabIndexer::from_row(&row).unwrap();
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("apikey", &row.api_key))
        .respond_with(ResponseTemplate::new(200).set_body_string("<rss><channel></channel></rss>"))
        .expect(1)
        .mount(&server)
        .await;
    assert!(other_client.search(&SearchQuery::default()).await.is_ok());
}

#[tokio::test]
async fn http_429_without_usable_retry_after_reports_the_default() {
    let (server, client) =
        new_fixture_with_row(|row| row.api_key = "missing-retry-after".into()).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.search(&SearchQuery::default()).await.unwrap_err();
    assert!(cooldown::is_rate_limit(&err) && !cooldown::is_deferred(&err));
    assert!(err.contains("default wait 60s"), "{err}");
    let deferred = client.search(&SearchQuery::default()).await.unwrap_err();
    assert!(cooldown::is_deferred(&deferred), "{deferred}");
}

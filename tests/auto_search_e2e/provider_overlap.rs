use super::*;

#[tokio::test]
async fn automatic_search_overlaps_provider_requests_and_keeps_nyaa_for_duplicates() {
    use axum::{Router, routing::get};
    use ryokan::services::{auto_search::find_best_for_target, indexers::torznab::TorznabIndexer};
    use std::{sync::Arc, time::Duration};
    use tokio::sync::{Barrier, OnceCell};

    let _gate = ENV_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hash = "5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c";
    let title = "[Fixture] Quartz Harbor - 03 (1080p) [WEB-DL]";
    let both_started = Arc::new(Barrier::new(2));
    // Each provider waits for the other's first request before returning
    // results. The extra queries for the same provider share that wait.
    let respond = |body: String| {
        let both_started = both_started.clone();
        let first_request = Arc::new(OnceCell::new());
        move || {
            let (both_started, first_request, body) =
                (both_started.clone(), first_request.clone(), body.clone());
            async move {
                first_request
                    .get_or_init(|| async { both_started.wait().await })
                    .await;
                body
            }
        }
    };
    let app = Router::new()
        .route(
            "/",
            get(respond(nyaa_results_page(&nyaa_row(
                hash, 10503, title, "1.4 GiB", 20,
            )))),
        )
        .route(
            "/api",
            get(respond(format!(
                r#"<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>
                <item><title>{title}</title><guid>mirrored-episode</guid>
                <enclosure url="{base}/duplicate.torrent" length="1503238553" type="application/x-bittorrent"/>
                <torznab:attr name="seeders" value="200"/><torznab:attr name="infohash" value="{hash}"/>
                </item></channel></rss>"#
            ))),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = build_state().await;
    sqlx::query("INSERT INTO indexers(id,name,kind,url) VALUES(195,'Fixture','torznab',?)")
        .bind(format!("{base}/api"))
        .execute(&state.db)
        .await
        .unwrap();
    let row = ryokan::models::indexers::get_by_id(&state.db, 195)
        .await
        .unwrap()
        .unwrap();
    *state.indexers.write().await = Arc::new(vec![TorznabIndexer::from_row_arc(&row).unwrap()]);

    set_nyaa_base(&base);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        find_best_for_target(
            &state.db,
            &detail_for(10503, "Quartz Harbor"),
            &default_config(),
            &SearchTarget::Episode(3),
            false,
            false,
            &[],
            &state.indexers,
        ),
    )
    .await;
    unset_nyaa_base();
    server.abort();

    let result = result
        .expect("both provider requests must start before either responds")
        .expect("the exact episode should be available");
    assert_eq!(result.info_hash, hash);
    assert_eq!(
        result.indexer_id, None,
        "the direct source wins deduplication"
    );
}

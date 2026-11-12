use super::*;
use crate::services::{indexers, interactive_search_cache, nyaa::SearchResult};
use crate::test_support::{build_test_app_state, in_memory_pool};
use scraper::{Html as Document, Selector};
use serde_json::json;
use std::sync::Arc;

fn result(indexer_id: Option<i64>, seeders: i32, leechers: i32) -> SearchResult {
    serde_json::from_value(json!({
        "title": "[Group] Example - 03 [1080p]",
        "link": "https://example.test/release/3",
        "torrent": "https://example.test/download/3",
        "magnet": "",
        "size": "90 MiB", "size_bytes": 94_371_840,
        "seeders": seeders, "leechers": leechers, "downloads": 0,
        "group": "Group", "resolution": "1080", "quality_label": "WEB-1080p",
        "source": "Web", "web_kind": "WEB-DL", "is_remux": false,
        "is_bdmv": false, "is_batch": false, "is_trusted": false,
        "score": 80, "info_hash": "0123456789abcdef0123456789abcdef01234567",
        "indexer_id": indexer_id, "indexer_name": "Example indexer",
        "source_guid": "release-3", "upload_date": "2026-11-12 09:30"
    }))
    .unwrap()
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).unwrap()
}

fn grab_json(row: scraper::ElementRef<'_>) -> serde_json::Value {
    serde_json::from_str(
        row.select(&selector("button[data-result]"))
            .next()
            .unwrap()
            .value()
            .attr("data-result")
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn protocol_availability_mixed_results_keep_only_torrent_peer_counts() {
    let mut torrent = result(Some(1), 17, 4);
    torrent.info_hash.clear(); // Hashless private torrents still have peers.
    let mut unknown = result(Some(9), 99, 88);
    unknown.indexer_name.clear();
    let protocols = HashMap::from([(1, "torrent"), (2, "usenet")]);
    let html = build_interactive_search_partial(
        vec![result(None, 42, 3), torrent, result(Some(2), 0, 0), unknown],
        Some(3),
        false,
        &protocols,
    )
    .render()
    .unwrap();
    let document = Document::parse_fragment(&html);
    let rows: Vec<_> = document.select(&selector("tbody > tr")).collect();
    assert_eq!(rows.len(), 4);
    assert!(html.contains(">Availability</th>"));
    for (index, seeds, leeches) in [(0, "42 S", "3 L"), (1, "17 S", "4 L")] {
        assert_eq!(
            rows[index]
                .select(&selector(".seed-count"))
                .next()
                .unwrap()
                .text()
                .collect::<String>(),
            seeds
        );
        assert_eq!(
            rows[index]
                .select(&selector(".leech-count"))
                .next()
                .unwrap()
                .text()
                .collect::<String>(),
            leeches
        );
        assert_eq!(grab_json(rows[index])["protocol"], "torrent");
    }
    let nzb = rows[2];
    let availability = nzb.select(&selector(".col-seeds")).next().unwrap();
    let text = availability.text().collect::<String>();
    assert!(text.contains("Usenet") && text.contains("Posted 2026-11-12"));
    assert_eq!(
        nzb.select(&selector(".seed-count, .leech-count, .peer-sep"))
            .count(),
        0
    );
    let data = grab_json(nzb);
    assert_eq!(data["protocol"], "usenet");
    assert!(data["seeders"].is_null() && data["leechers"].is_null());
    assert_eq!(data["indexer_id"], 2);
    assert_eq!(data["source_guid"], "release-3");
    assert_eq!(
        data["info_hash"], "0123456789abcdef0123456789abcdef01234567",
        "a stray hash must not turn an NZB into a torrent"
    );
    assert_eq!(
        rows[3]
            .select(&selector(".seed-count, .leech-count"))
            .count(),
        0
    );
    assert!(
        rows[3]
            .text()
            .collect::<String>()
            .contains("Unknown indexer")
    );
    assert_eq!(grab_json(rows[3])["protocol"], "unknown");

    // Root's centralized test run may request a real Askama-rendered preview.
    if let Ok(path) = std::env::var("RYOKAN_PROTOCOL_PREVIEW_HTML") {
        let page = format!(
            "<!doctype html><html><head><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"stylesheet\" href=\"/static/css/base.css\"><link rel=\"stylesheet\" href=\"/static/css/forms.css\"><link rel=\"stylesheet\" href=\"/static/css/badges.css\"><link rel=\"stylesheet\" href=\"/static/css/modals.css\"></head><body><main style=\"padding:16px\"><h2>Search results</h2><div style=\"overflow:auto\">{html}</div></main></body></html>"
        );
        std::fs::write(path, page).unwrap();
    }
}

#[test]
fn protocol_availability_without_a_publish_date_makes_no_availability_claim() {
    let mut nzb = result(Some(2), 200, 100);
    nzb.upload_date.clear();
    let html =
        build_interactive_search_partial(vec![nzb], None, true, &HashMap::from([(2, "usenet")]))
            .render()
            .unwrap();
    let document = Document::parse_fragment(&html);
    let cell = document.select(&selector("td.col-seeds")).next().unwrap();
    assert_eq!(cell.text().collect::<String>().trim(), "Usenet");
    assert_eq!(
        cell.select(&selector(".seed-count, .leech-count")).count(),
        0
    );
    assert!(
        !html.contains("Posted") && !html.contains("retention") && !html.contains("completion")
    );
    assert!(html.contains("data-wanted-grab-batch=\"1\""));
}

#[test]
fn protocol_availability_episode_sets_and_wanted_keep_the_same_protocol_metadata() {
    let hits = vec![
        auto_search::EpisodeSetHit {
            result: result(Some(2), 0, 0),
            episodes: vec![3],
        },
        auto_search::EpisodeSetHit {
            result: result(None, 4, 1),
            episodes: vec![4],
        },
    ];
    let html = build_episode_set_partial(hits, true, &HashMap::from([(2, "usenet")]))
        .render()
        .unwrap();
    let document = Document::parse_fragment(&html);
    let rows: Vec<_> = document.select(&selector("tbody > tr")).collect();
    assert_eq!(
        rows[0]
            .select(&selector(".seed-count, .leech-count"))
            .count(),
        0
    );
    assert_eq!(
        rows[0]
            .select(&selector("button"))
            .next()
            .unwrap()
            .value()
            .attr("data-wanted-grab"),
        Some("3")
    );
    assert_eq!(grab_json(rows[0])["protocol"], "usenet");
    assert_eq!(rows[1].select(&selector(".seed-count")).count(), 1);
    assert_eq!(grab_json(rows[1])["protocol"], "torrent");
}

struct ConfiguredIndexer(i64, &'static str);

#[async_trait::async_trait]
impl indexers::Indexer for ConfiguredIndexer {
    fn id(&self) -> i64 {
        self.0
    }
    fn name(&self) -> &str {
        "Configured source"
    }
    fn priority(&self) -> i32 {
        25
    }
    fn is_private_tracker(&self) -> bool {
        false
    }
    fn kind(&self) -> &str {
        self.1
    }
    async fn caps(&self) -> Result<indexers::IndexerCaps, String> {
        panic!("cached rendering must not request capabilities")
    }
    async fn search(&self, _: &indexers::SearchQuery) -> Result<Vec<indexers::Release>, String> {
        panic!("cached rendering must not search upstream")
    }
}

#[tokio::test]
async fn protocol_availability_cached_episode_and_batch_resolve_the_configured_indexer() {
    let state = build_test_app_state(in_memory_pool().await, None);
    *state.indexers.write().await = Arc::new(vec![
        Arc::new(ConfiguredIndexer(1, "torznab")),
        Arc::new(ConfiguredIndexer(2, "newznab")),
        Arc::new(ConfiguredIndexer(3, "unsupported")),
    ]);
    assert_eq!(
        indexer_protocols(&state).await,
        HashMap::from([(1, "torrent"), (2, "usenet"), (3, "unknown")])
    );
    for batch in [false, true] {
        let key = if batch {
            interactive_search_cache::Key::Batch(100)
        } else {
            interactive_search_cache::Key::Episode(100, 3)
        };
        interactive_search_cache::insert(
            &state.interactive_search_cache,
            key,
            vec![result(Some(2), 0, 0)],
        );
        let origin = InteractiveOrigin {
            from: Some("wanted".into()),
        };
        let response = if batch {
            interactive_search_batches(
                State(state.clone()),
                HxRequest(true),
                Query(origin),
                Path(100),
            )
            .await
        } else {
            interactive_search_episode(
                State(state.clone()),
                HxRequest(true),
                Query(origin),
                Path((100, 3)),
            )
            .await
        }
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Usenet"));
        assert!(!html.contains("class=\"seed-count\"") && !html.contains("class=\"leech-count\""));
        let document = Document::parse_fragment(&html);
        let row = document.select(&selector("tbody > tr")).next().unwrap();
        assert_eq!(grab_json(row)["protocol"], "usenet");
    }
}

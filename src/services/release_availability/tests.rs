use super::*;
use crate::services::{anilist::AnimeDetail, auto_search, indexers};
use crate::test_support::{build_test_app_state, in_memory_pool};
use serde_json::json;
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct NyaaOrigin(Option<std::ffi::OsString>);
impl NyaaOrigin {
    fn set(url: &str) -> Self {
        let previous = std::env::var_os("RYOKAN_NYAA_API_BASE");
        // nextest isolates tests; ENV_LOCK also serializes this module under
        // cargo test. Every request remains local to the mock server.
        unsafe { std::env::set_var("RYOKAN_NYAA_API_BASE", url) };
        Self(previous)
    }
}
impl Drop for NyaaOrigin {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(previous) => std::env::set_var("RYOKAN_NYAA_API_BASE", previous),
                None => std::env::remove_var("RYOKAN_NYAA_API_BASE"),
            }
        }
    }
}

#[derive(Clone)]
struct FixtureIndexer {
    id: i64,
    kind: &'static str,
    releases: Vec<indexers::Release>,
}
#[async_trait::async_trait]
impl Indexer for FixtureIndexer {
    fn id(&self) -> i64 {
        self.id
    }
    fn name(&self) -> &str {
        self.kind
    }
    fn priority(&self) -> i32 {
        25
    }
    fn kind(&self) -> &str {
        self.kind
    }
    fn is_private_tracker(&self) -> bool {
        false
    }
    async fn caps(&self) -> Result<indexers::IndexerCaps, String> {
        Ok(Default::default())
    }
    async fn search(&self, _: &indexers::SearchQuery) -> Result<Vec<indexers::Release>, String> {
        Ok(self.releases.clone())
    }
}

fn release(indexer_id: i64, group: &str, seeders: i32, pack: bool) -> indexers::Release {
    serde_json::from_value(json!({
        "indexer_id": indexer_id, "indexer_priority": 25, "indexer_name": group,
        "title": format!("[{group}] Example Show - {} [1080p WEB-DL]", if pack {"01-12 [Batch]"} else {"01"}),
        "guid": format!("{group}-{pack}"), "link": format!("https://example.test/{group}/{pack}"),
        "magnet": "", "publish_date": 0, "size_bytes": 500_000_000,
        "seeders": seeders, "leechers": 0, "info_hash": "", "categories": [5070],
        "download_volume_factor": null, "upload_volume_factor": null
    })).unwrap()
}

fn policy(minimum: i32) -> Policy {
    Policy::from_indexers(
        &Config {
            minimum_torrent_seeders: minimum,
            ..Default::default()
        },
        &[],
    )
}

#[test]
fn automatic_availability_enforces_configured_floor_without_inventing_unknown_counts() {
    assert_eq!(Config::default().minimum_torrent_seeders, 1);
    for minimum in [1, 3, 10_000] {
        let p = policy(minimum);
        for count in [None, Some(-1), Some(0), Some(minimum - 1)] {
            assert!(
                p.check(Protocol::Torrent, count).is_err(),
                "floor {minimum}, count {count:?}"
            );
        }
        assert!(p.check(Protocol::Torrent, Some(minimum)).is_ok());
        assert!(p.check(Protocol::Torrent, Some(minimum + 1)).is_ok());
    }
    for count in [None, Some(-1), Some(0), Some(1)] {
        assert!(policy(0).check(Protocol::Torrent, count).is_ok());
    }
    for invalid in [-1, 10_001, i32::MAX] {
        assert!(
            policy(invalid)
                .check(Protocol::Torrent, Some(i32::MAX))
                .is_err()
        );
    }
}

#[test]
fn automatic_availability_uses_configured_protocol_not_hashes_or_zero_peer_placeholders() {
    let sources: Vec<Arc<dyn Indexer>> = vec![
        Arc::new(FixtureIndexer {
            id: 1,
            kind: "torznab",
            releases: vec![],
        }),
        Arc::new(FixtureIndexer {
            id: 2,
            kind: "newznab",
            releases: vec![],
        }),
    ];
    let p = Policy::from_indexers(&Config::default(), &sources);
    let mut hit = release(1, "Zero", 0, false).into_search_result();
    assert!(
        p.check_result(&hit).is_err(),
        "hashless torrents still require seeds"
    );
    hit.info_hash = HASH.into();
    hit.indexer_id = Some(2);
    assert!(
        p.check_result(&hit).is_ok(),
        "a stray hash cannot make Usenet peer based"
    );
    hit.indexer_id = Some(99);
    hit.seeders = 10_000;
    assert!(
        p.check_result(&hit).is_err(),
        "unknown configured source must not become Nyaa"
    );
    hit.indexer_id = None;
    assert!(
        p.check_result(&hit).is_ok(),
        "built-in Nyaa is a known torrent source"
    );
    for minimum in [-1, 1, 10_000] {
        assert!(policy(minimum).check(Protocol::Usenet, None).is_ok());
        assert!(policy(minimum).check(Protocol::Usenet, Some(0)).is_ok());
    }
}

fn feed_item(source: RssSource, link: &str) -> RssItem {
    RssItem {
        seeders: None,
        title: "[Group] Example Show - 01 [1080p]".into(),
        link: link.into(),
        torrent: link.into(),
        guid: "entry".into(),
        magnet: String::new(),
        info_hash: HASH.into(),
        group: "Group".into(),
        resolution: "1080".into(),
        is_batch: false,
        source,
    }
}

fn listing(hash: &str, seeders: i32) -> String {
    format!(
        r#"<div class="panel"><div class="panel-heading"><h3 class="panel-title">[Group] Example Show - 01 [1080p]</h3></div>
      <div class="panel-body"><div class="row"><div class="col-md-1">Seeders:</div><div class="col-md-5">{seeders}</div></div></div>
      <div class="panel-footer"><a class="card-footer-item" href="magnet:?xt=urn:btih:{hash}">Magnet</a></div></div>"#
    )
}

#[tokio::test]
async fn automatic_availability_rss_resolves_missing_nyaa_seeders_from_the_same_release_only() {
    let _guard = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let _origin = NyaaOrigin::set(&server.uri());
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(listing(HASH, 4)))
        .expect(2)
        .mount(&server)
        .await;
    let p = policy(3);
    let source = RssSource::UserFeed {
        id: 7,
        name: "Direct feed".into(),
    };
    let mut item = feed_item(
        source.clone(),
        &format!("{}/download/123.torrent", server.uri()),
    );
    p.check_rss(&mut item, "torrent")
        .await
        .expect("the same-hash Nyaa header supplies its known seeder count");
    assert_eq!(item.seeders, Some(4));
    // A page pointer from another torrent must not supply the missing count.
    let mut wrong = feed_item(source, &item.link);
    wrong.info_hash = "b".repeat(40);
    assert!(p.check_rss(&mut wrong, "torrent").await.is_err());
    assert_eq!(wrong.seeders, None);
}

#[tokio::test]
async fn automatic_availability_rss_never_fetches_arbitrary_urls_or_uses_usenet_peer_counts() {
    let server = MockServer::start().await;
    let link = format!("{}/view/123", server.uri());
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let p = policy(1);
    let source = RssSource::UserFeed {
        id: 7,
        name: "Direct feed".into(),
    };
    let mut unknown = feed_item(source.clone(), &link);
    assert!(p.check_rss(&mut unknown, "torrent").await.is_err());
    assert!(policy(0).check_rss(&mut unknown, "torrent").await.is_ok());
    let mut nzb = feed_item(source, &link);
    assert!(p.check_rss(&mut nzb, "usenet").await.is_ok());
    nzb.source = RssSource::Indexer {
        id: 2,
        name: "Usenet".into(),
        kind: "newznab".into(),
    };
    nzb.seeders = Some(0);
    assert!(p.check_rss(&mut nzb, "unknown").await.is_ok());
}

fn detail() -> AnimeDetail {
    serde_json::from_value(json!({
        "id": 999_987, "id_mal": null, "title_romaji": "Example Show", "title_english": "Example Show",
        "title_native": "", "cover_url": "", "banner_url": "", "format": "TV", "status": "FINISHED",
        "status_display": "Finished", "episodes": 12, "duration": 24, "season": "SPRING",
        "season_year": 2026, "end_year": 2026, "description": "", "genres": [], "average_score": null,
        "average_score_display": null, "score_is_ten_point": false, "score_class": "",
        "next_airing_episode": null, "next_airing_at": null, "synonyms": [], "streaming_episodes": [], "relations": []
    })).unwrap()
}

async fn persist_indexer_identities(state: &crate::AppState) {
    // The source-replay boundary verifies configured indexer identity in the
    // database too; an in-memory fake alone is deliberately not sufficient.
    let snapshot = state.indexers.read().await.clone();
    for indexer in snapshot.iter() {
        sqlx::query("INSERT INTO indexers(id,name,kind,url,caps_json) VALUES(?,?,?,?, '{}')")
            .bind(indexer.id())
            .bind(indexer.name())
            .bind(indexer.kind())
            .bind("https://example.test/api")
            .execute(&state.db)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn automatic_availability_filters_before_ranking_so_healthy_torrent_and_usenet_can_win() {
    for (kind, seeds) in [("torznab", 2), ("newznab", 0)] {
        let state = build_test_app_state(in_memory_pool().await, None);
        *state.indexers.write().await = Arc::new(vec![
            Arc::new(FixtureIndexer {
                id: 1,
                kind: "torznab",
                releases: vec![release(1, "Premium", 0, false)],
            }),
            Arc::new(FixtureIndexer {
                id: 2,
                kind,
                releases: vec![release(2, "Alternative", seeds, false)],
            }),
        ]);
        persist_indexer_identities(&state).await;
        let cfg = Config {
            nyaa_enabled: false,
            preferred_groups: "Premium".into(),
            finished_series_quality: "same_as_airing".into(),
            ..Default::default()
        };
        let pick = auto_search::find_best_for_target(
            &state.db,
            &detail(),
            &cfg,
            &auto_search::SearchTarget::Episode(1),
            false,
            false,
            &[],
            &state.indexers,
        )
        .await
        .unwrap();
        assert_eq!(pick.indexer_id, Some(2));
        assert!(pick.title.contains("Alternative"));
        // Manual interactive search still exposes the preferred zero-seed row.
        let manual = auto_search::find_all_for_target(
            &state.db,
            &detail(),
            &cfg,
            &auto_search::SearchTarget::Episode(1),
            false,
            &[],
            &state.indexers,
        )
        .await;
        assert!(
            manual
                .iter()
                .any(|r| r.indexer_id == Some(1) && r.seeders == 0)
        );
    }
}

#[tokio::test]
async fn automatic_availability_filters_before_duplicate_source_attribution() {
    let state = build_test_app_state(in_memory_pool().await, None);
    let mut preferred = release(1, "Premium", 0, false);
    preferred.info_hash = HASH.into();
    preferred.indexer_priority = 1;
    let mut alternative = release(2, "Alternative", 3, false);
    alternative.info_hash = HASH.into();
    *state.indexers.write().await = Arc::new(vec![
        Arc::new(FixtureIndexer {
            id: 1,
            kind: "torznab",
            releases: vec![preferred],
        }),
        Arc::new(FixtureIndexer {
            id: 2,
            kind: "torznab",
            releases: vec![alternative],
        }),
    ]);
    persist_indexer_identities(&state).await;
    let cfg = Config {
        nyaa_enabled: false,
        finished_series_quality: "same_as_airing".into(),
        ..Default::default()
    };
    let pick = auto_search::find_best_for_target(
        &state.db,
        &detail(),
        &cfg,
        &auto_search::SearchTarget::Episode(1),
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
    .unwrap();
    assert_eq!(
        pick.indexer_id,
        Some(2),
        "stalled priority-1 source must not hide a healthy copy"
    );
    assert_eq!(pick.seeders, 3);
}

#[tokio::test]
async fn automatic_availability_batch_collection_gates_automation_but_keeps_manual_choices() {
    let state = build_test_app_state(in_memory_pool().await, None);
    let pack = release(1, "Premium", 0, true);
    assert!(pack.clone().into_search_result().is_batch);
    *state.indexers.write().await = Arc::new(vec![Arc::new(FixtureIndexer {
        id: 1,
        kind: "torznab",
        releases: vec![pack],
    })]);
    persist_indexer_identities(&state).await;
    let mut cfg = Config {
        nyaa_enabled: false,
        finished_series_quality: "same_as_airing".into(),
        ..Default::default()
    };
    for automatic in [true, false] {
        let hits = auto_search::collect_scored_batches_for_target(
            &state.db,
            &detail(),
            &cfg,
            &auto_search::SearchTarget::Single,
            &[],
            &state.indexers,
            automatic,
        )
        .await;
        assert_eq!(hits.is_empty(), automatic);
    }
    cfg.minimum_torrent_seeders = 0;
    let hits = auto_search::collect_scored_batches_for_target(
        &state.db,
        &detail(),
        &cfg,
        &auto_search::SearchTarget::Single,
        &[],
        &state.indexers,
        true,
    )
    .await;
    assert!(
        !hits.is_empty(),
        "explicit zero disables the new global floor"
    );
}

//! Exercise late RSS admission, rather than just its pure revision verdict.
use super::super::*;
use super::publication_date::NoPayloadClient;
use crate::models::direct_rss_feeds::{self, DirectRssFeedForm};
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn fixture(
    previous: &str,
    revision: bool,
) -> (
    AppState,
    Arc<NoPayloadClient>,
    tempfile::TempDir,
    MockServer,
) {
    let db = in_memory_pool().await;
    let root = tempfile::tempdir().unwrap();
    let cfg = config::Config {
        rss_enabled: false,
        disable_nyaa_rss: true,
        rss_master_enabled: true,
        post_processing_enabled: true,
        media_root: root.path().to_string_lossy().into(),
        media_requirements_enabled: false,
        seadex_enabled: false,
        proper_policy: "prefer_and_upgrade".into(),
        ..Default::default()
    };
    config::save_config(&db, &cfg).await.unwrap();
    let sid = seed_series(&db, 101, "Test Series").await;
    sqlx::query("UPDATE series SET format='TV',status='RELEASING',episodes=12,monitor_mode='all',folder_name='Test',title_romaji='Test Series' WHERE id=?").bind(sid).execute(&db).await.unwrap();
    let detail:crate::services::anilist::AnimeDetail=serde_json::from_value(serde_json::json!({
        "id":101,"id_mal":null,"title_romaji":"Test Series","title_english":"Test Series","title_native":"",
        "cover_url":"","banner_url":"","format":"TV","status":"RELEASING","status_display":"Airing","episodes":12,
        "season":"","description":"","genres":[],"score_is_ten_point":false,"score_class":"","synonyms":[],"streaming_episodes":[],"relations":[]
    })).unwrap();
    crate::models::metadata_cache::upsert(&db, sid, 101, None, &detail)
        .await
        .unwrap();
    let old = if revision {
        "[SubsPlease] Test Series - 07 [1080p WEB-DL]"
    } else {
        "[Old] Test Series - 07 [1080p WEB-DL]"
    };
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason,download_client_id) VALUES(?,?,?,'[7]',?,'requirements_rejected',1)")
        .bind("a".repeat(40)).bind(old).bind(sid).bind(previous).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO rss_seen(item_key,title,link,series_id,decision,reason) VALUES('old',?,'https://example.invalid/old',?,'grabbed','Grabbed')").bind(old).bind(sid).execute(&db).await.unwrap();
    let server = MockServer::start().await;
    let title = if revision {
        "[SubsPlease] Test Series - 07v2 [1080p WEB-DL]"
    } else {
        "[SubsPlease] Test Series - 07 [1080p WEB-DL]"
    };
    let hash = "b".repeat(40);
    let feed = format!(
        "<rss xmlns:nyaa=\"https://nyaa.si/xmlns/nyaa\"><channel><item><title>{title}</title><guid>new</guid><link>https://example.invalid/new</link><nyaa:magneturi>magnet:?xt=urn:btih:{hash}</nyaa:magneturi><nyaa:infohash>{hash}</nyaa:infohash><nyaa:seeders>10</nyaa:seeders></item></channel></rss>"
    );
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_string(feed))
        .mount(&server)
        .await;
    direct_rss_feeds::insert(
        &db,
        DirectRssFeedForm {
            name: "Fixture",
            url: &format!("{}/feed", server.uri()),
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        },
    )
    .await
    .unwrap();
    let client = Arc::new(NoPayloadClient(AtomicUsize::new(0)));
    let state = build_test_app_state(db, Some(client.clone()));
    (state, client, root, server)
}
#[tokio::test]
async fn failed_rss_source_allows_an_alternative_but_keeps_its_exact_block() {
    let (state, client, _root, _server) = fixture("failed", false).await;
    let result = sync_once(&state, "test").await.unwrap();
    let decisions = crate::models::rss::recent_decisions(&state.db, 10)
        .await
        .unwrap();
    assert_eq!(result.grabbed, 1, "{decisions:?}");
    assert_eq!(client.0.load(Ordering::SeqCst), 1);
    assert!(
        crate::models::grabbed_torrents::is_blocklisted_release(
            &state.db,
            1,
            &"a".repeat(40),
            "[Old] Test Series - 07 [1080p WEB-DL]"
        )
        .await
    );
}
#[tokio::test]
async fn late_admission_preserves_authorized_same_group_queued_revision() {
    let (state, client, _root, _server) = fixture("pending", true).await;
    let result = sync_once(&state, "test").await.unwrap();
    let decisions = crate::models::rss::recent_decisions(&state.db, 10)
        .await
        .unwrap();
    assert_eq!(result.grabbed, 1, "{decisions:?}");
    assert_eq!(client.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn late_rss_settings_receipt_ignores_poll_metrics_but_holds_policy_or_client_changes() {
    let (state, _client, _root, _server) = fixture("failed", false).await;
    let cfg = config::get_config(&state.db).await.unwrap().unwrap();
    let row = series::get_by_id(&state.db, 1).await.unwrap().unwrap();
    crate::services::monitoring::ensure_series_monitoring_rows(&state.db, &row)
        .await
        .unwrap();
    let monitored = monitoring::get_monitored_episode_numbers(&state.db, 1)
        .await
        .unwrap()
        .into_iter()
        .collect();
    let effective = crate::services::language_preferences::effective_config(&state.db, &cfg, 1)
        .await
        .unwrap();
    let snapshot = history::Settings::capture(&state, &cfg).await.unwrap();
    sqlx::query("UPDATE direct_rss_feeds SET last_polled_at=1,last_item_count=20")
        .execute(&state.db)
        .await
        .unwrap();
    assert!(
        snapshot
            .recheck(&state, &row, &effective, &monitored)
            .await
            .is_ok()
    );
    let mut changed = cfg.clone();
    changed.rss_master_enabled = false;
    config::save_config(&state.db, &changed).await.unwrap();
    assert!(
        snapshot
            .recheck(&state, &row, &effective, &monitored)
            .await
            .is_err()
    );
    config::save_config(&state.db, &cfg).await.unwrap();
    sqlx::query("INSERT INTO download_clients(name,kind,url,enabled) VALUES('changed','qbittorrent','http://fixture',0)").execute(&state.db).await.unwrap();
    assert!(
        snapshot
            .recheck(&state, &row, &effective, &monitored)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn source_failed_while_waiting_for_admission_is_not_added_again() {
    let (state, client, _root, _server) = fixture("failed", false).await;
    let config_lock = crate::handlers::settings::CONFIG_WRITE_LOCK.lock().await;
    let search_state = state.clone();
    let task = tokio::spawn(async move { sync_once(&search_state, "test").await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if crate::services::post_processing::POST_PROC_LOCK
                .try_lock()
                .is_err()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("RSS reached final admission after discovery");
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason,download_client_id) VALUES(?,'[SubsPlease] Test Series - 07 [1080p WEB-DL]',1,'[7]','failed','client_error',1)")
        .bind("b".repeat(40)).execute(&state.db).await.unwrap();
    drop(config_lock);
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.grabbed, 0);
    assert_eq!(client.0.load(Ordering::SeqCst), 0);
    let decisions = crate::models::rss::recent_decisions(&state.db, 10)
        .await
        .unwrap();
    assert!(
        decisions
            .iter()
            .any(|d| d.reason.contains("excluded after RSS discovery")),
        "{decisions:?}"
    );
}

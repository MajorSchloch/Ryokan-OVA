use super::super::*;
use crate::services::download_client::qbittorrent::QbitClient;
use crate::test_support::{in_memory_pool, seed_series};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn fixture(state: &str) -> (sqlx::SqlitePool, Vec<SeriesMeta>) {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 101, "Test Series").await;
    sqlx::query("UPDATE series SET episodes=12,format='TV',monitor_mode='all' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,download_client_id) VALUES(?,'[Old] Test Series - 07 (1080p)',?,'[7]',?,1)")
        .bind("a".repeat(40)).bind(id).bind(state).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO rss_seen(item_key,title,link,series_id,decision,reason) VALUES('old','[Old] Test Series - 07 (1080p)','https://example.test/old',?,'grabbed','Grabbed')")
        .bind(id).execute(&db).await.unwrap();
    let row = series::get_by_id(&db, id).await.unwrap().unwrap();
    (db, vec![SeriesMeta::from_series(&row)])
}
fn candidate(meta: &[SeriesMeta]) -> MatchResult {
    let item = RssItem {
        published_at: None,
        seeders: None,
        title: "[New] Test Series - 07 (1080p)".into(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: "New".into(),
        resolution: "1080p".into(),
        is_batch: false,
        source: RssSource::Nyaa,
    };
    best_series_match(&item, meta).unwrap()
}
async fn native(state: &str) -> (MockServer, QbitClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/api/v2/torrents/info")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{
        "hash":"a".repeat(40),"name":"[Old] Test Series - 07 (1080p)","size":100,"progress":1.0,"dlspeed":0,"state":state,"category":"test","eta":0
    }]))).mount(&server).await;
    let client = QbitClient::new(&server.uri(), "user", "password", "test");
    (server, client)
}
#[tokio::test]
async fn terminal_attempts_do_not_block_an_alternative_group_or_erase_history() {
    for state in ["failed", "removed", "cancelled", "replaced", "imported"] {
        let (db, meta) = fixture(state).await;
        let history = history::load(&db, None, Some(1), &meta).await.unwrap();
        assert!(!history.contains(&candidate(&meta), false, 1), "{state}");
        let decision: String =
            sqlx::query_scalar("SELECT decision FROM rss_seen WHERE item_key='old'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(decision, "grabbed");
    }
}
#[tokio::test]
async fn failed_native_job_does_not_recreate_the_logical_blocker() {
    for native_state in ["stoppedUP", "downloading", "error"] {
        let (db, meta) = fixture("failed").await;
        let (_server, client) = native(native_state).await;
        let history = history::load(&db, Some(&client), Some(1), &meta)
            .await
            .unwrap();
        assert!(!history.contains(&candidate(&meta), false, 1));
    }
}
#[tokio::test]
async fn pending_owner_still_blocks_while_native_job_downloads_or_awaits_import() {
    for native_state in ["downloading", "stoppedUP", "stalledUP"] {
        let (db, meta) = fixture("pending").await;
        let (_server, client) = native(native_state).await;
        let history = history::load(&db, Some(&client), Some(1), &meta)
            .await
            .unwrap();
        assert!(history.contains(&candidate(&meta), false, 1));
    }
}
#[tokio::test]
async fn imported_seeding_sources_leave_replacements_to_the_file_requirement_gate() {
    let (db, meta) = fixture("imported").await;
    let (_server, client) = native("uploading").await;
    let history = history::load(&db, Some(&client), Some(1), &meta)
        .await
        .unwrap();
    assert!(!history.contains(&candidate(&meta), false, 1));
}
#[tokio::test]
async fn routed_pending_ownership_uses_its_saved_episode_instead_of_rematching_the_title() {
    let (db, meta) = fixture("pending").await;
    let id = meta[0].series.id;
    sqlx::query("UPDATE grabbed_torrents SET torrent_name='[Old] A Different Heading - 01',episode_numbers='[1]'").execute(&db).await.unwrap();
    sqlx::query("INSERT INTO grabbed_torrent_series(grab_id,series_id,episode_numbers) SELECT id,?,'[7]' FROM grabbed_torrents").bind(id).execute(&db).await.unwrap();
    let history = history::load(&db, None, Some(1), &meta).await.unwrap();
    assert!(history.contains(&candidate(&meta), false, 1));
}

#[tokio::test]
async fn unstamped_or_other_client_terminal_rows_do_not_hide_active_native_jobs() {
    for client in [None, Some(2)] {
        let (db, meta) = fixture("failed").await;
        sqlx::query("UPDATE grabbed_torrents SET download_client_id=?")
            .bind(client)
            .execute(&db)
            .await
            .unwrap();
        let (_server, native) = native("downloading").await;
        assert!(
            history::load(&db, Some(&native), Some(1), &meta)
                .await
                .unwrap()
                .contains(&candidate(&meta), false, 1)
        );
    }
}

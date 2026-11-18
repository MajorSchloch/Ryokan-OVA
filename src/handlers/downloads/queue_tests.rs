use super::*;
use crate::services::download_client::{AddOutcome, DownloadFile, DownloadItem, SelectiveOutcome};
use std::sync::atomic::{AtomicBool, Ordering};

struct Client {
    items: Vec<DownloadItem>,
    fail: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl DownloadClient for Client {
    async fn test(&self) -> Result<String, String> {
        unreachable!()
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        unreachable!()
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        unreachable!()
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        if self.fail.load(Ordering::SeqCst) {
            Err("private transport diagnostic".into())
        } else {
            Ok(self.items.clone())
        }
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        panic!("Browsing must not probe source files")
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        unreachable!()
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        unreachable!()
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "NZBGet"
    }
    fn protocol(&self) -> &'static str {
        "usenet"
    }
}

fn item(hash: &str, name: &str) -> DownloadItem {
    DownloadItem {
        hash: hash.into(),
        name: name.into(),
        size: 1 << 30,
        progress: 1.0,
        dlspeed: 0,
        state: "SUCCESS/ALL".into(),
        category: "anime".into(),
        eta: 0,
        save_path: String::new(),
        content_path: String::new(),
        state_kind: DownloadItemState::PausedComplete,
        seeding_done: false,
    }
}

async fn page(state: AppState, fragment: Option<&str>) -> (StatusCode, String) {
    let response = downloads_page(
        State(state),
        Query(DownloadsQuery {
            tab: Some("queue".into()),
            fragment: fragment.map(str::to_owned),
        }),
    )
    .await;
    let status = response.status();
    (
        status,
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn queue_and_poll_keep_client_scoping_and_truthful_history_on_failures() {
    let db = crate::test_support::in_memory_pool().await;
    let sid = crate::test_support::seed_series(&db, 123, "Fixture").await;
    for (hash, status) in [
        ("same", "imported"),
        ("pending", "pending"),
        ("rejected", "failed"),
        ("missing-time", "imported"),
        ("empty-time", "imported"),
        ("bad-time", "imported"),
    ] {
        let id = crate::test_support::seed_grabbed_torrent(&db, sid, hash, hash, &[1]).await;
        sqlx::query("UPDATE grabbed_torrents SET download_client_id=7,state=?,failure_reason='requirements_rejected',imported_at=CASE WHEN ?='imported' THEN CURRENT_TIMESTAMP ELSE NULL END WHERE id=?")
            .bind(status).bind(status).bind(id).execute(&db).await.unwrap();
    }
    for (hash, time) in [
        ("missing-time", None),
        ("empty-time", Some("")),
        ("bad-time", Some("not-a-date")),
    ] {
        sqlx::query("UPDATE grabbed_torrents SET imported_at=? WHERE hash=?")
            .bind(time)
            .bind(hash)
            .execute(&db)
            .await
            .unwrap();
    }
    let before: Vec<(String, String)> =
        sqlx::query_as("SELECT hash,state FROM grabbed_torrents ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    let state = crate::test_support::build_test_app_state(db.clone(), None);
    let fail = Arc::new(AtomicBool::new(false));
    let clients: HashMap<i64, Arc<dyn DownloadClient>> = HashMap::from([
        (
            7,
            Arc::new(Client {
                items: vec![
                    item("same", "Imported source"),
                    item("pending", "Awaiting import source"),
                    item("rejected", "Rejected source"),
                    item("missing-time", "Missing completion time"),
                    item("empty-time", "Empty completion time"),
                    item("bad-time", "Malformed completion time"),
                ],
                fail: fail.clone(),
            }) as Arc<dyn DownloadClient>,
        ),
        (
            8,
            Arc::new(Client {
                items: vec![item("same", "Unowned source from another client")],
                fail: fail.clone(),
            }) as Arc<dyn DownloadClient>,
        ),
    ]);
    *state.download_clients.write().await = Arc::new(crate::DownloadClientPool {
        clients,
        default_torrent_id: None,
        default_usenet_id: Some(7),
    });
    for fragment in [None, Some("queue")] {
        let (status, html) = page(state.clone(), fragment).await;
        assert_eq!(status, StatusCode::OK);
        let (work, history) = html.split_once("id=\"usenet-history\"").unwrap();
        assert!(
            work.contains("Awaiting import source")
                && work.contains("Unowned source from another client")
        );
        assert!(work.contains("Import status unavailable"));
        assert!(
            work.contains("Missing completion time")
                && work.contains("Empty completion time")
                && work.contains("Malformed completion time")
        );
        assert!(
            !history.contains("Missing completion time")
                && !history.contains("Empty completion time")
                && !history.contains("Malformed completion time")
        );
        assert!(!work.contains("Imported source") && !work.contains("Rejected source"));
        assert!(history.contains("Imported source") && history.contains("Requirements rejected"));
        assert!(
            !history.contains("data-dl-action=\"resume\"")
                && !history.contains("data-dl-action=\"pause\"")
        );
    }
    let old_pool = state.download_clients.read().await.clone();
    let mut clients = old_pool.clients.clone();
    clients.insert(
        9,
        Arc::new(Client {
            items: vec![],
            fail: Arc::new(AtomicBool::new(true)),
        }),
    );
    *state.download_clients.write().await = Arc::new(crate::DownloadClientPool {
        clients,
        default_torrent_id: None,
        default_usenet_id: Some(7),
    });
    let (status, html) = page(state.clone(), Some("queue")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("other clients are still shown") && html.contains("Imported source"));
    assert!(!html.contains("private transport diagnostic"));
    let after: Vec<(String, String)> =
        sqlx::query_as("SELECT hash,state FROM grabbed_torrents ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(before, after);
    sqlx::query("DROP TABLE grabbed_torrents")
        .execute(&db)
        .await
        .unwrap();
    let (status, html) = page(state.clone(), Some("queue")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Import status is unavailable") && html.contains("Imported source"));
    assert!(!html.contains("id=\"usenet-history\""));
    fail.store(true, Ordering::SeqCst);
    let (status, html) = page(state, Some("queue")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(html.contains("last displayed queue is retained"));
}

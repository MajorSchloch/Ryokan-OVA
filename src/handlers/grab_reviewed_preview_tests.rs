use super::*;
use crate::services::download_client::{
    DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

const HASH: &str = "91a564c6d0f605344b67cef81eeab3199d61872b";

struct LegacyOnly {
    events: Arc<Mutex<Vec<&'static str>>>,
    files_ready: bool,
}

#[async_trait]
impl DownloadClient for LegacyOnly {
    async fn test(&self) -> Result<String, String> {
        Ok("private fixture".into())
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("preview must not add a whole download")
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("preview must not use optional selection")
    }
    async fn add_torrent_paused(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        self.events.lock().unwrap().push("legacy");
        Ok(AddOutcome::Added)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(Vec::new())
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        self.events.lock().unwrap().push("files");
        Ok(if self.files_ready {
            lagoon_files()
                .into_iter()
                .map(|name| DownloadFile {
                    name,
                    size: 100,
                    progress: 0.0,
                    wanted: true,
                })
                .collect()
        } else {
            Vec::new()
        })
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        panic!("preview cannot take over a job")
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        panic!("preview cannot start payload")
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("preview cannot delete client data")
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        panic!("preview cannot change file selection")
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

struct NativeClient {
    legacy: LegacyOnly,
    outcome: AddOutcome,
}

#[async_trait]
impl DownloadClient for NativeClient {
    async fn test(&self) -> Result<String, String> {
        self.legacy.test().await
    }
    async fn add_torrent(&self, url: &str, hash: &str) -> Result<AddOutcome, String> {
        self.legacy.add_torrent(url, hash).await
    }
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        self.legacy
            .add_torrent_with_file_filter(url, hash, pick)
            .await
    }
    async fn add_torrent_paused(&self, url: &str, hash: &str) -> Result<AddOutcome, String> {
        self.legacy.add_torrent_paused(url, hash).await
    }
    async fn add_torrent_metadata_only(&self, hash: &str) -> Result<AddOutcome, String> {
        assert_eq!(hash, HASH);
        self.legacy.events.lock().unwrap().push("native");
        Ok(self.outcome)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        self.legacy.list_scoped().await
    }
    async fn get_files(&self, hash: &str) -> Result<Vec<DownloadFile>, String> {
        self.legacy.get_files(hash).await
    }
    async fn pause(&self, hash: &str) -> Result<(), String> {
        self.legacy.pause(hash).await
    }
    async fn resume(&self, hash: &str) -> Result<(), String> {
        self.legacy.resume(hash).await
    }
    async fn delete(&self, hash: &str, files: bool) -> Result<(), String> {
        self.legacy.delete(hash, files).await
    }
    async fn set_file_wanted(
        &self,
        hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        self.legacy.set_file_wanted(hash, files, wanted).await
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

async fn state(client: Arc<dyn DownloadClient>, reviewed: bool) -> (AppState, i64) {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    if reviewed {
        crate::models::source_numbering::save_reviewed(&db, &lagoon_proof())
            .await
            .unwrap();
    }
    (build_test_app_state(db, Some(client)), id)
}

fn form(series_id: Option<i64>) -> GrabPreviewForm {
    GrabPreviewForm {
        url: "https://nyaa.si/download/1428087.torrent".into(),
        info_hash: HASH.into(),
        series_id,
        release_metadata: serde_json::json!({"title":"Black Lagoon complete source","is_batch":true}),
        indexer_id: None,
    }
}

#[tokio::test]
async fn reviewed_picker_uses_native_contract_and_never_starts_payload() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(NativeClient {
        legacy: LegacyOnly {
            events: events.clone(),
            files_ready: true,
        },
        outcome: AddOutcome::Added,
    });
    let (state, id) = state(client, true).await;
    let Json(created) = grab_preview(State(state.clone()), Json(form(Some(id))))
        .await
        .unwrap();
    for _ in 0..100 {
        let row = pending_grabs::get(&state.db, &created.preview_id)
            .await
            .unwrap()
            .unwrap();
        if !row.file_list_json.is_empty() {
            assert!(row.we_added_torrent);
            assert_eq!(events.lock().unwrap().first(), Some(&"native"));
            assert!(!events.lock().unwrap().contains(&"legacy"));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("private metadata was not persisted");
}

#[tokio::test]
async fn reviewed_picker_unsupported_client_holds_before_legacy_add() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(LegacyOnly {
        events: events.clone(),
        files_ready: true,
    });
    let (state, id) = state(client, true).await;
    assert!(
        grab_preview(State(state.clone()), Json(form(Some(id))))
            .await
            .is_err()
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pending_grabs::count(&state.db).await.unwrap(), 0);
}

#[tokio::test]
async fn reviewed_picker_lookup_failure_cannot_downgrade_to_legacy_add() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(LegacyOnly {
        events: events.clone(),
        files_ready: true,
    });
    let (state, id) = state(client, true).await;
    sqlx::query("DROP TABLE source_numbering_decisions")
        .execute(&state.db)
        .await
        .unwrap();
    assert!(
        grab_preview(State(state.clone()), Json(form(Some(id))))
            .await
            .is_err()
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pending_grabs::count(&state.db).await.unwrap(), 0);
}

#[tokio::test]
async fn reviewed_picker_preserves_preexisting_stopped_metadata_empty_job() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(NativeClient {
        legacy: LegacyOnly {
            events: events.clone(),
            files_ready: false,
        },
        outcome: AddOutcome::AlreadyPresent,
    });
    let (state, id) = state(client, true).await;
    let error = grab_preview(State(state.clone()), Json(form(Some(id))))
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(*events.lock().unwrap(), ["native"]);
    assert_eq!(pending_grabs::count(&state.db).await.unwrap(), 0);
}

#[tokio::test]
async fn reviewed_picker_without_owner_holds_before_client_add() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(LegacyOnly {
        events: events.clone(),
        files_ready: true,
    });
    let (state, _) = state(client, true).await;
    assert!(
        grab_preview(State(state.clone()), Json(form(None)))
            .await
            .is_err()
    );
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unreviewed_picker_keeps_legacy_client_support() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(LegacyOnly {
        events: events.clone(),
        files_ready: true,
    });
    let (state, id) = state(client, false).await;
    assert!(
        grab_preview(State(state), Json(form(Some(id))))
            .await
            .is_ok()
    );
    assert_eq!(events.lock().unwrap().first(), Some(&"legacy"));
}

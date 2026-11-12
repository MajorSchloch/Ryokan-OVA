//! Real picker and TTL entry points around the reviewed-source resume boundary.
use super::*;
use crate::models::{episode_tags, source_numbering};
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use crate::services::release_numbering::ReleaseMapping;
use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;
use sqlx::Row;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const PREVIEW: &str = "private-bound-picker";
const HASH: &str = "91a564c6d0f605344b67cef81eeab3199d61872b";

#[derive(Clone, Copy, Debug)]
enum Surface {
    Confirm,
    Walkaway,
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    None,
    Pause,
    PriorityRpc,
    ReadbackRpc,
    IgnoredPriority,
}

struct BoundClient {
    db: sqlx::SqlitePool,
    series_id: i64,
    files: Mutex<Vec<DownloadFile>>,
    events: Mutex<Vec<&'static str>>,
    reads: AtomicUsize,
    paused: AtomicBool,
    failure: Failure,
}

impl BoundClient {
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl DownloadClient for BoundClient {
    async fn test(&self) -> Result<String, String> {
        Ok("private picker fixture".into())
    }

    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("confirming a paused preview must not add another job");
    }

    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("a bound picker must not fall back to a whole download");
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(Vec::new())
    }

    async fn get_files(&self, hash: &str) -> Result<Vec<DownloadFile>, String> {
        assert_eq!(hash, HASH);
        self.events.lock().unwrap().push("read");
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        if matches!(self.failure, Failure::ReadbackRpc) && read > 0 {
            return Err("private priority readback failure".into());
        }
        Ok(self.files.lock().unwrap().clone())
    }

    async fn pause(&self, hash: &str) -> Result<(), String> {
        assert_eq!(hash, HASH);
        if matches!(self.failure, Failure::Pause) {
            self.events.lock().unwrap().push("pause-refused");
            return Err("private pause failure".into());
        }
        self.paused.store(true, Ordering::SeqCst);
        self.events.lock().unwrap().push("pause");
        Ok(())
    }

    async fn set_file_wanted(
        &self,
        hash: &str,
        indices: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        assert_eq!(hash, HASH);
        assert!(self.paused.load(Ordering::SeqCst));
        assert!(
            self.events().contains(&"pause"),
            "the source must be explicitly paused before any priority write"
        );
        self.events
            .lock()
            .unwrap()
            .push(if wanted { "wanted" } else { "unwanted" });
        if matches!(self.failure, Failure::PriorityRpc) {
            return Err("private priority write failure".into());
        }
        if !matches!(self.failure, Failure::IgnoredPriority) {
            let mut files = self.files.lock().unwrap();
            for index in indices {
                files[*index].wanted = wanted;
            }
        }
        Ok(())
    }

    async fn resume(&self, hash: &str) -> Result<(), String> {
        assert_eq!(hash, HASH);
        assert!(self.paused.load(Ordering::SeqCst));
        // Read durable ownership from the real database at the exact moment
        // network activity would resume, not after the handler has returned.
        let row = sqlx::query("SELECT series_id,episode_numbers,release_mapping FROM grabbed_torrents WHERE hash=? AND state='pending'")
            .bind(hash)
            .fetch_one(&self.db)
            .await
            .expect("canonical ownership must exist before resume");
        assert_eq!(row.get::<i64, _>("series_id"), self.series_id);
        let episodes: Vec<i32> = serde_json::from_str(row.get("episode_numbers")).unwrap();
        assert_eq!(episodes, (1..=12).collect::<Vec<_>>());
        let mapping: ReleaseMapping = serde_json::from_str(row.get("release_mapping")).unwrap();
        assert_eq!(mapping.episodes(), episodes);
        assert_eq!(mapping.bound_source().unwrap(), &lagoon_proof());
        let tags = episode_tags::get_for_series(&self.db, self.series_id)
            .await
            .unwrap();
        assert_eq!(tags.len(), 12);
        assert!(
            (1..=12).all(|episode| tags.get(&episode).is_some_and(|tag| tag.state == "grabbed"))
        );
        assert!(
            pending_grabs::get(&self.db, PREVIEW)
                .await
                .unwrap()
                .is_some()
        );
        let files = self.files.lock().unwrap();
        assert!(
            files
                .iter()
                .enumerate()
                .all(|(index, file)| file.wanted == (12..24).contains(&index))
        );
        drop(files);
        self.paused.store(false, Ordering::SeqCst);
        self.events.lock().unwrap().push("resume-after-record");
        Ok(())
    }

    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("a reviewed-source hold must preserve client data");
    }

    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

struct Fixture {
    state: AppState,
    client: Arc<BoundClient>,
}

async fn fixture(failure: Failure) -> Fixture {
    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
    // Deliberately unrelated to this finite source proof: graph offsets must
    // not turn source13–24 into a different canonical episode set.
    sqlx::query("UPDATE series SET format='TV',cumulative_prior_episodes=200 WHERE id=?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    source_numbering::save_reviewed(&db, &lagoon_proof())
        .await
        .unwrap();
    let metadata = serde_json::json!({
        "title": "[Legion] Black Lagoon - 01-29 [BD 1080p]",
        "is_batch": true,
        "magnet": format!("magnet:?xt=urn:btih:{HASH}"),
        "group": "Legion"
    })
    .to_string();
    pending_grabs::create(
        &db,
        PREVIEW,
        HASH,
        "qbittorrent",
        None,
        Some(series_id),
        &metadata,
        true,
        None,
    )
    .await
    .unwrap();
    let names = lagoon_files();
    let preview: Vec<_> = names
        .iter()
        .map(|name| PreviewFile {
            name: name.clone(),
            size: 1_000,
            episodes: Vec::new(),
        })
        .collect();
    pending_grabs::set_file_list(&db, PREVIEW, &serde_json::to_string(&preview).unwrap())
        .await
        .unwrap();
    pending_grabs::set_wanted_indices(
        &db,
        PREVIEW,
        &serde_json::to_string(&(12..24).collect::<Vec<_>>()).unwrap(),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE pending_grabs SET heartbeat_at=0 WHERE preview_id=?")
        .bind(PREVIEW)
        .execute(&db)
        .await
        .unwrap();
    let client = Arc::new(BoundClient {
        db: db.clone(),
        series_id,
        files: Mutex::new(
            names
                .into_iter()
                .map(|name| DownloadFile {
                    name,
                    size: 1_000,
                    progress: 0.0,
                    wanted: false,
                })
                .collect(),
        ),
        events: Mutex::new(Vec::new()),
        reads: AtomicUsize::new(0),
        paused: AtomicBool::new(true),
        failure,
    });
    Fixture {
        state: build_test_app_state(db, Some(client.clone())),
        client,
    }
}

async fn exercise(fixture: &Fixture, surface: Surface) -> bool {
    match surface {
        Surface::Confirm => match grab_confirm(
            State(fixture.state.clone()),
            Json(GrabConfirmForm {
                preview_id: PREVIEW.into(),
                wanted_indices: (12..24).collect(),
                unblock: false,
            }),
        )
        .await
        {
            Ok(Json(result)) => result.ok,
            Err(_) => false,
        },
        Surface::Walkaway => {
            assert_eq!(
                crate::services::grab_sweep::sweep_once(&fixture.state)
                    .await
                    .unwrap(),
                1
            );
            pending_grabs::get(&fixture.state.db, PREVIEW)
                .await
                .unwrap()
                .is_none()
        }
    }
}

async fn assert_held(fixture: &Fixture) {
    assert!(fixture.client.paused.load(Ordering::SeqCst));
    assert!(!fixture.client.events().contains(&"resume-after-record"));
    assert!(
        pending_grabs::get(&fixture.state.db, PREVIEW)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&fixture.state.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn bound_picker_and_walkaway_record_canonical_proof_before_resume() {
    for surface in [Surface::Confirm, Surface::Walkaway] {
        let fixture = fixture(Failure::None).await;
        assert!(exercise(&fixture, surface).await, "{surface:?}");
        let events = fixture.client.events();
        assert_eq!(
            events,
            [
                "read",
                "pause",
                "unwanted",
                "wanted",
                "read",
                "read",
                "resume-after-record"
            ]
        );
        assert!(
            pending_grabs::get(&fixture.state.db, PREVIEW)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn bound_picker_and_walkaway_changed_manifest_or_order_prevents_priority_writes() {
    for surface in [Surface::Confirm, Surface::Walkaway] {
        for change in 0..3 {
            let fixture = fixture(Failure::None).await;
            {
                let mut files = fixture.client.files.lock().unwrap();
                match change {
                    0 => files[0].name = "unselected source file changed.mkv".into(),
                    1 => files.swap(0, 1),
                    _ => files.push(DownloadFile {
                        name: "new checksum.sha256".into(),
                        size: 1,
                        progress: 0.0,
                        wanted: false,
                    }),
                }
            }
            assert!(
                !exercise(&fixture, surface).await,
                "{surface:?}, change {change}"
            );
            assert_eq!(fixture.client.events(), ["read"]);
            assert_held(&fixture).await;
        }
    }
}

#[tokio::test]
async fn bound_picker_and_walkaway_failed_pause_priority_or_readback_never_resumes() {
    for surface in [Surface::Confirm, Surface::Walkaway] {
        for failure in [
            Failure::Pause,
            Failure::PriorityRpc,
            Failure::ReadbackRpc,
            Failure::IgnoredPriority,
        ] {
            let fixture = fixture(failure).await;
            assert!(
                !exercise(&fixture, surface).await,
                "{surface:?}, {failure:?}"
            );
            if matches!(failure, Failure::Pause) {
                assert_eq!(fixture.client.events(), ["read", "pause-refused"]);
            }
            assert_held(&fixture).await;
        }
    }
}

#[tokio::test]
async fn bound_picker_and_walkaway_writer_failure_retains_paused_preview_across_sweeps() {
    for surface in [Surface::Confirm, Surface::Walkaway] {
        let fixture = fixture(Failure::None).await;
        sqlx::query("CREATE TRIGGER private_refuse_bound_grab BEFORE INSERT ON grabbed_torrents BEGIN SELECT RAISE(ABORT,'private writer failure'); END")
            .execute(&fixture.state.db).await.unwrap();
        assert!(!exercise(&fixture, surface).await, "{surface:?}");
        assert_held(&fixture).await;
        let events = fixture.client.events();
        assert!(events.contains(&"wanted"));
        assert!(
            pending_grabs::get(&fixture.state.db, PREVIEW)
                .await
                .unwrap()
                .unwrap()
                .error_message
                .contains("could not be saved")
        );
        // The next TTL tick must preserve the reviewed hold as well.
        assert_eq!(
            crate::services::grab_sweep::sweep_once(&fixture.state)
                .await
                .unwrap(),
            1
        );
        assert_held(&fixture).await;
        assert_eq!(fixture.client.events(), events);
    }
}

#[tokio::test]
async fn bound_picker_and_walkaway_corrupt_saved_proof_never_writes_priorities() {
    for surface in [Surface::Confirm, Surface::Walkaway] {
        let fixture = fixture(Failure::None).await;
        sqlx::query("UPDATE source_numbering_decisions SET proof_json='{}'")
            .execute(&fixture.state.db)
            .await
            .unwrap();
        assert!(!exercise(&fixture, surface).await, "{surface:?}");
        assert!(fixture.client.events().is_empty());
        assert_held(&fixture).await;
    }
}

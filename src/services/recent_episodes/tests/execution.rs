use super::*;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use crate::services::indexers::{Indexer, IndexerCaps, Release, SearchQuery};
use crate::test_support::build_test_app_state;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct AddGate {
    armed: AtomicBool,
    entered: Notify,
    finish: Notify,
}

struct Client {
    adds: AtomicUsize,
    duplicate: bool,
    gate: Option<AddGate>,
}
#[async_trait::async_trait]
impl DownloadClient for Client {
    async fn test(&self) -> Result<String, String> {
        Ok("fixture".into())
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        self.adds.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate
            && gate.armed.swap(false, Ordering::SeqCst)
        {
            gate.entered.notify_one();
            gate.finish.notified().await;
        }
        Ok(if self.duplicate {
            AddOutcome::AlreadyPresent
        } else {
            AddOutcome::Added
        })
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("recent recovery must not use pack selection")
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        panic!("existing state must be preserved")
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        panic!("recent recovery must not resume existing jobs")
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("recent recovery must not delete")
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        panic!("recent recovery must not change selections")
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}
struct Source {
    db: SqlitePool,
    sid: i64,
    mode: u8,
    changed: AtomicBool,
}
#[async_trait::async_trait]
impl Indexer for Source {
    fn id(&self) -> i64 {
        91
    }
    fn name(&self) -> &str {
        "Fixture"
    }
    fn priority(&self) -> i32 {
        1
    }
    fn is_private_tracker(&self) -> bool {
        false
    }
    async fn caps(&self) -> Result<IndexerCaps, String> {
        Ok(Default::default())
    }
    async fn search(&self, _: &SearchQuery) -> Result<Vec<Release>, String> {
        if !self.changed.swap(true, Ordering::SeqCst) {
            match self.mode {
                1 => crate::models::monitoring::set_episode_monitored(&self.db, self.sid, 7, false)
                    .await
                    .unwrap(),
                2 => {
                    sqlx::query("UPDATE config SET required_audio_languages='es'")
                        .execute(&self.db)
                        .await
                        .unwrap();
                }
                3 => {
                    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state) VALUES('other','other source',?,'[7]','pending')").bind(self.sid).execute(&self.db).await.unwrap();
                }
                7 => {
                    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason) VALUES(?,'[SubsPlease] Test Series - 07 [1080p WEB-DL]',?,'[7]','failed','client_error')").bind("a".repeat(40)).bind(self.sid).execute(&self.db).await.unwrap();
                }
                _ => {}
            }
        }
        let episodes: &[i32] = if self.mode == 8 {
            &[2, 7, 8, 9, 10]
        } else {
            &[7]
        };
        Ok(episodes
            .iter()
            .map(|&episode| {
                let hash = if episode == 7 { "a".repeat(40) } else { format!("{episode:040x}") };
                let title = if self.mode == 5 {
                    "[SubsPlease] Test Series - 01-12 [1080p WEB-DL] [Batch]".into()
                } else {
                    format!("[SubsPlease] Test Series - {episode:02} [1080p WEB-DL]")
                };
                serde_json::from_value(serde_json::json!({
                    "indexer_id":91,"indexer_priority":1,"indexer_name":"Fixture",
                    "title":title,"guid":format!("episode-{episode}"),"link":"https://example.invalid/release.torrent",
                    "magnet":format!("magnet:?xt=urn:btih:{hash}"),
                    "publish_date":0,"size_bytes":500000000,"seeders":10,"leechers":0,
                    "info_hash":hash,"categories":[5070],
                    "download_volume_factor":null,"upload_volume_factor":null
                })).unwrap()
            })
            .collect())
    }
}
async fn state(mode: u8) -> (AppState, Arc<Client>, tempfile::TempDir) {
    let (db, mut cfg, t, root) = fixture().await;
    cfg.nyaa_enabled = false;
    cfg.seadex_enabled = false;
    cfg.media_requirements_enabled = true;
    cfg.required_audio_languages = "ja".into();
    cfg.required_subtitle_languages = "en".into();
    if mode == 8 {
        cfg.rss_interval_minutes = 1;
    }
    config::save_config(&db, &cfg).await.unwrap();
    sqlx::query("INSERT INTO indexers(id,name,kind,url,min_seeders,enabled) VALUES(91,'Fixture','torznab','https://example.invalid',1,1)").execute(&db).await.unwrap();
    sqlx::query("INSERT OR REPLACE INTO download_clients(id,name,kind,url,enabled,is_default) VALUES(1,'Fixture','qbittorrent','https://example.invalid',1,1)").execute(&db).await.unwrap();
    let detail=serde_json::from_value(serde_json::json!({
        "id":101,"id_mal":null,"title_romaji":"Test Series","title_english":"Test Series","title_native":"",
        "cover_url":"","banner_url":"","format":"TV","status":"RELEASING","status_display":"Airing","episodes":12,
        "season":"","description":"","genres":[],"score_is_ten_point":false,"score_class":"","synonyms":[],"streaming_episodes":[],"relations":[]
    })).unwrap();
    crate::models::metadata_cache::upsert(&db, t.series_id, 101, None, &detail)
        .await
        .unwrap();
    let client = Arc::new(Client {
        adds: AtomicUsize::new(0),
        duplicate: mode == 6,
        gate: (mode == 8).then(AddGate::default),
    });
    let state = build_test_app_state(db.clone(), Some(client.clone()));
    *state.indexers.write().await = Arc::new(vec![Arc::new(Source {
        db,
        sid: t.series_id,
        mode,
        changed: AtomicBool::new(false),
    })]);
    (state, client, root)
}

async fn run_once(state: &AppState) -> Result<String, String> {
    super::run_once(state, &mut SearchTurn::default()).await
}

#[tokio::test]
async fn short_passes_finish_admitted_searches_discover_fresh_episodes_and_reserve_old_turns() {
    let (state, client, _root) = state(8).await;
    let sid = series::get_by_anilist_id(&state.db, 101)
        .await
        .unwrap()
        .unwrap()
        .id;
    let now = chrono::Utc::now().timestamp();
    for (episode, airing_at) in [
        (2, now - 400 * 86400),
        (8, now + 3600),
        (9, now - 7200),
        (10, now - 10800),
        (11, now - 600),
    ] {
        sqlx::query(
            "INSERT INTO episode_airings(series_id,episode,airing_at,duration_minutes) VALUES(?,?,?,24)",
        )
        .bind(sid)
        .bind(episode)
        .bind(airing_at)
        .execute(&state.db)
        .await
        .unwrap();
        crate::models::monitoring::set_episode_monitored(&state.db, sid, episode, true)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state)
         VALUES('pending','Pending source',?,'[11]','pending')",
    )
    .bind(sid)
    .execute(&state.db)
    .await
    .unwrap();

    let gate = client.gate.as_ref().unwrap();
    let mut turn = SearchTurn::default();
    for (pass, episode) in [7, 8, 9, 2, 10].into_iter().enumerate() {
        gate.armed.store(true, Ordering::SeqCst);
        let release_dispatch = async {
            tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
                .await
                .expect("recovery must reach the client");
            if pass == 0 {
                sqlx::query(
                    "UPDATE episode_airings SET airing_at=? WHERE series_id=? AND episode=8",
                )
                .bind(now - 1)
                .bind(sid)
                .execute(&state.db)
                .await
                .unwrap();
            }
            // Advance the deadline while the download client is busy.
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::time::resume();
            gate.finish.notify_one();
        };
        let (summary, ()) = tokio::join!(super::run_once(&state, &mut turn), release_dispatch);
        let summary = summary.unwrap();
        assert!(
            summary.contains("Checked 1 missing episodes: 1 queued"),
            "{summary}"
        );
        let finished: (i32, String) = sqlx::query_as(
            "SELECT json_extract(detail,'$.episode'),json_extract(detail,'$.outcome')
             FROM library_activity WHERE action=? ORDER BY id DESC LIMIT 1",
        )
        .bind(FINISHED)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(finished, (episode, "queued".into()));
        assert!(has_pending(&state.db, sid, episode).await.unwrap());
    }
    assert_eq!(client.adds.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn bounded_recovery_dispatches_one_missing_episode_once_and_records_the_outcome() {
    let (state, client, _root) = state(0).await;
    let result = run_once(&state).await.unwrap();
    assert!(result.contains("1 queued"), "{result}");
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    let pending = grabbed_torrents::get_all_pending(&state.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].episode_numbers, vec![7]);
    assert_eq!(pending[0].download_client_id, Some(1));
    assert!(run_once(&state).await.unwrap().contains("0 queued"));
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    let finished:i64=sqlx::query_scalar("SELECT COUNT(*) FROM library_activity WHERE action=? AND json_extract(detail,'$.episode')=7 AND json_extract(detail,'$.outcome')='queued'").bind(FINISHED).fetch_one(&state.db).await.unwrap();
    assert_eq!(finished, 1);
}
#[tokio::test]
async fn recovery_cooldown_uses_one_release_hold_and_leaves_search_slots_available() {
    let (state, client, _root) = state(0).await;
    let capped = seed_series(&state.db, 102, "Capped Series").await;
    sqlx::query(
        "UPDATE series SET format='TV',status='RELEASING',monitor_mode='all',folder_name='Capped' WHERE id=?",
    )
    .bind(capped)
    .execute(&state.db)
    .await
    .unwrap();
    // These episodes sort ahead of the fixture's older episode seven.
    let newer_airing = chrono::Utc::now().timestamp() - 1800;
    for episode in 1..=PASS_LIMIT as i32 + 1 {
        sqlx::query(
            "INSERT INTO episode_airings(series_id,episode,airing_at,duration_minutes) VALUES(?,?,?,24)",
        )
        .bind(capped)
        .bind(episode)
        .bind(newer_airing)
        .execute(&state.db)
        .await
        .unwrap();
        crate::models::monitoring::set_episode_monitored(&state.db, capped, episode, true)
            .await
            .unwrap();
    }
    for failure in 0..redownload::LOOP_BREAKER {
        sqlx::query(
            "INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state,failure_reason)
             VALUES(?,'Previous release',?,'[1]','failed','requirements_rejected')",
        )
        .bind(format!("failed-{failure}"))
        .bind(capped)
        .execute(&state.db)
        .await
        .unwrap();
    }

    let summary = run_once(&state).await.unwrap();
    assert!(
        summary.contains("Checked 1 missing episodes: 1 queued"),
        "{summary}"
    );
    assert!(
        summary.contains("Retry cooldown held 1 release."),
        "{summary}"
    );
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    run_once(&state).await.unwrap();
    let holds: Vec<(String, String)> =
        sqlx::query_as("SELECT action,reason FROM library_activity WHERE series_id=? ORDER BY id")
            .bind(capped)
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].0, COOLDOWN_HELD);
    assert!(holds[0].1.contains("Retry limit reached"));

    sqlx::query("UPDATE grabbed_torrents SET grabbed_at=datetime('now', ?) WHERE series_id=?")
        .bind(format!("-{} hours", redownload::WINDOW_HOURS + 1))
        .bind(capped)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(retry_limit_reason(&state.db, capped).await.unwrap(), None);
}

#[tokio::test]
async fn an_old_gap_is_revisited_when_a_suitable_source_appears_later() {
    let (state, client, _root) = state(0).await;
    let sources = state.indexers.read().await.clone();
    *state.indexers.write().await = Arc::new(Vec::new());
    sqlx::query("UPDATE episode_airings SET airing_at=strftime('%s','now')-400*86400")
        .execute(&state.db)
        .await
        .unwrap();
    let first = run_once(&state).await.unwrap();
    assert!(first.contains("0 queued"), "{first}");
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    *state.indexers.write().await = sources;
    let second = run_once(&state).await.unwrap();
    assert!(second.contains("1 queued"), "{second}");
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    let outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT json_extract(detail,'$.outcome') FROM library_activity WHERE action=? ORDER BY id",
    )
    .bind(FINISHED)
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(outcomes, vec!["held", "queued"]);
    assert!(run_once(&state).await.unwrap().contains("0 queued"));
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_repeat_hold_waits_without_spending_a_slot_and_revisits_when_due() {
    let (state, client, _root) = state(0).await;
    let sources = state.indexers.read().await.clone();
    *state.indexers.write().await = Arc::new(Vec::new());
    for _ in 0..2 {
        assert!(run_once(&state).await.unwrap().contains("1 held"));
    }

    let cfg = config::get_config(&state.db).await.unwrap().unwrap();
    let series = series::get_by_anilist_id(&state.db, 101)
        .await
        .unwrap()
        .unwrap();
    let projection = retry_statuses(&state.db, &cfg, &[series.id]).await.unwrap();
    let slot = &projection[&(series.id, 7)];
    assert!(slot.reason.contains("no matching release"));
    assert!(matches!(slot.timing, RetryTiming::After(at) if at > chrono::Utc::now().timestamp()));

    *state.indexers.write().await = sources;
    let deferred = run_once(&state).await.unwrap();
    assert!(
        deferred.contains("Checked 0 missing episodes"),
        "{deferred}"
    );
    assert!(
        deferred.contains("1 episode in retry backoff"),
        "{deferred}"
    );
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);

    sqlx::query("UPDATE library_activity SET detail=json_set(detail,'$.retry_at',unixepoch('now')-1) WHERE action=? AND json_extract(detail,'$.retry_at') IS NOT NULL")
        .bind(FINISHED).execute(&state.db).await.unwrap();
    let projection = retry_statuses(&state.db, &cfg, &[series.id]).await.unwrap();
    assert_eq!(projection[&(series.id, 7)].timing, RetryTiming::NextPass);
    assert!(run_once(&state).await.unwrap().contains("1 queued"));
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn standalone_metadata_cannot_redirect_missing_episode_demand_to_another_slot() {
    let (state, client, _root) = state(0).await;
    // The saved library row and provider cache disagree. Even with a valid
    // episode-seven candidate, Single would own episode one, not this demand.
    sqlx::query("UPDATE series_metadata_cache SET detail_json=json_set(detail_json,'$.format','MOVIE','$.episodes',2)")
        .execute(&state.db).await.unwrap();
    assert!(run_once(&state).await.unwrap().contains("0 queued"));
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changes_during_provider_search_and_packs_hold_before_native_add() {
    for mode in [1, 2, 3, 5, 7] {
        let (state, client, _root) = state(mode).await;
        let result = run_once(&state).await.unwrap();
        assert!(result.contains("0 queued"), "mode {mode}: {result}");
        assert_eq!(client.adds.load(Ordering::SeqCst), 0, "mode {mode}");
    }
}
#[tokio::test]
async fn duplicate_native_job_is_not_adopted_resumed_or_reselected() {
    let (state, client, _root) = state(6).await;
    assert!(run_once(&state).await.unwrap().contains("0 queued"));
    assert_eq!(client.adds.load(Ordering::SeqCst), 1);
    assert!(
        grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap()
            .is_empty()
    );
}

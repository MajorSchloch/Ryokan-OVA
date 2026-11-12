use super::super::*;
use crate::models::direct_rss_feeds::{self, DirectRssFeedForm};
use crate::services::download_client::{AddOutcome, DownloadClient, DownloadFile, DownloadItem};
use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct SelectiveClient {
    db: sqlx::SqlitePool,
    files: Mutex<Vec<DownloadFile>>,
    paused_adds: AtomicUsize,
    paused: AtomicBool,
    resumes: AtomicUsize,
    full: AtomicUsize,
}

#[async_trait]
impl DownloadClient for SelectiveClient {
    async fn test(&self) -> Result<String, String> {
        Ok("test".into())
    }
    async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
        self.full.fetch_add(1, AtomicOrdering::SeqCst);
        Err("a reviewed source must not use the unfiltered add path".into())
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<crate::services::download_client::SelectiveOutcome, String> {
        panic!("reviewed sources cannot use optional selection")
    }
    async fn add_torrent_paused(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("reviewed sources require the strict metadata-only contract");
    }
    async fn add_torrent_metadata_only(&self, _: &str) -> Result<AddOutcome, String> {
        self.paused_adds.fetch_add(1, AtomicOrdering::SeqCst);
        self.paused.store(true, AtomicOrdering::SeqCst);
        Ok(AddOutcome::Added)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(Vec::new())
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(self.files.lock().unwrap().clone())
    }
    async fn pause(&self, _hash: &str) -> Result<(), String> {
        self.paused.store(true, AtomicOrdering::SeqCst);
        Ok(())
    }
    async fn resume(&self, hash: &str) -> Result<(), String> {
        let rows = crate::models::grabbed_torrents::get_all_pending(&self.db)
            .await
            .map_err(|error| error.to_string())?;
        let row = rows
            .iter()
            .find(|row| row.hash == hash)
            .ok_or("resume happened before durable source ownership")?;
        let mapping = crate::models::grabbed_torrents::get_release_mapping(&self.db, row.id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("resume happened without a saved source mapping")?;
        if mapping.bound_source().is_none() {
            return Err("resume happened without an exact source proof".into());
        }
        self.resumes.fetch_add(1, AtomicOrdering::SeqCst);
        self.paused.store(false, AtomicOrdering::SeqCst);
        Ok(())
    }
    async fn delete(&self, _hash: &str, _delete: bool) -> Result<(), String> {
        Ok(())
    }
    async fn set_file_wanted(
        &self,
        _hash: &str,
        indices: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        assert!(self.paused.load(AtomicOrdering::SeqCst));
        let mut files = self.files.lock().unwrap();
        for index in indices {
            files[*index].wanted = wanted;
        }
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

#[tokio::test]
async fn reviewed_source_rss_uses_exact_owner_and_selective_dispatch_once() {
    let proof = lagoon_proof();
    let server = MockServer::start().await;
    let feed = format!(
        r#"<rss xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item>
        <title>[Legion] Black Lagoon - 15 [BD 1080p][Dual Audio]</title>
        <guid>reviewed-legion-source</guid><link>https://nyaa.si/view/1428087</link>
        <nyaa:magneturi>magnet:?xt=urn:btih:{}</nyaa:magneturi>
        <nyaa:infohash>{}</nyaa:infohash><nyaa:seeders>10</nyaa:seeders>
        </item></channel></rss>"#,
        proof.info_hash, proof.info_hash
    );
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_string(feed))
        .mount(&server)
        .await;
    for (reviewed, reject_record) in [(false, false), (true, false), (true, true)] {
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        config::save_config(
            &db,
            &config::Config {
                rss_enabled: false,
                media_root: temp.path().to_string_lossy().into(),
                media_requirements_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let sid = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
        sqlx::query("UPDATE series SET format='TV',episodes=12,cumulative_prior_episodes=12,monitor_mode='all' WHERE id=?")
            .bind(sid).execute(&db).await.unwrap();
        let detail:crate::services::anilist::AnimeDetail=serde_json::from_value(serde_json::json!({
            "id":1519,"title_romaji":"Black Lagoon: The Second Barrage","title_english":"Black Lagoon: The Second Barrage",
            "title_native":"","cover_url":"","banner_url":"","format":"TV","status":"FINISHED","status_display":"Finished",
            "episodes":12,"season":"","description":"","genres":[],"score_is_ten_point":false,"score_class":"",
            "synonyms":[],"streaming_episodes":[],"relations":[]
        })).unwrap();
        crate::models::metadata_cache::upsert(&db, sid, 1519, None, &detail)
            .await
            .unwrap();
        crate::models::nyaa_description_cache::upsert(
            &db,
            &proof.info_hash,
            "Source: Blu-ray; full English subtitles",
        )
        .await;
        if reviewed {
            crate::models::source_numbering::save_reviewed(&db, &proof)
                .await
                .unwrap();
        }
        if reject_record {
            sqlx::query("CREATE TRIGGER reject_source_grab BEFORE INSERT ON grabbed_torrents BEGIN SELECT RAISE(ABORT, 'test source record rejected'); END")
                .execute(&db).await.unwrap();
        }
        direct_rss_feeds::insert(
            &db,
            DirectRssFeedForm {
                name: "Exact source test",
                url: &format!("{}/feed", server.uri()),
                enabled: true,
                download_client_id: None,
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();
        let mut names = lagoon_files();
        names.reverse();
        let files = names
            .into_iter()
            .map(|name| DownloadFile {
                name,
                size: 1,
                progress: 0.0,
                wanted: true,
            })
            .collect();
        let client = Arc::new(SelectiveClient {
            db: db.clone(),
            files: Mutex::new(files),
            paused_adds: AtomicUsize::new(0),
            paused: AtomicBool::new(false),
            resumes: AtomicUsize::new(0),
            full: AtomicUsize::new(0),
        });
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let summary = sync_once(&state, "manual").await.unwrap();
        let decisions = crate::models::rss::recent_decisions(&db, 10).await.unwrap();
        let diagnostics: Vec<(String, String)> =
            sqlx::query_as("SELECT message, detail FROM logs ORDER BY id DESC LIMIT 20")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            summary.grabbed,
            i32::from(reviewed && !reject_record),
            "reviewed={reviewed}, reject_record={reject_record}: {summary:?}; decisions={}; logs={diagnostics:?}",
            serde_json::to_string(&decisions).unwrap()
        );
        assert_eq!(client.full.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            client.paused_adds.load(AtomicOrdering::SeqCst),
            usize::from(reviewed)
        );
        assert_eq!(
            client.resumes.load(AtomicOrdering::SeqCst),
            usize::from(reviewed && !reject_record)
        );
        if reject_record {
            assert!(client.paused.load(AtomicOrdering::SeqCst));
            assert!(
                crate::models::grabbed_torrents::get_all_pending(&db)
                    .await
                    .unwrap()
                    .is_empty()
            );
            continue;
        }
        if reviewed {
            let saved = crate::models::grabbed_torrents::get_all_pending(&db)
                .await
                .unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].episode_numbers, (1..=12).collect::<Vec<_>>());
            let mapping = crate::models::grabbed_torrents::get_release_mapping(&db, saved[0].id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(mapping.offset(), 12);
            assert!(mapping.bound_source().is_some());
            {
                let actual = client.files.lock().unwrap();
                let wanted: Vec<_> = actual
                    .iter()
                    .map(|file| (file.name.as_str(), file.wanted))
                    .collect();
                proof.validate_files(&proof.info_hash, &wanted).unwrap();
            }
            let repeat = sync_once(&state, "manual").await.unwrap();
            assert_eq!(repeat.grabbed, 0);
            assert_eq!(client.paused_adds.load(AtomicOrdering::SeqCst), 1);
            assert_eq!(client.resumes.load(AtomicOrdering::SeqCst), 1);
        }
    }
}

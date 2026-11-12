//! A publisher date can disprove an otherwise matching new-season identity.
//! Use the real Newznab XML parser and normal automatic search/retry routes.

use super::*;
use ryokan::services::anilist::RelatedEntry;
use ryokan::services::indexers::torznab::TorznabIndexer;
use std::sync::Arc;

const TARGET: i64 = 195604;
const ROOT: i64 = 97940;
const TITLE: &str = "Black Clover Season 2";
const RELEASE: &str = "Black.Clover.S02E06.1080p.Blu-Ray.10-Bit.Dual-Audio.TrueHD.x265-iAHD";
const OLD_DATE: &str = "Wed, 06 Nov 2019 18:34:30 +0000";
const RECENT_DATE: &str = "Thu, 12 Nov 2026 12:00:00 +0000";
const REJECTED_HASH: &str = "abab06060606060606060606060606060606abab";

#[derive(Default)]
struct NzbClient {
    additions: StdMutex<Vec<String>>,
    reject_adds: bool,
    torrent_sentinel: bool,
}

#[async_trait]
impl DownloadClient for NzbClient {
    async fn test(&self) -> Result<String, String> {
        Ok("private NZB fixture".into())
    }
    async fn add_torrent(&self, url: &str, _: &str) -> Result<AddOutcome, String> {
        assert!(
            !self.reject_adds,
            "a Newznab source must use the NZB client"
        );
        self.additions.lock().unwrap().push(url.into());
        Ok(AddOutcome::Added)
    }
    async fn add_torrent_returning_id(
        &self,
        url: &str,
        hash: &str,
    ) -> Result<(AddOutcome, String), String> {
        let outcome = self.add_torrent(url, hash).await?;
        Ok((outcome, "nzbget-publication-fixture-job".into()))
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("an ordinary NZB must not use a torrent file picker")
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        panic!("search must not alter existing jobs")
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        panic!("search must not resume existing jobs")
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("search must not delete jobs")
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        panic!("NZB search must not change torrent priorities")
    }
    fn sonarr_impl_name(&self) -> &'static str {
        if self.torrent_sentinel {
            "QBittorrent"
        } else {
            "Nzbget"
        }
    }
    fn protocol(&self) -> &'static str {
        if self.torrent_sentinel {
            "torrent"
        } else {
            "usenet"
        }
    }
}

async fn fixture(server: &MockServer) -> (tempfile::TempDir, AppState, i64, Arc<NzbClient>) {
    let temp = tempfile::tempdir().unwrap();
    let state = build_state().await;
    let mut cfg = default_config();
    cfg.media_root = temp.path().join("library").to_string_lossy().into_owned();
    cfg.media_requirements_enabled = true;
    cfg.auto_redownload_failed = true;
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    std::fs::create_dir_all(temp.path().join("library").join(TITLE)).unwrap();
    let sid = ryokan::test_support::seed_series(&state.db, TARGET, TITLE).await;
    sqlx::query("UPDATE series SET title_romaji='Black Clover 2nd Season',title_english=?,format='TV',status='RELEASING',episodes=NULL,cumulative_prior_episodes=170,cumulative_offset_known=1,monitor_mode='all',allow_upgrades=1 WHERE id=?")
        .bind(TITLE).bind(sid).execute(&state.db).await.unwrap();
    let mut root = detail_for(ROOT, "Black Clover");
    root.title_native.clear();
    root.episodes = Some(170);
    root.season_year = Some(2017);
    root.end_year = Some(2021);
    let mut detail = detail_for(TARGET, "Black Clover 2nd Season");
    detail.title_english = TITLE.into();
    detail.title_native.clear();
    detail.release_date = Some("2026-10-03".into());
    detail.season_year = Some(2026);
    detail.end_year = None;
    detail.episodes = None;
    detail.status = "RELEASING".into();
    detail.relations = vec![RelatedEntry {
        release_date: Some("2017-10-03".into()),
        id: ROOT,
        id_mal: None,
        title_romaji: root.title_romaji.clone(),
        title_english: root.title_english.clone(),
        title_native: root.title_native.clone(),
        cover_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(170),
        relation_type: "PREQUEL".into(),
        season_year: Some(2017),
        start_date: Some("2017-10-03".into()),
        media_type: "ANIME".into(),
    }];
    ryokan::models::metadata_cache::upsert(&state.db, sid, TARGET, None, &detail)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert_provider(&state.db, ROOT, None, &root)
        .await
        .unwrap();
    // Preserve the actual current provider shape. It uniquely assigns S02
    // to the new entry, so a competing-owner fixture cannot hide this bug.
    ryokan::services::anibridge::seed_mapping_data_for_tests(&serde_json::json!({
        "tvdb_show:900123:s1":{"anilist:97940":{"1-170":"1-170"}},
        "tvdb_show:900123:s2":{"anilist:195604":{"1-6":"1-6"}}
    }))
    .await;
    sqlx::query("INSERT INTO indexers(id,name,kind,url,min_seeders) VALUES(94,'Publication fixture','newznab',?,0)")
        .bind(format!("{}/api",server.uri())).execute(&state.db).await.unwrap();
    let row = ryokan::models::indexers::get_by_id(&state.db, 94)
        .await
        .unwrap()
        .unwrap();
    *state.indexers.write().await = Arc::new(vec![TorznabIndexer::from_row_arc(&row).unwrap()]);
    let client = Arc::new(NzbClient::default());
    let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
        std::collections::HashMap::new();
    clients.insert(1, client.clone());
    clients.insert(
        2,
        Arc::new(NzbClient {
            reject_adds: true,
            torrent_sentinel: true,
            ..Default::default()
        }),
    );
    *state.download_clients.write().await = Arc::new(DownloadClientPool {
        clients,
        default_torrent_id: Some(2),
        default_usenet_id: Some(1),
    });
    let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_numbering_decisions")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(decisions, 0, "no reviewed source may bypass the date check");
    (temp, state, sid, client)
}

async fn serve(server: &MockServer, title: &str, date: Option<&str>) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(server)
        .await;
    let date = date
        .map(|value| format!("<pubDate>{value}</pubDate>"))
        .unwrap_or_default();
    let xml = format!(
        r#"<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel><item>
<title>{title}</title><guid>publication-fixture-e06</guid><link>{url}/episode.nzb</link>{date}
<enclosure url="{url}/episode.nzb" length="1414313655" type="application/x-nzb"/>
<newznab:attr name="category" value="5070"/>
</item></channel></rss>"#,
        url = server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<rss><channel/></rss>"))
        .with_priority(10)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("q", format!("{TITLE} 06")))
        .respond_with(ResponseTemplate::new(200).set_body_string(xml))
        .with_priority(1)
        .expect(1..)
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

async fn automatic(state: &AppState, sid: i64) -> usize {
    auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((sid, 6)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .unwrap()
    .0
    .grabbed
    .len()
}

async fn assert_no_acquisition(state: &AppState, client: &NzbClient, existing: i64) {
    let logs = dump_logs(&state.db).await;
    assert!(client.additions.lock().unwrap().is_empty(), "{logs}");
    let counts: (i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM grabbed_torrents),(SELECT COUNT(*) FROM episode_quality_tags)")
        .fetch_one(&state.db).await.unwrap();
    assert_eq!(counts, (existing, 0), "{logs}");
}

#[tokio::test]
async fn publication_date_auto_holds_actual_2019_nzb_for_new_2026_season() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let (_temp, state, sid, client) = fixture(&server).await;
    serve(&server, RELEASE, Some(OLD_DATE)).await;
    assert_eq!(
        automatic(&state, sid).await,
        0,
        "{}",
        dump_logs(&state.db).await
    );
    assert_no_acquisition(&state, &client, 0).await;
    let logs = dump_logs(&state.db).await;
    assert!(logs.contains("predates canonical premiere"), "{logs}");
    let detail = ryokan::models::metadata_cache::get_by_series_id(&state.db, sid)
        .await
        .unwrap()
        .unwrap()
        .detail;
    let cfg = ryokan::models::config::get_config(&state.db)
        .await
        .unwrap()
        .unwrap();
    let interactive = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &SearchTarget::Episode(6),
        false,
        &[],
        &state.indexers,
    )
    .await;
    assert!(
        interactive.iter().any(|row| row.title == RELEASE),
        "an old candidate must remain visible for explicit review: {interactive:?}"
    );
    assert_no_acquisition(&state, &client, 0).await;
    unset_nyaa_base();
}

#[tokio::test]
async fn publication_date_verified_requirement_retry_also_holds_old_nzb() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let (_temp, state, sid, client) = fixture(&server).await;
    serve(&server, RELEASE, Some(OLD_DATE)).await;
    ryokan::models::monitoring::set_episode_monitored(&state.db, sid, 6, true)
        .await
        .unwrap();
    let failed_id = ryokan::models::grabbed_torrents::record_grab(
        &state.db,
        REJECTED_HASH,
        "[Fixture] Black Clover 2nd Season - 06 (176) [1080p rus].mkv",
        sid,
        &[6],
        false,
    )
    .await
    .unwrap()
    .unwrap();
    ryokan::models::grabbed_torrents::mark_failed_with_reason(
        &state.db,
        failed_id,
        ryokan::services::post_processing::REQUIREMENTS_REJECTED_REASON,
    )
    .await
    .unwrap();
    let failed = ryokan::models::grabbed_torrents::get_by_id(&state.db, failed_id)
        .await
        .unwrap()
        .unwrap();
    assert!(ryokan::services::redownload::after_requirements_rejected(&state, &failed).await);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let finished:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM logs WHERE message LIKE 'Re-search after a verified media-requirement rejection%' AND detail=?)")
            .bind(format!("series_id={sid}")).fetch_one(&state.db).await.unwrap();
        if finished {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}",
            dump_logs(&state.db).await
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_no_acquisition(&state, &client, 1).await;
    let logs = dump_logs(&state.db).await;
    assert!(logs.contains("predates canonical premiere"), "{logs}");
    assert_eq!(
        ryokan::models::grabbed_torrents::get_by_id(&state.db, failed_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "failed"
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn publication_date_recent_source_still_needs_ordinary_season_identity() {
    let _gate = ENV_LOCK.lock().await;
    for (title, expected) in [
        (RELEASE, 1),
        ("Black.Clover.S01E06.1080p.Blu-Ray.x265-iAHD", 0),
    ] {
        let server = MockServer::start().await;
        let (_temp, state, sid, client) = fixture(&server).await;
        serve(&server, title, Some(RECENT_DATE)).await;
        assert_eq!(
            automatic(&state, sid).await,
            expected,
            "title={title}\n{}",
            dump_logs(&state.db).await
        );
        assert_eq!(client.additions.lock().unwrap().len(), expected);
        if expected == 0 {
            assert_no_acquisition(&state, &client, 0).await;
        }
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn publication_date_absent_or_epoch_zero_keeps_unknown_date_behavior() {
    let _gate = ENV_LOCK.lock().await;
    for date in [None, Some("Thu, 01 Jan 1970 00:00:00 +0000")] {
        let server = MockServer::start().await;
        let (_temp, state, sid, client) = fixture(&server).await;
        serve(&server, RELEASE, date).await;
        assert_eq!(
            automatic(&state, sid).await,
            1,
            "date={date:?}\n{}",
            dump_logs(&state.db).await
        );
        assert_eq!(client.additions.lock().unwrap().len(), 1);
        let saved = ryokan::models::grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].episode_numbers, vec![6]);
        unset_nyaa_base();
    }
}

//! Actual publication metadata must survive RSS into the automatic hold.
use super::super::*;
use crate::models::direct_rss_feeds::{self, DirectRssFeedForm};
use crate::services::anilist::AnimeDetail;
use crate::services::download_client::{AddOutcome, DownloadClient, DownloadFile, DownloadItem};
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct NoPayloadClient(AtomicUsize);
#[async_trait]
impl DownloadClient for NoPayloadClient {
    async fn test(&self) -> Result<String, String> {
        Ok("private RSS fixture".into())
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<crate::services::download_client::SelectiveOutcome, String> {
        panic!("single-episode date checks cannot use a whole-pack fallback")
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        panic!("single-episode date checks cannot alter file priorities")
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("publication holds cannot delete files")
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

#[tokio::test]
async fn publication_date_rss_rejects_old_source_with_actual_unique_current_season_mapping() {
    for publication in [
        Some("Wed, 06 Nov 2019 18:34:30 +0000"),
        Some("Sat, 07 Nov 2026 00:00:00 +0000"),
        None,
    ] {
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        config::save_config(
            &db,
            &config::Config {
                rss_enabled: false,
                media_root: temp.path().to_string_lossy().into(),
                media_requirements_enabled: false,
                auto_redownload_failed: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let sid = seed_series(&db, 195604, "Black Clover Season 2").await;
        sqlx::query("UPDATE series SET format='TV',status='FINISHED',episodes=12,season_year=2026,title_romaji='Black Clover 2nd Season',title_english='Black Clover Season 2',cumulative_prior_episodes=170,monitor_mode='all',allow_upgrades=1 WHERE id=?")
            .bind(sid).execute(&db).await.unwrap();
        crate::models::monitoring::set_episode_monitored(&db, sid, 6, true)
            .await
            .unwrap();
        let detail:AnimeDetail=serde_json::from_value(serde_json::json!({
            "id":195604,"id_mal":null,"release_date":"2026-10-03","title_romaji":"Black Clover 2nd Season",
            "title_english":"Black Clover Season 2","title_native":"","cover_url":"","banner_url":"",
            "format":"TV","status":"FINISHED","status_display":"Finished","episodes":12,
            "season":"","season_year":2026,"description":"","genres":[],"score_is_ten_point":false,
            "score_class":"","synonyms":[],"streaming_episodes":[],"relations":[]
        })).unwrap();
        crate::models::metadata_cache::upsert(&db, sid, 195604, None, &detail)
            .await
            .unwrap();
        // The current published map calls the NEW anime S02. An association
        // conflict fixture would not reproduce the historical2019 collision.
        crate::services::anibridge::seed_mapping_data_for_tests(&serde_json::json!({
            "tvdb_show:331753:s1":{"anilist:97940":{"1-170":"1-170"}},
            "tvdb_show:331753:s2":{"anilist:195604":{"1-6":"1-6"}},
            "tmdb_show:73223:s1":{"anilist:97940":{"1-170":"1-170"}},
            "tmdb_show:73223:s2":{"anilist:195604":{"1-6":"1-6"}}
        }))
        .await;
        let server = MockServer::start().await;
        let date = publication
            .map(|value| format!("<pubDate>{value}</pubDate>"))
            .unwrap_or_default();
        let feed = format!(
            r#"<rss xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item>
            <title>Black.Clover.S02E06.1080p.Blu-Ray.10-Bit.Dual-Audio.TrueHD.x265-iAHD</title>
            <guid>publication-date-control</guid><link>https://example.invalid/release</link>{date}
            <nyaa:magneturi>magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</nyaa:magneturi>
            <nyaa:infohash>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</nyaa:infohash><nyaa:seeders>10</nyaa:seeders>
            </item></channel></rss>"#
        );
        Mock::given(method("GET"))
            .and(path("/feed"))
            .respond_with(ResponseTemplate::new(200).set_body_string(feed))
            .mount(&server)
            .await;
        direct_rss_feeds::insert(
            &db,
            DirectRssFeedForm {
                name: "Private publication date test",
                url: &format!("{}/feed", server.uri()),
                enabled: true,
                download_client_id: None,
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();
        let client = Arc::new(NoPayloadClient(AtomicUsize::new(0)));
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let summary = sync_once(&state, "manual").await.unwrap();
        let decisions = crate::models::rss::recent_decisions(&db, 10).await.unwrap();
        let diagnostics = serde_json::to_string(&decisions).unwrap();
        let old = publication.is_some_and(|date| date.contains("2019"));
        assert_eq!(summary.grabbed, i32::from(!old), "{diagnostics}");
        assert_eq!(
            client.0.load(Ordering::SeqCst),
            usize::from(!old),
            "{diagnostics}"
        );
        if old {
            assert!(
                diagnostics.contains("predates canonical premiere 2026-10-03"),
                "{diagnostics}"
            );
        }
    }
}

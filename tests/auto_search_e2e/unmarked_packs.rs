//! Published Nyaa file evidence for packs with no Batch/range/season marker.
use super::*;
use ryokan::services::auto_search::{
    collect_scored_batches_for_target, find_best_batch_for_target,
};

#[derive(serde::Deserialize)]
struct Pack {
    view_id: u64,
    anilist_id: i64,
    title: String,
    entry_title: String,
    format: String,
    episodes: i32,
    offset: i32,
    info_hash: String,
    files: Vec<String>,
}

fn packs() -> Vec<Pack> {
    serde_json::from_str(include_str!(
        "../fixtures/nyaa-file-list/mtbb-unmarked-packs.json"
    ))
    .unwrap()
}

// File-tree shape follows Nyaa's own view.html: folder links nest lists;
// leaves have a direct fa-file icon and a separate file-size span.
fn view(title: &str, hash: &str, files: &[String]) -> String {
    let leaves: String = files.iter().map(|name| format!("<li><i class=\"fa fa-file\"></i>{name} <span class=\"file-size\">(1 GiB)</span></li>")).collect();
    format!(
        "<div class=\"panel\"><div class=\"panel-heading\"><h3 class=\"panel-title\">{title}</h3></div><div class=\"panel-footer\"><a class=\"card-footer-item\" href=\"magnet:?xt=urn:btih:{hash}\">Magnet</a></div></div><div id=\"torrent-description\">Source: Blu-ray</div><div class=\"panel\"><div class=\"panel-heading\"><h3 class=\"panel-title\">File list</h3></div><div class=\"torrent-file-list panel-body\"><ul><li><a class=\"folder\"><i class=\"fa fa-folder\"></i>{title}</a><ul>{leaves}</ul></li></ul></div></div>"
    )
}

async fn seed_pack(state: &AppState, p: &Pack) -> AnimeDetail {
    let mut detail = detail_for(p.anilist_id, &p.entry_title);
    detail.format.clone_from(&p.format);
    detail.episodes = Some(p.episodes);
    let sid = ryokan::test_support::seed_series(&state.db, detail.id, &detail.title_romaji).await;
    sqlx::query("UPDATE series SET episodes=?,format=?,cumulative_prior_episodes=? WHERE id=?")
        .bind(p.episodes)
        .bind(&p.format)
        .bind(p.offset)
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert(&state.db, sid, detail.id, None, &detail)
        .await
        .unwrap();
    detail
}

async fn serve(server: &MockServer, rows: &str) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(rows)))
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

#[tokio::test]
async fn unmarked_packs_real_mtbb_file_trees_reach_interactive_and_automatic_with_real_offsets() {
    let _gate = ENV_LOCK.lock().await;
    for pack in packs() {
        let state = build_state().await;
        let detail = seed_pack(&state, &pack).await;
        let server = MockServer::start().await;
        serve(
            &server,
            &nyaa_row(&pack.info_hash, pack.view_id, &pack.title, "8 GiB", 50),
        )
        .await;
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", pack.view_id)))
            .respond_with(ResponseTemplate::new(200).set_body_string(view(
                &pack.title,
                &pack.info_hash,
                &pack.files,
            )))
            .mount(&server)
            .await;
        let interactive = collect_scored_batches_for_target(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Single,
            &[],
            &state.indexers,
            false,
        )
        .await;
        assert_eq!(
            interactive.len(),
            1,
            "unmarked published pack must appear: {}",
            pack.title
        );
        assert_eq!(interactive[0].info_hash, pack.info_hash);
        assert!(interactive[0].is_batch);
        let automatic = find_best_batch_for_target(&state.db,&detail,&default_config(),&SearchTarget::Single,&[],&state.indexers).await.expect("published complete own-entry files establish relative pack coverage despite the real franchise offset");
        assert_eq!(automatic.info_hash, pack.info_hash);
        assert!(automatic.is_batch);
        let mapping = automatic
            .release_mapping
            .expect("validated published-file mapping");
        assert_eq!(mapping.episodes(), (1..=pack.episodes).collect::<Vec<_>>());
        assert_eq!(mapping.offset(), 0);
        let saved_offset: i32 =
            sqlx::query_scalar("select cumulative_prior_episodes from series where anilist_id=?")
                .bind(pack.anilist_id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(
            saved_offset, pack.offset,
            "source evidence must never rewrite the library offset"
        );
        let views = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().starts_with("/view/"))
            .count();
        assert_eq!(
            views, 2,
            "one evidence fetch per search; classification reuses its description cache"
        );
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn unmarked_packs_movie_single_and_music_never_gain_batch_status_or_view_fetches() {
    let _gate = ENV_LOCK.lock().await;
    for (format, count) in [
        ("MOVIE", 1),
        ("MOVIE", 3),
        ("OVA", 1),
        ("ONA", 1),
        ("TV", 1),
        ("MUSIC", 12),
    ] {
        let state = build_state().await;
        let mut detail = detail_for(99001, "Quiet Horizon");
        detail.format = format.into();
        detail.episodes = Some(count);
        let server = MockServer::start().await;
        serve(
            &server,
            &nyaa_row(
                "1000000000000000000000000000000000000001",
                99001,
                "[Group] Quiet Horizon (BD 1080p)",
                "8 GiB",
                50,
            ),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/view/99001"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let rows = collect_scored_batches_for_target(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Single,
            &[],
            &state.indexers,
            false,
        )
        .await;
        assert!(rows.is_empty(), "{format}/{count} is not a season pack");
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn unmarked_packs_ordinary_automatic_search_honors_batch_opt_in_and_postproof_filter() {
    use ryokan::services::auto_search::{CandidateFilter, find_best_for_target_with_filter};
    let _gate = ENV_LOCK.lock().await;
    for (allow_batch, protect_batches) in [(true, false), (false, false), (true, true)] {
        let pack = packs().remove(0);
        let state = build_state().await;
        let detail = seed_pack(&state, &pack).await;
        let server = MockServer::start().await;
        serve(
            &server,
            &nyaa_row(&pack.info_hash, pack.view_id, &pack.title, "8 GiB", 50),
        )
        .await;
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", pack.view_id)))
            .respond_with(ResponseTemplate::new(200).set_body_string(view(
                &pack.title,
                &pack.info_hash,
                &pack.files,
            )))
            .mount(&server)
            .await;
        let protect =
            |release: &ryokan::services::nyaa::SearchResult,
             _classification: &ryokan::services::source::ClassificationResult| {
                !release.is_batch
                    || !release
                        .score_breakdown
                        .iter()
                        .any(|part| part.label == "Published episode files")
            };
        let filter: Option<CandidateFilter<'_>> = if protect_batches {
            Some(&protect)
        } else {
            None
        };
        let (best, _) = find_best_for_target_with_filter(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Episode(1),
            allow_batch,
            false,
            &[],
            &state.indexers,
            filter,
        )
        .await;
        assert_eq!(
            best.is_some(),
            allow_batch && !protect_batches,
            "allow_batch={allow_batch}, protection={protect_batches}"
        );
        if let Some(best) = best {
            assert!(best.is_batch);
            assert_eq!(best.info_hash, pack.info_hash);
            assert!(best.release_mapping.unwrap().contains_episode(1));
        }
        let requests = server.received_requests().await.unwrap();
        let views = requests
            .iter()
            .filter(|r| r.url.path().starts_with("/view/"))
            .count();
        assert_eq!(
            views,
            usize::from(allow_batch),
            "opt-out never launches pack discovery; protection is checked again after a candidate is proven to be a batch"
        );
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn unmarked_packs_published_view_budget_is_three_strongest_even_when_they_fail() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let mut detail = detail_for(99009, "Capped Horizon");
    detail.episodes = Some(3);
    let server = MockServer::start().await;
    let rows: String = (0..5)
        .map(|i| {
            nyaa_row(
                &format!("{:040x}", 1000 + i),
                1000 + i,
                &format!("[Group{i}] Capped Horizon (BD 1080p)"),
                "8 GiB",
                i as i32 + 1,
            )
        })
        .collect();
    serve(&server, &rows).await;
    for i in 0..5 {
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", 1000 + i)))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
    }
    let results = collect_scored_batches_for_target(
        &state.db,
        &detail,
        &default_config(),
        &SearchTarget::Single,
        &[],
        &state.indexers,
        false,
    )
    .await;
    assert!(results.is_empty());
    let mut views: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/view/"))
        .map(|r| r.url.path().to_string())
        .collect();
    views.sort();
    assert_eq!(views, ["/view/1002", "/view/1003", "/view/1004"]);
    unset_nyaa_base();
}

#[tokio::test]
async fn unmarked_packs_weekly_release_and_known_related_entry_never_request_file_evidence() {
    use ryokan::services::anilist::RelatedEntry;
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let mut detail = detail_for(99010, "Quiet Horizon");
    detail.episodes = Some(3);
    detail.relations = vec![RelatedEntry {
        release_date: None,
        id: 99011,
        id_mal: None,
        title_romaji: "Quiet Horizon: Scarlet Chronicle".into(),
        title_english: "Quiet Horizon: Scarlet Chronicle".into(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "OVA".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(3),
        relation_type: "OTHER".into(),
        season_year: Some(2024),
        start_date: None,
        media_type: "ANIME".into(),
    }];
    let server = MockServer::start().await;
    let rows = format!(
        "{}{}",
        nyaa_row(
            "1000000000000000000000000000000000000001",
            99010,
            "[Group] Quiet Horizon - 01 [1080p]",
            "1 GiB",
            50
        ),
        nyaa_row(
            "2000000000000000000000000000000000000002",
            99011,
            "[Group] Quiet Horizon: Scarlet Chronicle (BD 1080p)",
            "8 GiB",
            100
        )
    );
    serve(&server, &rows).await;
    let results = collect_scored_batches_for_target(
        &state.db,
        &detail,
        &default_config(),
        &SearchTarget::Single,
        &[],
        &state.indexers,
        false,
    )
    .await;
    assert!(results.is_empty());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/")
    );
    unset_nyaa_base();
}

#[tokio::test]
async fn unmarked_packs_interactive_requires_existing_picker_to_agree_with_proven_slots() {
    let _gate = ENV_LOCK.lock().await;
    for explicit_season in [false, true] {
        let files: Vec<_> = (1..=13)
            .map(|ep| {
                if explicit_season {
                    format!("Overlap Horizon S01E{ep:02}.mkv")
                } else {
                    format!("Overlap Horizon - {ep:02}.mkv")
                }
            })
            .collect();
        let pack = Pack {
            view_id: 99113,
            anilist_id: 99113,
            title: "[Group] Overlap Horizon (BD 1080p)".into(),
            entry_title: "Overlap Horizon".into(),
            format: "TV".into(),
            episodes: 13,
            offset: 1,
            info_hash: "3000000000000000000000000000000000000003".into(),
            files,
        };
        let state = build_state().await;
        let detail = seed_pack(&state, &pack).await;
        let server = MockServer::start().await;
        serve(
            &server,
            &nyaa_row(&pack.info_hash, pack.view_id, &pack.title, "8 GiB", 50),
        )
        .await;
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", pack.view_id)))
            .respond_with(ResponseTemplate::new(200).set_body_string(view(
                &pack.title,
                &pack.info_hash,
                &pack.files,
            )))
            .mount(&server)
            .await;
        let interactive = collect_scored_batches_for_target(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Single,
            &[],
            &state.indexers,
            false,
        )
        .await;
        assert_eq!(
            interactive.len(),
            usize::from(explicit_season),
            "new interactive eligibility must not expose a relative-file projection that its existing picker would shift"
        );
        let automatic = find_best_batch_for_target(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Single,
            &[],
            &state.indexers,
        )
        .await
        .expect("automatic path carries the proven entry-relative mapping");
        assert_eq!(automatic.release_mapping.unwrap().offset(), 0);
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn unmarked_packs_auto_batch_handler_persists_proven_relative_mapping_and_coverage() {
    use axum::extract::{Path, Query, State};
    use ryokan::handlers::library::search::{AutoSearchQuery, search_batch_releases};
    use ryokan::models::{episode_tags, grabbed_torrents};
    use ryokan::services::download_client::qbittorrent::QbitClient;
    use std::sync::Arc;

    let _gate = ENV_LOCK.lock().await;
    let pack = Pack {
        view_id: 99114,
        anilist_id: 99114,
        title: "[Group] Overlap Horizon (BD 1080p)".into(),
        entry_title: "Overlap Horizon".into(),
        format: "TV".into(),
        episodes: 13,
        offset: 1,
        info_hash: "4000000000000000000000000000000000000004".into(),
        files: (1..=13)
            .map(|ep| format!("Overlap Horizon - {ep:02}.mkv"))
            .collect(),
    };
    let server = MockServer::start().await;
    serve(
        &server,
        &nyaa_row(&pack.info_hash, pack.view_id, &pack.title, "8 GiB", 50),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/view/{}", pack.view_id)))
        .respond_with(ResponseTemplate::new(200).set_body_string(view(
            &pack.title,
            &pack.info_hash,
            &pack.files,
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .expect(1)
        .mount(&server)
        .await;
    let state = build_test_app_state(
        in_memory_pool().await,
        Some(Arc::new(QbitClient::new(
            &server.uri(),
            "test",
            "test",
            "ryokan-test",
        ))),
    );
    let detail = seed_pack(&state, &pack).await;
    sqlx::query(
        "INSERT INTO download_clients (id,name,kind,url) VALUES (1,'Test client','qbittorrent',?)",
    )
    .bind(server.uri())
    .execute(&state.db)
    .await
    .unwrap();
    ryokan::models::config::save_config(&state.db, &default_config())
        .await
        .unwrap();
    let sid: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id=?")
        .bind(detail.id)
        .fetch_one(&state.db)
        .await
        .unwrap();

    let report = search_batch_releases(
        State(state.clone()),
        Path(sid),
        Query(AutoSearchQuery::default()),
    )
    .await
    .expect("normal Auto batch handler must dispatch the verified entry pack")
    .0;
    assert_eq!(report.grabbed.len(), 1);
    let pending = grabbed_torrents::get_all_pending(&state.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash, pack.info_hash);
    assert_eq!(pending[0].episode_numbers, (1..=13).collect::<Vec<_>>());
    assert_eq!(pending[0].download_client_id, Some(1));
    let mapping = grabbed_torrents::get_release_mapping(&state.db, pending[0].id)
        .await
        .unwrap()
        .expect("Auto batch must persist the source-file numbering proof");
    assert_eq!(mapping.offset(), 0);
    assert_eq!(mapping.episodes(), (1..=13).collect::<Vec<_>>());
    assert_eq!(
        mapping
            .resolve_file(
                ryokan::services::media::parse_episode_span("overlap horizon - 12.mkv",).unwrap()
            )
            .unwrap()
            .first,
        12,
        "import must use proven relative E12, rather than subtract the library's offset to E11"
    );
    let tags = episode_tags::get_for_series(&state.db, sid).await.unwrap();
    assert_eq!(tags.len(), 13);
    assert!(tags.values().all(|tag| tag.state == "grabbed"));
    let offset: i32 = sqlx::query_scalar("SELECT cumulative_prior_episodes FROM series WHERE id=?")
        .bind(sid)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(offset, 1, "the source decision must not rewrite metadata");
    unset_nyaa_base();
}

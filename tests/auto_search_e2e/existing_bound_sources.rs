//! A known file-selection conflict must not hide another eligible source.
use super::*;
use ryokan::services::source_numbering::automatic::{DomainCandidate, resolve_unique};

fn existing_source() -> Source {
    Source {
        hash: "d301832a07da24c21955d6319df28799f92c0bcb".into(),
        view: 1178347,
        title: "[Erai-raws] Black Lagoon - The Second Barrage (01-12) [1080p WEB-DL]".into(),
        files: (1..=12).map(|number| format!("Second Barrage/[Erai-raws] Black Lagoon - The Second Barrage - {number:02} [1080p WEB-DL].mkv")).collect(),
        entries: vec![(889,"Black Lagoon".into(),12,0),(1519,"Black Lagoon: The Second Barrage".into(),12,12)],
        root: 1519,
        description: "Source: WEB-DL; full English subtitles",
    }
}

async fn seed_existing_four(state: &AppState, sid: i64, source: &Source, status: &str) -> i64 {
    let domain = ryokan::services::source_numbering::load_domain(&state.db, source.root)
        .await
        .unwrap();
    let interpretation = resolve_unique(
        &source.hash,
        &source.files,
        &[source.root],
        &[DomainCandidate {
            root_anilist_id: source.root,
            domain: Ok(domain),
        }],
    )
    .unwrap();
    let proof = interpretation
        .build_proof_for_owner(
            1519,
            &format!(
                "{}/view/{}",
                std::env::var("RYOKAN_NYAA_API_BASE").unwrap(),
                source.view
            ),
        )
        .unwrap()
        .unwrap();
    ryokan::models::source_numbering::save_automatic(&state.db, &proof)
        .await
        .unwrap();
    let narrowed = proof.narrow_to(&source.files[3..4]).unwrap();
    let mapping = ReleaseMapping::from_bound_source(&narrowed).unwrap();
    let id = grabbed_torrents::record_grab_with_release_mapping(
        &state.db,
        &source.hash,
        &source.title,
        sid,
        &[4],
        false,
        Some(&mapping),
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE grabbed_torrents SET state=?,download_client_id=1 WHERE id=?")
        .bind(status)
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    id
}

async fn old_snapshot(state: &AppState, id: i64) -> String {
    sqlx::query_scalar("SELECT json_object('id',id,'hash',hash,'series_id',series_id,'state',state,'episodes',episode_numbers,'mapping',release_mapping,'grabbed_at',grabbed_at,'imported_at',imported_at,'client',download_client_id,'source_url',source_url) FROM grabbed_torrents WHERE id=?")
        .bind(id).fetch_one(&state.db).await.unwrap()
}

async fn serve_candidates(server: &MockServer, old: &Source, fallback: Option<&Source>) {
    let mut rows = nyaa_row(&old.hash, old.view, &old.title, "3 GiB", 1000);
    if let Some(source) = fallback {
        rows.push_str(&nyaa_row(
            &source.hash,
            source.view,
            &source.title,
            "8 GiB",
            10,
        ));
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", source.view)))
            .respond_with(ResponseTemplate::new(200).set_body_string(published_page(
                source,
                &source.hash,
                &source.files,
            )))
            .mount(server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/view/{}", old.view)))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(published_page(old, &old.hash, &old.files)),
        )
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

#[tokio::test]
async fn existing_bound_source_episode_five_uses_eligible_fallback_without_retargeting_four() {
    let _gate = ENV_LOCK.lock().await;
    for status in ["pending", "imported"] {
        let old = existing_source();
        let fallback = lagoon_source();
        let server = MockServer::start().await;
        serve_candidates(&server, &old, Some(&fallback)).await;
        let (_temp, state, sid, client) = fixture(&fallback, 1519, &[5], &[17]).await;
        let mut cfg = ryokan::models::config::get_config(&state.db)
            .await
            .unwrap()
            .unwrap();
        cfg.preferred_groups = "Erai-raws".into();
        ryokan::models::config::save_config(&state.db, &cfg)
            .await
            .unwrap();
        let id = seed_existing_four(&state, sid, &old, status).await;
        let before = old_snapshot(&state, id).await;
        assert_eq!(
            search(&state, sid, 5).await,
            1,
            "{}",
            dump_logs(&state.db).await
        );
        assert_eq!(
            old_snapshot(&state, id).await,
            before,
            "existing E4 ownership is immutable"
        );
        let new: (String, String) =
            sqlx::query_as("SELECT hash,episode_numbers FROM grabbed_torrents WHERE id<>?")
                .bind(id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(new, (fallback.hash.clone(), "[5]".into()));
        let mapping: String =
            sqlx::query_scalar("SELECT release_mapping FROM grabbed_torrents WHERE id<>?")
                .bind(id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        let mapping: ReleaseMapping = serde_json::from_str(&mapping).unwrap();
        assert_eq!(
            mapping.bound_source().unwrap().selected_files[0].source_first,
            17
        );
        assert_eq!(client.adds.load(Ordering::SeqCst), 1);
        assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn existing_bound_source_without_fallback_holds_five_and_leaves_four_unchanged() {
    let _gate = ENV_LOCK.lock().await;
    let old = existing_source();
    let server = MockServer::start().await;
    serve_candidates(&server, &old, None).await;
    // Any attempted client call for the old source would violate this
    // client's exact fallback hash assertion. No add or control is allowed.
    let fallback = lagoon_source();
    let (_temp, state, sid, client) = fixture(&fallback, 1519, &[5], &[17]).await;
    let id = seed_existing_four(&state, sid, &old, "imported").await;
    let before = old_snapshot(&state, id).await;
    let decisions_before: Vec<(String, String)> =
        sqlx::query_as("SELECT proof_json,fingerprint FROM source_numbering_decisions")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(
        search(&state, sid, 5).await,
        0,
        "{}",
        dump_logs(&state.db).await
    );
    assert_eq!(old_snapshot(&state, id).await, before);
    let decisions_after: Vec<(String, String)> =
        sqlx::query_as("SELECT proof_json,fingerprint FROM source_numbering_decisions")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(decisions_after, decisions_before);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    assert!(!client.added.load(Ordering::SeqCst));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM grabbed_torrents")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(structural_season_numbering::library_videos(&_temp.path().join("library")).is_empty());
    unset_nyaa_base();
}

#[tokio::test]
async fn existing_bound_source_same_four_selection_remains_an_eligible_winner() {
    let _gate = ENV_LOCK.lock().await;
    let old = existing_source();
    let server = MockServer::start().await;
    serve_candidates(&server, &old, None).await;
    let (_temp, state, sid, client) = fixture(&old, 1519, &[4], &[4]).await;
    let id = seed_existing_four(&state, sid, &old, "imported").await;
    let before = old_snapshot(&state, id).await;
    let cfg = ryokan::models::config::get_config(&state.db)
        .await
        .unwrap()
        .unwrap();
    let candidate = ryokan::services::auto_search::find_best_for_target(
        &state.db,
        &detail_for(1519, "Black Lagoon: The Second Barrage"),
        &cfg,
        &SearchTarget::Episode(4),
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
    .expect("an unchanged source assignment remains eligible");
    assert_eq!(candidate.info_hash, old.hash);
    assert_eq!(candidate.release_mapping.unwrap().episodes(), [4]);
    assert_eq!(old_snapshot(&state, id).await, before);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    unset_nyaa_base();
}

#[tokio::test]
async fn existing_bound_source_rss_checks_final_monitored_selection_before_dispatch() {
    let _gate = ENV_LOCK.lock().await;
    let old = existing_source();
    let server = MockServer::start().await;
    serve_candidates(&server, &old, None).await;
    let (_temp, state, sid, client) = fixture(&old, 1519, &[5], &[5]).await;
    rss_source_setup(&state, &server, &old, Some(1519)).await;
    for episode in 1..=12 {
        ryokan::models::monitoring::set_episode_monitored(&state.db, sid, episode, episode == 5)
            .await
            .unwrap();
    }
    let id = seed_existing_four(&state, sid, &old, "imported").await;
    let before = old_snapshot(&state, id).await;
    let result = ryokan::services::rss::sync_once(&state, "scheduled")
        .await
        .unwrap();
    assert_eq!(result.items_seen, 1);
    assert_eq!(result.grabbed, 0);
    assert_eq!(old_snapshot(&state, id).await, before);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    assert!(!client.added.load(Ordering::SeqCst));
    let reason: String = sqlx::query_scalar("SELECT reason FROM rss_seen ORDER BY id DESC LIMIT 1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(
        reason.contains("different or unresolved saved source assignment"),
        "{reason}"
    );
    unset_nyaa_base();
}

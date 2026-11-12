//! Real query boundaries for unseen continuous packs in episode Auto search.
use super::*;

const ROOT_ENGLISH: &str = "Black Lagoon Canonical English";
const ROOT_NATIVE: &str = "ブラックラグーン";

fn sokudo_source() -> Source {
    let folder = "[Sokudo] Black Lagoon [1080p BD][AV1][dual audio]";
    Source {
        hash: "20de340393930e47e7fb9d9eb171588b92a75388".into(),
        view: 1708941,
        title: "[Sokudo] Black Lagoon v2 [1080p BD][AV1][dual audio]".into(),
        files: (1..=29)
            .map(|number| {
                format!(
                    "{folder}/[Sokudo] Black Lagoon - {number:02} v2 [1080p BD][AV1][dual audio].mkv"
                )
            })
            .collect(),
        entries: vec![
            (889, "Black Lagoon".into(), 12, 0),
            (1519, "Black Lagoon: The Second Barrage".into(), 12, 12),
        ],
        root: 889,
        description: "Source: Blu-ray; full English subtitles",
    }
}

async fn cache_root_aliases(state: &AppState) {
    let mut root = detail_for(889, "Black Lagoon");
    root.title_english = ROOT_ENGLISH.into();
    root.title_native = ROOT_NATIVE.into();
    ryokan::models::metadata_cache::upsert_provider(&state.db, 889, None, &root)
        .await
        .unwrap();
}

/// No ordinary episode, preferred-group or own-title BD query can see this
/// row. A regression must actually issue the selected canonical-root query.
async fn serve_only_root_query(server: &MockServer, source: &Source, query: &str, seeders: i32) {
    serve_root_query_after_rejections(server, source, query, seeders, 0, None).await;
}

async fn serve_root_query_after_rejections(
    server: &MockServer,
    source: &Source,
    query: &str,
    seeders: i32,
    prior_packs: usize,
    duplicate_query: Option<String>,
) {
    let expected = query.to_string();
    let result = nyaa_results_page(&nyaa_row(
        &source.hash,
        source.view,
        &source.title,
        "8.3 GiB",
        seeders,
    ));
    let mut earlier = String::new();
    for index in 0..prior_packs {
        let mut ambiguous = source.clone();
        ambiguous.hash = format!("{:040x}", index + 1);
        ambiguous.view = 990100 + index as u64;
        ambiguous.title = format!("[Earlier{index}] Black Lagoon (01-12) [1080p BD] [Batch]");
        ambiguous.files.truncate(12);
        earlier.push_str(&nyaa_row(
            &ambiguous.hash,
            ambiguous.view,
            &ambiguous.title,
            "4 GiB",
            12,
        ));
        Mock::given(method("GET"))
            .and(path(format!("/view/{}", ambiguous.view)))
            .respond_with(ResponseTemplate::new(200).set_body_string(published_page(
                &ambiguous,
                &ambiguous.hash,
                &ambiguous.files,
            )))
            .mount(server)
            .await;
    }
    let earlier = nyaa_results_page(&earlier);
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/(user/Sokudo)?$"))
        .respond_with(move |request: &wiremock::Request| {
            let query = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned());
            if query.as_deref() == Some(expected.as_str())
                || duplicate_query
                    .as_deref()
                    .is_some_and(|duplicate| query.as_deref() == Some(duplicate))
            {
                // Earlier bare-root results arrive first. The same resolver
                // must retain their failed evidence in its bounded cache.
                ResponseTemplate::new(200)
                    .set_body_string(result.clone())
                    .set_delay(std::time::Duration::from_millis(100))
            } else if prior_packs > 0 && query.as_deref() == Some("Black Lagoon") {
                ResponseTemplate::new(200).set_body_string(earlier.clone())
            } else {
                ResponseTemplate::new(200).set_body_string(nyaa_results_page(""))
            }
        })
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/view/{}", source.view)))
        .respond_with(ResponseTemplate::new(200).set_body_string(published_page(
            source,
            &source.hash,
            &source.files,
        )))
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

async fn assert_root_query_bound(server: &MockServer, required: &str) {
    let queries: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/")
        .filter_map(|request| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
        })
        .collect();
    assert!(queries.iter().any(|query| query == required), "{queries:?}");
    let root_queries: Vec<_> = queries
        .iter()
        .filter(|query| {
            ["Black Lagoon", ROOT_ENGLISH, ROOT_NATIVE]
                .iter()
                .any(|alias| **query == *alias || **query == format!("{alias} BD"))
        })
        .collect();
    assert_eq!(root_queries.len(), 4, "{queries:?}");
    assert!(!root_queries.iter().any(|query| query.contains(ROOT_NATIVE)));
    assert_eq!(
        root_queries
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4,
        "root discovery is capped at two canonical aliases and four unique requests"
    );
}

#[tokio::test]
async fn root_pack_discovery_normal_episode_auto_finds_bare_or_bd_only_pack() {
    let _gate = ENV_LOCK.lock().await;
    for query in ["Black Lagoon", "Black Lagoon BD"] {
        let source = sokudo_source();
        let server = MockServer::start().await;
        serve_only_root_query(&server, &source, query, 12).await;
        let (_temp, state, sid, client) = fixture(&source, 1519, &[4], &[16]).await;
        cache_root_aliases(&state).await;
        client.files.lock().unwrap().reverse();
        assert_eq!(
            search(&state, sid, 4).await,
            1,
            "{}",
            dump_logs(&state.db).await
        );
        let mapping = saved(&state, &source, &[4]).await;
        let proof = mapping.bound_source().unwrap();
        assert_eq!(proof.selected_files.len(), 1);
        assert_eq!(proof.selected_files[0].source_first, 16);
        assert_eq!(proof.selected_files[0].target_first, 4);
        assert_eq!(proof.published_files.len(), 29);
        assert_eq!(client.adds.load(Ordering::SeqCst), 1);
        assert_eq!(client.resumes.load(Ordering::SeqCst), 1);
        assert_root_query_bound(&server, query).await;
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn root_pack_discovery_keeps_ambiguous_foreign_and_wrong_folder_sources_held() {
    let _gate = ENV_LOCK.lock().await;
    for kind in ["ambiguous", "foreign", "folder_season"] {
        let mut source = sokudo_source();
        match kind {
            "ambiguous" => source.files.truncate(12),
            "foreign" => {
                for name in &mut source.files {
                    *name = name.replace("Black Lagoon -", "Black Lagoon Foreign Story -");
                }
            }
            "folder_season" => {
                for name in &mut source.files {
                    *name = format!("Season 2/{}", name.rsplit('/').next().unwrap());
                }
            }
            _ => unreachable!(),
        }
        let server = MockServer::start().await;
        serve_only_root_query(&server, &source, "Black Lagoon BD", 12).await;
        let (_temp, state, sid, client) = fixture(&source, 1519, &[4], &[]).await;
        cache_root_aliases(&state).await;
        assert_eq!(search(&state, sid, 4).await, 0, "{kind}");
        assert_eq!(client.adds.load(Ordering::SeqCst), 0, "{kind}");
        assert_eq!(client.resumes.load(Ordering::SeqCst), 0, "{kind}");
        let decisions: i64 = sqlx::query_scalar("SELECT count(*) FROM source_numbering_decisions")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(decisions, 0, "{kind}: no automatic or human receipt");
        if kind == "folder_season" {
            assert!(
                dump_logs(&state.db)
                    .await
                    .contains("folder names another source season"),
                "{}",
                dump_logs(&state.db).await
            );
        }
        assert_root_query_bound(&server, "Black Lagoon BD").await;
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn root_pack_discovery_does_not_override_the_automatic_seed_floor() {
    let _gate = ENV_LOCK.lock().await;
    let source = sokudo_source();
    let server = MockServer::start().await;
    serve_only_root_query(&server, &source, "Black Lagoon BD", 0).await;
    let (_temp, state, sid, client) = fixture(&source, 1519, &[4], &[16]).await;
    cache_root_aliases(&state).await;
    assert_eq!(search(&state, sid, 4).await, 0);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| { request.url.path() == format!("/view/{}", source.view) })
    );
    assert_root_query_bound(&server, "Black Lagoon BD").await;
    unset_nyaa_base();
}

#[tokio::test]
async fn root_pack_discovery_preserves_the_three_publication_evidence_budget() {
    let _gate = ENV_LOCK.lock().await;
    for prior_packs in [2, 3] {
        let source = sokudo_source();
        let server = MockServer::start().await;
        serve_root_query_after_rejections(
            &server,
            &source,
            "Black Lagoon BD",
            12,
            prior_packs,
            None,
        )
        .await;
        let (_temp, state, sid, client) = fixture(&source, 1519, &[4], &[16]).await;
        cache_root_aliases(&state).await;
        assert_eq!(
            search(&state, sid, 4).await,
            usize::from(prior_packs == 2),
            "{}",
            dump_logs(&state.db).await
        );
        assert_eq!(
            client.adds.load(Ordering::SeqCst),
            usize::from(prior_packs == 2)
        );
        let requests = server.received_requests().await.unwrap();
        let source_views = requests
            .iter()
            .filter(|request| request.url.path() == format!("/view/{}", source.view))
            .count();
        if prior_packs == 2 {
            assert!(source_views > 0);
            saved(&state, &source, &[4]).await;
        } else {
            assert_eq!(source_views, 0, "a fourth publication must not be fetched");
            assert!(
                dump_logs(&state.db)
                    .await
                    .contains("evidence budget reached")
            );
        }
        assert_root_query_bound(&server, "Black Lagoon BD").await;
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn root_pack_discovery_does_not_relabel_or_promote_a_human_review() {
    let _gate = ENV_LOCK.lock().await;
    let source = sokudo_source();
    let server = MockServer::start().await;
    serve_only_root_query(&server, &source, "Black Lagoon BD", 12).await;
    let (_temp, state, sid, client) = fixture(&source, 1519, &[4], &[16]).await;
    cache_root_aliases(&state).await;
    let domain = ryokan::services::source_numbering::load_domain(&state.db, 889)
        .await
        .unwrap();
    let proof = ryokan::services::source_numbering::prepare_review(
        domain,
        &source.hash,
        &source.files,
        1519,
    )
    .unwrap()
    .confirm(&source.files[15..16])
    .unwrap();
    assert_eq!(proof.version, 1);
    ryokan::models::source_numbering::save_reviewed(&state.db, &proof)
        .await
        .unwrap();
    let before: Vec<(String, String)> =
        sqlx::query_as("SELECT proof_json,fingerprint FROM source_numbering_decisions")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(search(&state, sid, 4).await, 0);
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    assert_eq!(client.resumes.load(Ordering::SeqCst), 0);
    let after: Vec<(String, String)> =
        sqlx::query_as("SELECT proof_json,fingerprint FROM source_numbering_decisions")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(
        after, before,
        "the new discovery phase cannot change human proof bytes"
    );
    assert_root_query_bound(&server, "Black Lagoon BD").await;
    unset_nyaa_base();
}

#[tokio::test]
async fn root_pack_discovery_keeps_uploader_tokens_and_candidate_requirements() {
    let _gate = ENV_LOCK.lock().await;
    let source = sokudo_source();
    let server = MockServer::start().await;
    serve_root_query_after_rejections(
        &server,
        &source,
        "Black Lagoon BD AV1",
        12,
        0,
        Some("Black Lagoon AV1".into()),
    )
    .await;
    let (_temp, state, _sid, client) = fixture(&source, 1519, &[4], &[16]).await;
    cache_root_aliases(&state).await;
    let mut cfg = ryokan::models::config::get_config(&state.db)
        .await
        .unwrap()
        .unwrap();
    cfg.default_restrict_to_uploader = "Sokudo".into();
    cfg.default_custom_query_tokens = "AV1".into();
    let reject = |_: &ryokan::services::nyaa::SearchResult,
                  _: &ryokan::services::source::ClassificationResult| false;
    let result = ryokan::services::auto_search::find_best_for_target_with_filter(
        &state.db,
        &detail_for(1519, "Black Lagoon: The Second Barrage"),
        &cfg,
        &SearchTarget::Episode(4),
        false,
        false,
        &[],
        &state.indexers,
        Some(&reject),
    )
    .await
    .0;
    assert!(
        result.is_none(),
        "source proof cannot bypass the candidate requirements"
    );
    assert_eq!(client.adds.load(Ordering::SeqCst), 0);
    let requests = server.received_requests().await.unwrap();
    let root_requests: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.url.path() == "/user/Sokudo"
                && request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "q" && value == "Black Lagoon BD AV1")
        })
        .collect();
    assert_eq!(root_requests.len(), 1);
    assert_eq!(root_requests[0].url.path(), "/user/Sokudo");
    // A valid receipt can precede the final candidate veto. Seeing the
    // duplicate source must still revalidate it, not authorize acquisition.
    let decision: String = sqlx::query_scalar("SELECT proof_json FROM source_numbering_decisions")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&decision).unwrap()["version"],
        2
    );
    let publication_requests = requests
        .iter()
        .filter(|request| request.url.path() == format!("/view/{}", source.view))
        .count();
    assert!(
        publication_requests >= 2,
        "saved evidence is refreshed on the duplicate"
    );
    assert!(
        grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap()
            .is_empty()
    );
    let previews: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_grabs")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(previews, 0);
    unset_nyaa_base();
}

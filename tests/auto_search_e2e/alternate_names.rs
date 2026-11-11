//! Full alternate names must reach normal batch queries without broadening
//! season or related-entry identity. All HTTP and classification data is local.
use super::*;
use ryokan::services::anilist::RelatedEntry;
use ryokan::services::auto_search::{collect_scored_batches_for_target, normalize_title};
use ryokan::services::nyaa::SearchResult;
use std::collections::{HashMap, HashSet};

struct Row {
    hash: String,
    title: String,
}

fn row(hash: &str, title: &str) -> Row {
    Row {
        hash: hash.into(),
        title: title.into(),
    }
}

async fn seed(state: &AppState, detail: &AnimeDetail, alternate_titles: &str) {
    let sid = ryokan::test_support::seed_series(&state.db, detail.id, &detail.title_romaji).await;
    sqlx::query("UPDATE series SET episodes=?,format=?,title_english=?,title_native=?,alternate_titles=? WHERE id=?")
        .bind(detail.episodes)
        .bind(&detail.format)
        .bind(&detail.title_english)
        .bind(&detail.title_native)
        .bind(alternate_titles)
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert(&state.db, sid, detail.id, None, detail)
        .await
        .unwrap();
}

/// Match decoded query tokens rather than URL encoding or punctuation. The
/// recorded raw queries below still distinguish a bare query from its quoted
/// duplicate, so a full fanout cannot masquerade as a collapsed query.
async fn serve(server: &MockServer, state: &AppState, pages: Vec<(&str, Vec<Row>)>) {
    let mut responses = HashMap::new();
    for (query, rows) in pages {
        let mut html = String::new();
        for (index, row) in rows.iter().enumerate() {
            html.push_str(&nyaa_row(
                &row.hash,
                99700 + index as u64,
                &row.title,
                "4.0 GiB",
                40,
            ));
            ryokan::models::nyaa_description_cache::upsert(&state.db, &row.hash, "Source: Blu-ray")
                .await;
        }
        responses.insert(normalize_title(query), nyaa_results_page(&html));
    }
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(move |request: &wiremock::Request| {
            let query = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| normalize_title(&value))
                .unwrap_or_default();
            let body = responses
                .get(&query)
                .cloned()
                .unwrap_or_else(|| nyaa_results_page(""));
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(server)
        .await;
    set_nyaa_base(&server.uri());
}

async fn search_batches(state: &AppState, detail: &AnimeDetail) -> Vec<SearchResult> {
    let mut cfg = default_config();
    cfg.seadex_enabled = false;
    collect_scored_batches_for_target(
        &state.db,
        detail,
        &cfg,
        &SearchTarget::Single,
        &[],
        &state.indexers,
        true,
    )
    .await
}

async fn queries(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/")
        .filter_map(|request| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
        })
        .collect()
}

fn unprobe(query: &str) -> &str {
    [" batch", " complete", " 01-"]
        .into_iter()
        .find_map(|suffix| query.strip_suffix(suffix))
        .unwrap_or(query)
}

#[tokio::test]
async fn alternate_names_full_provider_synonym_is_not_suppressed_by_a_canonical_batch() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let server = MockServer::start().await;
    let mut detail = detail_for(17074, "Monogatari Series: Second Season");
    detail.title_english = "Monogatari Series Second Season".into();
    detail.title_native = "物語シリーズ セカンドシーズン".into();
    detail.episodes = Some(26);
    // The downloaded listing uses this complete shorter name. It is a
    // provider alternate in this fixture, not a decomposed franchise root.
    detail.synonyms = vec!["Monogatari Second Season".into()];
    seed(&state, &detail, "").await;
    let canonical_hash = "1111111111111111111111111111111111111111";
    let alternate_hash = "466764b8df84e6233d04c855166dd44021330ff7";
    serve(
        &server,
        &state,
        vec![
            (
                &detail.title_english,
                vec![row(
                    canonical_hash,
                    "[Fixture] Monogatari Series Second Season 01-26 (Batch BD 1080p)",
                )],
            ),
            (
                "Monogatari Second Season",
                vec![row(
                    alternate_hash,
                    "[a-S_subs] Monogatari Second Season 1-26 (BDRip 1080p x265 AC3)",
                )],
            ),
        ],
    )
    .await;
    let results = search_batches(&state, &detail).await;
    unset_nyaa_base();
    let hashes: HashSet<_> = results.iter().map(|r| r.info_hash.as_str()).collect();
    assert_eq!(hashes, HashSet::from([canonical_hash, alternate_hash]));
    assert!(results.iter().all(|r| {
        r.match_provenance
            .as_ref()
            .is_some_and(|p| p.kind.as_str() == "verbatim")
    }));
    let requested = queries(&server).await;
    for primary in [
        &detail.title_romaji,
        &detail.title_english,
        &detail.title_native,
    ] {
        assert!(
            requested.contains(primary),
            "missing canonical query: {requested:?}"
        );
        assert!(requested.contains(&format!("\"{primary}\"")));
    }
    let alternate_primary: Vec<_> = requested
        .iter()
        .filter(|q| normalize_title(q) == normalize_title("Monogatari Second Season"))
        .collect();
    assert_eq!(alternate_primary.len(), 1, "{requested:?}");
    assert!(!alternate_primary[0].starts_with('"'));
}

#[tokio::test]
async fn alternate_names_user_title_gains_its_own_season_shorthand_queries() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let server = MockServer::start().await;
    let mut detail = detail_for(99801, "Original Chronicle 2nd Season");
    detail.title_native.clear();
    seed(&state, &detail, "Different Voyage 2nd Season").await;
    let hash = "2222222222222222222222222222222222222222";
    serve(
        &server,
        &state,
        vec![(
            "Different Voyage S02",
            vec![row(
                hash,
                "[Fixture] Different Voyage S02 01-12 (Batch BD 1080p)",
            )],
        )],
    )
    .await;
    let results = search_batches(&state, &detail).await;
    unset_nyaa_base();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].info_hash, hash);
    let requested = queries(&server).await;
    assert!(requested.iter().any(|q| q == "Different Voyage 2nd Season"));
    assert!(requested.iter().any(|q| q == "Different Voyage S02"));
}

#[tokio::test]
async fn alternate_names_provider_query_budget_keeps_all_full_names_available_to_match() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let server = MockServer::start().await;
    let mut detail = detail_for(99802, "Original Chronicle");
    detail.title_native.clear();
    detail.synonyms = (0..32)
        .map(|n| format!("Supplemental Alternate Label {n:02}"))
        .collect();
    detail
        .synonyms
        .extend([String::new(), "  ".into(), "ORIGINAL CHRONICLE".into()]);
    seed(&state, &detail, "").await;
    let expected: HashSet<String> = (1000..1032).map(|n| format!("{n:040x}")).collect();
    let rows = detail.synonyms[..32]
        .iter()
        .enumerate()
        .map(|(n, alias)| Row {
            hash: format!("{:040x}", 1000 + n),
            title: format!("[Fixture] {alias} 01-12 (Batch BD 1080p)"),
        })
        .collect();
    serve(&server, &state, vec![(&detail.title_english, rows)]).await;
    let results = search_batches(&state, &detail).await;
    unset_nyaa_base();
    let actual: HashSet<_> = results.iter().map(|r| r.info_hash.clone()).collect();
    assert_eq!(
        actual, expected,
        "a query budget must not erase matching names"
    );
    let requested = queries(&server).await;
    let supplemental: Vec<_> = requested
        .iter()
        .filter(|q| normalize_title(q).starts_with("supplemental alternate label "))
        .collect();
    let forms: HashSet<_> = supplemental
        .iter()
        .map(|q| normalize_title(unprobe(q)))
        .collect();
    assert!(!forms.is_empty());
    assert!(
        forms.len() <= 8,
        "unbounded supplemental forms: {requested:?}"
    );
    for form in forms {
        let bare: Vec<_> = supplemental
            .iter()
            .filter(|q| normalize_title(q) == form)
            .collect();
        assert_eq!(bare.len(), 1, "supplemental full fanout: {requested:?}");
        assert!(!bare[0].starts_with('"'));
    }
}

#[tokio::test]
async fn alternate_names_full_synonym_cannot_admit_a_different_season_batch() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let server = MockServer::start().await;
    let mut detail = detail_for(99803, "Moonlight Saga 2nd Season");
    detail.title_native.clear();
    detail.synonyms = vec!["Night Voyage".into()];
    seed(&state, &detail, "").await;
    let good = "3333333333333333333333333333333333333333";
    let wrong = "4444444444444444444444444444444444444444";
    serve(
        &server,
        &state,
        vec![(
            "Night Voyage",
            vec![
                row(good, "[Fixture] Night Voyage S02 01-12 (Batch BD 1080p)"),
                row(wrong, "[Fixture] Night Voyage S03 01-12 (Batch BD 1080p)"),
            ],
        )],
    )
    .await;
    let results = search_batches(&state, &detail).await;
    unset_nyaa_base();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].info_hash, good);
}

#[tokio::test]
async fn alternate_names_provider_cannot_rescue_missing_canonical_numeric_owner() {
    let _gate = ENV_LOCK.lock().await;
    for include_correct in [false, true] {
        let state = build_state().await;
        let server = MockServer::start().await;
        let mut detail = detail_for(1313, "Digimon Adventure 02");
        detail.title_native.clear();
        detail.episodes = Some(50);
        detail.synonyms = vec!["Digimon Adventure".into()];
        assert!(
            detail.relations.is_empty(),
            "the numeric owner must stand on its own"
        );
        seed(&state, &detail, "").await;
        let good = "7777777777777777777777777777777777777777";
        let wrong = "8888888888888888888888888888888888888888";
        let mut pages = vec![(
            "Digimon Adventure 02",
            vec![row(
                wrong,
                "[YakuboEncodes] Digimon Adventure (2020) - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]",
            )],
        )];
        if include_correct {
            pages.push((
                "Digimon Adventure 02 02",
                vec![row(good, "[Keyword] Digimon Adventure 02 - 02 [BD 1080p]")],
            ));
        }
        serve(&server, &state, pages).await;
        let selected = ryokan::services::auto_search::find_best_for_target(
            &state.db,
            &detail,
            &default_config(),
            &SearchTarget::Episode(2),
            false,
            false,
            &[],
            &state.indexers,
        )
        .await;
        unset_nyaa_base();
        assert_eq!(
            selected.as_ref().map(|r| r.info_hash.as_str()),
            include_correct.then_some(good),
            "a provider synonym must not borrow the title02 from the episode: {selected:?}"
        );
        assert!(
            queries(&server)
                .await
                .iter()
                .any(|q| q == "Digimon Adventure 02")
        );
    }
}

#[tokio::test]
async fn alternate_names_known_related_title_is_not_a_full_provider_alias() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let server = MockServer::start().await;
    let mut detail = detail_for(1575, "Code Geass: Hangyaku no Lelouch");
    detail.episodes = Some(25);
    detail.title_english = "Code Geass: Lelouch of the Rebellion".into();
    detail.title_native.clear();
    let related_title = "Code Geass: Akito the Exiled - The Wyvern Arrives";
    detail.synonyms = vec!["Code Geass".into(), related_title.into()];
    detail.relations = vec![RelatedEntry {
        release_date: None,
        id: 8888,
        id_mal: None,
        title_romaji: "Code Geass: Boukoku no Akito 1 - Yokuryuu wa Maiorita".into(),
        title_english: related_title.into(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "MOVIE".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(1),
        relation_type: "OTHER".into(),
        season_year: Some(2012),
        start_date: None,
        media_type: "ANIME".into(),
    }];
    seed(&state, &detail, "").await;
    let good = "5555555555555555555555555555555555555555";
    serve(
        &server,
        &state,
        vec![(
            "Code Geass",
            vec![
                row(good, "[Fixture] Code Geass 01-25 (Batch BD 1080p)"),
                row(
                    "6666666666666666666666666666666666666666",
                    "[LNS] Code Geass - Boukoku no Akito (Akito the Exiled) 01-05 (Batch BD 1080p)",
                ),
            ],
        )],
    )
    .await;
    let results = search_batches(&state, &detail).await;
    unset_nyaa_base();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].info_hash, good);
    assert!(
        queries(&server)
            .await
            .iter()
            .all(|q| { normalize_title(unprobe(q)) != normalize_title(related_title) })
    );
}

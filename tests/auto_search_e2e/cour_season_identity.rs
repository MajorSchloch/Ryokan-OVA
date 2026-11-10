//! Search identity checks for a split cour and a later numbered season.
//! These run the strict automatic collector against local Nyaa HTTP rows.
//! They do not bypass the final, conservative source-numbering gate.

use super::*;
use ryokan::services::auto_search::{MatchPhase, find_best_for_target};
use ryokan::services::nyaa::SearchResult;

const BASE: &str = "Mushoku Tensei: Isekai Ittara Honki Dasu";
const DKB_11: &str = "bfd785f3d8e79846b7326c004b41c8108dd66a02";
const DKB_12: &str = "32f88a96041a8c0c9801c0c7a590df84b5b77f54";

fn relation(detail: &AnimeDetail, kind: &str) -> ryokan::services::anilist::RelatedEntry {
    ryokan::services::anilist::RelatedEntry {
        release_date: None,
        id: detail.id,
        id_mal: None,
        title_romaji: detail.title_romaji.clone(),
        title_english: detail.title_english.clone(),
        title_native: detail.title_native.clone(),
        cover_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: detail.episodes,
        relation_type: kind.into(),
        season_year: detail.season_year,
        start_date: None,
        media_type: "ANIME".into(),
    }
}

fn cour_detail() -> AnimeDetail {
    let mut detail = detail_for(127720, &format!("{BASE} Part 2"));
    detail.title_english = "Mushoku Tensei: Jobless Reincarnation Cour 2".into();
    detail.title_native.clear();
    detail.season_year = Some(2021);
    detail.end_year = Some(2021);
    detail.relations = vec![
        relation(&base_detail(), "PREQUEL"),
        relation(&season_detail(), "SEQUEL"),
    ];
    detail
}

fn season_detail() -> AnimeDetail {
    let mut detail = detail_for(146065, "Mushoku Tensei II: Isekai Ittara Honki Dasu");
    detail.title_english = "Mushoku Tensei: Jobless Reincarnation Season 2".into();
    detail.title_native.clear();
    detail.episodes = Some(13);
    detail.season_year = Some(2023);
    detail.end_year = Some(2023);
    let mut prequel = detail_for(127720, &format!("{BASE} Part 2"));
    prequel.title_english = "Mushoku Tensei: Jobless Reincarnation Cour 2".into();
    prequel.title_native.clear();
    prequel.season_year = Some(2021);
    detail.relations = vec![relation(&prequel, "PREQUEL")];
    detail
}

async fn seed_target(state: &AppState, detail: &AnimeDetail, root: &AnimeDetail, offset: i32) {
    let sid = seed_franchise_chain(
        &state.db,
        detail.id,
        &detail.title_romaji,
        root.id,
        &root.title_romaji,
        offset,
    )
    .await;
    sqlx::query("UPDATE series SET episodes=? WHERE id=?")
        .bind(detail.episodes)
        .bind(sid)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert(&state.db, sid, detail.id, None, detail)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert_provider(&state.db, root.id, None, root)
        .await
        .unwrap();
}

fn base_detail() -> AnimeDetail {
    let mut detail = detail_for(108465, BASE);
    detail.title_english = "Mushoku Tensei: Jobless Reincarnation".into();
    detail.title_native.clear();
    detail.episodes = Some(11);
    detail.season_year = Some(2021);
    detail.end_year = Some(2021);
    detail
}

/// A query-specific fixture proves the row first appears in the franchise
/// phase. The other fixture exposes it to every primary/extended query.
async fn serve_rows(server: &MockServer, rows: &str, only_query: Option<&str>) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(
                if only_query.is_none() { rows } else { "" },
            )),
        )
        .with_priority(10)
        .mount(server)
        .await;
    if let Some(query) = only_query {
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("q", query))
            .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(rows)))
            .with_priority(1)
            .expect(1..)
            .mount(server)
            .await;
    }
}

async fn automatic_find(state: &AppState, detail: &AnimeDetail, ep: i32) -> Option<SearchResult> {
    find_best_for_target(
        &state.db,
        detail,
        &default_config(),
        &SearchTarget::Episode(ep),
        false,
        false,
        &[],
        &state.indexers,
    )
    .await
}

async fn mapping_holds(state: &AppState, title: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT detail FROM logs WHERE message=? AND detail LIKE 'Episode mapping:%'",
    )
    .bind(format!("Automatic grab held: {title}"))
    .fetch_all(&state.db)
    .await
    .unwrap()
}

async fn cache_description(state: &AppState, hash: &str) {
    // Source classification must never fetch a real publisher page.
    ryokan::models::nyaa_description_cache::upsert(&state.db, hash, "Source: WEB-DL").await;
}

#[tokio::test]
async fn cour_identity_rejects_actual_2023_dkb_s02e11_and_s02e12() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let detail = cour_detail();
    seed_target(&state, &detail, &base_detail(), 11).await;
    let server = MockServer::start().await;
    let titles: Vec<_> = [11, 12]
        .into_iter()
        .map(|ep| format!("[DKB] Mushoku Tensei - Isekai Ittara Honki Dasu - S02E{ep:02} [1080p][HEVC x265 10bit][Multi-Subs][weekly]"))
        .collect();
    let rows = format!(
        "{}{}",
        nyaa_row(DKB_11, 1718303, &titles[0], "631.8 MiB", 50),
        nyaa_row(DKB_12, 1721028, &titles[1], "647.2 MiB", 50),
    );
    serve_rows(&server, &rows, None).await;
    set_nyaa_base(&server.uri());
    cache_description(&state, DKB_11).await;
    cache_description(&state, DKB_12).await;
    for (ep, title) in [11, 12].into_iter().zip(&titles) {
        assert!(
            automatic_find(&state, &detail, ep).await.is_none(),
            "2021 Cour 2 must not acquire the separate 2023 Season 2: {title}"
        );
        assert!(
            mapping_holds(&state, title).await.is_empty(),
            "wrong-season identity must be rejected before the final numbering gate"
        );
    }
    unset_nyaa_base();
}

#[tokio::test]
async fn cour_identity_accepts_explicit_part_and_cour_relative_s01_episodes() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let detail = cour_detail();
    seed_target(&state, &detail, &base_detail(), 11).await;
    for (index, (title, ep)) in [
        (
            format!("[SubsPlease] {BASE} Part 2 - S01E11 [1080p] [WEB-DL]"),
            11,
        ),
        (
            "[SubsPlease] Mushoku Tensei: Jobless Reincarnation Cour 2 - S01E12 [1080p] [WEB-DL]"
                .into(),
            12,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let server = MockServer::start().await;
        let hash = format!("{:040x}", 6100 + index);
        serve_rows(
            &server,
            &nyaa_row(&hash, 6100 + index as u64, &title, "700 MiB", 50),
            None,
        )
        .await;
        cache_description(&state, &hash).await;
        set_nyaa_base(&server.uri());
        let hit = automatic_find(&state, &detail, ep)
            .await
            .expect("explicit own part/cour with the correct source season must remain selectable");
        assert_eq!(hit.info_hash, hash);
        assert_eq!(hit.match_provenance.unwrap().phase, MatchPhase::Primary);
        assert_eq!(hit.release_mapping.unwrap().episodes(), vec![ep]);
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn cour_identity_absolute_franchise_matches_keep_numbering_review_hold() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let detail = cour_detail();
    seed_target(&state, &detail, &base_detail(), 11).await;
    for (index, (number, label)) in [(22, "22"), (23, "S01E23")].into_iter().enumerate() {
        let server = MockServer::start().await;
        let hash = format!("{:040x}", 6200 + index);
        let title = format!("[SubsPlease] {BASE} - {label} [1080p] [WEB-DL]");
        let query = format!("{BASE} {number}");
        serve_rows(
            &server,
            &nyaa_row(&hash, 6200 + index as u64, &title, "700 MiB", 50),
            Some(&query),
        )
        .await;
        cache_description(&state, &hash).await;
        set_nyaa_base(&server.uri());
        assert!(automatic_find(&state, &detail, number - 11).await.is_none());
        let holds = mapping_holds(&state, &title).await;
        assert!(
            !holds.is_empty(),
            "a legitimate absolute title must pass the franchise identity gate; the existing numbering review hold still prevents unreviewed acquisition: {title}"
        );
        assert!(
            holds
                .iter()
                .all(|reason| reason.contains("continuous numbering")
                    || reason.contains("known episode count")),
            "unexpected final hold: {holds:?}"
        );
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn cour_identity_true_season_two_accepts_s02_but_rejects_old_part_in_fallback() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let detail = season_detail();
    seed_target(&state, &detail, &base_detail(), 23).await;
    let server = MockServer::start().await;
    let title = "[DKB] Mushoku Tensei - Isekai Ittara Honki Dasu - S02E11 [1080p][HEVC x265 10bit][Multi-Subs][weekly]";
    serve_rows(
        &server,
        &nyaa_row(DKB_11, 1718303, title, "631.8 MiB", 50),
        None,
    )
    .await;
    cache_description(&state, DKB_11).await;
    set_nyaa_base(&server.uri());
    let hit = automatic_find(&state, &detail, 11)
        .await
        .expect("the real 2023 Season 2 must retain its S02 alias");
    assert_eq!(hit.info_hash, DKB_11);
    assert_eq!(hit.release_mapping.unwrap().episodes(), vec![11]);
    unset_nyaa_base();
    for (index, marker) in ["Part 2", "Cour 2"].into_iter().enumerate() {
        let server = MockServer::start().await;
        let hash = format!("{:040x}", 6300 + index);
        // The absolute number deliberately collides with the fallback target.
        // Without the part discriminator it reaches the final mapping gate.
        let title = format!("[SubsPlease] {BASE} {marker} - S02E34 [1080p] [WEB-DL]");
        let query = format!("{BASE} 34");
        serve_rows(
            &server,
            &nyaa_row(&hash, 6300 + index as u64, &title, "700 MiB", 50),
            Some(&query),
        )
        .await;
        cache_description(&state, &hash).await;
        set_nyaa_base(&server.uri());
        assert!(automatic_find(&state, &detail, 11).await.is_none());
        assert!(
            mapping_holds(&state, &title).await.is_empty(),
            "franchise fallback must retain the target season's lack of a Part/Cour marker"
        );
        unset_nyaa_base();
    }
}

#[tokio::test]
async fn cour_identity_season_two_part_two_keeps_both_discriminators() {
    let _gate = ENV_LOCK.lock().await;
    let state = build_state().await;
    let mut detail = detail_for(166873, "Mushoku Tensei II: Isekai Ittara Honki Dasu Part 2");
    detail.title_english = "Mushoku Tensei: Jobless Reincarnation Season 2 Part 2".into();
    detail.title_native.clear();
    seed_target(&state, &detail, &season_detail(), 12).await;
    for (index, (marker, accepted)) in [("Part 1", false), ("", false), ("Part 2", true)]
        .into_iter()
        .enumerate()
    {
        let server = MockServer::start().await;
        let hash = format!("{:040x}", 6400 + index);
        let title = format!(
            "[SubsPlease] Mushoku Tensei: Jobless Reincarnation Season 2 {marker} - S02E11 [1080p] [WEB-DL]"
        );
        serve_rows(
            &server,
            &nyaa_row(&hash, 6400 + index as u64, &title, "700 MiB", 50),
            None,
        )
        .await;
        cache_description(&state, &hash).await;
        set_nyaa_base(&server.uri());
        let hit = automatic_find(&state, &detail, 11).await;
        assert_eq!(
            hit.is_some(),
            accepted,
            "compound season/part identity changed for {title}"
        );
        if let Some(hit) = hit {
            assert_eq!(hit.info_hash, hash);
            assert_eq!(hit.release_mapping.unwrap().episodes(), vec![11]);
        }
        unset_nyaa_base();
    }
}

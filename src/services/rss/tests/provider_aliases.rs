//! Provider names are a cache-only exact-match supplement, never canonical
//! season, part, numeric-title or franchise-family evidence.
use super::super::*;
use crate::services::anilist::{AnimeDetail, RelatedEntry};
use crate::test_support::{in_memory_pool, seed_series};

fn detail(series: &series::Series, synonyms: &[&str]) -> AnimeDetail {
    AnimeDetail {
        release_date: None,
        is_adult: false,
        id: series.anilist_id,
        id_mal: None,
        title_romaji: series.title_romaji.clone(),
        title_english: series.title_english.clone(),
        title_native: series.title_native.clone(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: series.format.clone(),
        status: series.status.clone(),
        status_display: "Finished".into(),
        episodes: series.episodes,
        duration: Some(24),
        season: String::new(),
        season_year: series.season_year,
        end_year: series.end_year,
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: synonyms.iter().map(|s| s.to_string()).collect(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

async fn target(db: &sqlx::SqlitePool, id: i64, title: &str) -> series::Series {
    let sid = seed_series(db, id, title).await;
    sqlx::query("UPDATE series SET episodes=12,format='TV',title_english=? WHERE id=?")
        .bind(title)
        .bind(sid)
        .execute(db)
        .await
        .unwrap();
    series::get_by_id(db, sid).await.unwrap().unwrap()
}

async fn cached(
    db: &sqlx::SqlitePool,
    series: &series::Series,
    detail: &AnimeDetail,
) -> Vec<SeriesMeta> {
    crate::models::metadata_cache::upsert(db, series.id, series.anilist_id, None, detail)
        .await
        .unwrap();
    load_series_meta(db, std::slice::from_ref(series)).await
}

fn item(title: &str) -> RssItem {
    RssItem {
        published_at: None,
        seeders: None,
        title: title.into(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: extract_group(title),
        resolution: extract_resolution(title),
        is_batch: detect_batch(title),
        source: RssSource::Nyaa,
    }
}

#[tokio::test]
async fn cached_provider_names_match_many_items_after_the_database_is_closed() {
    let db = in_memory_pool().await;
    let series = target(&db, 99811, "Evening Chronicle").await;
    let plain = SeriesMeta::from_series(&series);
    let meta = cached(
        &db,
        &series,
        &detail(&series, &["Night Voyage", "夜行の物語"]),
    )
    .await;
    assert_eq!(meta[0].original_aliases, plain.original_aliases);
    assert_eq!(meta[0].core_aliases, plain.core_aliases);
    assert_eq!(meta[0].collapsed_core_aliases, plain.collapsed_core_aliases);
    assert_eq!(meta[0].season_num, plain.season_num);
    assert_eq!(meta[0].target_parts, plain.target_parts);
    assert!(best_series_match(&item("[G] Night Voyage - 01 [1080p]"), &[plain]).is_none());
    // Preload is the only cache-read boundary. Matching many items cannot
    // query metadata again or fall back to provider HTTP after this point.
    db.close().await;
    for ep in 1..=6 {
        for alias in ["Night Voyage", "夜行の物語"] {
            let found = best_series_match(&item(&format!("[G] {alias} - {ep:02} [1080p]")), &meta)
                .expect("the full cached provider name must stay available in memory");
            assert_eq!(found.series.id, series.id);
            assert!(found.alias_score >= 1.0);
            assert_eq!(found.resolved_eps, HashSet::from([ep]));
        }
    }
}

#[tokio::test]
async fn missing_wrong_owner_and_malformed_cache_keep_the_canonical_fallback() {
    let db = in_memory_pool().await;
    let series = target(&db, 99812, "Evening Chronicle").await;
    for phase in 0..3 {
        if phase == 1 {
            let mut wrong = detail(&series, &["Night Voyage"]);
            wrong.id += 100;
            crate::models::metadata_cache::upsert(&db, series.id, wrong.id, None, &wrong)
                .await
                .unwrap();
        } else if phase == 2 {
            sqlx::query("UPDATE series_metadata_cache SET detail_json='{' WHERE series_id=?")
                .bind(series.id)
                .execute(&db)
                .await
                .unwrap();
        }
        let meta = load_series_meta(&db, std::slice::from_ref(&series)).await;
        assert!(meta[0].provider_aliases.is_empty());
        assert!(meta[0].related_identity.is_none());
        let canonical = best_series_match(&item("[G] Evening Chronicle - 01 [1080p]"), &meta)
            .expect("a missing or unusable cache must not disable canonical matching");
        assert!(canonical.alias_score >= 1.0);
        assert!(best_series_match(&item("[G] Night Voyage - 01 [1080p]"), &meta).is_none());
    }
}

#[tokio::test]
async fn provider_names_do_not_supply_fragments_or_partial_token_matches() {
    let db = in_memory_pool().await;
    let series = target(&db, 99813, "Evening Chronicle").await;
    let meta = cached(
        &db,
        &series,
        &detail(&series, &["Night Voyage: Voyager Archive"]),
    )
    .await;
    for title in [
        "[G] Night Voyage - 01 [1080p]",
        "[G] Voyager Archive - 01 [1080p]",
        "[G] Night Voyage Voyager Archives - 01 [1080p]",
    ] {
        assert!(best_series_match(&item(title), &meta).is_none(), "{title}");
    }
    let good = best_series_match(
        &item("[G] Night Voyage - Voyager Archive - 01 [1080p]"),
        &meta,
    )
    .expect("the complete name survives normalized punctuation");
    assert!(good.alias_score >= 1.0);
}

#[tokio::test]
async fn short_provider_name_does_not_admit_an_unknown_longer_title() {
    let db = in_memory_pool().await;
    let series = target(&db, 99814, "Evening Chronicle").await;
    let meta = cached(&db, &series, &detail(&series, &["Bocchi"])).await;
    assert!(meta[0].related_identity.is_none());
    assert!(best_series_match(&item("[G] Bocchi the Rock - 01 [WEB 1080p]"), &meta).is_none());
    let genuine = best_series_match(&item("[G] Bocchi - 01 [WEB 1080p]"), &meta)
        .expect("a complete short provider name remains valid");
    assert!(genuine.alias_score >= 1.0);
}

#[tokio::test]
async fn provider_synonym_cannot_rescue_a_missing_numeric_title_identity() {
    let db = in_memory_pool().await;
    let series = target(&db, 1313, "Digimon Adventure 02").await;
    let meta = cached(&db, &series, &detail(&series, &["Digimon Adventure"])).await;
    let wrong =
        "[YakuboEncodes] Digimon Adventure (2020) - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]";
    assert!(best_series_match(&item(wrong), &meta).is_none());
    let good = best_series_match(
        &item("[Keyword] Digimon Adventure 02 - 02 [BD 1080p]"),
        &meta,
    )
    .expect("the actual parsed title contains the identifying02");
    assert_eq!(good.resolved_eps, HashSet::from([2]));
    assert!(good.alias_score >= 1.0);
}

#[tokio::test]
async fn provider_synonyms_cannot_define_season_or_override_part_guards() {
    let db = in_memory_pool().await;
    let mut series = target(
        &db,
        127720,
        "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2",
    )
    .await;
    series.title_english = "Mushoku Tensei: Jobless Reincarnation Cour 2".into();
    series.cumulative_prior_episodes = 11;
    let meta = cached(
        &db,
        &series,
        &detail(&series, &["Night Voyage", "Night Voyage Season 2"]),
    )
    .await;
    assert_eq!(meta[0].season_num, None);
    assert_eq!(meta[0].target_parts, HashSet::from([2]));
    for title in [
        "[G] Night Voyage S02E11 [1080p]",
        "[G] Night Voyage Part 1 S01E11 [1080p]",
        "[G] Night Voyage - 11 [1080p]",
    ] {
        assert!(best_series_match(&item(title), &meta).is_none(), "{title}");
    }
    let good = best_series_match(&item("[G] Night Voyage Part 2 S01E11 [1080p]"), &meta)
        .expect("the full provider name still needs the correct part and season");
    assert!(good.alias_score >= 1.0);
    assert_eq!(good.resolved_eps, HashSet::from([11]));
}

#[tokio::test]
async fn provider_name_keeps_known_related_movie_identity_out_of_parent_rss() {
    let db = in_memory_pool().await;
    let mut series = target(&db, 1575, "Code Geass: Hangyaku no Lelouch").await;
    series.title_english = "Code Geass: Lelouch of the Rebellion".into();
    let related_title = "Code Geass: Akito the Exiled - The Wyvern Arrives";
    let mut cached_detail = detail(&series, &["Code Geass", related_title]);
    cached_detail.relations = vec![RelatedEntry {
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
    let meta = cached(&db, &series, &cached_detail).await;
    assert!(
        !meta[0]
            .provider_aliases
            .contains(&auto_search::normalize_title(related_title))
    );
    assert!(
        best_series_match(
            &item("[LNS] Code Geass - Boukoku no Akito (Akito the Exiled) - 01 [BD 1080p]"),
            &meta,
        )
        .is_none()
    );
    let good = best_series_match(&item("[G] Code Geass - 01 [1080p]"), &meta)
        .expect("the safe own provider name still matches the parent");
    assert_eq!(good.series.anilist_id, 1575);
    assert!(good.alias_score >= 1.0);
}

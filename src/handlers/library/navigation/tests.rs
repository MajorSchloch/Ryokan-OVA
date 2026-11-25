use super::*;
use crate::services::anilist;
use crate::{
    models::{library_layout, metadata_cache},
    test_support::{build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series},
};
use serde_json::json;

async fn release(db: &SqlitePool, id: i64, title: &str, root: &str) -> series::Series {
    let detail = anilist::parse_media_node(&json!({"id":id,"type":"ANIME","title":{"english":title,"romaji":format!("Alias {id}")},"format":"TV","episodes":12,"status":"FINISHED","seasonYear":2020,"startDate":{"year":2020,"month":1,"day":1}})).unwrap();
    seed_series(db, id, title).await;
    metadata_cache::upsert_provider(db, id, None, &detail)
        .await
        .unwrap();
    let row = series::get_by_anilist_id(db, id).await.unwrap().unwrap();
    let mut tx = db.begin().await.unwrap();
    library_layout::record_move(&mut tx, row.id, &format!("{root}/{title}"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    row
}

async fn seed_review(db: &SqlitePool, series_id: i64, provider: i64, episode: i32) -> i64 {
    let grab = seed_grabbed_torrent(db, series_id, "edition", "Short edition", &[episode]).await;
    sqlx::query("UPDATE grabbed_torrents SET state='failed' WHERE id=?")
        .bind(grab)
        .execute(db)
        .await
        .unwrap();
    sqlx::query_scalar(
        "INSERT INTO episode_edition_reviews
         (grab_id, grabbed_at, series_id, provider_id, episode, source_root, source_path,
          source_identity, current_path, current_identity, reason)
         SELECT id, grabbed_at, series_id, ?, ?, '', 'candidate.mkv', '', 'current.mkv', '', 'Edition differs'
         FROM grabbed_torrents WHERE id=? RETURNING id",
    )
    .bind(provider)
    .bind(episode)
    .bind(grab)
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn saved_franchises_do_not_merge_through_provider_relations() {
    let db = in_memory_pool().await;
    let first = release(&db, 100, "First show", "First").await;
    let second = release(&db, 200, "Second show", "Second").await;
    let detail = anilist::parse_media_node(&json!({"id":100,"title":{"english":"First show"},"type":"ANIME","relations":{"edges":[{"relationType":"SEQUEL","node":{"id":200,"type":"ANIME","title":{"english":"Second show"}}}]}})).unwrap();
    metadata_cache::upsert_provider(&db, 100, None, &detail)
        .await
        .unwrap();
    let releases = vec![
        (first, CardProgress::default()),
        (second, CardProgress::default()),
    ];
    let (total, cards) = cards(&db, &releases, None, "", "title_asc", "")
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(cards.len(), 2);
    assert!(cards.iter().all(|c| c.tracked == 1));
    assert_ne!(cards[0].path, cards[1].path);
}

#[tokio::test]
async fn poster_group_search_scope_and_sort_keep_complete_franchises() {
    let db = in_memory_pool().await;
    let first = release(&db, 100, "First show", "Franchise A").await;
    let second = release(&db, 200, "Second season", "Franchise A").await;
    let other = release(&db, 300, "Other show", "Franchise Z").await;
    let ids = HashSet::from([second.id]);
    let releases = vec![
        (first, CardProgress::default()),
        (second, CardProgress::default()),
        (other, CardProgress::default()),
    ];
    let (total, selected) = cards(&db, &releases, Some(&ids), "Alias 200", "title_asc", "")
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].tracked, 2);
    assert!(selected[0].matches_search);
    let (_, all) = cards(&db, &releases, None, "Second season", "title_desc", "")
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        2,
        "All cards stay in the DOM so clearing a live search recovers them"
    );
    assert_eq!(all[0].title, "Franchise Z");
    assert_eq!(all.iter().filter(|c| c.matches_search).count(), 1);
    assert_eq!(
        all[1].progress.state, "idle",
        "Offline roots never report complete"
    );
}

#[tokio::test]
async fn parent_page_is_read_only_and_links_releases_by_provider_identity() {
    let db = in_memory_pool().await;
    let first = release(&db, 100, "First show", "Franchise").await;
    release(&db, 200, "Second season", "Franchise").await;
    let page_id = library_layout::page_for_provider(&db, 100)
        .await
        .unwrap()
        .unwrap()
        .0;
    let state = build_test_app_state(db.clone(), None);
    let response = page(State(state), Path(page_id), Query(Default::default()))
        .await
        .unwrap();
    assert!(response.0.contains("/anime/anilist/100"));
    assert!(response.0.contains("/anime/anilist/200"));
    assert!(!response.0.contains(&format!("id=\"series-{}\"", first.id)));
    assert!(response.0.contains("Automatically add future releases"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM release_follows")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM series")
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
    assert!(
        sqlx::query_scalar::<_, String>("SELECT folder_name FROM series WHERE anilist_id=100")
            .fetch_one(&db)
            .await
            .unwrap()
            .contains("First show")
    );
}

#[tokio::test]
async fn release_rows_support_watch_order_and_hiding_unfollowed() {
    let db = in_memory_pool().await;
    let original = release(&db, 100, "Original", "Franchise").await;
    let prequel = release(&db, 200, "Prequel", "Franchise").await;
    sqlx::query("UPDATE series SET monitor_mode = 'none' WHERE id IN (?, ?)")
        .bind(original.id)
        .bind(prequel.id)
        .execute(&db)
        .await
        .unwrap();
    let detail = anilist::parse_media_node(&json!({
        "id": 100, "type": "ANIME", "title": {"english": "Original"},
        "format": "TV", "status": "FINISHED", "episodes": 12,
        "startDate": {"year": 2020, "month": 1, "day": 1},
        "relations": {"edges": [
            {"relationType": "PREQUEL", "node": {"id": 200, "type": "ANIME",
                "title": {"english": "Prequel"}, "startDate": {"year": 2022, "month": 1, "day": 1}}},
            {"relationType": "SIDE_STORY", "node": {"id": 300, "type": "ANIME",
                "title": {"english": "Side story"}, "startDate": {"year": 2024, "month": 1, "day": 1}}}
        ]}
    })).unwrap();
    metadata_cache::upsert_provider(&db, 100, None, &detail)
        .await
        .unwrap();
    let detail = anilist::parse_media_node(&json!({
        "id": 200, "type": "ANIME", "title": {"english": "Prequel"},
        "format": "TV", "status": "FINISHED", "episodes": 12,
        "startDate": {"year": 2022, "month": 1, "day": 1}
    }))
    .unwrap();
    metadata_cache::upsert_provider(&db, 200, None, &detail)
        .await
        .unwrap();
    crate::models::release_follows::start(&db, 100, "Franchise", true, true, false)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO franchise_choices(anilist_id, follow_id, title, selected)
                 SELECT 100, id, 'Original', 1 FROM release_follows WHERE anilist_id = 100",
    )
    .execute(&db)
    .await
    .unwrap();

    let page_id = library_layout::page_for_provider(&db, 100)
        .await
        .unwrap()
        .unwrap()
        .0;
    let render = |filters| {
        page(
            State(build_test_app_state(db.clone(), None)),
            Path(page_id.clone()),
            Query(filters),
        )
    };
    let rows = |html: String| {
        let document = scraper::Html::parse_document(&html);
        let row =
            scraper::Selector::parse("[data-franchise-list] > [data-franchise-release]").unwrap();
        let title = scraper::Selector::parse("h4 a, h5 a").unwrap();
        document
            .select(&row)
            .map(|entry| {
                (
                    entry
                        .select(&title)
                        .next()
                        .unwrap()
                        .text()
                        .collect::<String>(),
                    entry.attr("data-followed") == Some("true"),
                    entry.attr("hidden").is_some(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rows(render(FranchiseFilters::default()).await.unwrap().0),
        [
            ("Original".into(), true, false),
            ("Prequel".into(), false, false),
            ("Side story".into(), false, false),
        ]
    );
    let filters = FranchiseFilters {
        f_order: "watch".into(),
        f_hide_unfollowed: "1".into(),
        ..Default::default()
    };
    assert_eq!(
        rows(render(filters).await.unwrap().0),
        [
            ("Prequel".into(), false, true),
            ("Original".into(), true, false),
            ("Side story".into(), false, true),
        ]
    );

    let storylines = render(FranchiseFilters {
        f_order: "storylines".into(),
        f_q: "original".into(),
        ..Default::default()
    })
    .await
    .unwrap()
    .0;
    let document = scraper::Html::parse_document(&storylines);
    let selector = |css| scraper::Selector::parse(css).unwrap();
    let guide = document
        .select(&selector("a[href^='/discover']"))
        .next()
        .unwrap();
    assert_eq!(
        guide.attr("href"),
        Some("/discover?source=al&franchise_id=100&order=storylines&guide=true")
    );
    assert!(
        document
            .select(&selector("option[value='storylines'][selected]"))
            .next()
            .is_some()
    );
    assert!(
        document
            .select(&selector(
                "details[data-franchise-section][hidden] #franchise-release-300"
            ))
            .next()
            .is_some()
    );
    assert_eq!(
        rows(storylines),
        [
            ("Prequel".into(), false, true),
            ("Original".into(), true, false),
            ("Side story".into(), false, true),
        ]
    );
}

#[test]
fn watch_order_falls_back_when_links_are_conflicting_or_outside_the_franchise() {
    let mut entries = [groups::Entry::default(), groups::Entry::default()];
    entries[0].provider_id = 1;
    entries[0].watch_predecessors = vec![2];
    entries[1].provider_id = 2;
    entries[1].watch_predecessors = vec![1];
    assert_eq!(groups::watch_sequence(&entries[..1]), None);
    assert_eq!(groups::watch_sequence(&entries), None);
}

async fn index_html(db: &SqlitePool, individual: Option<&str>, cookie: Option<&str>) -> String {
    index_html_search(db, individual, cookie, "").await
}

async fn index_html_search(
    db: &SqlitePool,
    individual: Option<&str>,
    cookie: Option<&str>,
    search: &str,
) -> String {
    let mut headers = axum::http::HeaderMap::new();
    if let Some(cookie) = cookie {
        headers.insert(axum::http::header::COOKIE, cookie.parse().unwrap());
    }
    let response = super::super::pages::index(
        State(build_test_app_state(db.clone(), None)),
        axum::extract::Query(super::super::pages::LibraryIndexQuery {
            individual: individual.map(str::to_owned),
            search: (!search.is_empty()).then(|| search.to_owned()),
            ..Default::default()
        }),
        headers,
        axum_htmx::HxRequest(false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn grouped_posters_are_default_and_release_mode_keeps_bulk_library_ids() {
    let db = in_memory_pool().await;
    let first = release(&db, 100, "First show", "Franchise").await;
    let second = release(&db, 200, "Second season", "Franchise").await;
    let grouped = index_html(&db, None, Some("library_view=groups")).await;
    let document = scraper::Html::parse_document(&grouped);
    let selector = |s| scraper::Selector::parse(s).unwrap();
    assert_eq!(document.select(&selector(".franchise-card")).count(), 1);
    assert_eq!(
        document
            .select(&selector(".series-card, #bulk-select-toggle"))
            .count(),
        0
    );
    assert!(grouped.contains("1 franchise &middot; 2 releases"));
    let releases = index_html(&db, Some("1"), None).await;
    let document = scraper::Html::parse_document(&releases);
    assert_eq!(document.select(&selector(".series-card")).count(), 2);
    assert_eq!(document.select(&selector(".franchise-card")).count(), 0);
    for row in [first, second] {
        assert!(releases.contains(&format!("id=\"series-{}\"", row.id)));
        assert!(releases.contains(&format!("href=\"/anime/anilist/{}\"", row.anilist_id)));
    }
    assert_eq!(document.select(&selector("#bulk-select-toggle")).count(), 1);
    let preferred = index_html(&db, None, Some("library_view=poster-releases")).await;
    assert!(preferred.contains("class=\"series-card\""));
    let explicit = index_html(&db, Some("0"), Some("library_view=poster-releases")).await;
    assert!(explicit.contains("class=\"franchise-card\""));
}

#[tokio::test]
async fn library_ratings_use_the_main_release_and_omit_unknown_scores() {
    let db = in_memory_pool().await;
    release(&db, 100, "First show", "Franchise").await;
    release(&db, 200, "Second season", "Franchise").await;
    release(&db, 300, "Unrated show", "Unrated").await;
    for (id, score) in [(100, 84), (200, 97)] {
        let mut detail = metadata_cache::get_by_provider_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .detail;
        detail.average_score = Some(score);
        metadata_cache::upsert_provider(&db, id, None, &detail)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE provider_metadata_cache SET cached_at=datetime('now','-20 days') WHERE provider_id=100")
        .execute(&db)
        .await
        .unwrap();
    let badges = |html, selector| {
        let doc = scraper::Html::parse_document(html);
        let selector = scraper::Selector::parse(selector).unwrap();
        let mut ratings: Vec<_> = doc
            .select(&selector)
            .map(|el| el.text().last().unwrap_or_default().trim().to_owned())
            .collect();
        ratings.sort_unstable();
        ratings
    };
    let grouped = index_html(&db, None, None).await;
    assert_eq!(
        badges(&grouped, ".franchise-card .provider-rating"),
        ["84%"]
    );
    let individual = index_html(&db, Some("1"), None).await;
    assert_eq!(
        badges(&individual, ".series-card .provider-rating"),
        ["84%", "97%"]
    );

    let (page_id, _) = library_layout::page_for_provider(&db, 100)
        .await
        .unwrap()
        .unwrap();
    let Html(franchise) = page(
        State(build_test_app_state(db.clone(), None)),
        Path(page_id),
        Query(Default::default()),
    )
    .await
    .unwrap();
    assert_eq!(
        badges(&franchise, ".franchise-heading .provider-rating"),
        ["84%"]
    );
    assert_eq!(
        badges(&franchise, ".franchise-release-row .provider-rating"),
        ["84%", "97%"]
    );

    let response = super::super::table::page(State(db)).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let table = std::str::from_utf8(&bytes).unwrap();
    assert_eq!(badges(table, "tbody .provider-rating"), ["84%", "97%"]);
}

#[tokio::test]
async fn poster_review_link_opens_only_held_releases_on_first_load() {
    let db = in_memory_pool().await;
    release(&db, 100, "First show", "Franchise").await;
    let second = release(&db, 200, "Second season", "Franchise").await;
    let review = seed_review(&db, second.id, 200, 2).await;
    let (page_id, _) = library_layout::page_for_provider(&db, 100)
        .await
        .unwrap()
        .unwrap();

    let html = index_html(&db, None, None).await;
    let document = scraper::Html::parse_document(&html);
    let selector = |s| scraper::Selector::parse(s).unwrap();
    let link = document
        .select(&selector(".franchise-card .franchise-status-review"))
        .next()
        .unwrap();
    let query = link.value().attr("href").unwrap();
    assert_eq!(
        query,
        format!("/franchises/{page_id}?tab=releases&f_status=review")
    );
    let query = format!("http://example.test{query}").parse().unwrap();
    let Query(filters) = Query::try_from_uri(&query).unwrap();
    let response = page(
        State(build_test_app_state(db, None)),
        Path(page_id),
        Query(filters),
    )
    .await
    .unwrap();
    let document = scraper::Html::parse_document(&response.0);
    let rows = document
        .select(&selector("[data-franchise-release]:not([hidden])"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].text().any(|text| text == "Second season"));
    assert_eq!(
        rows[0]
            .select(&selector("[data-franchise-review]"))
            .next()
            .unwrap()
            .value()
            .attr("href"),
        Some(format!("/system/edition-review/{review}").as_str())
    );
    assert!(
        document
            .select(&selector(
                "select[name=f_status] option[selected][value=review]"
            ))
            .next()
            .is_some()
    );
}

#[tokio::test]
async fn saved_boundaries_hold_across_an_untracked_bridge_and_follow_preview() {
    let db = in_memory_pool().await;
    let first = release(&db, 100, "First show", "First").await;
    let second = release(&db, 200, "Second show", "Second").await;
    for (id, peer) in [(100, 150), (150, 200)] {
        let detail = anilist::parse_media_node(&json!({"id":id,"title":{"english":format!("Title {id}")},"type":"ANIME","relations":{"edges":[{"relationType":"SEQUEL","node":{"id":peer,"type":"ANIME","title":{"english":format!("Title {peer}")}}}]}})).unwrap();
        metadata_cache::upsert_provider(&db, id, None, &detail)
            .await
            .unwrap();
    }
    let releases = vec![
        (first, CardProgress::default()),
        (second, CardProgress::default()),
    ];
    let (total, cards) = cards(&db, &releases, None, "", "title_asc", "")
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert!(cards.iter().all(|c| c.tracked == 1));
    let preview = groups::preview::page(
        State(db.clone()),
        axum::extract::Path(100),
        axum::extract::Query(vec![]),
    )
    .await
    .unwrap()
    .0;
    assert!(
        !preview.contains("/anime/anilist/150"),
        "A shared outside release must not be assigned to an arbitrary parent"
    );
    assert!(!preview.contains("/anime/anilist/200"));
    assert!(
        crate::models::release_follows::list(&db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn untracked_child_links_to_saved_parent_and_mal_identity_keeps_its_page() {
    let db = in_memory_pool().await;
    release(&db, 100, "Main title", "Parent").await;
    let detail = anilist::parse_media_node(&json!({"id":100,"type":"ANIME","title":{"english":"Main title"},"relations":{"edges":[{"relationType":"SEQUEL","node":{"id":200,"type":"ANIME","title":{"english":"Upcoming sequel"}}}]}})).unwrap();
    metadata_cache::upsert_provider(&db, 100, None, &detail)
        .await
        .unwrap();
    let parent = library_layout::page_for_provider(&db, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent_for_provider(&db, 200).await.unwrap(), Some(parent));
    let row = seed_series(&db, -800, "MAL release").await;
    sqlx::query("UPDATE series SET mal_id=800 WHERE id=?")
        .bind(row)
        .execute(&db)
        .await
        .unwrap();
    let detail = anilist::parse_media_node(
        &json!({"id":8000,"idMal":800,"type":"ANIME","title":{"english":"MAL release"}}),
    )
    .unwrap();
    metadata_cache::upsert_provider(&db, 8000, Some(800), &detail)
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    library_layout::record_move(&mut tx, row, "MAL parent/MAL release")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let page_id = library_layout::page_for_provider(&db, -800)
        .await
        .unwrap()
        .unwrap()
        .0;
    let response = page(
        State(build_test_app_state(db, None)),
        Path(page_id),
        Query(Default::default()),
    )
    .await
    .unwrap();
    assert!(response.0.contains("/anime/mal/800"));
}

#[tokio::test]
async fn individual_search_retains_all_cards_when_toggling_or_reloading_a_filtered_view() {
    let db = in_memory_pool().await;
    let first = release(&db, 100, "First show", "Parent").await;
    release(&db, 200, "Second season", "Parent").await;
    let html =
        index_html_search(&db, Some("1"), Some("library_view=poster"), "Second season").await;
    let doc = scraper::Html::parse_document(&html);
    let selector = |value| scraper::Selector::parse(value).unwrap();
    assert_eq!(doc.select(&selector(".series-card")).count(), 2);
    let hidden_selector = scraper::Selector::parse(&format!("#series-{}", first.id)).unwrap();
    let hidden = doc.select(&hidden_selector).next().unwrap();
    assert_eq!(hidden.value().attr("style"), Some("display: none"));
    let html = index_html_search(&db, Some("1"), None, "No matching title").await;
    let doc = scraper::Html::parse_document(&html);
    assert_eq!(doc.select(&selector(".series-card")).count(), 2);
    assert_eq!(
        doc.select(&selector("#library-grid"))
            .next()
            .unwrap()
            .value()
            .attr("style"),
        Some("display: none")
    );
    assert!(
        doc.select(&selector("#library-search-empty"))
            .next()
            .unwrap()
            .value()
            .attr("style")
            .is_none()
    );
}

#[tokio::test]
async fn grouping_preference_survives_the_table_and_query_values_override_it() {
    let db = in_memory_pool().await;
    release(&db, 100, "First show", "Parent").await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        "library_view=table; library_individual=1".parse().unwrap(),
    );
    let response = super::super::pages::index(
        State(build_test_app_state(db.clone(), None)),
        axum::extract::Query(super::super::pages::LibraryIndexQuery {
            view: Some("poster".into()),
            ..Default::default()
        }),
        headers,
        axum_htmx::HxRequest(false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .count(),
        2
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("class=\"series-card\"")
    );
    let explicit = index_html(&db, Some("0"), Some("library_individual=1")).await;
    assert!(explicit.contains("class=\"franchise-card\""));
}

/// Local, isolated browser fixture. It has no download clients, background jobs,
/// account data, or live-library connection. Start explicitly for visual QA.
#[tokio::test]
#[ignore]
async fn navigation_browser_preview() {
    if std::env::var("RYOKAN_NAVIGATION_PREVIEW").as_deref() != Ok("1") {
        return;
    }
    let db = in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    config::save_config(
        &db,
        &config::Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            title_language: "english".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for (id, title, root, kind, episodes, have) in [
        (100, "Bakemonogatari", "Monogatari", "TV", 15, 15),
        (200, "Nisemonogatari", "Monogatari", "TV", 11, 8),
        (
            300,
            "Kizumonogatari Part 1 - Tekketsu",
            "Monogatari",
            "MOVIE",
            1,
            1,
        ),
        (
            400,
            "Demon Slayer - Kimetsu no Yaiba",
            "Demon Slayer",
            "TV",
            26,
            26,
        ),
        (
            500,
            "Demon Slayer - Kimetsu no Yaiba - The Movie - Mugen Train",
            "Demon Slayer",
            "MOVIE",
            1,
            0,
        ),
        (600, "BOCCHI THE ROCK!", "BOCCHI THE ROCK!", "TV", 12, 12),
        (
            800,
            "Delicious in Dungeon",
            "Delicious in Dungeon",
            "TV",
            24,
            0,
        ),
        (
            900,
            "Delicious in Dungeon Season 2",
            "Delicious in Dungeon",
            "TV",
            12,
            0,
        ),
        (
            1000,
            "A partially monitored season",
            "Partial monitoring",
            "TV",
            6,
            2,
        ),
        (
            1100,
            "A monitored movie",
            "Partial monitoring",
            "MOVIE",
            1,
            0,
        ),
    ] {
        let row = release(&db, id, title, root).await;
        let folder = format!("{root}/{title}");
        sqlx::query("UPDATE series SET title_english=?,folder_name=?,format=?,episodes=?,status='FINISHED',monitor_mode='all',added_at='2026-11-08 12:00:00',cover_url=? WHERE id=?")
            .bind(title).bind(&folder).bind(kind).bind(episodes).bind(format!("/preview/poster/{id}")).bind(row.id).execute(&db).await.unwrap();
        let wanted = if id == 800 {
            0
        } else if id == 1000 {
            4
        } else {
            episodes
        };
        let states = (1..=episodes)
            .map(
                |episode_number| crate::models::monitoring::EpisodeMonitorState {
                    episode_number,
                    monitored: episode_number <= wanted,
                },
            )
            .collect::<Vec<_>>();
        crate::models::monitoring::replace_series_states(&db, row.id, &states)
            .await
            .unwrap();
        if id == 800 {
            sqlx::query("UPDATE series SET monitor_mode='none' WHERE id=?")
                .bind(row.id)
                .execute(&db)
                .await
                .unwrap();
        }
        if id == 900 {
            sqlx::query("UPDATE series SET status='NOT_YET_RELEASED',season_year=2099 WHERE id=?")
                .bind(row.id)
                .execute(&db)
                .await
                .unwrap();
            let detail = anilist::parse_media_node(&json!({"id":id,"type":"ANIME","title":{"english":title},"format":kind,"episodes":episodes,"status":"NOT_YET_RELEASED","seasonYear":2099,"startDate":{"year":2099,"month":6},"coverImage":{"large":format!("/preview/poster/{id}")}})).unwrap();
            metadata_cache::upsert_provider(&db, id, None, &detail)
                .await
                .unwrap();
        }
        if id == 1000 {
            sqlx::query("INSERT INTO episode_quality_tags(series_id,episode_number,state) VALUES(?,3,'grabbed')").bind(row.id).execute(&db).await.unwrap();
        }
        std::fs::create_dir_all(temp.path().join(&folder)).unwrap();
        for ep in 1..=have {
            std::fs::write(
                temp.path()
                    .join(&folder)
                    .join(format!("Show - S01E{ep:02}.mkv")),
                b"fixture",
            )
            .unwrap();
        }
        if id == 200 {
            seed_review(&db, row.id, id, 7).await;
        }
    }
    let detail = anilist::parse_media_node(&json!({"id":600,"type":"ANIME","title":{"english":"BOCCHI THE ROCK!"},"relations":{"edges":[{"relationType":"SEQUEL","node":{"id":700,"type":"ANIME","title":{"english":"BOCCHI THE ROCK! Season 2"},"format":"TV","status":"NOT_YET_RELEASED","seasonYear":2027,"coverImage":{"large":"/preview/poster/700"}}}]}})).unwrap();
    metadata_cache::upsert_provider(&db, 600, None, &detail)
        .await
        .unwrap();
    let detail = anilist::parse_media_node(&json!({"id":100,"type":"ANIME","title":{"english":"Bakemonogatari"},"relations":{"edges":[{"relationType":"SIDE_STORY","node":{"id":1200,"type":"ANIME","title":{"english":"Monogatari Side Story","romaji":"Koyomi history"},"format":"OVA","status":"FINISHED","seasonYear":2019,"coverImage":{"large":"/preview/poster/1200"}}},{"relationType":"SEQUEL","node":{"id":1300,"type":"ANIME","title":{"english":"Monogatari Future Season"},"format":"ONA","status":"NOT_YET_RELEASED","seasonYear":2099,"coverImage":{"large":"/preview/poster/1300"}}}]}})).unwrap();
    metadata_cache::upsert_provider(&db, 100, None, &detail)
        .await
        .unwrap();
    crate::models::release_follows::start(&db, 100, "Monogatari", true, true, false)
        .await
        .unwrap();
    async fn poster(Path(id): Path<String>) -> impl axum::response::IntoResponse {
        // Deliberately stylized fixture covers; these are not provider artwork.
        (
            [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="320" height="480" viewBox="0 0 320 480"><rect width="320" height="480" fill="#303044"/><circle cx="250" cy="110" r="140" fill="#777082"/><path d="M0 390L160 160l160 230v90H0" fill="#20232c"/><text x="24" y="430" fill="#eee" font-family="sans-serif" font-size="24">RELEASE {id}</text></svg>"##
            ),
        )
    }
    let state = build_test_app_state(db.clone(), None);
    let router = axum::Router::new()
        .route("/", axum::routing::get(super::super::pages::index))
        .route("/franchises/{id}", axum::routing::get(page))
        .route("/library/groups", axum::routing::get(groups::page))
        .route(
            "/library/table",
            axum::routing::get(super::super::table::page),
        )
        .route(
            "/library/groups/{seed}/preview",
            axum::routing::get(groups::preview::page),
        )
        .route(
            "/library/groups/{seed}/follow",
            axum::routing::post(groups::follow::save),
        )
        .route("/preview/poster/{id}", axum::routing::get(poster))
        .route(
            "/system/edition-review/{id}",
            axum::routing::get(crate::handlers::edition_review::page)
                .post(crate::handlers::edition_review::decide),
        )
        .merge(super::super::detail::routes())
        .nest_service("/static", tower_http::services::ServeDir::new("static"))
        .with_state(state);
    let port = std::env::var("RYOKAN_NAVIGATION_PREVIEW_PORT").unwrap_or_else(|_| "18979".into());
    let address = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await.unwrap();
    println!("Franchise fixture ready at http://{address}");
    axum::serve(listener, router).await.unwrap();
}

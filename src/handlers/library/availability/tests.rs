use super::*;
use crate::{
    models::{config, library_layout, monitoring, release_follows},
    test_support::{build_test_app_state, seed_series},
};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
};

async fn fixture(root: &std::path::Path) -> SqlitePool {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::models::migrate(&db).await.unwrap();
    config::save_config(
        &db,
        &config::Config {
            media_root: root.to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db
}
async fn release(
    db: &SqlitePool,
    provider: i64,
    title: &str,
    total: Option<i32>,
    status: &str,
    mode: &str,
    parent: &str,
) -> series::Series {
    let id = seed_series(db, provider, title).await;
    let folder = format!("{parent}/{title}");
    sqlx::query("UPDATE series SET episodes=?,status=?,monitor_mode=?,folder_name=?,added_at='2026-11-08 12:00:00' WHERE id=?")
        .bind(total).bind(status).bind(mode).bind(&folder).bind(id).execute(db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    library_layout::record_move(&mut tx, id, &folder)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    series::get_by_id(db, id).await.unwrap().unwrap()
}
async fn states(db: &SqlitePool, row: &series::Series, total: i32, wanted: &[i32]) {
    let states: Vec<_> = (1..=total)
        .map(|ep| EpisodeMonitorState {
            episode_number: ep,
            monitored: wanted.contains(&ep),
        })
        .collect();
    monitoring::replace_series_states(db, row.id, &states)
        .await
        .unwrap();
}
fn file(root: &std::path::Path, row: &series::Series, name: &str) {
    let path = root.join(&row.folder_name).join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"fixture").unwrap();
}

#[tokio::test]
async fn delicious_unmonitored_first_season_and_future_sequel_are_neutral() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    let first = release(
        &db,
        100,
        "Delicious in Dungeon",
        Some(24),
        "FINISHED",
        "none",
        "Delicious in Dungeon",
    )
    .await;
    let second = release(
        &db,
        200,
        "Delicious in Dungeon Season 2",
        Some(12),
        "NOT_YET_RELEASED",
        "all",
        "Delicious in Dungeon",
    )
    .await;
    states(&db, &first, 24, &[]).await;
    states(&db, &second, 12, &(1..=12).collect::<Vec<_>>()).await;
    let library = vec![first.clone(), second.clone()];
    let before:Vec<(i64,i32,bool)>=sqlx::query_as("SELECT series_id,episode_number,monitored FROM episode_monitor_state ORDER BY series_id,episode_number").fetch_all(&db).await.unwrap();
    sqlx::query("PRAGMA query_only=ON")
        .execute(&db)
        .await
        .unwrap();
    let progress = load(&db, root.path().to_str().unwrap(), &library)
        .await
        .unwrap();
    assert_eq!(
        (progress[&first.id].missing, progress[&first.id].unmonitored),
        (0, 24)
    );
    assert_eq!(
        (progress[&second.id].missing, progress[&second.id].future),
        (0, 12)
    );
    let combined = aggregate(progress.values());
    assert_eq!(
        (
            combined.state.as_str(),
            combined.downloaded,
            combined.missing,
            combined.future,
            combined.unmonitored
        ),
        ("idle", 0, 0, 12, 24)
    );
    assert!(
        combined
            .segments
            .iter()
            .all(|s| s.kind != "missing" && s.kind != "available")
    );
    let after:Vec<(i64,i32,bool)>=sqlx::query_as("SELECT series_id,episode_number,monitored FROM episode_monitor_state ORDER BY series_id,episode_number").fetch_all(&db).await.unwrap();
    assert_eq!(before, after, "Browsing must preserve explicit monitoring");
}

#[tokio::test]
async fn partial_monitoring_range_files_downloads_and_movies_have_disjoint_counts() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    let series = release(
        &db,
        100,
        "Partial season",
        Some(6),
        "FINISHED",
        "all",
        "Parent",
    )
    .await;
    states(&db, &series, 6, &[1, 2, 3, 4]).await;
    file(root.path(), &series, "Show S01E01-E02.mkv");
    file(root.path(), &series, "Duplicate S01E01.mkv");
    file(root.path(), &series, "Show S01E05-E06.mkv");
    file(root.path(), &series, "Specials/Show S00E03.mkv");
    sqlx::query("INSERT INTO episode_quality_tags(series_id,episode_number,state) VALUES(?,3,'grabbed'),(?,4,'completed')")
        .bind(series.id).bind(series.id).execute(&db).await.unwrap();
    let movie = release(&db, 200, "Movie", Some(1), "FINISHED", "all", "Parent").await;
    states(&db, &movie, 1, &[1]).await;
    let future = release(
        &db,
        300,
        "Future movie",
        Some(1),
        "NOT_YET_RELEASED",
        "all",
        "Parent",
    )
    .await;
    states(&db, &future, 1, &[1]).await;
    let p = load(
        &db,
        root.path().to_str().unwrap(),
        &[series.clone(), movie.clone(), future],
    )
    .await
    .unwrap();
    let first = &p[&series.id];
    assert_eq!(
        (
            first.downloaded,
            first.downloading,
            first.missing,
            first.unmonitored
        ),
        (2, 1, 1, 2)
    );
    assert_eq!(first.files_label, "4 episodes on disk");
    assert_eq!(first.monitoring_label, "Monitoring 4 of 6 episodes");
    assert_eq!(p[&movie.id].monitoring_label, "Monitoring 1 episode");
    let combined = aggregate(p.values());
    assert_eq!(
        (
            combined.downloaded,
            combined.downloading,
            combined.missing,
            combined.future,
            combined.unmonitored
        ),
        (2, 1, 2, 1, 2)
    );
    assert_eq!(combined.segments.iter().map(|s| s.count).sum::<i64>(), 8);
}

#[tokio::test]
async fn explicit_episode_choice_overrides_the_series_mode_and_unknown_is_not_missing() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    let row = release(
        &db,
        100,
        "Individual override",
        Some(3),
        "FINISHED",
        "none",
        "Parent",
    )
    .await;
    states(&db, &row, 3, &[2]).await;
    let airing = release(
        &db,
        200,
        "Unknown airing dates",
        Some(6),
        "RELEASING",
        "all",
        "Parent",
    )
    .await;
    states(&db, &airing, 6, &(1..=6).collect::<Vec<_>>()).await;
    let p = load(
        &db,
        root.path().to_str().unwrap(),
        &[row.clone(), airing.clone()],
    )
    .await
    .unwrap();
    assert_eq!((p[&row.id].missing, p[&row.id].unmonitored), (1, 2));
    assert_eq!(p[&row.id].monitoring_label, "Monitoring 1 of 3 episodes");
    assert_eq!((p[&airing.id].missing, p[&airing.id].unknown), (0, 6));
    let offline = load(
        &db,
        "/not-mounted/ryokan-availability",
        std::slice::from_ref(&row),
    )
    .await
    .unwrap();
    assert_eq!((offline[&row.id].missing, offline[&row.id].unknown), (0, 1));
    assert_eq!(offline[&row.id].state, "idle");
}

#[tokio::test]
async fn future_unknown_counts_and_dates_keep_their_precision() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    let row = release(
        &db,
        100,
        "Future season",
        None,
        "NOT_YET_RELEASED",
        "future",
        "Parent",
    )
    .await;
    let detail=crate::services::anilist::parse_media_node(&serde_json::json!({"id":100,"type":"ANIME","title":{"english":"Future season"},"status":"NOT_YET_RELEASED","startDate":{"year":2099,"month":6}})).unwrap();
    crate::models::metadata_cache::upsert_provider(&db, 100, None, &detail)
        .await
        .unwrap();
    let p = load(
        &db,
        root.path().to_str().unwrap(),
        std::slice::from_ref(&row),
    )
    .await
    .unwrap();
    assert_eq!(
        (p[&row.id].missing, p[&row.id].downloaded, p[&row.id].pct),
        (0, 0, 0)
    );
    assert_eq!(p[&row.id].state, "idle");
    assert_eq!(p[&row.id].next_airing, "Premiere · June 2099 (day TBA)");
    assert!(p[&row.id].next_airing_at.is_none());
    assert_eq!(premiere_label("2099"), "2099 (date TBA)");
    assert_eq!(premiere_label("2099-06-03"), "Jun 3, 2099");
    assert_eq!(premiere_label(""), "TBA");
    assert_eq!(p[&row.id].added_date, "Nov 8, 2026");
    sqlx::query("UPDATE series SET episodes=1,status='RELEASING' WHERE id=?")
        .bind(row.id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO series_episode_metadata(series_id,episode_number,aired) VALUES(?,1,'2099-06-03')").bind(row.id).execute(&db).await.unwrap();
    let row = series::get_by_id(&db, row.id).await.unwrap().unwrap();
    let p = load(
        &db,
        root.path().to_str().unwrap(),
        std::slice::from_ref(&row),
    )
    .await
    .unwrap();
    assert_eq!(p[&row.id].next_airing, "Episode 1 · Jun 3, 2099 (time TBA)");
}

#[tokio::test]
async fn follow_selection_never_hides_or_creates_demand_and_parent_matches_poster() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    let wanted = release(
        &db,
        100,
        "Wanted but outside follow",
        Some(3),
        "FINISHED",
        "all",
        "Parent",
    )
    .await;
    states(&db, &wanted, 3, &[1, 2, 3]).await;
    let ignored = release(
        &db,
        200,
        "Followed but unmonitored",
        Some(2),
        "FINISHED",
        "none",
        "Parent",
    )
    .await;
    states(&db, &ignored, 2, &[]).await;
    release_follows::start(&db, 100, "Parent", true, false, false)
        .await
        .unwrap();
    sqlx::query("INSERT INTO franchise_choices(anilist_id,follow_id,title,selected) SELECT 100,id,'Wanted but outside follow',0 FROM release_follows UNION ALL SELECT 200,id,'Followed but unmonitored',1 FROM release_follows").execute(&db).await.unwrap();
    let library = series::get_all(&db).await.unwrap();
    let mut p = load(&db, root.path().to_str().unwrap(), &library)
        .await
        .unwrap();
    assert!(
        !p[&wanted.id].following,
        "An explicitly deselected main release is outside the follow"
    );
    assert!(
        p[&ignored.id].following,
        "Follow membership is independent of episode monitoring"
    );
    let cards: Vec<_> = library
        .into_iter()
        .map(|s| {
            let info = p.remove(&s.id).unwrap();
            (s, info)
        })
        .collect();
    let (_, poster) = super::super::navigation::cards(
        &db,
        &cards,
        None,
        "",
        "recent",
        root.path().to_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        (poster[0].progress.missing, poster[0].progress.unmonitored),
        (3, 2)
    );
    let page_id = library_layout::page_for_provider(&db, 100)
        .await
        .unwrap()
        .unwrap()
        .0;
    let parent = super::super::navigation::page(
        State(build_test_app_state(db.clone(), None)),
        Path(page_id),
    )
    .await
    .unwrap()
    .0;
    let doc = scraper::Html::parse_document(&parent);
    let selector =
        scraper::Selector::parse(".franchise-heading-content .availability-bar").unwrap();
    let summary = doc.select(&selector).next().unwrap();
    assert_eq!(summary.value().attr("data-missing"), Some("3"));
    assert_eq!(summary.value().attr("data-unmonitored"), Some("2"));
    assert!(parent.contains("Back to library"));
    let response = super::super::pages::index(
        State(build_test_app_state(db, None)),
        Query(super::super::pages::LibraryIndexQuery {
            individual: Some("0".into()),
            ..Default::default()
        }),
        HeaderMap::new(),
        axum_htmx::HxRequest(false),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();
    let doc = scraper::Html::parse_document(&html);
    let selector = scraper::Selector::parse(".franchise-card .availability-bar").unwrap();
    assert_eq!(
        doc.select(&selector)
            .next()
            .unwrap()
            .value()
            .attr("data-missing"),
        Some("3")
    );
}

#[tokio::test]
async fn legacy_mal_identity_keeps_its_follow_badge() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    let row = release(
        &db,
        -55,
        "Legacy identity",
        Some(1),
        "FINISHED",
        "none",
        "Legacy",
    )
    .await;
    sqlx::query("UPDATE series SET mal_id=55 WHERE id=?")
        .bind(row.id)
        .execute(&db)
        .await
        .unwrap();
    let detail = crate::services::anilist::parse_media_node(&serde_json::json!({"id":200,"idMal":55,"type":"ANIME","title":{"english":"Legacy identity"}})).unwrap();
    crate::models::metadata_cache::upsert_provider(&db, 200, Some(55), &detail)
        .await
        .unwrap();
    release_follows::start(&db, 100, "Legacy", true, false, false)
        .await
        .unwrap();
    sqlx::query("INSERT INTO followed_releases(follow_id,anilist_id,title) SELECT id,200,'Legacy identity' FROM release_follows").execute(&db).await.unwrap();
    let progress = load(
        &db,
        root.path().to_str().unwrap(),
        std::slice::from_ref(&row),
    )
    .await
    .unwrap();
    assert!(progress[&row.id].following);
    assert_eq!(progress[&row.id].missing, 0);
}

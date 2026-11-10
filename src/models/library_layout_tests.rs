use super::*;
use crate::models::{migrate, series::SeriesCore};
use crate::test_support::{in_memory_pool, seed_series};

fn core(id: i64, title: &str) -> SeriesCore<'_> {
    SeriesCore {
        anilist_id: id,
        mal_id: None,
        title,
        title_romaji: title,
        title_english: title,
        title_native: "",
        cover_url: "",
        format: "TV",
        status: "FINISHED",
        episodes: Some(12),
        season_year: Some(2020),
        end_year: None,
    }
}

async fn add(db: &SqlitePool, id: i64, title: &str) -> series::Series {
    let (row, _) = series::upsert(db, core(id, title)).await.unwrap();
    series::get_by_id(db, row).await.unwrap().unwrap()
}

async fn edge(db: &SqlitePool, owner: i64, peer: i64, relation: &str, kind: &str) {
    sqlx::query("INSERT INTO provider_relations_cache(provider_id,related_provider_id,relation_type,media_type) VALUES(?,?,?,?)")
        .bind(owner).bind(peer).bind(relation).bind(kind).execute(db).await.unwrap();
}

async fn move_binding(db: &SqlitePool, row: &series::Series, path: &str) {
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
        .bind(path)
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    record_move(&mut tx, row.id, path).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_fresh_install_assigns_nested_paths_and_keeps_them_on_refresh() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    migrate(&db).await.unwrap();
    assert!(enabled(&db).await.unwrap());
    let row = add(&db, 1, "A One-Off: Film").await;
    assert_eq!(row.folder_name, "A One-Off - Film/A One-Off - Film");
    let refreshed = add(&db, 1, "A New Display Title").await;
    assert_eq!(refreshed.folder_name, row.folder_name);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM release_follows")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn an_existing_library_requires_adoption_and_keeps_its_paths_and_custom_settings() {
    let db = in_memory_pool().await;
    let row = add(&db, 1, "Legacy").await;
    sqlx::query("DROP TABLE library_franchise_members; DROP TABLE library_franchises; DROP TABLE library_path_policy").execute(&db).await.unwrap();
    migrate(&db).await.unwrap();
    assert!(!enabled(&db).await.unwrap());
    let cfg = config::Config {
        series_folder_format: "Custom {series.title}".into(),
        ..Default::default()
    };
    config::save_config(&db, &cfg).await.unwrap();
    for _ in 0..2 {
        enable(&db).await.unwrap();
    }
    assert_eq!(
        series::get_by_id(&db, row.id)
            .await
            .unwrap()
            .unwrap()
            .folder_name,
        row.folder_name
    );
    assert_eq!(
        config::get_config(&db)
            .await
            .unwrap()
            .unwrap()
            .series_folder_format,
        cfg.series_folder_format
    );
    assert_eq!(add(&db, 2, "New").await.folder_name, "New/Custom New");
}

#[tokio::test]
async fn adoption_removes_only_the_old_default_year_template() {
    let db = in_memory_pool().await;
    config::save_config(
        &db,
        &config::Config {
            series_folder_format: "{series.title} ({series.year})".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    enable(&db).await.unwrap();
    assert_eq!(
        config::get_config(&db)
            .await
            .unwrap()
            .unwrap()
            .series_folder_format,
        naming::DEFAULT_SERIES_FOLDER_FORMAT
    );
}

#[tokio::test]
async fn older_promotional_specials_and_spinoffs_do_not_name_the_parent() {
    let db = in_memory_pool().await;
    enable(&db).await.unwrap();
    let peer = |id, title: &str, format: &str, year, relation: &str| {
        serde_json::json!({
            "id":id,"id_mal":null,"title_romaji":title,"title_english":title,"title_native":"",
            "cover_url":"","format":format,"status":"FINISHED","status_display":"","episodes":1,
            "relation_type":relation,"season_year":year,"media_type":"ANIME"
        })
    };
    let detail = serde_json::json!({
        "id":1,"id_mal":null,"title_romaji":"Main Show","title_english":"Main Show","title_native":"",
        "cover_url":"","banner_url":"","format":"TV","status":"FINISHED","status_display":"",
        "episodes":12,"duration":24,"season":"SPRING","season_year":2017,"description":"","genres":[],
        "average_score":null,"average_score_display":null,"score_is_ten_point":false,"score_class":"",
        "next_airing_episode":null,"next_airing_at":null,"synonyms":[],"streaming_episodes":[],
        "relations":[peer(2,"Promotional Pilot","SPECIAL",2016,"ALTERNATIVE"),
            peer(3,"Short Side Story","ONA",2015,"SPIN_OFF"),peer(4,"Main Show Season 2","TV",2020,"SEQUEL")]
    });
    // Exercise the real cached-detail decoder, including each related format.
    let _: AnimeDetail = serde_json::from_value(detail.clone()).unwrap();
    sqlx::query("INSERT INTO provider_metadata_cache(provider_id,detail_json) VALUES(1,?)")
        .bind(detail.to_string())
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        add(&db, 4, "Main Show Season 2").await.folder_name,
        "Main Show/Main Show Season 2"
    );
}

#[tokio::test]
async fn cached_identity_relations_join_a_franchise_through_an_untracked_bridge() {
    let db = in_memory_pool().await;
    let first = add(&db, 1, "Original").await;
    move_binding(&db, &first, "Franchise/Original").await;
    enable(&db).await.unwrap();
    edge(&db, 1, 99, "SIDE_STORY", "ANIME").await;
    edge(&db, 99, 2, "SEQUEL", "ANIME").await;
    edge(&db, 2, 3, "ADAPTATION", "MANGA").await;
    assert_eq!(
        add(&db, 2, "Later Part").await.folder_name,
        "Franchise/Later Part"
    );
    assert_eq!(
        add(&db, 3, "Original Story").await.folder_name,
        "Original Story/Original Story"
    );
    // Persisted membership continues to work when relation caches expire.
    sqlx::query("DELETE FROM provider_relations_cache")
        .execute(&db)
        .await
        .unwrap();
    edge(&db, 2, 4, "SEQUEL", "ANIME").await;
    assert_eq!(
        add(&db, 4, "Another Part").await.folder_name,
        "Franchise/Another Part"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM release_follows")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_new_release_cannot_silently_merge_two_saved_franchises() {
    let db = in_memory_pool().await;
    let a = add(&db, 1, "One").await;
    let b = add(&db, 2, "Two").await;
    move_binding(&db, &a, "First/One").await;
    move_binding(&db, &b, "Second/Two").await;
    enable(&db).await.unwrap();
    edge(&db, 1, 3, "SEQUEL", "ANIME").await;
    edge(&db, 2, 3, "PREQUEL", "ANIME").await;
    let err = series::upsert(&db, core(3, "Bridge")).await.unwrap_err();
    assert!(err.to_string().contains("different franchise folders"));
    assert!(series::get_by_anilist_id(&db, 3).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM library_franchise_members")
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn preview_reservations_match_inserts_and_do_not_write_to_the_database() {
    let db = in_memory_pool().await;
    enable(&db).await.unwrap();
    edge(&db, 1, 2, "SEQUEL", "ANIME").await;
    let mut preview = catalog(&db).await.unwrap();
    let one = preview.plan(1, "Show".into(), Some(2020), None).unwrap();
    let two = preview.plan(2, "Show".into(), Some(2020), None).unwrap();
    let unrelated = preview.plan(3, "Show".into(), Some(2020), None).unwrap();
    assert_eq!(
        preview
            .plan(1, "Show".into(), Some(2020), None)
            .unwrap()
            .path,
        one.path
    );
    assert_eq!(one.path, "Show/Show");
    assert_eq!(two.path, "Show/Show (2020)");
    assert_eq!(unrelated.path, "Show (2020)/Show");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM library_franchises")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    for (id, assignment) in [(1, one), (2, two), (3, unrelated)] {
        assert_eq!(add(&db, id, "Show").await.folder_name, assignment.path);
    }
}

#[tokio::test]
async fn a_mal_identity_promotion_preserves_path_and_storage_membership() {
    let db = in_memory_pool().await;
    enable(&db).await.unwrap();
    let mut fallback = core(-10, "Fallback");
    fallback.mal_id = Some(10);
    let (id, _) = series::upsert(&db, fallback).await.unwrap();
    let old = series::get_by_id(&db, id).await.unwrap().unwrap();
    let mut promoted = core(20, "Canonical");
    promoted.mal_id = Some(10);
    assert_eq!(series::upsert(&db, promoted).await.unwrap(), (id, false));
    assert_eq!(
        series::get_by_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .folder_name,
        old.folder_name
    );
    edge(&db, 20, 21, "SEQUEL", "ANIME").await;
    assert_eq!(add(&db, 21, "Next").await.folder_name, "Fallback/Next");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM library_franchise_members WHERE provider_id=-10"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_move_reassigns_the_remaining_root_owner_and_a_reverse_clears_empty_roots() {
    let db = in_memory_pool().await;
    let a = add(&db, 1, "One").await;
    let b = add(&db, 2, "Two").await;
    move_binding(&db, &a, "Family/One").await;
    move_binding(&db, &b, "Family/Two").await;
    move_binding(&db, &a, "Elsewhere/One").await;
    enable(&db).await.unwrap();
    edge(&db, 2, 3, "SEQUEL", "ANIME").await;
    assert_eq!(add(&db, 3, "Three").await.folder_name, "Family/Three");
    move_binding(&db, &a, "One").await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM library_franchises WHERE folder='Elsewhere'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn regeneration_assigns_a_nested_path_and_excludes_the_invalid_old_path() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 1, "Show").await;
    series::update_folder(&db, id, "..").await.unwrap();
    enable(&db).await.unwrap();
    let row = series::get_by_id(&db, id).await.unwrap().unwrap();
    assert_eq!(generate(&db, &row).await.unwrap(), "Show/Show");
}

#[tokio::test]
async fn an_unowned_nested_directory_without_an_nfo_is_a_collision() {
    let temp = tempfile::tempdir().unwrap();
    let db = in_memory_pool().await;
    config::save_config(
        &db,
        &config::Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let first = add(&db, 1, "First").await;
    move_binding(&db, &first, "Family/First").await;
    std::fs::create_dir_all(temp.path().join("Family/Next")).unwrap();
    enable(&db).await.unwrap();
    edge(&db, 1, 2, "SEQUEL", "ANIME").await;
    assert_eq!(add(&db, 2, "Next").await.folder_name, "Family/Next (2020)");
}

#[tokio::test]
async fn public_page_ids_backfill_idempotently_without_changing_storage_or_using_triggers() {
    let db = in_memory_pool().await;
    sqlx::query("DROP TABLE library_franchise_members; DROP TABLE library_franchises")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE library_franchises(folder TEXT PRIMARY KEY COLLATE NOCASE,owner_provider_id INTEGER NOT NULL); CREATE TABLE library_franchise_members(provider_id INTEGER PRIMARY KEY,folder TEXT NOT NULL COLLATE NOCASE REFERENCES library_franchises(folder)); INSERT INTO library_franchises VALUES('Existing parent',123); INSERT INTO library_franchise_members VALUES(123,'Existing parent')")
        .execute(&db).await.unwrap();
    super::migrate(&db).await.unwrap();
    let before = page_for_provider(&db, 123).await.unwrap().unwrap();
    assert_eq!(before.0.len(), 32);
    assert!(before.0.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(before.1, "Existing parent");
    super::migrate(&db).await.unwrap();
    assert_eq!(page_for_provider(&db, 123).await.unwrap().unwrap(), before);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name='library_franchises'").fetch_one(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn singleton_page_identity_survives_root_rename_promotion_and_sequel_adoption() {
    let db = in_memory_pool().await;
    sqlx::query("UPDATE library_path_policy SET enabled=1 WHERE id=1")
        .execute(&db)
        .await
        .unwrap();
    let row = add(&db, 101, "One off").await;
    let initial = page_for_provider(&db, 101).await.unwrap().unwrap().0;
    move_binding(&db, &row, "New parent/One off").await;
    assert_eq!(
        page_for_provider(&db, 101).await.unwrap().unwrap().0,
        initial
    );
    let mut tx = db.begin().await.unwrap();
    promote(&mut tx, 101, 102).await.unwrap();
    sqlx::query("UPDATE series SET anilist_id=102 WHERE id=?")
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        page_for_provider(&db, 102).await.unwrap().unwrap().0,
        initial
    );
    edge(&db, 102, 200, "SEQUEL", "ANIME").await;
    let second = add(&db, 200, "Later sequel").await;
    assert!(second.folder_name.starts_with("New parent/"));
    assert_eq!(
        page_for_provider(&db, 200).await.unwrap().unwrap().0,
        initial
    );
}

#[tokio::test]
async fn moving_one_release_out_preserves_the_remaining_parent_page() {
    let db = in_memory_pool().await;
    sqlx::query("UPDATE library_path_policy SET enabled=1 WHERE id=1")
        .execute(&db)
        .await
        .unwrap();
    let first = add(&db, 101, "First").await;
    edge(&db, 101, 200, "SEQUEL", "ANIME").await;
    add(&db, 200, "Second").await;
    let initial = page_for_provider(&db, 101).await.unwrap().unwrap().0;
    move_binding(&db, &first, "Separate/First").await;
    assert_eq!(
        page_for_provider(&db, 200).await.unwrap().unwrap().0,
        initial
    );
    assert_ne!(
        page_for_provider(&db, 101).await.unwrap().unwrap().0,
        initial
    );
}

#[tokio::test]
async fn sequential_parent_moves_keep_every_published_page_address_and_flatten_aliases() {
    let db = in_memory_pool().await;
    sqlx::query("UPDATE library_path_policy SET enabled=1 WHERE id=1")
        .execute(&db)
        .await
        .unwrap();
    let first = add(&db, 101, "First").await;
    edge(&db, 101, 200, "SEQUEL", "ANIME").await;
    let second = add(&db, 200, "Second").await;
    let original = page_for_provider(&db, 101).await.unwrap().unwrap().0;
    move_binding(&db, &first, "Renamed/First").await;
    let interim = page_for_provider(&db, 101).await.unwrap().unwrap().0;
    assert_eq!(
        resolve_page_id(&db, &original).await.unwrap(),
        Some(original.clone())
    );
    move_binding(&db, &second, "Renamed/Second").await;
    assert_eq!(
        resolve_page_id(&db, &original).await.unwrap(),
        Some(interim.clone())
    );
    assert_eq!(
        resolve_page_id(&db, &interim).await.unwrap(),
        Some(interim.clone())
    );
    move_binding(&db, &first, "Again/First").await;
    move_binding(&db, &second, "Again/Second").await;
    let current = page_for_provider(&db, 101).await.unwrap().unwrap().0;
    assert_eq!(
        resolve_page_id(&db, &original).await.unwrap(),
        Some(current.clone())
    );
    assert_eq!(resolve_page_id(&db, &interim).await.unwrap(), Some(current));
}

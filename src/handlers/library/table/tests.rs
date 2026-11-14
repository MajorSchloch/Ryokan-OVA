use super::*;
use crate::test_support::seed_series;
use std::fs;

async fn fixture(root: &std::path::Path) -> SqlitePool {
    // A single connection makes query_only apply to every read in load().
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

#[tokio::test]
async fn counts_local_slots_and_only_confirmed_airings_for_each_part() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path()).await;
    for (provider, name, aired) in [(101, "Part One", 12), (102, "Part Two", 13)] {
        let id = seed_series(&db, provider, name).await;
        sqlx::query("UPDATE series SET episodes=13,status='RELEASING',mal_id=900,monitor_mode='all' WHERE id=?")
            .bind(id).execute(&db).await.unwrap();
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        for file in [
            "Test S01E01-E06.mkv",
            "Test S01E07-E12.mkv",
            "Duplicate S01E01.mkv",
            "Other season S02E13.mkv",
            "Special S00E13.mkv",
        ] {
            fs::write(dir.join(file), []).unwrap();
        }
        for ep in 1..=aired {
            sqlx::query("INSERT INTO series_episode_metadata(series_id,episode_number,aired) VALUES(?,?,'2020-01-01')")
                .bind(id).bind(ep).execute(&db).await.unwrap();
        }
        if aired == 12 {
            sqlx::query("INSERT INTO episode_airings(series_id,episode,airing_at) VALUES(?,13,unixepoch()+86400)")
                .bind(id).execute(&db).await.unwrap();
            // The precise future time wins over stale episode metadata.
            sqlx::query("INSERT INTO series_episode_metadata(series_id,episode_number,aired) VALUES(?,13,'2020-01-01')")
                .bind(id).execute(&db).await.unwrap();
        } else {
            sqlx::query("INSERT INTO episode_quality_tags(series_id,episode_number,state) VALUES(?,13,'completed')")
                .bind(id).execute(&db).await.unwrap();
        }
    }
    release_follows::start(&db, 101, "Part One", false, false, false)
        .await
        .unwrap();
    release_follows::start(&db, 200, "Follow without files", false, false, false)
        .await
        .unwrap();
    sqlx::query("PRAGMA query_only=ON")
        .execute(&db)
        .await
        .unwrap();
    let table = load(&db).await.unwrap();
    let [follow, first, second] = table.rows.as_slice() else {
        panic!("expected three entries")
    };
    assert!(follow.follow_only && follow.followed);
    assert_eq!(
        (follow.disk, follow.missing, follow.total),
        (Some(0), None, None)
    );
    assert_eq!(follow.href, "/anime/anilist/200");
    assert!(first.followed && !first.follow_only);
    assert_eq!(
        (first.disk, first.missing, first.total),
        (Some(12), Some(0), Some(13))
    );
    assert!(first.next.as_ref().unwrap().starts_with("Episode 13"));
    assert!(!second.followed);
    assert_eq!(
        (second.mode, second.disk, second.missing),
        ("all", Some(12), Some(1))
    );
    assert_ne!(first.href, second.href);
}

#[tokio::test]
async fn unreadable_root_and_sparse_air_dates_stay_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("offline");
    let db = fixture(&root).await;
    seed_series(&db, 17, "No schedule").await;
    seed_series(&db, 18, "Finished").await;
    sqlx::query(
        "UPDATE series SET status='FINISHED',episodes=1,monitor_mode='all' WHERE anilist_id=18",
    )
    .execute(&db)
    .await
    .unwrap();
    let rows = load(&db).await.unwrap().rows;
    assert!(rows.iter().all(|r| r.disk.is_none() && r.missing.is_none()));
    fs::create_dir(root).unwrap();
    let rows = load(&db).await.unwrap().rows;
    assert_eq!((rows[0].disk, rows[0].missing), (Some(0), Some(1)));
    assert_eq!(
        (rows[1].disk, rows[1].missing, rows[1].total),
        (Some(0), None, None)
    );
}

#[tokio::test]
async fn legacy_identity_requires_a_unique_cached_part() {
    let temp = tempfile::tempdir().unwrap();
    let db = fixture(temp.path()).await;
    let id = seed_series(&db, -90, "Legacy").await;
    sqlx::query("UPDATE series SET mal_id=90 WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    release_follows::start(&db, 100, "AniList part", false, false, false)
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO provider_metadata_cache(provider_id,mal_id,detail_json)
        VALUES(100,90,'{"format":"TV","status":"FINISHED","episodes":13}')"#,
    )
    .execute(&db)
    .await
    .unwrap();
    let rows = load(&db).await.unwrap().rows;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].followed && !rows[0].follow_only);
    sqlx::query(
        "INSERT INTO provider_metadata_cache(provider_id,mal_id,detail_json) VALUES(101,90,'{}')",
    )
    .execute(&db)
    .await
    .unwrap();
    let rows = load(&db).await.unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert!(rows[0].follow_only);
    assert_eq!((rows[0].format.as_str(), rows[0].total), ("TV", Some(13)));
    assert!(!rows[1].followed);
}

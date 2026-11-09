use super::*;
use crate::{
    models::{self, config::Config, metadata_cache},
    services::anilist,
    test_support::seed_series,
};
use serde_json::{Value, json};

const FIRST: i64 = 19603;
const SECOND: i64 = 20792;
const SUNNY: i64 = 21318;
const TITLE: &str = "Fate/stay night: Unlimited Blade Works";
const SECOND_TITLE: &str = "Fate/stay night: Unlimited Blade Works 2nd Season";
const SUNNY_TITLE: &str = "Fate/stay night: Unlimited Blade Works 2nd Season - sunny day";

// AniList's anime records, restricted to the three entries in the example.
// The relation is ALTERNATIVE. The media format of Sunny Day is SPECIAL.
fn fate_node(id: i64) -> Value {
    let (title, format, episodes, mal, year, month, day) = match id {
        FIRST => (TITLE, "TV", 13, 22297, 2014, 10, 5),
        SECOND => (SECOND_TITLE, "TV", 13, 28701, 2015, 4, 5),
        SUNNY => (SUNNY_TITLE, "SPECIAL", 1, 31389, 2015, 10, 7),
        _ => panic!("unexpected Fate ID"),
    };
    json!({"id":id,"idMal":mal,"type":"ANIME","title":{"romaji":title},"format":format,
        "episodes":episodes,"status":"FINISHED","seasonYear":year,
        "startDate":{"year":year,"month":month,"day":day}})
}

async fn cache(db: &SqlitePool, id: i64, relations: &[(&str, i64)]) -> AnimeDetail {
    let mut node = fate_node(id);
    node["relations"] = json!({"edges":relations.iter().map(|(kind,peer)| {
        json!({"relationType":kind,"node":fate_node(*peer)})
    }).collect::<Vec<_>>()});
    let detail = anilist::parse_media_node(&node).unwrap();
    metadata_cache::upsert_provider(db, id, detail.id_mal, &detail)
        .await
        .unwrap();
    local_metadata::replace_relations_for_provider(db, id, &detail)
        .await
        .unwrap();
    detail
}

#[tokio::test]
async fn grouped_library_fate_release_order_ownership_and_local_numbers_are_read_only() {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    models::migrate(&db).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    config::save_config(
        &db,
        &Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    // Deliberately insert in reverse release order. The two Ryokan row IDs
    // also differ from their AniList IDs.
    let second = seed_series(&db, SECOND, SECOND_TITLE).await;
    let first = seed_series(&db, FIRST, TITLE).await;
    sqlx::query("UPDATE series SET episodes=13, format='TV', status='FINISHED'")
        .execute(&db)
        .await
        .unwrap();
    for title in [TITLE, SECOND_TITLE] {
        let folder = title.replace('/', "-");
        sqlx::query("UPDATE series SET folder_name=? WHERE title=?")
            .bind(&folder)
            .bind(title)
            .execute(&db)
            .await
            .unwrap();
        let dir = temp.path().join(&folder);
        std::fs::create_dir(&dir).unwrap();
        let count = if title == TITLE { 13 } else { 12 };
        for ep in 1..=count {
            std::fs::write(dir.join(format!("UBW - S01E{ep:02}.mkv")), []).unwrap();
        }
    }
    // A file with the missing number in a different season must not fill it.
    let dir = temp.path().join(SECOND_TITLE.replace('/', "-"));
    std::fs::write(dir.join("UBW - S02E13.mkv"), []).unwrap();
    let first_detail = cache(&db, FIRST, &[("SEQUEL", SECOND)]).await;
    metadata_cache::upsert(&db, first, FIRST, first_detail.id_mal, &first_detail)
        .await
        .unwrap();
    cache(&db, SECOND, &[("PREQUEL", FIRST), ("ALTERNATIVE", SUNNY)]).await;
    // No detail row or library row for Sunny Day. The relation alone supplies
    // its premiere, format and episode count.
    sqlx::query("PRAGMA query_only = ON")
        .execute(&db)
        .await
        .unwrap();
    let view = load(&db).await.unwrap();
    assert_eq!(view.groups.len(), 1);
    let group = &view.groups[0];
    assert_eq!((group.tracked, group.outside), (2, 1));
    let rows = &group.entries;
    assert_eq!(
        rows.iter()
            .map(|r| (r.title.as_str(), r.date.as_str(), r.episodes, r.series_id))
            .collect::<Vec<_>>(),
        [
            (TITLE, "2014-10-05", Some(13), Some(first)),
            (SECOND_TITLE, "2015-04-05", Some(13), Some(second)),
            (SUNNY_TITLE, "2015-10-07", Some(1), None)
        ]
    );
    assert_eq!(rows[0].files, "13 episodes on disk");
    assert_eq!(rows[1].files, "Episode 13 missing on disk");
    assert!(
        rows[1]
            .links
            .iter()
            .any(|l| l.label == "Sequel to" && l.peer == TITLE)
    );
    assert_eq!(rows[2].format, "SPECIAL");
    assert!(
        rows[2]
            .links
            .iter()
            .any(|l| l.label == "Alternative to" && l.peer == SECOND_TITLE)
    );
    let response = page(State(db.clone())).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();
    let doc = scraper::Html::parse_document(&html);
    let select = |s| scraper::Selector::parse(s).unwrap();
    assert_eq!(doc.select(&select("details.library-group li")).count(), 3);
    assert_eq!(doc.select(&select("li.outside-entry")).count(), 1);
    assert_eq!(
        doc.select(&select(
            "li.outside-entry a, .library-groups form, .library-groups [hx-post]"
        ))
        .count(),
        0
    );
    assert!(html.contains("Episodes 1–13"));
    assert!(html.contains("Episode 1"));
    assert_eq!(crate::test_support::count_series(&db).await, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn grouped_library_uses_old_relation_cache_and_preserves_ambiguous_mal_ids() {
    let db = crate::test_support::in_memory_pool().await;
    let first = seed_series(&db, FIRST, TITLE).await;
    seed_series(&db, -28701, "Unresolved MAL entry").await;
    sqlx::query("UPDATE series SET mal_id=28701 WHERE anilist_id < 0")
        .execute(&db)
        .await
        .unwrap();
    let mut node = fate_node(FIRST);
    let mut second = fate_node(SECOND);
    let mut sunny = fate_node(SUNNY);
    // Exercise a shared MAL ID and a legacy relationship without a start date.
    second["startDate"] = Value::Null;
    sunny["idMal"] = json!(28701);
    node["relations"] = json!({"edges":[{"relationType":"SEQUEL","node":second},{"relationType":"ALTERNATIVE","node":sunny}]});
    let detail = anilist::parse_media_node(&node).unwrap();
    local_metadata::replace_relations_for_series(&db, first, &detail)
        .await
        .unwrap();
    let groups = load(&db).await.unwrap().groups;
    assert_eq!(groups.len(), 2);
    let group = groups.iter().find(|g| g.title == TITLE).unwrap();
    assert_eq!((group.tracked, group.outside), (1, 2));
    assert!(
        group
            .entries
            .iter()
            .any(|e| e.title == SECOND_TITLE && e.date == "2015")
    );
    assert!(
        groups
            .iter()
            .any(|g| g.title == "Unresolved MAL entry" && g.entries.len() == 1)
    );
}

#[tokio::test]
async fn grouped_library_reaches_sunny_day_when_only_the_first_part_is_tracked() {
    let db = crate::test_support::in_memory_pool().await;
    seed_series(&db, FIRST, TITLE).await;
    cache(&db, FIRST, &[("SEQUEL", SECOND)]).await;
    cache(&db, SECOND, &[("ALTERNATIVE", SUNNY)]).await;
    let groups = load(&db).await.unwrap().groups;
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!((group.tracked, group.outside), (1, 2));
    assert_eq!(group.entries[2].title, SUNNY_TITLE);
    assert!(group.entries[2].series_id.is_none());
    assert!(
        group.entries[2]
            .links
            .iter()
            .any(|l| l.peer == SECOND_TITLE && l.outside)
    );
}

#[tokio::test]
async fn grouped_library_previews_a_follow_on_an_untracked_sequel_without_backfill() {
    use axum::extract::{Path, Query};
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    models::migrate(&db).await.unwrap();
    config::save_config(
        &db,
        &Config {
            media_root: "/data/anime".into(),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    cache(&db, FIRST, &[("SEQUEL", SECOND), ("CHARACTER", SUNNY)]).await;
    crate::models::release_follows::start(&db, SECOND, SECOND_TITLE, true, false, false)
        .await
        .unwrap();
    sqlx::query("PRAGMA query_only=ON")
        .execute(&db)
        .await
        .unwrap();
    let query = vec![
        ("selection".into(), "1".into()),
        ("release".into(), SECOND.to_string()),
        ("name".into(), "Fate/stay night".into()),
        ("include".into(), "movies".into()),
    ];
    let Html(html) = preview::page(State(db.clone()), Path(SECOND), Query(query))
        .await
        .unwrap();
    let doc = scraper::Html::parse_document(&html);
    let select = |s| scraper::Selector::parse(s).unwrap();
    assert_eq!(
        doc.select(&select("#franchise-owner option[selected]"))
            .next()
            .unwrap()
            .value()
            .attr("value"),
        Some("19603")
    );
    assert_eq!(doc.select(&select("[name=release]")).count(), 2);
    assert_eq!(doc.select(&select("[name=release][checked]")).count(), 1);
    assert!(html.contains("Fate+stay night [AniList 19603]/"));
    assert!(html.contains(
        "2015-04-05 - TV - Fate+stay night - Unlimited Blade Works 2nd Season [AniList 20792]"
    ));
    assert!(html.contains("Movies on"));
    assert_eq!(crate::test_support::count_series(&db).await, 0);
    let err = preview::page(
        State(db),
        Path(SECOND),
        Query(vec![("release".into(), SUNNY.to_string())]),
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
}

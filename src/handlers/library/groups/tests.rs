use super::*;
use crate::{
    models::{self, config::Config, metadata_cache},
    services::anilist,
    test_support::seed_series,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use serde_json::{Value, json};

const FIRST: i64 = 19603;
const SECOND: i64 = 20792;
const SUNNY: i64 = 21318;
const TITLE: &str = "Fate/stay night: Unlimited Blade Works";
const SECOND_TITLE: &str = "Fate/stay night: Unlimited Blade Works 2nd Season";
const SUNNY_TITLE: &str = "Fate/stay night: Unlimited Blade Works 2nd Season - sunny day";

#[tokio::test]
async fn franchise_preview_browses_cached_releases_without_library_or_follow_records() {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    models::migrate(&db).await.unwrap();
    cache(&db, FIRST, &[("SEQUEL", SECOND)]).await;
    cache(&db, SECOND, &[("PREQUEL", FIRST)]).await;
    sqlx::query("PRAGMA query_only=ON")
        .execute(&db)
        .await
        .unwrap();
    let response = preview::page(State(db.clone()), Path(SECOND), Query(vec![]))
        .await
        .unwrap();
    let doc = scraper::Html::parse_document(&response.0);
    let links = scraper::Selector::parse(".franchise-release > a").unwrap();
    let hrefs: Vec<_> = doc
        .select(&links)
        .map(|a| a.value().attr("href").unwrap())
        .collect();
    assert_eq!(hrefs, ["/anime/anilist/19603", "/anime/anilist/20792"]);
    assert!(response.0.contains("A new follow starts today."));
    for table in [
        "series",
        "release_follows",
        "franchise_choices",
        "followed_releases",
        "grabbed_torrents",
    ] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 0, "Browsing changed {table}");
    }
}

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
    let response = Html(load(&db).await.unwrap().render().unwrap()).into_response();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();
    let doc = scraper::Html::parse_document(&html);
    let select = |s| scraper::Selector::parse(s).unwrap();
    assert_eq!(doc.select(&select("details.library-group li")).count(), 3);
    assert_eq!(doc.select(&select("li.outside-entry")).count(), 1);
    assert_eq!(
        doc.select(&select(".library-groups form, .library-groups [hx-post]"))
            .count(),
        0
    );
    assert_eq!(
        doc.select(&select("li.outside-entry a"))
            .next()
            .unwrap()
            .value()
            .attr("href"),
        Some("/anime/anilist/21318")
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
    assert!(html.contains("Fate+stay night/"));
    assert!(html.contains("Fate+stay night - Unlimited Blade Works 2nd Season"));
    assert_eq!(
        doc.select(&select("#follow-future-releases input[checked]"))
            .map(|input| input.value().attr("value").unwrap())
            .collect::<Vec<_>>(),
        ["movies"]
    );
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

#[tokio::test]
async fn franchise_follow_merges_season_follows_and_saves_only_reviewed_releases() {
    use crate::models::release_follows;
    use axum::{Form, extract::Path};
    use axum_htmx::HxRequest;
    let db = crate::test_support::in_memory_pool().await;
    let second = seed_series(&db, SECOND, SECOND_TITLE).await;
    sqlx::query(
        "UPDATE series SET monitor_mode='none', folder_name='My existing folder' WHERE id=?",
    )
    .bind(second)
    .execute(&db)
    .await
    .unwrap();
    // The first season is only available from its own cached detail and outgoing edges.
    cache(&db, FIRST, &[("SEQUEL", SECOND), ("ALTERNATIVE", SUNNY)]).await;
    release_follows::start(&db, SECOND, SECOND_TITLE, false, false, false)
        .await
        .unwrap();
    release_follows::start(&db, SUNNY, SUNNY_TITLE, true, false, true)
        .await
        .unwrap();
    sqlx::query("UPDATE release_follows SET started_on='2021-01-02' WHERE anilist_id=?")
        .bind(SECOND)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO followed_releases(follow_id,anilist_id,title) SELECT id,901,'Prior match' FROM release_follows WHERE anilist_id=?")
        .bind(SUNNY).execute(&db).await.unwrap();
    crate::models::sync_exclusions::add(&db, SUNNY, None, SUNNY_TITLE)
        .await
        .unwrap();
    let form = |version: String| {
        Form(vec![
            ("version".into(), version),
            ("selection".into(), "1".into()),
            ("owner".into(), FIRST.to_string()),
            ("name".into(), "Fate".into()),
            ("include".into(), "movies".into()),
            ("release".into(), FIRST.to_string()),
            ("release".into(), SECOND.to_string()),
            ("release".into(), SUNNY.to_string()),
        ])
    };
    let initial = preview::draft(&db, SECOND, &[]).await.unwrap().version;
    // No partial merge, monitoring changes, or series insert after an interrupted save.
    sqlx::query("CREATE TRIGGER fail_choice BEFORE INSERT ON franchise_choices BEGIN SELECT RAISE(ABORT,'test failure'); END")
        .execute(&db).await.unwrap();
    assert!(
        follow::save(
            State(db.clone()),
            Path(SECOND),
            HxRequest(false),
            form(initial.clone())
        )
        .await
        .is_err()
    );
    assert_eq!(release_follows::list(&db).await.unwrap().len(), 2);
    assert_eq!(crate::test_support::count_series(&db).await, 1);
    sqlx::query("DROP TRIGGER fail_choice")
        .execute(&db)
        .await
        .unwrap();
    let response = follow::save(
        State(db.clone()),
        Path(SECOND),
        HxRequest(false),
        form(initial.clone()),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"],
        format!("/library/groups/{FIRST}/preview")
    );
    let stale = follow::save(
        State(db.clone()),
        Path(FIRST),
        HxRequest(false),
        form(initial),
    )
    .await
    .unwrap_err();
    assert_eq!(stale.0, StatusCode::CONFLICT);
    let follows = release_follows::list_with_matches(&db).await.unwrap();
    assert_eq!(follows.len(), 1);
    let f = &follows[0];
    assert_eq!(
        (f.anilist_id, f.title.as_str(), f.started_on.as_str()),
        (FIRST, "Fate", "2021-01-02")
    );
    assert!(f.movies && !f.tv && !f.alternatives);
    assert_eq!(f.matches[0].anilist_id, 901);
    let second_row = series::get_by_id(&db, second).await.unwrap().unwrap();
    assert_eq!(
        (
            second_row.folder_name.as_str(),
            second_row.monitor_mode.as_str()
        ),
        ("My existing folder", "none")
    );
    assert_eq!(crate::test_support::count_series(&db).await, 2);
    assert!(
        series::get_by_anilist_id(&db, SUNNY)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("DELETE FROM provider_metadata_cache")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM provider_relations_cache")
        .execute(&db)
        .await
        .unwrap();
    let saved = preview::draft(&db, SECOND, &[]).await.unwrap();
    assert_eq!(saved.owner, Some(FIRST));
    assert_eq!(saved.rows.iter().filter(|r| r.selected).count(), 2);
    assert!(
        saved
            .rows
            .iter()
            .any(|r| r.entry.provider_id == SUNNY && r.entry.excluded && !r.selected)
    );
    follow::save(
        State(db.clone()),
        Path(SECOND),
        HxRequest(false),
        form(saved.version),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(crate::test_support::count_series(&db).await, 2);
    release_follows::start(&db, SECOND, SECOND_TITLE, true, true, true)
        .await
        .unwrap();
    assert_eq!(release_follows::list(&db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn franchise_singleton_grows_without_starting_a_second_follow() {
    use axum::{Form, extract::Path};
    use axum_htmx::HxRequest;
    let db = crate::test_support::in_memory_pool().await;
    seed_series(&db, FIRST, TITLE).await;
    seed_series(&db, -500, "Standalone MAL title").await;
    let initial = preview::draft(&db, FIRST, &[]).await.unwrap();
    assert_eq!(initial.owner, Some(FIRST));
    assert_eq!(initial.rows.len(), 1);
    assert!(initial.rows[0].selected);
    assert!(models::release_follows::list(&db).await.unwrap().is_empty());
    let mal = preview::draft(&db, -500, &[])
        .await
        .unwrap()
        .render()
        .unwrap();
    assert!(mal.contains("Standalone MAL title") && !mal.contains("Save franchise follow"));
    follow::save(
        State(db.clone()),
        Path(FIRST),
        HxRequest(false),
        Form(vec![
            ("version".into(), initial.version),
            ("selection".into(), "1".into()),
            ("owner".into(), FIRST.to_string()),
            ("release".into(), FIRST.to_string()),
            ("include".into(), "tv".into()),
        ]),
    )
    .await
    .unwrap();
    cache(&db, FIRST, &[("SEQUEL", SECOND), ("CHARACTER", SUNNY)]).await;
    seed_series(&db, SUNNY, SUNNY_TITLE).await;
    let later = preview::draft(&db, FIRST, &[]).await.unwrap();
    assert_eq!(later.owner, Some(FIRST));
    assert_eq!(later.rows.len(), 2);
    assert!(
        later
            .rows
            .iter()
            .any(|r| r.entry.provider_id == SECOND && !r.selected)
    );
    assert_eq!(preview::draft(&db, SUNNY, &[]).await.unwrap().rows.len(), 1);
    assert_eq!(models::release_follows::list(&db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn franchise_file_preview_reads_selected_files_without_saving_the_draft() {
    let db = crate::test_support::in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    let id = seed_series(&db, FIRST, TITLE).await;
    sqlx::query("UPDATE series SET folder_name='Source' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    config::save_config(
        &db,
        &Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    std::fs::create_dir_all(temp.path().join("Source/Season 01")).unwrap();
    std::fs::write(temp.path().join("Source/Season 01/Show - S01E01.mkv"), []).unwrap();
    std::fs::write(
        temp.path().join("Source/Season 01/Show - S01E01.en.ass"),
        [],
    )
    .unwrap();
    sqlx::query("INSERT INTO grabbed_torrents(hash,torrent_name,series_id,episode_numbers,state) VALUES('preview-active','Selected download',?,'[1]','pending')")
        .bind(id).execute(&db).await.unwrap();
    sqlx::query("PRAGMA query_only=ON")
        .execute(&db)
        .await
        .unwrap();
    let Html(html) = preview::page(
        State(db.clone()),
        Path(FIRST),
        Query(vec![("files".into(), "1".into())]),
    )
    .await
    .unwrap();
    assert!(html.contains("1 media files") && html.contains("Show - S01E01.en.ass"));
    assert!(html.contains("Proposed path"));
    assert!(html.contains("Downloads or imports are active"));
    assert_eq!(crate::test_support::count_series(&db).await, 1);
    assert!(models::release_follows::list(&db).await.unwrap().is_empty());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(
        std::fs::metadata(temp.path().join("Source/Season 01/Show - S01E01.mkv"))
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn follow_editor_separates_release_choices_from_advanced_organization() {
    use crate::models::{library_layout, monitoring};
    let db = crate::test_support::in_memory_pool().await;
    let id = seed_series(&db, FIRST, TITLE).await;
    cache(&db, FIRST, &[("SEQUEL", SECOND)]).await;
    sqlx::query("UPDATE series SET episodes=13, cover_url='/static/test-cover.jpg' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    monitoring::replace_series_states(
        &db,
        id,
        &[
            monitoring::EpisodeMonitorState {
                episode_number: 1,
                monitored: true,
            },
            monitoring::EpisodeMonitorState {
                episode_number: 2,
                monitored: false,
            },
        ],
    )
    .await
    .unwrap();
    let mut tx = db.begin().await.unwrap();
    library_layout::record_move(&mut tx, id, "Fate/First season")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let page_id = library_layout::page_for_provider(&db, FIRST)
        .await
        .unwrap()
        .unwrap()
        .0;
    let Html(html) = preview::page(State(db.clone()), Path(FIRST), Query(vec![]))
        .await
        .unwrap();
    let doc = scraper::Html::parse_document(&html);
    let select = |s| scraper::Selector::parse(s).unwrap();
    let advanced = doc
        .select(&select("details.follow-editor-advanced"))
        .next()
        .unwrap();
    assert!(advanced.value().attr("open").is_none());
    assert_eq!(
        advanced
            .select(&select("[name=owner], [name=name]"))
            .count(),
        2
    );
    assert_eq!(
        advanced
            .select(&select(".follow-editor-folder-preview"))
            .count(),
        1
    );
    assert_eq!(
        doc.select(&select("#follow-current-releases [name=release]"))
            .count(),
        2
    );
    assert_eq!(
        doc.select(&select("#follow-current-releases [name=release][checked]"))
            .count(),
        1
    );
    assert_eq!(
        doc.select(&select("#follow-future-releases [name=include]"))
            .count(),
        4
    );
    assert_eq!(
        doc.select(&select(
            "#follow-current-releases [name=owner], #follow-future-releases code"
        ))
        .count(),
        0
    );
    assert!(html.contains("Monitoring 1 of 13 episodes"));
    assert!(html.contains("src=\"/static/test-cover.jpg\""));
    assert_eq!(
        doc.select(&select(".follow-editor-heading > a"))
            .next()
            .unwrap()
            .value()
            .attr("href"),
        Some(format!("/franchises/{page_id}").as_str())
    );
    assert_eq!(
        doc.select(&select("[name=return_to]"))
            .next()
            .unwrap()
            .value()
            .attr("value"),
        Some(format!("/franchises/{page_id}").as_str())
    );
}

#[tokio::test]
async fn follow_editor_keeps_first_add_return_context_through_preview_and_save() {
    use axum::Form;
    use axum_htmx::HxRequest;
    let db = crate::test_support::in_memory_pool().await;
    cache(&db, FIRST, &[("SEQUEL", SECOND)]).await;
    let initial = preview::draft(&db, FIRST, &[]).await.unwrap();
    let html = initial.render().unwrap();
    let doc = scraper::Html::parse_document(&html);
    let back = scraper::Selector::parse(".follow-editor-heading > a").unwrap();
    assert_eq!(
        doc.select(&back).next().unwrap().value().attr("href"),
        Some("/anime/anilist/19603")
    );
    let context = "/?search=Fate&sort=added_desc&individual=true";
    let response = follow::save(
        State(db.clone()),
        Path(FIRST),
        HxRequest(false),
        Form(vec![
            ("version".into(), initial.version),
            ("selection".into(), "1".into()),
            ("owner".into(), FIRST.to_string()),
            ("release".into(), FIRST.to_string()),
            ("include".into(), "movies".into()),
            ("return_to".into(), context.into()),
        ]),
    )
    .await
    .unwrap();
    let location = response.headers()["location"].to_str().unwrap();
    assert_eq!(
        location,
        format!(
            "/library/groups/{FIRST}/preview?return_to={}",
            urlencoding::encode(context)
        )
    );
    let Query(query) =
        Query::<Vec<(String, String)>>::try_from_uri(&location.parse().unwrap()).unwrap();
    let html = preview::draft(&db, FIRST, &query)
        .await
        .unwrap()
        .render()
        .unwrap();
    let doc = scraper::Html::parse_document(&html);
    assert_eq!(
        doc.select(&back).next().unwrap().value().attr("href"),
        Some(context)
    );
    assert!(
        doc.select(&back)
            .next()
            .unwrap()
            .text()
            .collect::<String>()
            .contains("Back to library")
    );
    // Untrusted return destinations are ignored and never become a redirect.
    let html = preview::draft(
        &db,
        FIRST,
        &[("return_to".into(), "//other.example/".into())],
    )
    .await
    .unwrap()
    .render()
    .unwrap();
    assert!(!html.contains("other.example"));
}

#[tokio::test]
async fn follow_save_keeps_unchecked_library_files_and_explicit_episode_monitoring() {
    use crate::models::{monitoring, release_follows};
    use axum::Form;
    use axum_htmx::HxRequest;
    let db = crate::test_support::in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    config::save_config(
        &db,
        &Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let existing = seed_series(&db, SECOND, SECOND_TITLE).await;
    sqlx::query("UPDATE series SET folder_name='Keep/Season', monitor_mode='existing', episodes=13 WHERE id=?")
        .bind(existing).execute(&db).await.unwrap();
    let file = temp.path().join("Keep/Season/Show - S01E01.mkv");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"existing media stays unchanged").unwrap();
    monitoring::replace_series_states(
        &db,
        existing,
        &[
            monitoring::EpisodeMonitorState {
                episode_number: 1,
                monitored: false,
            },
            monitoring::EpisodeMonitorState {
                episode_number: 2,
                monitored: true,
            },
        ],
    )
    .await
    .unwrap();
    cache(&db, FIRST, &[("SEQUEL", SECOND), ("ALTERNATIVE", SUNNY)]).await;
    release_follows::start(&db, SECOND, SECOND_TITLE, true, false, false)
        .await
        .unwrap();
    sqlx::query("UPDATE release_follows SET started_on='2020-02-03'")
        .execute(&db)
        .await
        .unwrap();
    models::sync_exclusions::add(&db, SUNNY, None, SUNNY_TITLE)
        .await
        .unwrap();
    let before = preview::draft(&db, SECOND, &[]).await.unwrap();
    follow::save(
        State(db.clone()),
        Path(SECOND),
        HxRequest(false),
        Form(vec![
            ("version".into(), before.version),
            ("selection".into(), "1".into()),
            ("owner".into(), FIRST.to_string()),
            ("release".into(), FIRST.to_string()),
            ("release".into(), SUNNY.to_string()), // Exclusions still win over a crafted form.
            ("include".into(), "movies".into()),
        ]),
    )
    .await
    .unwrap();
    let follows = release_follows::list(&db).await.unwrap();
    assert_eq!(follows.len(), 1);
    assert_eq!(follows[0].anilist_id, FIRST);
    assert_eq!(follows[0].started_on, "2020-02-03");
    assert!(follows[0].movies && !follows[0].tv && !follows[0].ovas && !follows[0].alternatives);
    let old = series::get_by_id(&db, existing).await.unwrap().unwrap();
    assert_eq!(
        (old.folder_name.as_str(), old.monitor_mode.as_str()),
        ("Keep/Season", "existing")
    );
    assert_eq!(
        monitoring::get_series_states(&db, existing)
            .await
            .unwrap()
            .iter()
            .map(|state| (state.episode_number, state.monitored))
            .collect::<Vec<_>>(),
        [(1, false), (2, true)]
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"existing media stays unchanged"
    );
    let added = series::get_by_anilist_id(&db, FIRST)
        .await
        .unwrap()
        .unwrap();
    let states = monitoring::get_series_states(&db, added.id).await.unwrap();
    assert_eq!(states.len(), 13);
    assert!(states.iter().all(|state| state.monitored));
    assert!(
        series::get_by_anilist_id(&db, SUNNY)
            .await
            .unwrap()
            .is_none()
    );
    let saved = preview::draft(&db, FIRST, &[]).await.unwrap();
    assert!(
        !saved
            .rows
            .iter()
            .find(|row| row.entry.provider_id == SECOND)
            .unwrap()
            .selected
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn future_only_controls_preserve_existing_releases_choices_and_monitoring() {
    use axum::Form;
    use axum_htmx::HxRequest;
    let db = crate::test_support::in_memory_pool().await;
    let id = seed_series(&db, FIRST, TITLE).await;
    sqlx::query("UPDATE series SET monitor_mode='none',allow_upgrades=0 WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    cache(&db, FIRST, &[("SEQUEL", SECOND)]).await;
    cache(&db, SECOND, &[("PREQUEL", FIRST)]).await;
    for enabled in [true, false] {
        let version = preview::draft(&db, FIRST, &[]).await.unwrap().version;
        let mut fields = vec![
            ("version".into(), version),
            ("selection".into(), "1".into()),
            ("owner".into(), FIRST.to_string()),
            ("future_controls".into(), "1".into()),
            ("include".into(), "tv".into()),
        ];
        if enabled {
            fields.push(("future_enabled".into(), "on".into()));
        }
        follow::save(
            State(db.clone()),
            Path(FIRST),
            HxRequest(false),
            Form(fields),
        )
        .await
        .unwrap();
        let follows = crate::models::release_follows::list(&db).await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].tv, enabled);
        assert_eq!(crate::test_support::count_series(&db).await, 1);
        let row = series::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.monitor_mode, "none");
        assert!(!row.allow_upgrades);
        let choices: i64 = sqlx::query_scalar("SELECT count(*) FROM franchise_choices")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            choices, 0,
            "Future formats must not exclude known outside releases or alter membership"
        );
        let grabs: i64 = sqlx::query_scalar("SELECT count(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(grabs, 0);
    }
}

//! The follow form and scheduled check share a real SQLite file across startup.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use ryokan::{
    handlers,
    models::{self, monitoring, release_follows, series},
    services, test_support,
};
use serde_json::{Value, json};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, Request as MockRequest, ResponseTemplate,
    matchers::{body_string_contains, method},
};

fn edge(id: i64, format: &str, status: &str, year: i32) -> Value {
    json!({"relationType":"SEQUEL", "node":{
        "type":"ANIME", "id":id, "format":format, "status":status, "episodes":3,
        "title":{"romaji":format!("Title {id}")}, "startDate":{"year":year,"month":1,"day":1}
    }})
}

async fn form(db: &SqlitePool, path: &str, body: &str) {
    let app = handlers::release_follows::routes()
        .with_state(test_support::build_test_app_state(db.clone(), None));
    let request = Request::post(path)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["Location"], "/library/follows");
}

async fn graph(server: &MockServer, phase: u8) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(body_string_contains("startDate { year month day }"))
        .respond_with(move |req: &MockRequest| {
            let body: Value = req.body_json().unwrap();
            let media: Vec<_> = body["variables"]["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| {
                    let edges = match (id.as_i64().unwrap(), phase) {
                        // No announced sequel when the first follow starts.
                        (1, 1..) => vec![edge(2, "MOVIE", "FINISHED", 2026)],
                        (2, 2..) => vec![
                            edge(3, "TV", "FINISHED", 2026),
                            edge(1, "TV", "FINISHED", 2000),
                        ],
                        // A separate follow already has an announced, undated sequel.
                        (10, _) => {
                            let mut upcoming = edge(11, "TV", "NOT_YET_RELEASED", 2027);
                            upcoming["node"]["startDate"] = Value::Null;
                            vec![
                                upcoming,
                                edge(12, "TV", "FINISHED", 2000),
                                edge(13, "TV", "RELEASING", 2026),
                            ]
                        }
                        _ => vec![],
                    };
                    json!({"id": id, "relations":{"edges":edges}})
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"data":{"Page":{"media":media}}}))
        })
        .mount(server)
        .await;
}

#[tokio::test]
async fn release_follows_survive_restart_and_follow_sequels_across_an_excluded_movie() {
    let server = MockServer::start().await;
    // This integration binary has one test and owns the provider override.
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", server.uri());
    }
    let temp = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(temp.path().join("ryokan.db"))
        .create_if_missing(true);
    let db = SqlitePool::connect_with(options.clone()).await.unwrap();
    models::migrate(&db).await.unwrap();
    let root = test_support::seed_series(&db, 1, "Finished TV").await;
    sqlx::query("UPDATE series SET status='FINISHED', episodes=12, monitor_mode='none' WHERE id=?")
        .bind(root)
        .execute(&db)
        .await
        .unwrap();
    graph(&server, 0).await;
    form(&db, "/library/follows", "anilist_id=1&title=Finished+TV").await;
    let follow = release_follows::list(&db).await.unwrap().remove(0);
    assert_eq!(
        follow.started_on,
        chrono::Utc::now().date_naive().to_string()
    );
    // Pin the follow day to make the pre/post-air and same-day cases deterministic.
    sqlx::query("UPDATE release_follows SET started_on='2026-01-01'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(services::release_follows::refresh(&db).await.unwrap(), 0);
    graph(&server, 1).await;
    assert_eq!(services::release_follows::refresh(&db).await.unwrap(), 0);
    assert_eq!(
        series::get_by_id(&db, root)
            .await
            .unwrap()
            .unwrap()
            .monitor_mode,
        "none"
    );
    assert!(
        monitoring::get_series_states(&db, root)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(test_support::count_grabs_for_series(&db, root).await, 0);
    series::remove(&db, root).await.unwrap();
    db.close().await;

    // The new TV season already finished before the next startup.
    graph(&server, 2).await;
    let db = SqlitePool::connect_with(options).await.unwrap();
    models::migrate(&db).await.unwrap();
    assert_eq!(services::release_follows::refresh(&db).await.unwrap(), 1);
    let season = series::get_by_anilist_id(&db, 3).await.unwrap().unwrap();
    assert_eq!(season.monitor_mode, "all");
    assert!(!season.allow_upgrades);
    assert_eq!(
        monitoring::get_monitored_episode_numbers(&db, season.id)
            .await
            .unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(test_support::count_series(&db).await, 1);

    // A user can change settings or delete the sequel between checks.
    series::update_monitor_mode(&db, season.id, "none")
        .await
        .unwrap();
    monitoring::set_episode_monitored(&db, season.id, 2, false)
        .await
        .unwrap();
    assert_eq!(services::release_follows::refresh(&db).await.unwrap(), 0);
    assert_eq!(
        series::get_by_id(&db, season.id)
            .await
            .unwrap()
            .unwrap()
            .monitor_mode,
        "none"
    );
    assert_eq!(
        monitoring::get_monitored_episode_numbers(&db, season.id)
            .await
            .unwrap(),
        vec![1, 3]
    );
    series::remove(&db, season.id).await.unwrap();
    assert_eq!(services::release_follows::refresh(&db).await.unwrap(), 0);

    // An existing library entry keeps its manually selected episodes and upgrade setting.
    let existing = test_support::seed_series(&db, 13, "Existing sequel").await;
    series::update_monitor_mode(&db, existing, "existing")
        .await
        .unwrap();
    series::update_allow_upgrades(&db, existing, true)
        .await
        .unwrap();
    monitoring::set_episode_monitored(&db, existing, 2, true)
        .await
        .unwrap();
    form(
        &db,
        "/library/follows",
        "anilist_id=10&title=Another+series&ovas=true",
    )
    .await;
    sqlx::query("UPDATE release_follows SET started_on='2026-01-01' WHERE anilist_id=10")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(services::release_follows::refresh(&db).await.unwrap(), 1);
    assert_eq!(
        series::get_by_anilist_id(&db, 11)
            .await
            .unwrap()
            .unwrap()
            .monitor_mode,
        "all"
    );
    assert!(series::get_by_anilist_id(&db, 12).await.unwrap().is_none());
    let row = series::get_by_id(&db, existing).await.unwrap().unwrap();
    assert!(row.allow_upgrades);
    assert_eq!(row.monitor_mode, "existing");
    assert_eq!(
        monitoring::get_monitored_episode_numbers(&db, existing)
            .await
            .unwrap(),
        vec![2]
    );
    let edit = format!("/library/follows/{}", follow.id);
    form(&db, &edit, "movies=true&alternatives=true").await;
    let follows = release_follows::list(&db).await.unwrap();
    let edited = follows.iter().find(|f| f.id == follow.id).unwrap();
    assert!(edited.movies && edited.alternatives && !edited.ovas);
    assert_eq!(edited.started_on, "2026-01-01");
    assert!(
        follows
            .iter()
            .any(|f| f.anilist_id == 10 && f.ovas && !f.movies && !f.alternatives)
    );
    form(&db, &format!("{edit}/stop"), "").await;
    assert_eq!(release_follows::list(&db).await.unwrap().len(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

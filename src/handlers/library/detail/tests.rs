use super::*;
use crate::{
    models::{config, metadata_cache, series},
    services::anilist,
    test_support::{build_test_app_state, in_memory_pool, seed_series},
};
use axum::{body::Body, http::Request};
use serde_json::json;
use sqlx::SqlitePool;
use tower::ServiceExt;

async fn cache(db: &SqlitePool, id: i64, title: &str, relations: &[(i64, &str)]) {
    let mut detail = anilist::parse_media_node(&json!({
        "id": id.abs(), "title":{"english":title,"romaji":title}, "type":"ANIME",
        "format":"MOVIE", "episodes":1, "status":"FINISHED", "seasonYear":2022,
        "description":"Explore before adding.",
        "relations":{"edges":relations.iter().map(|(id,title)| json!({
            "relationType":"SEQUEL", "node":{"id":id,"type":"ANIME",
            "title":{"english":title,"romaji":title},"format":"MOVIE","episodes":1}
        })).collect::<Vec<_>>()}
    }))
    .unwrap();
    detail.id = id;
    detail.id_mal = (id < 0).then_some(id.abs());
    metadata_cache::upsert_provider(db, id, detail.id_mal, &detail)
        .await
        .unwrap();
}

async fn request(db: &SqlitePool, uri: &str, htmx: bool) -> Response {
    let mut request = Request::builder().uri(uri);
    if htmx {
        request = request.header("HX-Request", "true");
    }
    routes()
        .with_state(build_test_app_state(db.clone(), None))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn provider_detail_browses_relations_without_creating_library_or_download_records() {
    let db = in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    config::save_config(
        &db,
        &config::Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    cache(
        &db,
        130003,
        "Bocchi test release",
        &[(186712, "Next season")],
    )
    .await;
    cache(&db, 186712, "Next season", &[]).await;
    for (uri, title) in [
        ("/anime/anilist/130003", "Bocchi test release"),
        ("/anime/anilist/186712", "Next season"),
    ] {
        let html = body(request(&db, uri, false).await).await;
        assert!(html.contains(title));
        let provider_id = uri.rsplit('/').next().unwrap();
        assert!(html.contains(&format!(
            "id=\"btn-track\" href=\"/library/add?provider_id={provider_id}\""
        )));
        assert!(html.contains("Explore franchise"));
        assert!(!html.contains("id=\"btn-auto-search\""));
        assert!(!html.contains("Remove from Library"));
        assert!(html.contains("data-db-id=\"\""));
        if uri.ends_with("130003") {
            assert!(html.contains("href=\"/anime/anilist/186712\" class=\"relation-card\""));
        }
    }
    for table in [
        "series",
        "release_follows",
        "franchise_choices",
        "followed_releases",
        "grabbed_torrents",
        "episode_quality_tags",
        "library_franchises",
        "series_library_aliases",
    ] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 0, "Browsing changed {table}");
    }
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn provider_url_keeps_identity_before_and_after_tracking_despite_internal_id_collision() {
    let db = in_memory_pool().await;
    let other = seed_series(&db, 999, "Other library title").await;
    assert_eq!(other, 1);
    cache(&db, 1, "Provider title", &[]).await;
    let before = body(request(&db, "/anime/anilist/1", true).await).await;
    assert!(before.contains("data-provider-id=\"1\""));
    assert!(before.contains("data-db-id=\"\""));
    let local = seed_series(&db, 1, "Provider title").await;
    let after = body(request(&db, "/anime/anilist/1", true).await).await;
    assert!(after.contains(&format!("data-db-id=\"{local}\"")));
    assert!(after.contains(&format!("data-id=\"{local}\"")));
    assert!(after.contains("data-provider-id=\"1\""));
    assert!(after.contains(&format!("removeSeries({local})")));
    assert!(!after.contains(&format!("removeSeries({other})")));
    assert_eq!(crate::test_support::count_series(&db).await, 2);
    series::remove(&db, local).await.unwrap();
    let removed = body(request(&db, "/anime/anilist/1", false).await).await;
    assert!(removed.contains("data-db-id=\"\""));
    assert!(removed.contains("id=\"btn-track\" href=\"/library/add?provider_id=1\""));
}

#[tokio::test]
async fn legacy_links_redirect_without_confusing_library_and_provider_ids() {
    let db = in_memory_pool().await;
    let local = seed_series(&db, 130003, "Bocchi").await;
    for (uri, expected) in [
        (format!("/series/{local}"), "/anime/anilist/130003"),
        (format!("/series/{local}?by=anilist"), "/anime/anilist/1"),
        ("/series/186712?by=anilist".into(), "/anime/anilist/186712"),
        ("/series/-5680?by=anilist".into(), "/anime/mal/5680"),
    ] {
        for htmx in [false, true] {
            let response = request(&db, &uri, htmx).await;
            assert_eq!(
                response.status(),
                if htmx {
                    StatusCode::OK
                } else {
                    StatusCode::SEE_OTHER
                }
            );
            assert_eq!(
                response.headers()[if htmx { "HX-Redirect" } else { "Location" }],
                expected
            );
        }
    }
}

#[tokio::test]
async fn mal_page_uses_its_own_namespace_and_survives_a_unique_anilist_promotion() {
    let db = in_memory_pool().await;
    cache(&db, -5680, "MAL release", &[]).await;
    let html = body(request(&db, "/anime/mal/5680", false).await).await;
    assert!(html.contains("data-provider-id=\"-5680\""));
    assert!(html.contains("data-db-id=\"\""));
    assert!(html.contains("id=\"btn-track\" href=\"/library/add?provider_id=-5680\""));
    let local = seed_series(&db, 5081, "Promoted release").await;
    sqlx::query("UPDATE series SET mal_id=5680 WHERE id=?")
        .bind(local)
        .execute(&db)
        .await
        .unwrap();
    cache(&db, 5081, "Promoted release", &[]).await;
    let html = body(request(&db, "/anime/mal/5680", false).await).await;
    assert!(html.contains(&format!("data-db-id=\"{local}\"")));
    assert!(html.contains("data-provider-id=\"5081\""));
}

#[tokio::test]
async fn provider_pages_reject_non_positive_ids_without_creating_records() {
    let db = in_memory_pool().await;
    for uri in [
        "/anime/anilist/0",
        "/anime/anilist/-1",
        "/anime/mal/0",
        "/anime/mal/-1",
        "/anime/mal/-9223372036854775808",
    ] {
        assert_eq!(
            request(&db, uri, false).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(crate::test_support::count_series(&db).await, 0);
}

#[tokio::test]
async fn anilist_page_membership_respects_unique_legacy_mappings_and_split_parts() {
    let db = in_memory_pool().await;
    let fallback = seed_series(&db, -800, "Legacy release").await;
    sqlx::query("UPDATE series SET mal_id=800 WHERE id=?")
        .bind(fallback)
        .execute(&db)
        .await
        .unwrap();
    cache(&db, 12, "First part", &[]).await;
    sqlx::query("UPDATE provider_metadata_cache SET mal_id=800 WHERE provider_id=12")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        resolve_series_request(&db, 12, true)
            .await
            .unwrap()
            .0
            .unwrap()
            .id,
        fallback
    );
    sqlx::query("INSERT INTO discovery_media_ids(anilist_id,mal_id) VALUES(13,800)")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        resolve_series_request(&db, 12, true)
            .await
            .unwrap()
            .0
            .is_none(),
        "A known premiere split cannot be treated as a unique MAL match"
    );
    sqlx::query("DELETE FROM discovery_media_ids")
        .execute(&db)
        .await
        .unwrap();
    cache(&db, 13, "Second part", &[]).await;
    sqlx::query("UPDATE provider_metadata_cache SET mal_id=800 WHERE provider_id=13")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        resolve_series_request(&db, 12, true)
            .await
            .unwrap()
            .0
            .is_none()
    );
    sqlx::query("INSERT INTO series_metadata_cache(series_id,provider_id,mal_id,detail_json) VALUES (?,12,800,'{}')").bind(fallback).execute(&db).await.unwrap();
    assert_eq!(
        resolve_series_request(&db, 12, true)
            .await
            .unwrap()
            .0
            .unwrap()
            .id,
        fallback
    );
    let other = seed_series(&db, -801, "Conflicting legacy release").await;
    sqlx::query("INSERT INTO series_metadata_cache(series_id,provider_id,mal_id,detail_json) VALUES (?,12,801,'{}')").bind(other).execute(&db).await.unwrap();
    assert!(
        resolve_series_request(&db, 12, true)
            .await
            .unwrap()
            .0
            .is_none()
    );
    let exact = seed_series(&db, 12, "Exact release").await;
    assert_eq!(
        resolve_series_request(&db, 12, true)
            .await
            .unwrap()
            .0
            .unwrap()
            .id,
        exact
    );
}

#[tokio::test]
async fn mal_page_cannot_choose_library_controls_for_a_split_entry() {
    let db = in_memory_pool().await;
    seed_series(&db, 12, "First part").await;
    seed_series(&db, 13, "Second part").await;
    sqlx::query("UPDATE series SET mal_id=800")
        .execute(&db)
        .await
        .unwrap();
    cache(&db, -800, "MAL entry with two parts", &[]).await;
    let html = body(request(&db, "/anime/mal/800", false).await).await;
    assert!(html.contains("data-db-id=\"\""));
    assert!(!html.contains("Remove from Library"));
}

#[tokio::test]
async fn all_detail_routes_remain_behind_authentication() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = routes()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::handlers::auth::require_auth,
        ))
        .with_state(state);
    for uri in ["/anime/anilist/130003", "/anime/mal/5680", "/series/1"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["Location"], "/login");
    }
}

#[tokio::test]
async fn release_overview_shows_inferred_origin_separate_from_effective_requirements() {
    use crate::models::series_language_preferences::{self, LanguagePreferences};
    let db = in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    let cfg = config::Config {
        media_root: temp.path().to_string_lossy().into_owned(),
        media_requirements_enabled: true,
        required_audio_languages: "ja".into(),
        required_subtitle_languages: "en".into(),
        minimum_resolution: "720".into(),
        target_resolution: "1080".into(),
        ..Default::default()
    };
    config::save_config(&db, &cfg).await.unwrap();
    let mut detail = anilist::parse_media_node(&json!({
        "id": 123, "countryOfOrigin": "JP", "format": "TV", "episodes": 1,
        "status": "FINISHED", "title": {"english": "Language test"}
    }))
    .unwrap();
    let series_id = seed_series(&db, 123, "Language test").await;
    metadata_cache::upsert_provider(&db, 123, None, &detail)
        .await
        .unwrap();
    let html = body(request(&db, "/anime/anilist/123", false).await).await;
    let overview = html
        .split("aria-label=\"Release management\"")
        .nth(1)
        .unwrap()
        .split("<!-- Episodes -->")
        .next()
        .unwrap();
    assert!(overview.contains("Japanese"));
    assert!(overview.contains("inferred from Japan"));
    assert!(overview.contains("English"));
    assert!(overview.contains("App default"));
    assert!(overview.contains("720p"));
    assert!(overview.contains("1080p"));
    assert_eq!(html.matches("id=\"monitor-mode\"").count(), 1);
    assert_eq!(html.matches("id=\"allow-upgrades\"").count(), 1);
    assert!(html.contains("Choose a batch"));
    assert!(overview.contains("/api/library/monitoring"));
    assert!(overview.contains("/api/library/allow-upgrades"));

    let mut connection = db.acquire().await.unwrap();
    crate::models::library_layout::record(
        &mut connection,
        123,
        &crate::models::library_layout::Assignment {
            path: "Language test/Language test".into(),
            root: "Language test".into(),
            owner: 123,
        },
    )
    .await
    .unwrap();
    drop(connection);
    let page_id = crate::models::library_layout::page_for_provider(&db, 123)
        .await
        .unwrap()
        .unwrap()
        .0;
    crate::models::requirement_preferences::save_franchise(
        &db,
        &crate::models::requirement_preferences::FranchisePreferences {
            page_id,
            audio_languages: Some("zh".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let inherited = body(request(&db, "/anime/anilist/123", false).await).await;
    let inherited = inherited
        .split("aria-label=\"Release management\"")
        .nth(1)
        .unwrap()
        .split("<!-- Episodes -->")
        .next()
        .unwrap();
    assert!(inherited.contains("Chinese"));
    assert!(inherited.contains("<small>Franchise</small>"));
    assert!(inherited.contains("inferred from Japan"));

    let exception = LanguagePreferences {
        audio_languages: Some("de".into()),
        subtitle_languages: Some(String::new()),
        ..Default::default()
    };
    series_language_preferences::save(&db, series_id, &exception)
        .await
        .unwrap();
    let html = body(request(&db, "/anime/anilist/123", false).await).await;
    let overview = html
        .split("aria-label=\"Release management\"")
        .nth(1)
        .unwrap()
        .split("<!-- Episodes -->")
        .next()
        .unwrap();
    assert!(overview.contains("inferred from Japan"));
    assert!(overview.contains("German"));
    assert!(overview.contains("No language requirement"));
    assert_eq!(
        overview.matches("No required subtitle languages").count(),
        2
    );
    assert!(overview.contains("<small>Release</small>"));
    assert_eq!(
        series_language_preferences::get(&db, series_id)
            .await
            .unwrap(),
        exception
    );

    detail.country_of_origin = Some("CN".into());
    metadata_cache::upsert_provider(&db, 123, None, &detail)
        .await
        .unwrap();
    let chinese = LanguagePreferences {
        audio_languages: Some("zh".into()),
        ..Default::default()
    };
    series_language_preferences::save(&db, series_id, &chinese)
        .await
        .unwrap();
    let html = body(request(&db, "/anime/anilist/123", false).await).await;
    assert!(html.contains("Chinese"));
    assert!(html.contains("inferred from China"));
    assert_eq!(
        series_language_preferences::get(&db, series_id)
            .await
            .unwrap(),
        chinese
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    detail.country_of_origin = None;
    metadata_cache::upsert_provider(&db, 123, None, &detail)
        .await
        .unwrap();
    config::save_config(
        &db,
        &config::Config {
            media_requirements_enabled: false,
            ..cfg
        },
    )
    .await
    .unwrap();
    let html = body(request(&db, "/anime/anilist/123", false).await).await;
    assert!(html.contains("Original language hint:</strong> Unknown"));
    assert!(html.contains("Minimum + good enough mode is off."));
    assert!(html.contains("Uses the configured quality cutoff."));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

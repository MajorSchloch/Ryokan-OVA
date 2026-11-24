//! Browser coverage for saving monitoring from the dedicated Add Anime page.

use ryokan::test_support::{build_test_app_state, in_memory_pool};
use serde_json::json;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
};

#[tokio::test]
async fn adding_from_the_setup_page_saves_monitoring_and_opens_the_release() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let provider_id = 911_023;
    let detail = serde_json::from_value(json!({
        "id": provider_id,
        "title_romaji": "Orbit Archive",
        "title_english": "Orbit Archive",
        "title_native": "",
        "cover_url": "",
        "banner_url": "",
        "format": "TV",
        "status": "FINISHED",
        "status_display": "Finished",
        "episodes": 2,
        "season": "FALL",
        "season_year": 2023,
        "description": "",
        "genres": [],
        "score_is_ten_point": false,
        "score_class": "",
        "synonyms": [],
        "streaming_episodes": [],
        "relations": []
    }))
    .expect("valid provider detail");
    ryokan::models::metadata_cache::upsert_provider(&db, provider_id, None, &detail)
        .await
        .expect("cache provider detail");
    let session = seed_user_session(&db).await;
    let addr = spawn_app(build_test_app_state(db.clone(), None)).await;

    open_with_session(
        &client,
        addr,
        &session,
        &format!("/library/add?provider_id={provider_id}"),
    )
    .await
    .expect("open Add Anime setup");
    assert_htmx_loaded(&client).await.expect("htmx loaded");
    assert_eq!(ryokan::test_support::count_series(&db).await, 0);

    client
        .execute(
            r#"
            document.querySelector('input[name=monitor_mode][value=none]').click();
            document.querySelector('.add-anime-actions button[type=submit]').click();
            "#,
            vec![],
        )
        .await
        .expect("submit Add Anime setup");

    client
        .wait()
        .for_url(
            &format!("http://{addr}/anime/anilist/{provider_id}")
                .parse()
                .expect("valid release URL"),
        )
        .await
        .expect("save opens the canonical release page");
    let settings: (String, bool) = sqlx::query_as(
        "SELECT monitor_mode, monitor_mode_manual_override FROM series WHERE anilist_id = ?",
    )
    .bind(provider_id)
    .fetch_one(&db)
    .await
    .expect("read monitoring choice");
    assert_eq!(settings, ("none".into(), true));

    let _ = client.close().await;
}

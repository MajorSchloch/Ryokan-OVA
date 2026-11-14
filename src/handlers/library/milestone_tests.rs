//! Behavioral regressions for the library/navigation milestone.
use super::*;
use crate::{
    models::{
        config,
        requirement_preferences::{self, FranchisePreferences, QualityPreferences},
        series_language_preferences::{self, LanguagePreferences},
    },
    services::{language_preferences, media_info},
    test_support::{build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series},
};
use axum::{
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn parent(db: &sqlx::SqlitePool, provider: i64) -> String {
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    sqlx::query("INSERT INTO library_franchises(folder,owner_provider_id,page_id) VALUES('Test franchise',?,?)").bind(provider).bind(id).execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO library_franchise_members(provider_id,folder) VALUES(?,'Test franchise')",
    )
    .bind(provider)
    .execute(db)
    .await
    .unwrap();
    id.into()
}
fn policy() -> config::Config {
    config::Config {
        media_requirements_enabled: true,
        minimum_resolution: "720".into(),
        target_resolution: "1080".into(),
        required_audio_languages: "ja".into(),
        required_subtitle_languages: "en".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn requirements_hierarchy_preserves_explicit_empty_and_independent_fields() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 7001, "Test").await;
    let page = parent(&db, 7001).await;
    requirement_preferences::save_franchise(
        &db,
        &FranchisePreferences {
            page_id: page,
            audio_languages: Some("es".into()),
            audio_match: Some("all".into()),
            target_resolution: Some("2160".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    series_language_preferences::save(
        &db,
        id,
        &LanguagePreferences {
            audio_languages: Some("de".into()),
            audio_match: Some("all".into()),
            subtitle_languages: Some(String::new()),
            subtitle_match: Some("all".into()),
        },
    )
    .await
    .unwrap();
    let cfg = policy();
    let p = language_preferences::resolve_all(&db, &cfg, &[id])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(p.config.required_audio_languages, "de");
    assert_eq!(p.config.required_subtitle_languages, "");
    assert_eq!(p.config.target_resolution, "2160");
    assert_eq!(p.origins["audio"], "Release");
    assert_eq!(p.origins["target"], "Franchise");
    requirement_preferences::save_quality(
        &db,
        id,
        &QualityPreferences {
            target_resolution: Some("1080".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p = language_preferences::effective_config(&db, &cfg, id)
        .await
        .unwrap();
    assert_eq!(p.target_resolution, "1080");
    assert_eq!(p.required_audio_languages, "de");
    requirement_preferences::save_quality(&db, id, &QualityPreferences::default())
        .await
        .unwrap();
    let p = language_preferences::effective_config(&db, &cfg, id)
        .await
        .unwrap();
    assert_eq!(p.target_resolution, "2160");
    assert!(p.required_subtitle_languages.is_empty());
    assert_eq!(cfg.required_audio_languages, "ja");
    assert_eq!(cfg.required_subtitle_languages, "en");
}

#[tokio::test]
async fn requirements_invalid_release_is_scoped_and_leaf_can_repair_inherited_quality() {
    let db = in_memory_pool().await;
    let bad = seed_series(&db, 7001, "Bad").await;
    let good = seed_series(&db, 7002, "Good").await;
    requirement_preferences::save_quality(
        &db,
        bad,
        &QualityPreferences {
            target_resolution: Some("480".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let cfg = policy();
    let snapshot = language_preferences::resolve_snapshot(&db, &cfg, &[bad, good])
        .await
        .unwrap();
    assert!(snapshot[&bad].is_err());
    assert!(snapshot[&good].is_ok());
    assert!(
        language_preferences::effective_config(&db, &cfg, good)
            .await
            .is_ok()
    );
    let page = parent(&db, 7001).await;
    requirement_preferences::save_franchise(
        &db,
        &FranchisePreferences {
            page_id: page,
            target_resolution: Some("480".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    requirement_preferences::save_quality(
        &db,
        bad,
        &QualityPreferences {
            target_resolution: Some("1080".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        language_preferences::effective_config(&db, &cfg, bad)
            .await
            .unwrap()
            .target_resolution,
        "1080"
    );
}

#[tokio::test]
async fn requirements_form_repairs_invalid_saved_target_without_acquisition() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 7001, "Test").await;
    config::save_config(&db, &policy()).await.unwrap();
    requirement_preferences::save_quality(
        &db,
        id,
        &QualityPreferences {
            target_resolution: Some("480".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        !requirements::release_view(&db, &policy(), id)
            .await
            .unwrap()
            .error
            .is_empty()
    );
    let app = requirements::routes().with_state(build_test_app_state(db.clone(), None));
    let response=app.oneshot(Request::post("/library/requirements").header("content-type","application/x-www-form-urlencoded").header("HX-Request","true").body(Body::from(format!("scope=release&id={id}&audio_mode=inherit&subtitle_mode=none&audio_match=all&subtitle_match=all&target_resolution=1080"))).unwrap()).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("Requirements saved"));
    let cfg = language_preferences::effective_config(&db, &policy(), id)
        .await
        .unwrap();
    assert_eq!(cfg.target_resolution, "1080");
    assert!(cfg.required_subtitle_languages.is_empty());
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

async fn cached_file(db: &sqlx::SqlitePool, path: &std::path::Path, audio: &str, height: i32) {
    std::fs::write(path, b"test video bytes").unwrap();
    let path = path.canonicalize().unwrap();
    let identity =
        media_info::FileIdentity::from_metadata(&std::fs::metadata(&path).unwrap()).unwrap();
    let raw=serde_json::json!({"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":height},{"index":1,"codec_type":"audio","codec_name":"aac","tags":{"language":audio}},{"index":2,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng","title":"Full dialogue"}}]}).to_string();
    crate::models::media_probe_cache::upsert_identified(
        db,
        path.to_str().unwrap(),
        &identity.cache_key(),
        identity.size as i64,
        &raw,
    )
    .await;
}

#[tokio::test]
async fn file_summary_counts_range_once_and_invalidates_changed_video() {
    let db = in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("Test")).unwrap();
    let id = seed_series(&db, 7001, "Test").await;
    let mut cfg = policy();
    cfg.media_root = temp.path().to_string_lossy().into_owned();
    config::save_config(&db, &cfg).await.unwrap();
    let file = temp.path().join("Test/Test - S01E01-E02.mkv");
    cached_file(&db, &file, "jpn", 1080).await;
    std::fs::write(file.with_extension("nfo"), b"not media").unwrap();
    let rows = crate::models::series::get_all(&db).await.unwrap();
    let s = summary::load(&db, &cfg.media_root, &rows)
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(s.files, 1);
    assert_eq!(s.episodes, 2);
    assert_eq!(s.bytes, 16);
    assert_eq!(s.target, 1);
    assert_eq!(s.gates.len(), 2);
    std::fs::write(&file, b"changed video").unwrap();
    let s = summary::load(&db, &cfg.media_root, &rows)
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(s.target, 0);
    assert_eq!(s.unverified, 1);
}

#[tokio::test]
async fn advanced_quality_mode_still_displays_audio_failure() {
    let db = in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("Test")).unwrap();
    let id = seed_series(&db, 7001, "Test").await;
    let mut cfg = policy();
    cfg.media_root = temp.path().to_string_lossy().into_owned();
    cfg.media_requirements_enabled = false;
    cfg.required_audio_languages = "de".into();
    config::save_config(&db, &cfg).await.unwrap();
    cached_file(
        &db,
        &temp.path().join("Test/Test - S01E01.mkv"),
        "jpn",
        1080,
    )
    .await;
    let rows = crate::models::series::get_all(&db).await.unwrap();
    let s = summary::load(&db, &cfg.media_root, &rows)
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(s.unmet, 1);
    assert_eq!(s.disabled, 0);
    assert_eq!(s.gates[&1].label, "Audio requirement unmet");
}

#[tokio::test]
async fn title_search_includes_manual_aliases_and_cached_synonyms() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 7001, "Test").await;
    sqlx::query("UPDATE series SET alternate_titles='Custom release name' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO series_metadata_cache(series_id,provider_id,detail_json) VALUES(?,7001,?)",
    )
    .bind(id)
    .bind(r#"{"synonyms":["Another provider title"]}"#)
    .execute(&db)
    .await
    .unwrap();
    let rows = crate::models::series::get_all(&db).await.unwrap();
    let names = summary::titles(&db, &rows).await.unwrap();
    assert!(names[&id].contains("Custom release name"));
    assert!(names[&id].contains("Another provider title"));
}

#[tokio::test]
async fn activity_is_deduplicated_and_survives_series_removal() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 7001, "Test").await;
    let page = parent(&db, 7001).await;
    let mut event = crate::models::library_activity::Event::new(
        "unique-delete",
        "Deleted",
        Some(id),
        "Test",
        "Manual removal",
    );
    event.note = "Optional note".into();
    event.capture_parent(&db).await;
    crate::models::series::remove(&db, id).await.unwrap();
    crate::models::library_activity::append(&db, event.clone()).await;
    crate::models::library_activity::append(&db, event).await;
    let rows = crate::models::library_activity::recent(&db, &[], Some(&page))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Test");
    assert_eq!(rows[0].note, "Optional note");
}

#[tokio::test]
async fn downloads_scope_precedes_limit_and_preserves_shared_ownership() {
    let db = in_memory_pool().await;
    let selected = seed_series(&db, 7001, "Selected").await;
    let other = seed_series(&db, 7002, "Other").await;
    let shared = seed_grabbed_torrent(&db, other, "shared-hash", "Shared correct job", &[1]).await;
    sqlx::query(
        "INSERT INTO grabbed_torrent_series(grab_id,series_id,episode_numbers) VALUES(?,?, '[1]')",
    )
    .bind(shared)
    .bind(selected)
    .execute(&db)
    .await
    .unwrap();
    for i in 0..105 {
        seed_grabbed_torrent(
            &db,
            other,
            &format!("unrelated-{i}"),
            &format!("Unrelated job {i}"),
            &[1],
        )
        .await;
    }
    let app = management::routes().with_state(build_test_app_state(db.clone(), None));
    let response = app
        .oneshot(
            Request::get(format!("/library/title-downloads?series_id={selected}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("Shared correct job"));
    assert!(body.contains("Shared download"));
    assert!(!body.contains("Unrelated job"));
    let state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id=?")
        .bind(shared)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(state, "pending");
}

#[tokio::test]
async fn requirements_migration_is_idempotent_and_preserves_language_exceptions() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 1313, "Digimon").await;
    let p = LanguagePreferences {
        audio_languages: Some("de".into()),
        audio_match: Some("all".into()),
        subtitle_languages: Some("".into()),
        subtitle_match: Some("all".into()),
    };
    series_language_preferences::save(&db, id, &p)
        .await
        .unwrap();
    for _ in 0..2 {
        requirement_preferences::migrate(&db).await.unwrap();
        crate::models::library_activity::migrate(&db).await.unwrap();
    }
    assert_eq!(series_language_preferences::get(&db, id).await.unwrap(), p);
    assert_eq!(
        requirement_preferences::quality(&db, id).await.unwrap(),
        QualityPreferences::default()
    );
    let effective = language_preferences::effective_config(&db, &policy(), id)
        .await
        .unwrap();
    assert_eq!(effective.required_audio_languages, "de");
    assert!(effective.required_subtitle_languages.is_empty());
}

#[tokio::test]
async fn invalid_language_can_be_repaired_and_invalid_submit_keeps_form_without_acquisition() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 7001, "Test").await;
    config::save_config(&db, &policy()).await.unwrap();
    sqlx::query("INSERT INTO series_language_preferences(series_id,audio_languages,audio_match) VALUES(?,'invalid-language','broken')").bind(id).execute(&db).await.unwrap();
    assert!(
        language_preferences::effective_config(&db, &policy(), id)
            .await
            .is_err()
    );
    let view = requirements::release_view(&db, &policy(), id)
        .await
        .unwrap();
    assert!(!view.error.is_empty());
    let app = super::detail::routes().with_state(build_test_app_state(db.clone(), None));
    let fields = format!(
        "scope=release&id={id}&audio_mode=require&audio_languages=Japanese&audio_match=all&subtitle_mode=inherit&target_resolution=480"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/library/requirements")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("HX-Request", "true")
                .body(Body::from(fields))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(response.into_body(), 200_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Japanese"));
    assert!(html.contains("at least as high"));
    assert!(
        language_preferences::effective_config(&db, &policy(), id)
            .await
            .is_err()
    );
    let fields = format!("scope=release&id={id}&audio_mode=inherit&subtitle_mode=inherit");
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/library/requirements")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("HX-Request", "true")
                .body(Body::from(fields))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        language_preferences::effective_config(&db, &policy(), id)
            .await
            .unwrap()
            .required_audio_languages,
        "ja"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn structured_import_activity_supersedes_matching_legacy_record() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, 7001, "Test").await;
    sqlx::query("INSERT INTO episode_grab_history(series_id,episode_number,quality_tag,release_title,state) VALUES(?,1,'1080p','Accepted release','completed'),(?,2,'720p','Older release','completed')").bind(id).bind(id).execute(&db).await.unwrap();
    let mut event = crate::models::library_activity::Event::new(
        "one",
        "Imported",
        Some(id),
        "Test",
        "Import accepted",
    );
    event.grab_id = Some(1);
    event.detail = serde_json::json!({"release":"Accepted release","file":"Test.mkv"}).to_string();
    crate::models::library_activity::append(&db, event).await;
    let events = crate::models::library_activity::recent(&db, &[id], None)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.reason == "Reason not recorded")
            .count(),
        1
    );
    assert!(events.iter().any(|e| e.detail == "Older release"));
}

#[tokio::test]
async fn logical_size_includes_unparsed_media_once_and_excludes_recovery_and_views() {
    let db = in_memory_pool().await;
    let temp = tempfile::tempdir().unwrap();
    let id = seed_series(&db, 7001, "Test").await;
    let folder = temp.path().join("Test");
    std::fs::create_dir_all(folder.join("recovery")).unwrap();
    std::fs::write(folder.join("unparsed movie.mkv"), b"movie").unwrap();
    std::fs::write(folder.join("recovery/old.mkv"), b"exclude").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(folder.join("unparsed movie.mkv"), folder.join("view.mkv")).unwrap();
    let cfg = config::Config {
        media_root: temp.path().display().to_string(),
        recycle_bin_path: folder.join("recovery").display().to_string(),
        ..policy()
    };
    config::save_config(&db, &cfg).await.unwrap();
    let rows = series::get_all(&db).await.unwrap();
    let s = summary::load(&db, &cfg.media_root, &rows)
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(s.bytes, 5);
    assert_eq!(s.files, 1);
    assert_eq!(s.episodes, 0);
    assert_eq!(s.unverified, 1);
    let mut aggregate = s.clone();
    aggregate.add(&s);
    assert_eq!(aggregate.bytes, 5);
    assert_eq!(aggregate.files, 1);
}

#[tokio::test]
async fn release_page_and_live_refresh_keep_cached_gates_and_neutral_unmonitored_rows() {
    let db = in_memory_pool().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("Test")).unwrap();
    let id = seed_series(&db, 7001, "Test").await;
    let cfg = config::Config {
        media_root: root.path().display().to_string(),
        ..policy()
    };
    config::save_config(&db, &cfg).await.unwrap();
    sqlx::query("UPDATE series SET monitor_mode='none',episodes=2,status='FINISHED' WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    let detail=crate::services::anilist::parse_media_node(&serde_json::json!({"id":7001,"title":{"english":"Test","romaji":"Test"},"type":"ANIME","format":"TV","episodes":2,"status":"FINISHED"})).unwrap();
    crate::models::metadata_cache::upsert(&db, id, 7001, None, &detail)
        .await
        .unwrap();
    for ep in 1..=2 {
        sqlx::query("INSERT INTO series_episode_metadata(series_id,episode_number,title,title_english,aired) VALUES(?,?,'Fixture episode','Fixture episode','2020-01-01')").bind(id).bind(ep).execute(&db).await.unwrap();
    }
    cached_file(
        &db,
        &root.path().join("Test/Test - S01E01.mkv"),
        "jpn",
        1080,
    )
    .await;
    let state = build_test_app_state(db.clone(), None);
    let response = detail::routes()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .uri("/anime/anilist/7001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let doc = scraper::Html::parse_document(&html);
    let rows = scraper::Selector::parse(".episode-table tr[data-monitored='false']").unwrap();
    let text: Vec<_> = doc
        .select(&rows)
        .map(|e| e.text().collect::<String>())
        .collect();
    assert!(text.iter().any(|s| s.contains("Unmonitored")));
    assert!(text.iter().all(|s| !s.contains("Missing")));
    assert!(html.contains("Target met"));
    let payload = episodes::series_episodes_json(State(state), Path(id))
        .await
        .unwrap()
        .0;
    let first = payload.iter().find(|e| e.number == 1).unwrap();
    assert_eq!(first.file_gate.as_ref().unwrap().state, "target");
    assert!(!first.monitored);
}

#[tokio::test]
async fn mal_release_uses_saved_franchise_defaults_through_canonical_or_saved_membership() {
    for member in [-800, 8000] {
        let db = in_memory_pool().await;
        let id = seed_series(&db, -800, "MAL release").await;
        sqlx::query("UPDATE series SET mal_id=800 WHERE id=?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let detail=crate::services::anilist::parse_media_node(&serde_json::json!({"id":8000,"idMal":800,"type":"ANIME","title":{"english":"MAL release"}})).unwrap();
        crate::models::metadata_cache::upsert_provider(&db, 8000, Some(800), &detail)
            .await
            .unwrap();
        let page = parent(&db, member).await;
        requirement_preferences::save_franchise(
            &db,
            &FranchisePreferences {
                page_id: page.clone(),
                audio_languages: Some("es".into()),
                audio_match: Some("all".into()),
                target_resolution: Some("2160".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cfg = policy();
        assert_eq!(
            language_preferences::franchise_for_release(&db, id)
                .await
                .unwrap(),
            Some(page)
        );
        for provider in [-800, 8000] {
            let p = language_preferences::for_provider(&db, &cfg, provider)
                .await
                .unwrap();
            assert_eq!(p.required_audio_languages, "es");
            assert_eq!(p.target_resolution, "2160");
        }
        let view = requirements::release_view(&db, &cfg, id).await.unwrap();
        assert!(view.fields.iter().any(|f| f.name == "audio_mode"
            && f.origin == "Franchise"
            && f.effective == "Spanish"));
    }
}

#[tokio::test]
async fn unmapped_mal_release_keeps_exact_overrides_parent_and_download_scope() {
    let db = in_memory_pool().await;
    let id = seed_series(&db, -800, "MAL only").await;
    sqlx::query("UPDATE series SET mal_id=800 WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    let page = parent(&db, -800).await;
    requirement_preferences::save_franchise(
        &db,
        &FranchisePreferences {
            page_id: page.clone(),
            target_resolution: Some("2160".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    series_language_preferences::save(
        &db,
        id,
        &LanguagePreferences {
            audio_languages: Some("de".into()),
            audio_match: Some("all".into()),
            subtitle_languages: Some("".into()),
            subtitle_match: Some("all".into()),
        },
    )
    .await
    .unwrap();
    let cfg = policy();
    let p = language_preferences::for_provider(&db, &cfg, -800)
        .await
        .unwrap();
    assert_eq!(p.required_audio_languages, "de");
    assert!(p.required_subtitle_languages.is_empty());
    assert_eq!(p.target_resolution, "2160");
    assert_eq!(
        language_preferences::franchise_for_release(&db, id)
            .await
            .unwrap(),
        Some(page.clone())
    );
    let p = language_preferences::resolve_all(&db, &cfg, &[id])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(p.origins["audio"], "Release");
    assert_eq!(p.origins["target"], "Franchise");
    seed_grabbed_torrent(&db, id, "hash", "German dub pending", &[1]).await;
    let response = detail::routes()
        .with_state(build_test_app_state(db, None))
        .oneshot(
            Request::builder()
                .uri(format!("/library/title-downloads?franchise_id={page}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("German dub pending"));
}

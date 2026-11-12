//! Read-only media inspection, resolved from a tracked episode rather than a caller path.
use crate::{
    AppState,
    models::{config, series},
    services::{media, media_info, media_requirements},
};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"ok": false, "code": code, "message": message})),
    )
        .into_response()
}

pub async fn episode_file_info(
    State(state): State<AppState>,
    Path((series_id, episode_number)): Path<(i64, i32)>,
) -> Response {
    if episode_number < 1 {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_episode",
            "Choose a positive episode number.",
        );
    }
    let tracked = match series::get_by_id(&state.db, series_id).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "untracked",
                "Series is not in the library.",
            );
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                "Could not read the library entry.",
            );
        }
    };
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(value)) => value,
        _ => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_error",
                "Could not read the library settings.",
            );
        }
    };
    let cfg =
        match crate::services::language_preferences::effective_config(&state.db, &cfg, series_id)
            .await
        {
            Ok(value) => value,
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "language_settings_unavailable",
                    "Could not read this release's language settings.",
                );
            }
        };
    let root = match tokio::fs::canonicalize(&cfg.media_root).await {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "media_unavailable",
                "The media folder is unavailable.",
            );
        }
    };
    let folder = match tokio::fs::canonicalize(root.join(&tracked.folder_name)).await {
        Ok(value) if value.starts_with(&root) && value != root => value,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_folder",
                "The series folder is outside the media folder or unavailable.",
            );
        }
    };
    let files = media::scan_series_folder(&cfg.media_root, &tracked.folder_name).await;
    let file = files
        .iter()
        .find(|file| file.holds(episode_number) && file.is_own_season())
        .or_else(|| files.iter().find(|file| file.holds(episode_number)));
    let Some(file) = file else {
        return error(
            StatusCode::NOT_FOUND,
            "file_missing",
            "No local file is available for this episode.",
        );
    };
    let full_path = match tokio::fs::canonicalize(folder.join(&file.filename)).await {
        Ok(value) if value.starts_with(&folder) => value,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_file",
                "The file is outside the series folder or unavailable.",
            );
        }
    };
    match media_info::inspect(&state.db, &full_path).await {
        Ok(info) => {
            let required = cfg
                .required_subtitle_languages
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            let subtitles = media_info::evaluate_subtitle_requirements(
                &info,
                &required,
                &cfg.subtitle_language_match,
            );
            let requirements = media_requirements::Policy::from_config(&cfg).status(&info);
            let audio = media_info::evaluate_audio_requirements(
                &info,
                &cfg.required_audio_languages
                    .split(',')
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                &cfg.audio_language_match,
            );
            Json(serde_json::json!({
                "ok": true,
                "filename": file.filename,
                "media": info,
                "subtitle_requirements": subtitles,
                "audio_requirements": audio,
                "media_requirements": requirements,
                "upgrades_allowed": tracked.allow_upgrades,
            }))
            .into_response()
        }
        Err(failure) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            failure.code,
            &failure.message,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
    use axum::body::to_bytes;

    async fn observed_file(
        db: &sqlx::SqlitePool,
        root: &std::path::Path,
        streams: serde_json::Value,
    ) -> i64 {
        let id = seed_series(db, 1234, "Test").await;
        std::fs::create_dir(root.join("Test")).unwrap();
        let file = root.join("Test/Test - S01E01.mkv");
        std::fs::write(&file, b"cached media fixture").unwrap();
        let path = std::fs::canonicalize(&file).unwrap();
        let identity =
            media_info::FileIdentity::from_metadata(&std::fs::metadata(&path).unwrap()).unwrap();
        crate::models::media_probe_cache::upsert_identified(
            db,
            path.to_str().unwrap(),
            &identity.cache_key(),
            identity.size as i64,
            &serde_json::json!({"streams": streams}).to_string(),
        )
        .await;
        id
    }

    #[tokio::test]
    async fn configured_status_uses_actual_file_and_preserves_track_facts() {
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            media_root: temp.path().to_string_lossy().into_owned(),
            media_requirements_enabled: true,
            minimum_resolution: "720".into(),
            target_resolution: "2160".into(),
            required_subtitle_languages: "de".into(),
            minimum_subtitle_check: false,
            target_subtitle_check: true,
            ..Default::default()
        };
        config::save_config(&db, &cfg).await.unwrap();
        let id = observed_file(
            &db,
            temp.path(),
            serde_json::json!([
                {"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080},
                {"index":1,"codec_type":"audio","codec_name":"aac","tags":{"language":"jpn"}},
                {"index":2,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng"}}
            ]),
        )
        .await;
        // Even an eligible quality result does not switch on a release's upgrades.
        sqlx::query("UPDATE series SET allow_upgrades=0 WHERE id=?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("PRAGMA query_only=ON")
            .execute(&db)
            .await
            .unwrap();
        let response =
            episode_file_info(State(build_test_app_state(db.clone(), None)), Path((id, 1))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["media_requirements"]["enabled"], true);
        assert_eq!(json["media_requirements"]["minimum_met"], true);
        assert_eq!(json["media_requirements"]["target_met"], false);
        assert_eq!(json["media_requirements"]["resolution"], "1080p");
        assert_eq!(json["media_requirements"]["minimum_resolution"], "720p");
        assert_eq!(json["media_requirements"]["target_resolution"], "2160p");
        assert_eq!(
            json["media_requirements"]["selectable_subtitles_satisfied"],
            false
        );
        assert_eq!(json["media_requirements"]["minimum_subtitle_check"], false);
        assert_eq!(
            json["subtitle_requirements"]["required_languages"],
            serde_json::json!(["de"])
        );
        assert_eq!(json["media"]["subtitles"][0]["language"], "en");
        assert_eq!(json["media"]["audio"][0]["language"], "ja");
        assert_eq!(json["upgrades_allowed"], false);
        assert_eq!(crate::test_support::count_series(&db).await, 1);
        assert_eq!(
            std::fs::read(temp.path().join("Test/Test - S01E01.mkv")).unwrap(),
            b"cached media fixture"
        );
    }

    #[tokio::test]
    async fn unknown_resolution_cannot_report_a_met_minimum_or_target() {
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        config::save_config(
            &db,
            &config::Config {
                media_root: temp.path().to_string_lossy().into_owned(),
                media_requirements_enabled: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let id = observed_file(
            &db,
            temp.path(),
            serde_json::json!([
                {"index":0,"codec_type":"video","codec_name":"h264"}
            ]),
        )
        .await;
        let response =
            episode_file_info(State(build_test_app_state(db, None)), Path((id, 1))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["media_requirements"]["resolution"], "Unknown");
        assert_eq!(json["media_requirements"]["minimum_met"], false);
        assert_eq!(json["media_requirements"]["target_met"], false);
        assert!(
            json["media_requirements"]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("could not be verified"))
        );
        assert!(json["media"]["subtitles"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_tracked_id_does_not_fetch_provider_metadata() {
        let state = build_test_app_state(in_memory_pool().await, None);
        let response = episode_file_info(State(state), Path((211877, 6))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("untracked"));
    }

    #[tokio::test]
    async fn file_info_uses_release_audio_override_and_explicit_no_subtitles() {
        use crate::models::series_language_preferences::{self, LanguagePreferences};
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        config::save_config(
            &db,
            &config::Config {
                media_root: temp.path().to_string_lossy().into_owned(),
                media_requirements_enabled: true,
                required_subtitle_languages: "en".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let id = observed_file(
            &db,
            temp.path(),
            serde_json::json!([
                {"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
                {"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"language":"deu"}}
            ]),
        )
        .await;
        series::update_allow_upgrades(&db, id, false).await.unwrap();
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
        let response =
            episode_file_info(State(build_test_app_state(db.clone(), None)), Path((id, 1))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            result["audio_requirements"]["required_languages"],
            serde_json::json!(["de"])
        );
        assert_eq!(result["audio_requirements"]["satisfied"], true);
        assert_eq!(result["subtitle_requirements"]["enabled"], false);
        assert_eq!(result["media_requirements"]["minimum_met"], true);
        assert_eq!(result["media_requirements"]["target_met"], true);
        assert_eq!(result["upgrades_allowed"], false);
        assert_eq!(
            config::get_config(&db)
                .await
                .unwrap()
                .unwrap()
                .required_subtitle_languages,
            "en"
        );
    }

    #[tokio::test]
    async fn folder_escape_is_rejected_before_probe() {
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        std::fs::create_dir(&root).unwrap();
        let cfg = config::Config {
            media_root: root.to_string_lossy().into_owned(),
            ..Default::default()
        };
        config::save_config(&db, &cfg).await.unwrap();
        let id = seed_series(&db, 1234, "Test").await;
        series::update_folder(&db, id, "..").await.unwrap();
        let response =
            episode_file_info(State(build_test_app_state(db, None)), Path((id, 1))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_episode_cannot_inspect_another_series() {
        let db = in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("Test")).unwrap();
        std::fs::write(root.join("foreign.mkv"), b"not media").unwrap();
        std::os::unix::fs::symlink(
            root.join("foreign.mkv"),
            root.join("Test/Test - S01E01.mkv"),
        )
        .unwrap();
        config::save_config(
            &db,
            &config::Config {
                media_root: root.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let id = seed_series(&db, 1234, "Test").await;
        let response =
            episode_file_info(State(build_test_app_state(db, None)), Path((id, 1))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

//! Read-only media inspection, resolved from a tracked episode rather than a caller path.
use crate::{
    AppState,
    models::{config, series},
    services::{media, media_info},
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
            Json(serde_json::json!({"ok": true, "filename": file.filename, "media": info, "subtitle_requirements": subtitles})).into_response()
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

    #[tokio::test]
    async fn missing_tracked_id_does_not_fetch_provider_metadata() {
        let state = build_test_app_state(in_memory_pool().await, None);
        let response = episode_file_info(State(state), Path((211877, 6))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("untracked"));
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

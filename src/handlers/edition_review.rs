//! Browser entry points for an exact episode edition review.
use std::{path::Path, process::Stdio, time::Duration};

use askama::Template;
use axum::{
    Form,
    body::Body,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use axum_htmx::HxRequest;

use crate::{
    AppState,
    models::{config, edition_review as reviews, series},
    services::{
        edition_review::{checked_current, checked_paths},
        media_info::{self, MediaInfo},
        media_requirements::Policy,
    },
};

const MAX_CLIP_BYTES: u64 = 4 << 20;
static CLIP_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Template)]
#[template(path = "edition_review.html")]
struct Page {
    page: &'static str,
    title_language: String,
    title: String,
    series_id: i64,
    episode: i32,
    current: Candidate,
    candidates: Vec<Candidate>,
    message: String,
}

struct Candidate {
    id: i64,
    name: String,
    duration: String,
    quality: String,
    audio: String,
    subtitles: String,
    status: String,
    reason: String,
    playable: bool,
    meets_minimum: bool,
}

impl Candidate {
    fn unavailable(review: &reviews::Review) -> Self {
        Self {
            id: review.id,
            name: Path::new(&review.current_path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            duration: "Unknown".into(),
            quality: "Unknown".into(),
            audio: "Unverified".into(),
            subtitles: "Unverified".into(),
            status: "Current file changed or unavailable".into(),
            reason: String::new(),
            playable: false,
            meets_minimum: false,
        }
    }

    fn from_info(
        review: &reviews::Review,
        path: &Path,
        info: &MediaInfo,
        policy: &Policy,
        source: bool,
    ) -> Self {
        let verdict = policy.status(info);
        let names = |languages: Vec<&str>, empty: &str| {
            let names = languages
                .into_iter()
                .map(language_name)
                .collect::<std::collections::BTreeSet<_>>();
            if names.is_empty() {
                empty.to_string()
            } else {
                names.into_iter().collect::<Vec<_>>().join(", ")
            }
        };
        Self {
            id: review.id,
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            duration: info
                .duration_seconds
                .map(|seconds| {
                    let seconds = seconds.round() as u64;
                    format!("{}:{:02}", seconds / 60, seconds % 60)
                })
                .unwrap_or_else(|| "Unknown".into()),
            quality: verdict.resolution.into(),
            audio: names(
                info.audio
                    .iter()
                    .filter(|a| !a.commentary && !a.audio_description)
                    .map(|a| a.language.as_deref().unwrap_or("Unknown language"))
                    .collect(),
                "No dialogue tracks found",
            ),
            subtitles: names(
                info.subtitles
                    .iter()
                    .filter(|s| s.valid && !s.forced && !s.signs_only)
                    .map(|s| s.language.as_deref().unwrap_or("Unknown language"))
                    .collect(),
                "No full selectable tracks found",
            ),
            status: if !policy.enabled {
                "Quality checks disabled"
            } else if verdict.target_met {
                "Target met"
            } else if verdict.minimum_met {
                "Minimum met"
            } else {
                "Requirements unmet"
            }
            .into(),
            reason: if source {
                review.reason.clone()
            } else {
                String::new()
            },
            playable: info.duration_seconds.is_some_and(|seconds| seconds > 0.0),
            meets_minimum: policy.enabled && verdict.minimum_met,
        }
    }
}

fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "ko" => "Korean",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "it" => "Italian",
        "pt" => "Portuguese",
        _ => code,
    }
}

fn unavailable(status: StatusCode, reason: &str) -> Response {
    (status, reason.to_string()).into_response()
}

pub async fn page(State(state): State<AppState>, UrlPath(id): UrlPath<i64>) -> Response {
    show(&state, id, String::new()).await
}

async fn show(state: &AppState, id: i64, message: String) -> Response {
    let Ok(Some(review)) = reviews::get_open(&state.db, id).await else {
        return unavailable(
            StatusCode::NOT_FOUND,
            "This edition review is closed or unavailable.",
        );
    };
    let Ok(Some(series)) = series::get_by_id(&state.db, review.series_id).await else {
        return unavailable(
            StatusCode::NOT_FOUND,
            "This release is no longer in the library.",
        );
    };
    let Ok(Some(cfg)) = config::get_config(&state.db).await else {
        return unavailable(
            StatusCode::SERVICE_UNAVAILABLE,
            "Ryokan settings are unavailable.",
        );
    };
    let Ok(cfg) =
        crate::services::language_preferences::effective_config(&state.db, &cfg, series.id).await
    else {
        return unavailable(
            StatusCode::SERVICE_UNAVAILABLE,
            "Media requirements are unavailable.",
        );
    };
    let policy = Policy::from_config(&cfg);
    let mut current = Candidate::unavailable(&review);
    if let Ok(path) = checked_current(&review, &cfg, &series).await
        && let Ok(info) = media_info::inspect(&state.db, &path).await
        && info.identity.cache_key() == review.current_identity
    {
        current = Candidate::from_info(&review, &path, &info, &policy, false);
    }
    let rows = reviews::for_episode(&state.db, series.id, review.episode)
        .await
        .unwrap_or_default();
    let mut candidates = Vec::new();
    for receipt in rows.into_iter().filter(|other| {
        other.current_path == review.current_path
            && other.current_identity == review.current_identity
    }) {
        let Ok((_, source)) = checked_paths(&receipt, &cfg, &series).await else {
            continue;
        };
        let Ok(info) = media_info::inspect(&state.db, &source).await else {
            continue;
        };
        if info.identity.cache_key() != receipt.source_identity {
            continue;
        }
        candidates.push(Candidate::from_info(
            &receipt, &source, &info, &policy, true,
        ));
        if candidates.len() == 5 {
            break;
        }
    }
    if let Some(candidate) = candidates.first() {
        current.id = candidate.id;
    } else {
        current.playable = false;
    }
    let view = Page {
        page: "system",
        title_language: cfg.title_language,
        title: series.title,
        series_id: series.id,
        episode: review.episode,
        current,
        candidates,
        message,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "Could not render edition review");
            unavailable(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not show the review.",
            )
        }
    }
}

#[derive(serde::Deserialize)]
pub struct Decision {
    decision: String,
    dismiss_others: Option<String>,
}

pub async fn decide(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    UrlPath(id): UrlPath<i64>,
    Form(form): Form<Decision>,
) -> Response {
    let (use_candidate, dismiss_others) = match form.decision.as_str() {
        "use" => (true, form.dismiss_others.is_some()),
        "keep" => (false, true),
        "dismiss" => (false, false),
        _ => return unavailable(StatusCode::BAD_REQUEST, "Select a file decision."),
    };
    let owned_state = state.clone();
    match tokio::spawn(async move {
        crate::services::post_processing::resolve_edition(
            &owned_state,
            id,
            use_candidate,
            dismiss_others,
        )
        .await
    })
    .await
    {
        Ok(Ok(())) => {
            crate::handlers::responses::htmx_aware_redirect(is_htmx, "/system?tab=review")
        }
        Ok(Err(error)) => show(&state, id, error).await,
        Err(error) => {
            tracing::error!(%error, "Edition review task failed");
            unavailable(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The decision could not finish. Check Activity.",
            )
        }
    }
}

pub async fn preview(
    State(state): State<AppState>,
    UrlPath((id, part, sample)): UrlPath<(i64, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = crate::handlers::auth::refuse_cross_site_get(&headers) {
        return refused;
    }
    let source = match part.as_str() {
        "candidate" => true,
        "current" => false,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let Ok(_slot) = CLIP_SLOTS.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(Some(review)) = reviews::get_open(&state.db, id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (Ok(Some(cfg)), Ok(Some(series))) = tokio::join!(
        config::get_config(&state.db),
        series::get_by_id(&state.db, review.series_id),
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok((current_path, source_path)) = checked_paths(&review, &cfg, &series).await else {
        return StatusCode::CONFLICT.into_response();
    };
    let (Ok(current_info), Ok(source_info)) = tokio::join!(
        media_info::inspect(&state.db, &current_path),
        media_info::inspect(&state.db, &source_path),
    ) else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let current_duration = current_info.duration_seconds.unwrap_or_default();
    let source_duration = source_info.duration_seconds.unwrap_or_default();
    let offset = match sample.as_str() {
        "start" => (current_duration.min(source_duration) - 12.0).clamp(0.0, 60.0),
        "end" => {
            let duration = if source {
                source_duration
            } else {
                current_duration
            };
            (duration - 30.0).max(0.0)
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    .floor()
    .to_string();
    let (path, info) = if source {
        (source_path, source_info)
    } else {
        (current_path, current_info)
    };
    let Some(stream) = info
        .video
        .iter()
        .find(|v| v.default)
        .or_else(|| info.video.first())
    else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let Ok(dir) = tempfile::tempdir() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let out = dir.path().join("preview.mp4");
    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2,mpegts,avi,mpeg,flv,asf",
            "-max_alloc",
            "134217728",
            "-threads",
            "1",
            "-ss",
            &offset,
            "-i",
        ])
        .arg(&path)
        .args([
            "-map",
            &format!("0:{}", stream.index),
            "-map",
            "0:a:0?",
            "-sn",
            "-dn",
            "-t",
            "12",
            "-filter_threads",
            "1",
            "-vf",
            "scale=640:360:force_original_aspect_ratio=decrease:force_divisible_by=2",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-b:v",
            "700k",
            "-maxrate",
            "900k",
            "-bufsize",
            "1800k",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-b:a",
            "96k",
            "-movflags",
            "+faststart",
            "-fs",
            "4194304",
        ])
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(25), command.status()).await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(size) = tokio::fs::metadata(&out).await.map(|meta| meta.len()) else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    if !status.success()
        || size == 0
        || size > MAX_CLIP_BYTES
        || review.check_files().await.is_err()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let Ok(bytes) = tokio::fs::read(out).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "video/mp4"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(bytes),
    )
        .into_response()
}

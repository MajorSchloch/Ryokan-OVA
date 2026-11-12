//! Grab handlers for interactive + batch search results. Reach into
//! the auto-expand pipeline (`auto_expand_library_from_pack`) when a
//! batch grab lands so per-file routing works the same as the
//! auto-search batch path.

use axum::{
    extract::{Path, State},
    response::Json,
};

use crate::AppState;
use crate::models::episode_tags;
use crate::models::log::LogCategory;
use crate::services::{auto_expand::AutoExpandGrabContext, auto_search, logger};

use super::super::reconcile::resolve_series_context;
use super::auto_search::{auto_expand_library_from_pack, batch_episode_numbers};

/// A magnet and its separately posted hash must describe the same source.
/// An omitted hash on a magnet cannot bypass a saved exact-source decision.
fn source_info_hash(raw: &str, url: &str) -> Result<String, (axum::http::StatusCode, String)> {
    let supplied = match raw.trim() {
        "" => None,
        raw => Some(
            crate::services::download_client::normalize_info_hash(raw).ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "info_hash must be a 40- or 64-character hex info-hash".to_string(),
            ))?,
        ),
    };
    let from_magnet = url
        .trim()
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("magnet:"))
        .then(|| {
            crate::services::download_client::normalize_info_hash(
                &crate::services::nyaa::extract_hash(url),
            )
        })
        .flatten();
    if supplied
        .as_ref()
        .zip(from_magnet.as_ref())
        .is_some_and(|(left, right)| left != right)
    {
        return Err((
            axum::http::StatusCode::CONFLICT,
            "The magnet and source hash differ. Open a fresh search result.".to_string(),
        ));
    }
    Ok(from_magnet.or(supplied).unwrap_or_default())
}

async fn saved_source_mapping(
    db: &sqlx::SqlitePool,
    tracked: Option<&crate::models::series::Series>,
    hash: &str,
    episode: Option<i32>,
) -> Result<
    Option<crate::services::release_numbering::ReleaseMapping>,
    (axum::http::StatusCode, String),
> {
    let Some(tracked) = tracked else {
        return Ok(None);
    };
    let mapping =
        crate::services::release_numbering::bound_mapping_for_source(db, hash, tracked.anilist_id)
            .await
            .map_err(|reason| (axum::http::StatusCode::CONFLICT, reason))?;
    if let (Some(mapping), Some(episode)) = (&mapping, episode)
        && mapping.episodes() != [episode]
    {
        return Err((axum::http::StatusCode::CONFLICT,
            "This reviewed source covers a different episode selection. Use the file picker to choose its verified files.".to_string()));
    }
    Ok(mapping)
}

/// Grab a specific batch release chosen from interactive batch search.
///
/// Mirrors `grab_interactive_result` but without an episode number —
/// batches cover a range of episodes — the episode list is resolved
/// from the release title via [`batch_episode_numbers`] at grab time
/// so per-episode `episode_tags::record_grab` writes land immediately
/// and the UI shows the batch's quality tier without waiting on
/// post-processing.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab-batch",
    tag = "Library",
    summary = "Grab a specific batch release",
    description = "Send a specific batch torrent release (chosen from interactive batch search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Batch grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_batch_result(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = source_info_hash(body["info_hash"].as_str().unwrap_or(""), &url)?;
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);
    // Multi-client routing — the search-result row carries `indexer_id`
    // (None for Nyaa-direct, Some for torznab/newznab fan-out). The
    // frontend round-trips it on grab so the dispatch routes through
    // the indexer's pin (or Nyaa pin for Nyaa-direct).
    let indexer_id: Option<i64> = body["indexer_id"].as_i64();
    let source_guid = body["source_guid"].as_str().unwrap_or("");

    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "No URL provided".to_string(),
        ));
    }

    // Only server-owned saved evidence can authorize source numbering.
    // Client-posted release_mapping JSON is intentionally not consulted.
    let bound_mapping =
        saved_source_mapping(&state.db, tracked_row.as_ref(), &info_hash, None).await?;

    let resolved_client = if indexer_id.is_some() {
        state.client_for_indexer_with_id(indexer_id).await
    } else {
        let cfg = crate::models::config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state
            .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
            .await
    };
    let (qbit, dispatch_client_id) = resolved_client.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    // Same selective-file path as `grab_interactive_result`: narrow
    // a megapack to just the target if it has its own subtitle or
    // part number. Franchise roots (JoJo S1) deliberately fall
    // through so the multi-series auto-expand path below can route
    // each sibling's files into its own library entry instead.
    let wants_selective = qbit.protocol() == "torrent"
        && !info_hash.is_empty()
        && auto_search::has_selective_discriminator(&detail);
    // `effective_hash` is the id Ryokan persists on `grabbed_torrents.hash`
    // and uses to reconcile the grab against the client's later state. For
    // BT clients it equals the precomputed v1 infohash from the magnet/.torrent
    // URL (default `add_torrent_returning_id` impl echoes it back). For SAB it
    // becomes the `nzo_id` returned from `mode=addurl`, which is the only id
    // SAB lets you key queue/history ops by — without this capture, SAB grabs
    // got `hash=""` persisted, the episode-progress poller never matched them
    // in `list_scoped`, and after 30s the reconcile loop marked them removed.
    let prepared_bound = if let Some(mapping) = &bound_mapping {
        Some(
            crate::services::source_numbering_grab::add_bound(&*qbit, &url, &info_hash, mapping)
                .await
                .map_err(|reason| (axum::http::StatusCode::CONFLICT, reason))?,
        )
    } else {
        None
    };
    let (selective_outcome, effective_hash): (Option<Vec<usize>>, String) =
        if let Some(prepared) = &prepared_bound {
            (prepared.kept.clone(), prepared.info_hash.clone())
        } else if wants_selective {
            let detail_clone = detail.clone();
            let mut pick =
                move |files: &[String]| auto_search::pick_wanted_file_indices(files, &detail_clone);
            match qbit
                .add_torrent_with_file_filter(&url, &info_hash, &mut pick)
                .await
            {
                // Selective branch echoes the precomputed info_hash since
                // `add_torrent_with_file_filter` doesn't return an id. SAB's
                // file-filter impl no-ops the filter and returns FullDownload,
                // so SAB grabs that route through here keep the v1 picker-path
                // limitation (documented in services/download_client/sabnzbd/mod.rs).
                // Practically rare — SAB grabs require info_hash to be empty
                // (NZBs have no infohash), and `wants_selective` requires
                // info_hash to be non-empty, so this branch shouldn't fire for
                // SAB in the first place.
                Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => {
                    (Some(kept), info_hash.clone())
                }
                Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => {
                    (None, info_hash.clone())
                }
                Err(e) => {
                    logger::warn(
                        &state.db,
                        LogCategory::Grab,
                        &format!(
                            "Selective batch download failed, falling back to full grab: {}",
                            title
                        ),
                        &e,
                    )
                    .await;
                    let (_, hash) = qbit
                        .add_torrent_returning_id(&url, &info_hash)
                        .await
                        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                    (None, hash)
                }
            }
        } else {
            let (_, hash) = qbit
                .add_torrent_returning_id(&url, &info_hash)
                .await
                .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
            (None, hash)
        };

    // Classify so the log line carries the actual quality tier. Pass the
    // chosen release as a batch in NyaaContext (Layer 4 uses this for the
    // finished-series BluRay rule).
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        Some(crate::services::source::NyaaContext {
            info_hash: &info_hash,
            view_url: "",
            is_batch: true,
        }),
        Some(crate::services::source::SeriesContext {
            status: &detail.status,
            season_year: detail.season_year,
            end_year: detail.end_year,
        }),
    )
    .await;

    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Grabbed batch (interactive): {}", title),
        &format!(
            "group={}, tier={}{}",
            group,
            classification.label(),
            selective_suffix
        ),
    )
    .await;

    if let Some(sid) = series_id {
        // Parse episode list from the batch title so every covered
        // episode gets a per-episode `episode_quality_tags` row at
        // grab time. Same reasoning as in `search_batch_releases` —
        // without this, batch grabs leave every episode showing
        // UNKNOWN in the UI, and with post-processing disabled the
        // rows never get created at all.
        let ep_nums = bound_mapping.as_ref().map_or_else(
            || batch_episode_numbers(&title, &detail),
            |mapping| mapping.episodes(),
        );
        let saved = crate::models::grabbed_torrents::source_replay::record(
            &state.db,
            &effective_hash,
            &title,
            sid,
            &ep_nums,
            true,
            bound_mapping.as_ref(),
            None,
            crate::models::grabbed_torrents::source_replay::SourceRef {
                url: &url,
                guid: source_guid,
                indexer_id,
                client_id: Some(dispatch_client_id),
                required_replay: false,
            },
        )
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("Download added but its episode decision could not be saved: {e}"),
            )
        })?;
        if prepared_bound.is_some() && saved.is_none() {
            return Err((axum::http::StatusCode::CONFLICT,
                "The reviewed download could not be recorded. Open the file picker to review it before continuing.".to_string()));
        }
        let grab_id = saved.as_ref().map(|s| s.id);
        let replayed = saved.as_ref().is_some_and(|s| s.replayed);
        let ep_nums = saved.map(|s| s.episodes).unwrap_or_default();
        if let Some(gid) = grab_id {
            let _ = crate::models::grabbed_torrents::set_download_client(
                &state.db,
                gid,
                Some(dispatch_client_id),
            )
            .await;
            // Issue #118 — fire `Grabbed` for the interactive batch
            // path. Episode number is the lowest in the parsed range
            // (matches the existing single-i32 contract on the event).
            let indexer =
                crate::services::notifications::resolve_indexer_name(&state, indexer_id).await;
            crate::services::notifications::emit_grabbed(
                &state,
                sid,
                ep_nums.first().copied().unwrap_or(0),
                &title,
                indexer,
                None,
                Some(qbit.sonarr_impl_name().to_string()),
            )
            .await;
        }
        for ep_num in ep_nums.iter().filter(|_| !replayed) {
            let _ = episode_tags::record_grab(
                &state.db,
                sid,
                *ep_num,
                &classification,
                &title,
                &group,
                size_bytes,
                true,
            )
            .await;
        }
        // Phase 2 sibling auto-expand. Skip when selective narrowing
        // successfully applied — the user picked a specific sibling
        // (e.g. Stardust Crusaders) out of a megapack and the other
        // siblings' files are marked priority=0 in qBit and won't
        // land. Creating library entries for them would leave ghost
        // rows with no imported files. The `selective_outcome.is_none()
        // && wants_selective` fallback (filter timed out → full
        // download) still auto-expands because the whole pack is
        // actually downloading.
        let selective_narrowed = wants_selective && selective_outcome.is_some();
        if !replayed
            && bound_mapping.is_none()
            && !selective_narrowed
            && let Some(grab_id) = grab_id
        {
            // Fire-and-forget so the HTTP handler doesn't block
            // up to ~60s on qBit metadata discovery. See the
            // matching spawn in `run_auto_search_targets_with_upgrades`.
            let db_task = state.db.clone();
            let qbit_task = qbit.clone();
            // `effective_hash` (the SAB nzo_id or the BT infohash)
            // is what `get_files` keys off in the auto-expand path
            // — passing the original BT-shape `info_hash` would be
            // empty for SAB grabs and miss the file list entirely.
            let info_hash_task = effective_hash.clone();
            let detail_task = detail.clone();
            let title_task = title.clone();
            let ep_nums_task = ep_nums.clone();
            let grab_ctx_task = AutoExpandGrabContext {
                classification: classification.clone(),
                release_group: group.clone(),
                size_bytes,
            };
            tokio::spawn(async move {
                auto_expand_library_from_pack(
                    &db_task,
                    qbit_task,
                    &info_hash_task,
                    &detail_task,
                    sid,
                    &ep_nums_task,
                    grab_id,
                    &title_task,
                    &grab_ctx_task,
                )
                .await;
            });
        }
    }

    if let Some(prepared) = &prepared_bound {
        prepared
            .resume_after_record(qbit.as_ref())
            .await
            .map_err(|reason| (axum::http::StatusCode::BAD_GATEWAY, reason))?;
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "title": title,
        "tier": classification.label(),
        "selective_files": selective_outcome,
    })))
}

/// Grab a specific release chosen from the interactive search.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab/{episode_number}",
    tag = "Library",
    summary = "Grab a specific release",
    description = "Send a specific torrent release (chosen from interactive search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Release grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_interactive_result(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = source_info_hash(body["info_hash"].as_str().unwrap_or(""), &url)?;
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);
    // Multi-client routing — the search-result row carries `indexer_id`
    // (None for Nyaa-direct, Some for torznab/newznab fan-out). The
    // frontend round-trips it on grab so the dispatch routes through
    // the indexer's pin (or Nyaa pin for Nyaa-direct).
    let indexer_id: Option<i64> = body["indexer_id"].as_i64();
    let source_guid = body["source_guid"].as_str().unwrap_or("");
    // Misgrab guardrails: the interactive table round-trips the match
    // provenance the search stamped on the row so the grab history and
    // the log say how this release matched. Absent or malformed means
    // an older page or a hand-built request; the grab still proceeds.
    let match_provenance: Option<auto_search::MatchProvenance> =
        serde_json::from_value(body["match_provenance"].clone()).ok();

    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "No URL provided".to_string(),
        ));
    }

    let bound_mapping = saved_source_mapping(
        &state.db,
        tracked_row.as_ref(),
        &info_hash,
        Some(episode_number),
    )
    .await?;

    let resolved_client = if indexer_id.is_some() {
        state.client_for_indexer_with_id(indexer_id).await
    } else {
        let cfg = crate::models::config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state
            .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
            .await
    };
    let (qbit, dispatch_client_id) = resolved_client.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    // If the target is a multi-part entry ("Kizumonogatari II") OR a
    // subtitled season of a franchise ("Stardust Crusaders"), try the
    // selective-file download path so a megapack release only pulls
    // the files the user is tracking. Franchise roots without their
    // own subtitle return `false` here and fall through to the plain
    // `add_torrent` path — interactive single-episode grabs don't
    // auto-expand the library (that's `grab_batch_result`'s job).
    let wants_selective = qbit.protocol() == "torrent"
        && !info_hash.is_empty()
        && auto_search::has_selective_discriminator(&detail);
    // See `grab_batch_result` above for why `add_torrent_returning_id`
    // matters. Same SAB-nzo_id capture rationale applies to the
    // single-episode interactive grab path.
    let prepared_bound = if let Some(mapping) = &bound_mapping {
        Some(
            crate::services::source_numbering_grab::add_bound(&*qbit, &url, &info_hash, mapping)
                .await
                .map_err(|reason| (axum::http::StatusCode::CONFLICT, reason))?,
        )
    } else {
        None
    };
    let (selective_outcome, effective_hash): (Option<Vec<usize>>, String) =
        if let Some(prepared) = &prepared_bound {
            (prepared.kept.clone(), prepared.info_hash.clone())
        } else if wants_selective {
            let detail_clone = detail.clone();
            let mut pick =
                move |files: &[String]| auto_search::pick_wanted_file_indices(files, &detail_clone);
            match qbit
                .add_torrent_with_file_filter(&url, &info_hash, &mut pick)
                .await
            {
                Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => {
                    (Some(kept), info_hash.clone())
                }
                Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => {
                    (None, info_hash.clone())
                }
                Err(e) => {
                    logger::warn(
                        &state.db,
                        LogCategory::Grab,
                        &format!(
                            "Selective download failed, falling back to full grab: {}",
                            title
                        ),
                        &e,
                    )
                    .await;
                    let (_, hash) = qbit
                        .add_torrent_returning_id(&url, &info_hash)
                        .await
                        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                    (None, hash)
                }
            }
        } else {
            let (_, hash) = qbit
                .add_torrent_returning_id(&url, &info_hash)
                .await
                .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
            (None, hash)
        };

    // Interactive grab: the frontend doesn't currently round-trip the Nyaa
    // view URL, so Layer 2 is skipped here. These grabs are user-initiated
    // and rarely land on the ambiguous tail that Layer 2 targets anyway.
    // Layer 4 still runs when we have a tracked series — it's a pure
    // function with no round-trip cost.
    let series_ctx = tracked_row
        .as_ref()
        .map(|s| crate::services::source::SeriesContext {
            status: &s.status,
            season_year: s.season_year,
            end_year: s.end_year,
        });
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        None,
        series_ctx,
    )
    .await;
    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Interactive grab: {}", title),
        &format!(
            "episode={}, group={}, tier={}{}{}",
            episode_number,
            group,
            classification.label(),
            selective_suffix,
            auto_search::MatchProvenance::log_suffix(match_provenance.as_ref())
        ),
    )
    .await;

    if let Some(sid) = series_id {
        // Interactive grab: not a batch by definition, but a release whose
        // title names a short run of episodes around the one the user
        // clicked (`- 05-06`, issue #246) is one file holding all of them,
        // so the grab row and the grabbed tags cover the run and every
        // episode shows as downloading. The import writes the final rows
        // from the file name either way.
        let held = bound_mapping.as_ref().map_or_else(
            || held_episodes(&title, episode_number),
            |mapping| mapping.episodes(),
        );
        let saved = crate::models::grabbed_torrents::source_replay::record(
            &state.db,
            &effective_hash,
            &title,
            sid,
            &held,
            false,
            bound_mapping.as_ref(),
            Some(episode_number),
            crate::models::grabbed_torrents::source_replay::SourceRef {
                url: &url,
                guid: source_guid,
                indexer_id,
                client_id: Some(dispatch_client_id),
                required_replay: false,
            },
        )
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("Download added but its episode decision could not be saved: {e}"),
            )
        })?;
        if prepared_bound.is_some() && saved.is_none() {
            return Err((axum::http::StatusCode::CONFLICT,
                "The reviewed download could not be recorded. Open the file picker to review it before continuing.".to_string()));
        }
        let grab_id = saved.as_ref().map(|s| s.id);
        let replayed = saved.as_ref().is_some_and(|s| s.replayed);
        let held = saved.map(|s| s.episodes).unwrap_or_default();
        if let Some(gid) = grab_id {
            let _ = crate::models::grabbed_torrents::set_download_client(
                &state.db,
                gid,
                Some(dispatch_client_id),
            )
            .await;
            // Issue #118 — fire `Grabbed` for the interactive
            // single-episode path. Indexer resolved from the
            // search-result row's `indexer_id`; score is None
            // (interactive search runs scoring on the candidate
            // list but the user picks the row, so the score
            // wasn't load-bearing for the grab decision).
            let indexer =
                crate::services::notifications::resolve_indexer_name(&state, indexer_id).await;
            crate::services::notifications::emit_grabbed(
                &state,
                sid,
                episode_number,
                &title,
                indexer,
                None,
                Some(qbit.sonarr_impl_name().to_string()),
            )
            .await;
        }
        for ep in held.iter().filter(|_| !replayed) {
            let _ = episode_tags::record_grab_with_match(
                &state.db,
                sid,
                *ep,
                &classification,
                &title,
                &group,
                size_bytes,
                false,
                match_provenance.as_ref(),
            )
            .await;
        }
    }

    if let Some(prepared) = &prepared_bound {
        prepared
            .resume_after_record(qbit.as_ref())
            .await
            .map_err(|reason| (axum::http::StatusCode::BAD_GATEWAY, reason))?;
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "selective_files": selective_outcome,
    })))
}

/// The episodes an interactive grab records (issue #246): the run the
/// release title names when it reads as a multi-episode file by the
/// same strict rule the import applies to the file name
/// (`media::parse_episode_span`: no whitespace around the dash, two
/// digits on both sides, ascending, at most `MAX_FILE_SPAN`) and that
/// run includes the requested episode; else the requested episode
/// alone. The release-side `parse_release_numbers` is not used here on
/// purpose: its range regex tolerates whitespace, so `Show Season 2 -
/// 03` would read as episodes 2-3 and leave a stale grabbed tag on an
/// episode the file never delivers.
pub(crate) fn held_episodes(title: &str, episode_number: i32) -> Vec<i32> {
    match crate::services::media::parse_episode_span(&title.to_lowercase()) {
        Some(span) if span.is_multi() && span.episodes().contains(&episode_number) => {
            span.episodes().collect()
        }
        _ => vec![episode_number],
    }
}

#[cfg(test)]
mod bound_source_tests {
    use super::*;
    use crate::models::{metadata_cache, series, source_numbering};
    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
    use std::sync::{Arc, Mutex};

    #[test]
    fn direct_bound_grab_uses_the_actual_magnet_hash_and_refuses_a_conflicting_posted_hash() {
        let hash = lagoon_proof().info_hash;
        let url = format!("magnet:?xt=urn:btih:{}", hash.to_ascii_uppercase());
        assert_eq!(source_info_hash("", &url).unwrap(), hash);
        assert_eq!(source_info_hash(&hash, &url).unwrap(), hash);
        assert_eq!(
            source_info_hash(&"f".repeat(40), &url).unwrap_err().0,
            axum::http::StatusCode::CONFLICT
        );
        assert_eq!(
            source_info_hash("", &format!("https://example.test/?btih:{hash}")).unwrap(),
            ""
        );
    }

    #[derive(Default)]
    struct BoundClient {
        selections: Mutex<Vec<Vec<usize>>>,
        resumes: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl DownloadClient for BoundClient {
        async fn test(&self) -> Result<String, String> {
            Ok("ok".into())
        }
        async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
            panic!("reviewed source must not use a legacy whole grab")
        }
        async fn add_torrent_with_file_filter(
            &self,
            _: &str,
            _: &str,
            _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
        ) -> Result<SelectiveOutcome, String> {
            panic!("a reviewed source cannot use the legacy optional filter")
        }
        async fn add_torrent_paused(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
            Ok(AddOutcome::Added)
        }
        async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
            Ok(vec![])
        }
        async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
            let selections = self.selections.lock().unwrap();
            let selected = selections
                .last()
                .cloned()
                .unwrap_or_else(|| (0..lagoon_files().len()).collect());
            Ok(lagoon_files()
                .into_iter()
                .enumerate()
                .map(|(index, name)| DownloadFile {
                    name,
                    size: 100,
                    progress: 0.0,
                    wanted: selected.contains(&index),
                })
                .collect())
        }
        async fn pause(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self, _: &str) -> Result<(), String> {
            self.resumes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
            panic!("a direct reviewed grab must not delete jobs")
        }
        async fn set_file_wanted(
            &self,
            _: &str,
            indices: &[usize],
            wanted: bool,
        ) -> Result<(), String> {
            let mut selections = self.selections.lock().unwrap();
            let mut selected = selections
                .last()
                .cloned()
                .unwrap_or_else(|| (0..lagoon_files().len()).collect());
            for index in indices {
                selected.retain(|item| item != index);
                if wanted {
                    selected.push(*index);
                }
            }
            selected.sort_unstable();
            selections.push(selected);
            Ok(())
        }
        fn sonarr_impl_name(&self) -> &'static str {
            "QBittorrent"
        }
    }

    async fn fixture() -> (AppState, i64, Arc<BoundClient>) {
        let db = in_memory_pool().await;
        let id = seed_series(&db, 1519, "Black Lagoon: The Second Barrage").await;
        let detail = serde_json::from_value(serde_json::json!({
            "id": 1519, "id_mal": null, "title_romaji": "Black Lagoon: The Second Barrage", "title_english": "Black Lagoon: The Second Barrage",
            "title_native": "", "cover_url": "", "banner_url": "", "format": "TV", "status": "FINISHED",
            "status_display": "Finished", "episodes": 12, "duration": 24, "season": "FALL", "season_year": 2006,
            "description": "", "genres": [], "average_score": null, "average_score_display": null,
            "score_is_ten_point": false, "score_class": "", "next_airing_episode": null, "next_airing_at": null,
            "synonyms": [], "streaming_episodes": [], "relations": []
        })).unwrap();
        metadata_cache::upsert(&db, id, 1519, None, &detail)
            .await
            .unwrap();
        let client = Arc::new(BoundClient::default());
        (build_test_app_state(db, Some(client.clone())), id, client)
    }

    fn body() -> serde_json::Value {
        serde_json::json!({
            "url": format!("magnet:?xt=urn:btih:{}", lagoon_proof().info_hash),
            "title": "[Legion] Black Lagoon - 01-29 [BD 1080p]", "resolution": "1080", "group": "Legion",
            // A stale or forged client decision never supplies the authority.
            "release_mapping": {"target_first": 99}
        })
    }

    #[tokio::test]
    async fn direct_bound_grab_batch_selects_server_proof_and_records_canonical_coverage() {
        let (state, id, client) = fixture().await;
        source_numbering::save_reviewed(&state.db, &lagoon_proof())
            .await
            .unwrap();
        let _ = grab_batch_result(State(state.clone()), Path(id), Json(body()))
            .await
            .unwrap();
        assert_eq!(
            client.selections.lock().unwrap().last(),
            Some(&(12..24).collect::<Vec<_>>())
        );
        let (episodes, json): (String, String) =
            sqlx::query_as("SELECT episode_numbers,release_mapping FROM grabbed_torrents")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<i32>>(&episodes).unwrap(),
            (1..=12).collect::<Vec<_>>()
        );
        let mapping: crate::services::release_numbering::ReleaseMapping =
            serde_json::from_str(&json).unwrap();
        assert_eq!(mapping.bound_source(), Some(&lagoon_proof()));
        assert_eq!(
            series::get_all(&state.db).await.unwrap().len(),
            1,
            "bound grabs cannot auto-expand siblings"
        );
        assert_eq!(client.resumes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn direct_bound_grab_single_refuses_a_multi_episode_or_outside_selection_before_add() {
        let (state, id, client) = fixture().await;
        source_numbering::save_reviewed(&state.db, &lagoon_proof())
            .await
            .unwrap();
        for episode in [3, 13] {
            let error =
                grab_interactive_result(State(state.clone()), Path((id, episode)), Json(body()))
                    .await
                    .unwrap_err();
            assert_eq!(error.0, axum::http::StatusCode::CONFLICT);
            assert!(error.1.contains("file picker"));
        }
        assert!(client.selections.lock().unwrap().is_empty());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn direct_bound_grab_single_accepts_only_a_server_reviewed_single_episode() {
        let (state, id, client) = fixture().await;
        let proof = lagoon_proof()
            .narrow_to(&[lagoon_files()[14].clone()])
            .unwrap();
        source_numbering::save_reviewed(&state.db, &proof)
            .await
            .unwrap();
        let _ = grab_interactive_result(State(state.clone()), Path((id, 3)), Json(body()))
            .await
            .unwrap();
        assert_eq!(client.selections.lock().unwrap().last(), Some(&vec![14]));
        let episodes: String = sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<i32>>(&episodes).unwrap(),
            vec![3]
        );
        assert_eq!(client.resumes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn direct_bound_grab_never_resumes_when_recording_errors_or_deduplicates() {
        for deduplicated in [false, true] {
            let (state, id, client) = fixture().await;
            let proof = lagoon_proof();
            source_numbering::save_reviewed(&state.db, &proof)
                .await
                .unwrap();
            let mut request = body();
            if deduplicated {
                let mapping =
                    crate::services::release_numbering::ReleaseMapping::from_bound_source(&proof)
                        .unwrap();
                crate::models::grabbed_torrents::record_grab_with_release_mapping(
                    &state.db,
                    &proof.info_hash,
                    "Existing pending source",
                    id,
                    &mapping.episodes(),
                    true,
                    Some(&mapping),
                )
                .await
                .unwrap()
                .unwrap();
            } else {
                // The source identity disappears after preparation but before
                // recording. No successful transaction may be invented.
                request["indexer_id"] = 999.into();
                request["source_guid"] = "missing-source".into();
            }
            let error = grab_batch_result(State(state.clone()), Path(id), Json(request))
                .await
                .unwrap_err();
            assert!(matches!(
                error.0,
                axum::http::StatusCode::CONFLICT | axum::http::StatusCode::BAD_GATEWAY
            ));
            assert!(!client.selections.lock().unwrap().is_empty());
            assert_eq!(client.resumes.load(std::sync::atomic::Ordering::SeqCst), 0);
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
                .fetch_one(&state.db)
                .await
                .unwrap();
            assert_eq!(count, i64::from(deduplicated));
        }
    }
}

#[cfg(test)]
mod held_episodes_tests {
    use super::held_episodes;

    #[test]
    fn a_range_title_holds_the_run_around_the_requested_episode() {
        assert_eq!(
            held_episodes("[SakuraCircle] Saimin Seishidou - 05-06 (DVD 720x480)", 5),
            vec![5, 6]
        );
        assert_eq!(
            held_episodes(
                "[SubsPlease] RWBY - Hyousetsu Teikoku - 01-03 (720p) [CCDDC9CB].mkv",
                2
            ),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_single_or_pack_title_records_the_requested_episode_only() {
        assert_eq!(
            held_episodes(
                "[SubsPlease] RWBY - Hyousetsu Teikoku - 02 (720p) [4A0FE134].mkv",
                2
            ),
            vec![2]
        );
        // A pack keeps its range in parentheses; a run wider than one
        // file is not a file; a run that does not include the requested
        // episode is not this grab.
        assert_eq!(
            held_episodes(
                "[SubsPlease] RWBY - Hyousetsu Teikoku (01-12) (720p) [Batch]",
                4
            ),
            vec![4]
        );
        assert_eq!(held_episodes("Show - 01-12 [BD]", 4), vec![4]);
        assert_eq!(held_episodes("Show - 05-06 [1080p]", 9), vec![9]);
        // Season and part markers before the dash are not a run.
        assert_eq!(held_episodes("[Group] Show Season 2 - 03", 3), vec![3]);
        assert_eq!(held_episodes("[Group] Show Part 2 - 05", 5), vec![5]);
        assert_eq!(held_episodes("[Group] Show 3 - 05", 5), vec![5]);
    }
}

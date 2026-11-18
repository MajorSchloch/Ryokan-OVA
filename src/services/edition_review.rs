//! Resolve an exact episode edition hold without accepting paths from the browser.
use std::path::{Path, PathBuf};

use crate::{
    AppState,
    models::{
        config::Config, edition_review::Review, grabbed_torrents::GrabbedTorrent, series::Series,
    },
};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_hold(
    db: &sqlx::SqlitePool,
    grab: &GrabbedTorrent,
    provider_id: i64,
    episode: i32,
    source_root: &Path,
    source_path: &Path,
    current_path: &Path,
    source_identity: String,
    current_identity: String,
    reason: String,
) {
    let (Ok(source_root), Ok(source_path), Ok(current_path)) = tokio::join!(
        tokio::fs::canonicalize(source_root),
        tokio::fs::canonicalize(source_path),
        tokio::fs::canonicalize(current_path),
    ) else {
        return;
    };
    if !source_path.starts_with(&source_root) {
        return;
    }
    let (Some(source_root), Some(source_path), Some(current_path)) = (
        source_root.to_str(),
        source_path.to_str(),
        current_path.to_str(),
    ) else {
        return;
    };
    let review = Review {
        id: 0,
        grab_id: grab.id,
        grabbed_at: grab.grabbed_at.clone(),
        series_id: grab.series_id,
        provider_id,
        episode,
        source_root: source_root.into(),
        source_path: source_path.into(),
        source_identity,
        current_path: current_path.into(),
        current_identity,
        reason,
    };
    if review.check_files().await.is_err() {
        return;
    }
    if let Err(error) = crate::models::edition_review::save(db, &review).await {
        tracing::warn!(grab_id = grab.id, %error, "Could not record an edition review");
        return;
    }
    let mut event = crate::models::library_activity::Event::new(
        format!("edition:hold:{}:{}", grab.id, grab.grabbed_at),
        "Held",
        Some(grab.series_id),
        "",
        &review.reason,
    );
    event.provider_id = Some(provider_id);
    event.grab_id = Some(grab.id);
    event.detail = format!("Episode {episode} · {}", grab.torrent_name);
    crate::models::library_activity::append(db, event).await;
}

pub(crate) async fn checked_current(
    review: &Review,
    cfg: &Config,
    series: &Series,
) -> Result<PathBuf, String> {
    if cfg.media_root.trim().is_empty() || series.folder_name.trim().is_empty() {
        return Err("This episode no longer has a library folder.".into());
    }
    let root = tokio::fs::canonicalize(&cfg.media_root)
        .await
        .map_err(|_| "The library is unavailable.".to_string())?;
    let folder = tokio::fs::canonicalize(root.join(&series.folder_name))
        .await
        .map_err(|_| "The episode folder is unavailable.".to_string())?;
    let current = review.checked_file(false).await?;
    if series.id != review.series_id
        || series.anilist_id != review.provider_id
        || !folder.starts_with(&root)
        || !current.starts_with(&folder)
    {
        return Err("The current file location changed. Reload the review.".into());
    }
    Ok(current)
}

pub(crate) async fn checked_paths(
    review: &Review,
    cfg: &Config,
    series: &Series,
) -> Result<(PathBuf, PathBuf), String> {
    let current = checked_current(review, cfg, series).await?;
    let root = tokio::fs::canonicalize(&cfg.media_root)
        .await
        .map_err(|_| "The library is unavailable.".to_string())?;
    let source = review.checked_file(true).await?;
    let recovery = if cfg.recycle_bin_path.trim().is_empty() {
        None
    } else {
        tokio::fs::canonicalize(&cfg.recycle_bin_path).await.ok()
    };
    if !source.starts_with(Path::new(&review.source_root))
        || source.starts_with(&root)
        || recovery.is_some_and(|bin| source.starts_with(bin))
        || review.source_identity == review.current_identity
    {
        return Err("The candidate location changed. Reload the review.".into());
    }
    Ok((current, source))
}

pub(crate) async fn keep(state: &AppState, review: &Review, all: bool) -> Result<(), String> {
    crate::models::edition_review::dismiss(&state.db, review, all)
        .await
        .map_err(|err| err.to_string())?;
    let mut event = crate::models::library_activity::Event::new(
        format!("edition:kept:{}", review.id),
        "Reviewed",
        Some(review.series_id),
        "",
        "Dismissed the edition review and retained the source files.",
    );
    event.provider_id = Some(review.provider_id);
    event.grab_id = Some(review.grab_id);
    event.detail = format!("Episode {}", review.episode);
    crate::models::library_activity::append(&state.db, event).await;
    Ok(())
}

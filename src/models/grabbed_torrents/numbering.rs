//! Durable record of an already selected pack route. This preserves a choice;
//! it does not establish that a release uses MAL, AniDB, or AniList numbering.
//! File paths, not download-client indices, bind the choice to its input.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection, SqlitePool};

use super::GrabSeriesRoute;
use crate::services::episode_mapping::{MappedFile, MappingRule, resolve_file};
use crate::services::{media, post_processing};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MappingBasis {
    /// Existing pack detection selected these routes. Do not label them as
    /// provider-verified merely because they can now survive a restart.
    SelectedPackRoutes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberingSnapshot {
    version: u32,
    basis: MappingBasis,
    files: Vec<FrozenFileRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenFileRoute {
    pub name: String,
    pub series_id: i64,
    pub anilist_id: i64,
    pub episode_offset: i32,
    pub source_span: Option<media::EpisodeSpan>,
    pub special: bool,
}

impl FrozenFileRoute {
    pub fn mapped_episode(&self) -> Result<Option<MappedFile>, String> {
        if self.special {
            return Ok(None);
        }
        let Some(span) = self.source_span else {
            return Ok(None);
        };
        let rule =
            MappingRule::for_offset(self.anilist_id, span.first, span.last, self.episode_offset)
                .map_err(|e| e.to_string())?;
        resolve_file(&[rule], span.first, span.last)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

fn parsed_span(name: &str) -> Option<media::EpisodeSpan> {
    let basename = Path::new(name).file_name()?.to_str()?;
    media::parse_episode_span(&basename.to_lowercase())
}

fn special_file(name: &str, span: Option<media::EpisodeSpan>, format: &str) -> bool {
    let basename = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name)
        .to_lowercase();
    media::is_tv_format(format)
        && (media::path_names_specials(Path::new(name))
            || span.is_some_and(|s| s.special)
            || (span.is_none() && media::has_special_marker(&basename)))
}

impl NumberingSnapshot {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.files.is_empty() {
            return Err("Episode mapping: unsupported or empty saved mapping".into());
        }
        let mut names = BTreeSet::new();
        let mut identities = BTreeMap::new();
        for file in &self.files {
            post_processing::validate_relative_path_fragment(&file.name)
                .map_err(|e| format!("Episode mapping: invalid saved file path: {e}"))?;
            if file.series_id <= 0
                || file.anilist_id == 0
                || !post_processing::is_video_file(&file.name)
                || !names.insert(file.name.as_str())
                || parsed_span(&file.name) != file.source_span
            {
                return Err(format!(
                    "Episode mapping: invalid saved file '{}'",
                    file.name
                ));
            }
            if let Some(previous) = identities.insert(file.series_id, file.anilist_id)
                && previous != file.anilist_id
            {
                return Err("Episode mapping: conflicting saved entry identities".into());
            }
            file.mapped_episode()?;
        }
        Ok(())
    }

    /// Validate the current file set before any import writes. New files and
    /// changed paths require a new decision, never a fallback to the parent.
    /// A known file explicitly marked unwanted may be skipped. A missing file
    /// cannot silently turn a complete pack into a successful partial import.
    /// A different client ordering is harmless.
    pub fn bind_files<'a>(
        &'a self,
        files: &[(usize, &str, bool)],
    ) -> Result<HashMap<usize, &'a FrozenFileRoute>, String> {
        self.validate()?;
        let by_name: HashMap<_, _> = self.files.iter().map(|f| (f.name.as_str(), f)).collect();
        let mut seen = BTreeSet::new();
        let mut bound = HashMap::new();
        for &(index, name, wanted) in files {
            if !seen.insert(name) {
                return Err(format!(
                    "Episode mapping: duplicate downloaded file '{name}'"
                ));
            }
            if wanted {
                let file = by_name.get(name).ok_or_else(|| {
                    format!("Episode mapping: downloaded file '{name}' has no saved mapping")
                })?;
                bound.insert(index, *file);
            }
        }
        if let Some(missing) = self
            .files
            .iter()
            .find(|file| !seen.contains(file.name.as_str()))
        {
            return Err(format!(
                "Episode mapping: saved file '{}' is missing from the download",
                missing.name
            ));
        }
        Ok(bound)
    }

    /// Canonical numbers for progress and replacement lookups. Never use the
    /// caller's requested episode or the current cumulative offset here.
    fn episodes_for_series(&self, series_id: i64) -> Result<Vec<i32>, String> {
        let mut episodes = BTreeSet::new();
        for file in self.files.iter().filter(|f| f.series_id == series_id) {
            if let Some(mapped) = file.mapped_episode()? {
                episodes.extend(mapped.first..=mapped.last);
            }
        }
        Ok(episodes.into_iter().collect())
    }
}

fn decode_error(message: impl Into<String>) -> sqlx::Error {
    sqlx::Error::Decode(message.into().into())
}

pub async fn get_numbering_snapshot(
    db: &SqlitePool,
    grab_id: i64,
) -> Result<Option<NumberingSnapshot>, sqlx::Error> {
    let json: Option<String> =
        sqlx::query_scalar("SELECT snapshot FROM grabbed_torrent_numbering WHERE grab_id = ?")
            .bind(grab_id)
            .fetch_optional(db)
            .await?;
    json.map(|json| {
        let snapshot: NumberingSnapshot = serde_json::from_str(&json)
            .map_err(|e| decode_error(format!("Episode mapping: invalid saved mapping: {e}")))?;
        snapshot.validate().map_err(decode_error)?;
        Ok(snapshot)
    })
    .transpose()
}

/// Store a complete selected route set and its file/identity binding in one
/// transaction. A pending download's saved choice is immutable. Repeating the
/// same decision is allowed, including with reordered client file indices.
pub async fn record_grab_series_routes_with_files(
    db: &SqlitePool,
    routes: &[GrabSeriesRoute],
    filenames: &[String],
) -> Result<(), sqlx::Error> {
    let grab_id = routes
        .first()
        .ok_or_else(|| decode_error("Episode mapping: no routes to save"))?
        .grab_id;
    let mut tx = db.begin().await?;
    // Acquire the writer lock before reading identity/snapshot state. This
    // also prevents two async auto-expand tasks from both choosing a winner.
    let parent_series: Option<i64> =
        sqlx::query_scalar("UPDATE grabbed_torrents SET id = id WHERE id = ? AND state = 'pending' RETURNING series_id")
            .bind(grab_id)
            .fetch_optional(&mut *tx)
            .await?;
    let parent_series =
        parent_series.ok_or_else(|| decode_error("Episode mapping: grab is no longer pending"))?;
    let migrated: bool = sqlx::query_scalar(
        "SELECT library_migration_bridge IS NOT NULL FROM grabbed_torrents WHERE id=?",
    )
    .bind(grab_id)
    .fetch_one(&mut *tx)
    .await?;
    if migrated {
        return Err(decode_error(
            "Episode mapping: a reviewed migration replay cannot be replaced by new offsets",
        ));
    }
    let mut files = BTreeMap::new();
    let mut targets = BTreeSet::new();
    for route in routes {
        if route.grab_id != grab_id || !targets.insert(route.series_id) {
            return Err(decode_error("Episode mapping: conflicting route owners"));
        }
        let row = sqlx::query("SELECT anilist_id, format FROM series WHERE id = ?")
            .bind(route.series_id)
            .fetch_one(&mut *tx)
            .await?;
        let anilist_id: i64 = row.try_get("anilist_id")?;
        let format: String = row.try_get("format")?;
        for &index in &route.file_indices {
            let name = filenames.get(index).ok_or_else(|| {
                decode_error(format!("Episode mapping: missing file index {index}"))
            })?;
            // Pack detection recognizes a few containers the importer does
            // not yet handle. Preserve its route row, but freeze only files
            // this importer can actually consume. A future format addition
            // then requires a new choice instead of silently widening one.
            if !post_processing::is_video_file(name) {
                continue;
            }
            let span = parsed_span(name);
            let file = FrozenFileRoute {
                name: name.clone(),
                series_id: route.series_id,
                anilist_id,
                episode_offset: route.episode_offset,
                source_span: span,
                special: special_file(name, span, &format),
            };
            if let Some(previous) = files.insert(name.clone(), file.clone())
                && previous != file
            {
                return Err(decode_error(format!(
                    "Episode mapping: conflicting routes for '{name}'"
                )));
            }
        }
    }
    let snapshot = NumberingSnapshot {
        version: 1,
        basis: MappingBasis::SelectedPackRoutes,
        files: files.into_values().collect(),
    };
    snapshot.validate().map_err(decode_error)?;
    // A complete pack decision covers every supplied video, including extras
    // deliberately set aside. Empty names are the picker's unwanted entries.
    let video_files: Vec<_> = filenames
        .iter()
        .enumerate()
        .filter(|(_, name)| post_processing::is_video_file(name))
        .map(|(i, name)| (i, name.as_str(), true))
        .collect();
    snapshot.bind_files(&video_files).map_err(decode_error)?;
    let json = serde_json::to_string(&snapshot).map_err(|e| decode_error(e.to_string()))?;
    let previous: Option<String> =
        sqlx::query_scalar("SELECT snapshot FROM grabbed_torrent_numbering WHERE grab_id = ?")
            .bind(grab_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(previous) = previous {
        let saved: NumberingSnapshot = serde_json::from_str(&previous)
            .map_err(|e| decode_error(format!("Episode mapping: invalid saved mapping: {e}")))?;
        if saved != snapshot {
            return Err(decode_error(
                "Episode mapping: this download already has a different saved mapping",
            ));
        }
    }
    sqlx::query("DELETE FROM grabbed_torrent_series WHERE grab_id = ?")
        .bind(grab_id)
        .execute(&mut *tx)
        .await?;
    for route in routes {
        let mut normalized = route.clone();
        normalized.episode_numbers = snapshot
            .episodes_for_series(route.series_id)
            .map_err(decode_error)?;
        write_route(&mut tx, &normalized).await?;
    }
    let parent_episodes = snapshot
        .episodes_for_series(parent_series)
        .map_err(decode_error)?;
    sqlx::query("UPDATE grabbed_torrents SET episode_numbers = ?, is_batch = CASE WHEN ? > 1 THEN 1 ELSE is_batch END WHERE id = ?")
        .bind(serde_json::to_string(&parent_episodes).map_err(|e| decode_error(e.to_string()))?)
        .bind(parent_episodes.len() as i64)
        .bind(grab_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO grabbed_torrent_numbering (grab_id, snapshot) VALUES (?, ?) ON CONFLICT(grab_id) DO NOTHING")
        .bind(grab_id)
        .bind(json)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

pub(super) async fn write_route(
    connection: &mut SqliteConnection,
    route: &GrabSeriesRoute,
) -> Result<(), sqlx::Error> {
    let indices =
        serde_json::to_string(&route.file_indices).map_err(|e| decode_error(e.to_string()))?;
    let episodes =
        serde_json::to_string(&route.episode_numbers).map_err(|e| decode_error(e.to_string()))?;
    sqlx::query(
        "INSERT OR REPLACE INTO grabbed_torrent_series
        (grab_id, series_id, file_indices, episode_numbers, matched_subtitle, episode_offset)
        VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(route.grab_id)
    .bind(route.series_id)
    .bind(indices)
    .bind(episodes)
    .bind(&route.matched_subtitle)
    .bind(route.episode_offset)
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;

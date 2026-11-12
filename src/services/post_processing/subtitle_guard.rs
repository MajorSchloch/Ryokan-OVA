//! Required subtitle import transaction. New subtitle files and rollback
//! copies live outside recycle's `<old stem>.` companion sweep.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tempfile::TempDir;

use crate::models::config::Config;
use crate::services::media::EpisodeSpan;
use crate::services::media_info::{self, FileIdentity};
use crate::services::recycle;

use super::{do_file_op, extras};

/// Only a confirmed content mismatch gets this tag. Probe, configuration,
/// staging and rollback errors must remain reviewable import failures.
const CONTENT_REJECTION_PREFIX: &str = "Media rejected: ";

pub(crate) fn is_content_rejection(reason: &str) -> bool {
    reason.starts_with(CONTENT_REJECTION_PREFIX)
}

pub(crate) struct RequiredSubtitleImport {
    dir: Option<TempDir>,
    source: PathBuf,
    source_identity: FileIdentity,
    observed_source: crate::services::media_requirements::Observed,
    dest_video: PathBuf,
    sidecars: Vec<(PathBuf, PathBuf)>,
    source_sidecars: Vec<(PathBuf, FileIdentity)>,
    backups: Vec<(PathBuf, PathBuf)>,
    installed: Vec<PathBuf>,
}

pub(crate) fn required_languages(cfg: &Config) -> Vec<String> {
    cfg.required_subtitle_languages
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn required(cfg: &Config) -> bool {
    cfg.media_requirements_enabled
        || !required_languages(cfg).is_empty()
        || !cfg.required_audio_languages.trim().is_empty()
}

/// Preflight the actual video and the exact sidecars the importer will
/// name. Required sidecars travel even when optional extra import is off.
pub(crate) async fn prepare(
    db: &SqlitePool,
    cfg: &Config,
    source: &Path,
    dest_video: &Path,
    candidates: &[PathBuf],
    span: Option<EpisodeSpan>,
    videos_in_download: usize,
) -> Result<Option<RequiredSubtitleImport>, String> {
    let required = required_languages(cfg);
    if !self::required(cfg) {
        return Ok(None);
    }
    let policy = crate::services::media_requirements::Policy::from_config(cfg);
    let enforce_subtitles = !policy.enabled || policy.minimum_subtitle_check;
    let info = media_info::inspect_with_sidecars(db, source, candidates, span, videos_in_download)
        .await
        .map_err(|e| format!("Subtitle requirement: {e}"))?;
    policy.validate_minimum(&info).map_err(|reason| {
        if policy.minimum_failure_is_content_rejection(&info) {
            format!("{CONTENT_REJECTION_PREFIX}{reason}")
        } else {
            reason
        }
    })?;
    let verdict =
        media_info::evaluate_subtitle_requirements(&info, &required, &cfg.subtitle_language_match);
    if enforce_subtitles && !verdict.satisfied {
        return Err(format!(
            "Subtitle requirement: {}",
            verdict.reasons.join("; ")
        ));
    }
    let wanted: BTreeSet<_> = required
        .iter()
        .filter_map(|language| media_info::normalize_language(language))
        .collect();
    let source_sidecar_paths: Vec<PathBuf> = info
        .subtitles
        .iter()
        .filter(|s| s.external && s.valid && !s.forced && !s.signs_only)
        .filter(|s| {
            s.language
                .as_ref()
                .is_some_and(|language| wanted.contains(language))
        })
        .filter_map(|s| s.path.as_ref().map(PathBuf::from))
        .collect();
    let mut source_sidecars = Vec::new();
    for path in &source_sidecar_paths {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| format!("Subtitle requirement: cannot snapshot subtitle identity: {e}"))?;
        source_sidecars.push((
            path.clone(),
            FileIdentity::from_metadata(&meta).map_err(|e| e.to_string())?,
        ));
    }
    let parent = dest_video
        .parent()
        .ok_or_else(|| "Subtitle requirement: destination has no parent".to_string())?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| format!("Subtitle requirement: cannot create destination: {e}"))?;
    let parent = parent.to_path_buf();
    let dir = tokio::task::spawn_blocking(move || {
        tempfile::Builder::new()
            .prefix(".ryokan-subtitle-import-")
            .tempdir_in(parent)
    })
    .await
    .map_err(|e| format!("Subtitle requirement: cannot prepare staging: {e}"))?
    .map_err(|e| format!("Subtitle requirement: cannot prepare staging: {e}"))?;
    let staged_video = dir.path().join(dest_video.file_name().unwrap_or_default());
    let report = extras::import_subtitles(
        "copy",
        source,
        &staged_video,
        span,
        &source_sidecar_paths,
        videos_in_download,
    )
    .await;
    if !report.errors.is_empty() {
        return Err(format!(
            "Subtitle requirement: cannot stage required subtitle files: {}",
            report.errors.join("; ")
        ));
    }
    // Stage names can differ after renumbering. This explicit, isolated
    // list belongs only to this video, so single-video fallback is safe.
    let staged = media_info::inspect_with_sidecars(db, source, &report.imported, None, 1)
        .await
        .map_err(|e| format!("Subtitle requirement: {e}"))?;
    let verdict = media_info::evaluate_subtitle_requirements(
        &staged,
        &required,
        &cfg.subtitle_language_match,
    );
    if enforce_subtitles && !verdict.satisfied {
        return Err(format!(
            "Subtitle requirement: required subtitle files could not be imported and verified: {}",
            verdict.reasons.join("; ")
        ));
    }
    for (path, expected) in &source_sidecars {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| format!("Subtitle requirement: subtitle changed during staging: {e}"))?;
        if FileIdentity::from_metadata(&meta).map_err(|e| e.to_string())? != *expected {
            return Err("Subtitle requirement: subtitle file changed while staging; existing media was kept".to_string());
        }
    }
    let sidecars = report
        .imported
        .into_iter()
        .map(|path| {
            let dest = dest_video.parent().unwrap().join(path.file_name().unwrap());
            (path, dest)
        })
        .collect();
    Ok(Some(RequiredSubtitleImport {
        dir: Some(dir),
        source: source.to_path_buf(),
        source_identity: info.identity,
        observed_source: policy.observe(&staged),
        dest_video: dest_video.to_path_buf(),
        sidecars,
        source_sidecars,
        backups: Vec::new(),
        installed: Vec::new(),
    }))
}

impl RequiredSubtitleImport {
    /// Keep complete old video + sidecars even when recycle is disabled.
    /// A backup failure stops the operation before any old file is touched.
    pub(crate) async fn snapshot(&mut self, old_videos: &[PathBuf]) -> Result<(), String> {
        let old_videos = old_videos.to_vec();
        let mut paths = tokio::task::spawn_blocking(move || {
            let mut paths = BTreeSet::new();
            for video in old_videos {
                paths.extend(recycle::helpers::companions(&video));
                paths.insert(video);
            }
            paths
        })
        .await
        .map_err(|e| format!("Subtitle requirement: cannot prepare rollback copies: {e}"))?;
        for (_, dest) in &self.sidecars {
            match tokio::fs::symlink_metadata(dest).await {
                Ok(meta) if meta.is_file() => {
                    paths.insert(dest.clone());
                }
                Ok(_) => {
                    return Err(format!(
                        "Subtitle requirement: subtitle destination is not a regular file: {}",
                        dest.display()
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "Subtitle requirement: cannot check subtitle destination: {e}"
                    ));
                }
            }
        }
        let backup_dir = self.dir.as_ref().unwrap().path().join("old");
        for (index, original) in paths.into_iter().enumerate() {
            let backup = backup_dir.join(index.to_string());
            do_file_op("hardlink", &original, &backup).await
                .map_err(|e| format!("Subtitle requirement: cannot preserve existing media before replacement: {e}"))?;
            self.backups.push((backup, original));
        }
        Ok(())
    }

    /// Required move imports use copy staging so a failed transaction
    /// never consumes the source. Successful copy plus stable source
    /// identity establishes that the inspected bytes were placed.
    pub(crate) async fn check_placement(&self, landing: &Path) -> Result<(), String> {
        let source_meta = tokio::fs::metadata(&self.source)
            .await
            .map_err(|e| format!("Subtitle requirement: source changed during import: {e}"))?;
        if FileIdentity::from_metadata(&source_meta).map_err(|e| e.to_string())?
            != self.source_identity
        {
            return Err("Subtitle requirement: source video changed after inspection; existing media was kept".to_string());
        }
        let placed = tokio::fs::metadata(landing)
            .await
            .map_err(|e| format!("Subtitle requirement: cannot verify placed video: {e}"))?;
        if !placed.is_file() || placed.len() != self.source_identity.size {
            return Err(
                "Subtitle requirement: placed video is incomplete; existing media was kept"
                    .to_string(),
            );
        }
        Ok(())
    }

    pub(crate) async fn commit_sidecars(&mut self) -> Result<(), String> {
        for (staged, dest) in &self.sidecars {
            tokio::fs::rename(staged, dest).await.map_err(|e| {
                format!(
                    "Subtitle requirement: cannot install required subtitles at {}: {e}",
                    dest.display()
                )
            })?;
            self.installed.push(dest.clone());
        }
        Ok(())
    }

    /// On any commit failure restore original paths, including companion
    /// files which recycle may already have removed. Keep recovery copies
    /// on disk if even rollback is refused by the filesystem.
    pub(crate) async fn rollback(&mut self, new_video_installed: bool) -> Result<(), String> {
        let mut errors = Vec::new();
        for path in &self.installed {
            if let Err(e) = tokio::fs::remove_file(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(format!("cannot remove {}: {e}", path.display()));
            }
        }
        if new_video_installed
            && let Err(e) = tokio::fs::remove_file(&self.dest_video).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("cannot remove replacement video: {e}"));
        }
        for (backup, original) in &self.backups {
            if let Err(e) = do_file_op("hardlink", backup, original).await {
                errors.push(format!("cannot restore {}: {e}", original.display()));
            }
        }
        if errors.is_empty() {
            return Ok(());
        }
        let recovery = self.dir.take().unwrap().keep();
        Err(format!(
            "Subtitle import rollback needs recovery from {}: {}",
            recovery.display(),
            errors.join("; ")
        ))
    }

    pub(crate) async fn finish(&self, mode: &str) -> Result<(), String> {
        if mode != "move" {
            return Ok(());
        }
        let mut source_paths = self.source_sidecars.clone();
        source_paths.push((self.source.clone(), self.source_identity.clone()));
        for (path, expected) in source_paths {
            // A re-import whose source already is the final media must
            // never unlink that final copy.
            if path == self.dest_video || self.sidecars.iter().any(|(_, dest)| *dest == path) {
                continue;
            }
            let Ok(meta) = tokio::fs::metadata(&path).await else {
                continue;
            };
            if FileIdentity::from_metadata(&meta).map_err(|e| e.to_string())? == expected {
                tokio::fs::remove_file(&path).await.map_err(|e| {
                    format!(
                        "Imported media is safe, but could not remove source {}: {e}",
                        path.display()
                    )
                })?;
            }
        }
        Ok(())
    }
}

/// Complete a required-subtitle import as one recoverable bundle. No
/// accepted source is consumed and no old client/history is changed until
/// the new video and its verified subtitles are installed together.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn import_required(
    db: &SqlitePool,
    cfg: &Config,
    mode: &str,
    source: &Path,
    dest: &Path,
    old_videos: &[PathBuf],
    series_id: i64,
    series_title: &str,
    candidates: &[PathBuf],
    span: Option<EpisodeSpan>,
    videos_in_download: usize,
    automatic: bool,
) -> Result<(), String> {
    let mut transaction = prepare(db, cfg, source, dest, candidates, span, videos_in_download)
        .await?
        .ok_or_else(|| "Subtitle requirement: policy is disabled".to_string())?;
    if automatic && cfg.media_requirements_enabled {
        let policy = crate::services::media_requirements::Policy::from_config(cfg);
        for old in old_videos {
            let info = media_info::inspect(db, old)
                .await
                .map_err(|e| format!("Media requirement: cannot verify existing file: {e}"))?;
            policy
                .validate_replacement(&policy.observe(&info), &transaction.observed_source)
                .map_err(|reason| {
                    format!("Media requirement: {reason}; existing media was kept")
                })?;
        }
    }
    transaction.snapshot(old_videos).await?;
    let landing = super::staging_path(dest);
    // Copy mode for a move request keeps the source recoverable until
    // the complete video/subtitle bundle has committed.
    let placement_mode = if mode == "move" { "copy" } else { mode };
    let outcome = async {
        do_file_op(placement_mode, source, &landing)
            .await
            .map_err(|e| format!("Subtitle requirement: could not stage video: {e}"))?;
        transaction.check_placement(&landing).await?;
        for old in old_videos {
            recycle::recycle(
                db,
                &cfg.recycle_bin_path,
                recycle::RecycleKind::Episode,
                Some(series_id),
                series_title,
                old,
            )
            .await
            .map_err(|e| format!("Subtitle requirement: could not retire existing media: {e}"))?;
        }
        tokio::fs::rename(&landing, dest).await.map_err(|e| {
            format!("Subtitle requirement: could not install replacement video: {e}")
        })?;
        Ok::<_, String>(())
    }
    .await;
    if let Err(reason) = outcome {
        let rollback = transaction.rollback(false).await;
        let _ = tokio::fs::remove_file(&landing).await;
        return Err(match rollback {
            Ok(()) => reason,
            Err(recovery) => format!("{reason}; {recovery}"),
        });
    }
    if let Err(reason) = transaction.commit_sidecars().await {
        let rollback = transaction.rollback(true).await;
        return Err(match rollback {
            Ok(()) => reason,
            Err(recovery) => format!("{reason}; {recovery}"),
        });
    }
    if let Err(reason) = transaction.finish(mode).await {
        // A source cleanup failure does not undo a valid library file.
        crate::services::logger::warn(
            db,
            crate::models::log::LogCategory::PostProcess,
            "Imported video and required subtitles; source cleanup needs attention",
            &reason,
        )
        .await;
    }
    Ok(())
}

/// Safe same-download walk for manual import and client-less imports.
pub(crate) async fn nearby_candidates(
    source: &Path,
    configured_extensions: &str,
) -> (Vec<PathBuf>, usize) {
    let Some(parent) = source.parent() else {
        return (Vec::new(), usize::MAX);
    };
    let parent = parent.to_path_buf();
    let mut extensions = extras::parse_extensions(configured_extensions);
    for extension in ["srt", "ass", "ssa"] {
        if !extensions.iter().any(|e| e == extension) {
            extensions.push(extension.to_string());
        }
    }
    tokio::task::spawn_blocking(move || extras::walk_extras_with_video_count(&parent, &extensions))
        .await
        .unwrap_or_else(|_| (Vec::new(), usize::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_probe(db: &SqlitePool, path: &Path, subtitles: bool) {
        seed_probe_dimensions(db, path, subtitles, 1280, 720).await;
    }

    async fn seed_audio_probe(
        db: &SqlitePool,
        path: &Path,
        language: &str,
        width: u32,
        height: u32,
    ) {
        let canonical = tokio::fs::canonicalize(path).await.unwrap();
        let identity =
            FileIdentity::from_metadata(&tokio::fs::metadata(&canonical).await.unwrap()).unwrap();
        crate::models::media_probe_cache::upsert_identified(db, canonical.to_str().unwrap(), &identity.cache_key(), identity.size as i64,
            &serde_json::json!({"streams":[
                {"index":0,"codec_type":"video","codec_name":"h264","width":width,"height":height},
                {"index":1,"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"language":language}}
            ]}).to_string()).await;
    }

    #[tokio::test]
    async fn audio_requirement_refuses_missing_or_unknown_dub_before_retirement() {
        for language in ["eng", "jpn", "und", "deu"] {
            let db = crate::test_support::in_memory_pool().await;
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("Release German Dub [1080p].mkv");
            let old = temp.path().join("old.mkv");
            let old_subtitle = temp.path().join("old.de.srt");
            tokio::fs::write(&source, "new video bundle").await.unwrap();
            tokio::fs::write(&old, "original German video")
                .await
                .unwrap();
            tokio::fs::write(
                &old_subtitle,
                "1\n00:00:01,000 --> 00:00:02,000\nOriginal\n",
            )
            .await
            .unwrap();
            seed_audio_probe(&db, &old, "deu", 1280, 720).await;
            seed_audio_probe(&db, &source, language, 1920, 1080).await;
            let cfg = Config {
                media_requirements_enabled: true,
                required_audio_languages: "de".into(),
                required_subtitle_languages: String::new(),
                ..Default::default()
            };
            let result = import_required(
                &db,
                &cfg,
                "copy",
                &source,
                &old,
                std::slice::from_ref(&old),
                34,
                "Release",
                &[],
                None,
                1,
                true,
            )
            .await;
            if language == "deu" {
                result.unwrap();
                assert_eq!(
                    tokio::fs::read_to_string(&old).await.unwrap(),
                    "new video bundle"
                );
            } else {
                let reason = result.unwrap_err();
                assert!(reason.contains("audio"), "{reason}");
                assert_eq!(
                    is_content_rejection(&reason),
                    language != "und",
                    "unknown tags need review, not a guessed rejection"
                );
                assert_eq!(
                    tokio::fs::read_to_string(&old).await.unwrap(),
                    "original German video"
                );
                assert!(old_subtitle.exists());
            }
            assert_eq!(
                tokio::fs::read_to_string(&source).await.unwrap(),
                "new video bundle"
            );
        }
    }

    #[tokio::test]
    async fn audio_only_rule_applies_without_enabling_resolution_gates() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.mkv");
        let dest = temp.path().join("dest.mkv");
        tokio::fs::write(&source, "Spanish audio fixture")
            .await
            .unwrap();
        seed_audio_probe(&db, &source, "spa", 854, 480).await;
        let cfg = Config {
            media_requirements_enabled: false,
            required_audio_languages: "es".into(),
            ..Default::default()
        };
        assert!(required(&cfg));
        import_required(
            &db,
            &cfg,
            "copy",
            &source,
            &dest,
            &[],
            1,
            "Release",
            &[],
            None,
            1,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&dest).await.unwrap(),
            "Spanish audio fixture"
        );
    }

    #[tokio::test]
    async fn routed_release_language_rule_is_independent_of_parent_subtitles() {
        use crate::models::series_language_preferences::{self, LanguagePreferences};
        let db = crate::test_support::in_memory_pool().await;
        let parent = crate::test_support::seed_series(&db, 100, "Parent").await;
        let sibling = crate::test_support::seed_series(&db, 101, "Sibling").await;
        series_language_preferences::save(
            &db,
            sibling,
            &LanguagePreferences {
                audio_languages: Some("de".into()),
                audio_match: Some("all".into()),
                subtitle_languages: Some(String::new()),
                subtitle_match: Some("all".into()),
            },
        )
        .await
        .unwrap();
        let global = Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            ..Default::default()
        };
        let parent_cfg =
            crate::services::language_preferences::effective_config(&db, &global, parent)
                .await
                .unwrap();
        let sibling_cfg =
            crate::services::language_preferences::effective_config(&db, &global, sibling)
                .await
                .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.mkv");
        let dest = temp.path().join("old.mkv");
        tokio::fs::write(&source, "German source").await.unwrap();
        tokio::fs::write(&dest, "Old German source").await.unwrap();
        seed_audio_probe(&db, &source, "deu", 1920, 1080).await;
        seed_audio_probe(&db, &dest, "deu", 1280, 720).await;
        assert!(
            import_required(
                &db,
                &parent_cfg,
                "copy",
                &source,
                &dest,
                std::slice::from_ref(&dest),
                parent,
                "Parent",
                &[],
                None,
                1,
                true
            )
            .await
            .is_err()
        );
        assert_eq!(
            tokio::fs::read_to_string(&dest).await.unwrap(),
            "Old German source"
        );
        import_required(
            &db,
            &sibling_cfg,
            "copy",
            &source,
            &dest,
            std::slice::from_ref(&dest),
            sibling,
            "Sibling",
            &[],
            None,
            1,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&dest).await.unwrap(),
            "German source"
        );
    }

    async fn seed_probe_dimensions(
        db: &SqlitePool,
        path: &Path,
        subtitles: bool,
        width: u32,
        height: u32,
    ) {
        let canonical = tokio::fs::canonicalize(path).await.unwrap();
        let identity =
            FileIdentity::from_metadata(&tokio::fs::metadata(&canonical).await.unwrap()).unwrap();
        let mut streams = vec![
            serde_json::json!({"index":0,"codec_type":"video","codec_name":"h264","width":width,"height":height}),
        ];
        if subtitles {
            streams.push(serde_json::json!({"index":1,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng"}}));
        }
        crate::models::media_probe_cache::upsert_identified(
            db,
            canonical.to_str().unwrap(),
            &identity.cache_key(),
            identity.size as i64,
            &serde_json::json!({"streams":streams}).to_string(),
        )
        .await;
    }

    async fn seed_probe_duration(
        db: &SqlitePool,
        path: &Path,
        width: u32,
        height: u32,
        duration: f64,
    ) {
        seed_probe_dimensions(db, path, true, width, height).await;
        let canonical = tokio::fs::canonicalize(path).await.unwrap();
        let identity =
            FileIdentity::from_metadata(&tokio::fs::metadata(&canonical).await.unwrap()).unwrap();
        let raw = crate::models::media_probe_cache::get_identified(
            db,
            canonical.to_str().unwrap(),
            &identity.cache_key(),
        )
        .await
        .unwrap();
        let mut probe: serde_json::Value = serde_json::from_str(&raw).unwrap();
        probe["format"] = serde_json::json!({"duration":duration.to_string()});
        crate::models::media_probe_cache::upsert_identified(
            db,
            canonical.to_str().unwrap(),
            &identity.cache_key(),
            identity.size as i64,
            &probe.to_string(),
        )
        .await;
    }

    #[tokio::test]
    async fn shorter_cut_holds_real_import_bundle_until_explicit_replacement() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("new.mkv");
        let old = temp.path().join("old.mkv");
        let old_sub = temp.path().join("old.en.srt");
        tokio::fs::write(&source, "shorter1080video").await.unwrap();
        tokio::fs::write(&old, "full720video").await.unwrap();
        tokio::fs::write(
            &old_sub,
            "1\n00:00:01,000 --> 00:00:02,000\nOriginal subtitle\n",
        )
        .await
        .unwrap();
        seed_probe_duration(&db, &source, 1920, 1080, 720.0).await;
        seed_probe_duration(&db, &old, 1280, 720, 1340.0).await;
        let cfg = Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            ..Default::default()
        };
        let reason = import_required(
            &db,
            &cfg,
            "copy",
            &source,
            &old,
            std::slice::from_ref(&old),
            1,
            "Show",
            &[],
            None,
            1,
            true,
        )
        .await
        .unwrap_err();
        assert!(reason.contains("shorter"), "{reason}");
        assert!(
            !is_content_rejection(&reason),
            "a different cut needs review, not repeated downloads"
        );
        assert_eq!(
            tokio::fs::read_to_string(&old).await.unwrap(),
            "full720video"
        );
        assert!(old_sub.exists());
        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "shorter1080video"
        );
        import_required(
            &db,
            &cfg,
            "copy",
            &source,
            &old,
            std::slice::from_ref(&old),
            1,
            "Show",
            &[],
            None,
            1,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&old).await.unwrap(),
            "shorter1080video"
        );
        assert!(
            !old_sub.exists(),
            "old cut's subtitles must not be reused for a different cut"
        );
        assert!(source.exists());
    }

    #[tokio::test]
    async fn missing_required_subtitles_preserves_old_video_sidecars_and_source() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("download/Show - 06.mkv");
        let old = temp.path().join("library/Show - S01E06.mkv");
        let old_sub = old.with_extension("en.srt");
        tokio::fs::create_dir_all(source.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::create_dir_all(old.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&source, "new video").await.unwrap();
        tokio::fs::write(&old, "old video").await.unwrap();
        tokio::fs::write(&old_sub, "old subtitles").await.unwrap();
        seed_probe(&db, &source, false).await;
        let cfg = Config {
            required_subtitle_languages: "en".to_string(),
            ..Config::default()
        };
        let error = import_required(
            &db,
            &cfg,
            "move",
            &source,
            &old,
            std::slice::from_ref(&old),
            1,
            "Show",
            &[],
            Some(EpisodeSpan::single(None, 6)),
            1,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.starts_with("Subtitle requirement:"));
        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "new video"
        );
        assert_eq!(tokio::fs::read_to_string(&old).await.unwrap(), "old video");
        assert_eq!(
            tokio::fs::read_to_string(&old_sub).await.unwrap(),
            "old subtitles"
        );
    }

    #[tokio::test]
    async fn required_sidecars_are_verified_and_imported_even_when_optional_extras_are_off() {
        for mode in ["copy", "hardlink", "move"] {
            let db = crate::test_support::in_memory_pool().await;
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("download/Show - 06.mkv");
            let subtitle = source.with_extension("eng.srt");
            let dest = temp.path().join("library/Show - S01E06.mkv");
            tokio::fs::create_dir_all(source.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(&source, "new video").await.unwrap();
            tokio::fs::write(&subtitle, "1\n00:00:01,000 --> 00:00:02,000\nHello\n")
                .await
                .unwrap();
            seed_probe(&db, &source, false).await;
            let cfg = Config {
                required_subtitle_languages: "en".to_string(),
                import_extra_files: false,
                ..Config::default()
            };
            import_required(
                &db,
                &cfg,
                mode,
                &source,
                &dest,
                &[],
                1,
                "Show",
                std::slice::from_ref(&subtitle),
                Some(EpisodeSpan::single(None, 6)),
                12,
                false,
            )
            .await
            .unwrap();
            assert_eq!(tokio::fs::read_to_string(&dest).await.unwrap(), "new video");
            assert!(dest.with_extension("en.srt").exists());
            assert_eq!(source.exists(), mode != "move");
            assert_eq!(subtitle.exists(), mode != "move");
        }
    }

    #[tokio::test]
    async fn failed_sidecar_commit_restores_old_bundle_with_recycle_disabled() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("download/Show - 06.mkv");
        let subtitle = source.with_extension("en.srt");
        let old = temp.path().join("library/Show - S01E06.mkv");
        let old_sub = old.with_extension("en.srt");
        let old_nfo = old.with_extension("nfo");
        tokio::fs::create_dir_all(source.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::create_dir_all(old.parent().unwrap())
            .await
            .unwrap();
        for (path, bytes) in [
            (&source, "new video"),
            (&subtitle, "1\n00:00:01,000 --> 00:00:02,000\nHello\n"),
            (&old, "old video"),
            (&old_sub, "old subtitles"),
            (&old_nfo, "old metadata"),
        ] {
            tokio::fs::write(path, bytes).await.unwrap();
        }
        seed_probe(&db, &source, false).await;
        let cfg = Config {
            required_subtitle_languages: "en".to_string(),
            ..Config::default()
        };
        let mut transaction = prepare(
            &db,
            &cfg,
            &source,
            &old,
            &[subtitle],
            Some(EpisodeSpan::single(None, 6)),
            1,
        )
        .await
        .unwrap()
        .unwrap();
        transaction
            .snapshot(std::slice::from_ref(&old))
            .await
            .unwrap();
        let landing = super::super::staging_path(&old);
        do_file_op("copy", &source, &landing).await.unwrap();
        transaction.check_placement(&landing).await.unwrap();
        recycle::recycle(
            &db,
            "",
            recycle::RecycleKind::Episode,
            Some(1),
            "Show",
            &old,
        )
        .await
        .unwrap();
        tokio::fs::rename(&landing, &old).await.unwrap();
        // Deterministically fail the final subtitle rename after retire.
        tokio::fs::remove_file(&transaction.sidecars[0].0)
            .await
            .unwrap();
        assert!(transaction.commit_sidecars().await.is_err());
        transaction.rollback(true).await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&old).await.unwrap(), "old video");
        assert_eq!(
            tokio::fs::read_to_string(&old_sub).await.unwrap(),
            "old subtitles"
        );
        assert_eq!(
            tokio::fs::read_to_string(&old_nfo).await.unwrap(),
            "old metadata"
        );
        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "new video"
        );
    }

    #[tokio::test]
    async fn empty_or_invalid_sidecars_cannot_satisfy_import_requirements() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("Show - 06.mkv");
        let subtitle = source.with_extension("en.ass");
        tokio::fs::write(&source, "video").await.unwrap();
        tokio::fs::write(&subtitle, "[Script Info]\nTitle: no dialogue\n[Events]")
            .await
            .unwrap();
        seed_probe(&db, &source, false).await;
        let cfg = Config {
            required_subtitle_languages: "en".to_string(),
            ..Config::default()
        };
        let dest = temp.path().join("library/Show - 06.mkv");
        assert!(
            import_required(
                &db,
                &cfg,
                "copy",
                &source,
                &dest,
                &[],
                1,
                "Show",
                &[subtitle],
                Some(EpisodeSpan::single(None, 6)),
                1,
                false,
            )
            .await
            .is_err()
        );
        assert!(!dest.exists());
    }
    #[tokio::test]
    async fn minimum_resolution_rejection_preserves_source_and_existing_files() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("new.mkv");
        let old = temp.path().join("old.mkv");
        tokio::fs::write(&source, "source video").await.unwrap();
        tokio::fs::write(&old, "existing video").await.unwrap();
        seed_probe_dimensions(&db, &source, true, 854, 480).await;
        let cfg = Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            ..Default::default()
        };
        let error = import_required(
            &db,
            &cfg,
            "move",
            &source,
            &old,
            std::slice::from_ref(&old),
            1,
            "Show",
            &[],
            None,
            1,
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("below the 720p minimum"));
        assert_eq!(
            tokio::fs::read_to_string(&old).await.unwrap(),
            "existing video"
        );
        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "source video"
        );
    }

    #[tokio::test]
    async fn actual_upgrade_gates_keep_target_met_and_non_improving_files() {
        for (old_resolution, old_subs, new_resolution, new_subs, should_replace) in [
            (720, true, 1080, true, true),
            (1080, true, 2160, true, false),
            (1080, false, 1080, false, false),
            (1080, false, 720, true, false),
            (720, true, 1080, false, false),
            (1080, false, 1080, true, true),
        ] {
            let db = crate::test_support::in_memory_pool().await;
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("new.mkv");
            let dest = temp.path().join("old.mkv");
            tokio::fs::write(&source, "new video data").await.unwrap();
            tokio::fs::write(&dest, "old video data").await.unwrap();
            let width = |height| match height {
                2160 => 3840,
                1080 => 1920,
                _ => 1280,
            };
            seed_probe_dimensions(
                &db,
                &source,
                new_subs,
                width(new_resolution),
                new_resolution,
            )
            .await;
            seed_probe_dimensions(&db, &dest, old_subs, width(old_resolution), old_resolution)
                .await;
            let cfg = Config {
                media_requirements_enabled: true,
                minimum_subtitle_check: false,
                required_subtitle_languages: "en".into(),
                ..Default::default()
            };
            let result = import_required(
                &db,
                &cfg,
                "copy",
                &source,
                &dest,
                std::slice::from_ref(&dest),
                1,
                "Show",
                &[],
                None,
                1,
                true,
            )
            .await;
            assert_eq!(
                result.is_ok(),
                should_replace,
                "{old_resolution}/{old_subs} -> {new_resolution}/{new_subs}: {result:?}"
            );
            assert_eq!(
                tokio::fs::read_to_string(&dest).await.unwrap(),
                if should_replace {
                    "new video data"
                } else {
                    "old video data"
                }
            );
            assert!(source.exists());
        }
    }

    #[tokio::test]
    async fn explicit_unverified_minimum_accepts_fallback_without_fabricating_subtitles() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.mkv");
        let dest = temp.path().join("library/episode.mkv");
        tokio::fs::write(&source, "video with unverified captions")
            .await
            .unwrap();
        seed_probe(&db, &source, false).await;
        let cfg = Config {
            media_requirements_enabled: true,
            minimum_subtitle_check: false,
            required_subtitle_languages: "es".into(),
            ..Default::default()
        };
        import_required(
            &db,
            &cfg,
            "copy",
            &source,
            &dest,
            &[],
            1,
            "Show",
            &[],
            None,
            1,
            true,
        )
        .await
        .unwrap();
        assert!(dest.exists());
        assert!(!dest.with_extension("es.srt").exists());
        let info = media_info::inspect(&db, &source).await.unwrap();
        let policy = crate::services::media_requirements::Policy::from_config(&cfg);
        assert!(policy.minimum_met(&policy.observe(&info)));
        assert!(!policy.target_met(&policy.observe(&info)));
        assert!(info.subtitles.is_empty());
    }

    #[tokio::test]
    async fn explicit_manual_replacement_obeys_minimum_without_automatic_target_stop() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("new.mkv");
        let dest = temp.path().join("old.mkv");
        tokio::fs::write(&source, "new video").await.unwrap();
        tokio::fs::write(&dest, "old video").await.unwrap();
        seed_probe_dimensions(&db, &source, true, 1920, 1080).await;
        seed_probe_dimensions(&db, &dest, true, 1920, 1080).await;
        let cfg = Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            ..Default::default()
        };
        import_required(
            &db,
            &cfg,
            "copy",
            &source,
            &dest,
            std::slice::from_ref(&dest),
            1,
            "Show",
            &[],
            None,
            1,
            false,
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read_to_string(dest).await.unwrap(), "new video");
    }
}

//! Configurable acceptance floor and an observed, finite automatic-upgrade target.
//!
//! Release labels are prospective claims. Only local media facts can satisfy
//! the target or authorize retirement of an existing file.
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    media,
    media_info::{self, MediaInfo},
    source::{ClassificationResult, Resolution, UpgradeKind, UpgradeRejection},
};
use crate::models::config::Config;

#[derive(Debug, Clone)]
pub struct Policy {
    pub enabled: bool,
    pub minimum_resolution: Resolution,
    pub target_resolution: Resolution,
    pub minimum_subtitle_check: bool,
    pub target_subtitle_check: bool,
    pub required_languages: Vec<String>,
    pub language_match: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Observed {
    pub resolution: Resolution,
    pub subtitles_satisfied: bool,
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub minimum_met: bool,
    pub target_met: bool,
    pub resolution: &'static str,
    pub minimum_resolution: &'static str,
    pub target_resolution: &'static str,
    pub selectable_subtitles_satisfied: bool,
    pub minimum_subtitle_check: bool,
    pub target_subtitle_check: bool,
    pub reasons: Vec<String>,
}

impl Policy {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.media_requirements_enabled,
            minimum_resolution: Resolution::from_str(&cfg.minimum_resolution),
            target_resolution: Resolution::from_str(&cfg.target_resolution),
            minimum_subtitle_check: cfg.minimum_subtitle_check,
            target_subtitle_check: cfg.target_subtitle_check,
            required_languages: cfg
                .required_subtitle_languages
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            language_match: cfg.subtitle_language_match.clone(),
        }
    }

    pub fn observe(&self, info: &MediaInfo) -> Observed {
        // The parser excludes attached cover art. Use the primary/default
        // video, rather than allowing a secondary stream to satisfy a gate.
        let video = info
            .video
            .iter()
            .find(|v| v.default)
            .or_else(|| info.video.first());
        let resolution = video
            .map(|v| {
                Resolution::from_observed_dimensions(v.width.unwrap_or(0), v.height.unwrap_or(0))
            })
            .unwrap_or(Resolution::Unknown);
        let subtitles = media_info::evaluate_subtitle_requirements(
            info,
            &self.required_languages,
            &self.language_match,
        );
        Observed {
            resolution,
            subtitles_satisfied: subtitles.satisfied,
            duration_seconds: info.duration_seconds.filter(|d| d.is_finite() && *d > 0.0),
        }
    }

    fn valid(&self) -> bool {
        self.minimum_resolution != Resolution::Unknown
            && self.target_resolution != Resolution::Unknown
            && self.target_resolution.rank() >= self.minimum_resolution.rank()
    }

    pub fn minimum_met(&self, observed: &Observed) -> bool {
        self.valid()
            && observed.resolution != Resolution::Unknown
            && observed.resolution.rank() >= self.minimum_resolution.rank()
            && (!self.minimum_subtitle_check || observed.subtitles_satisfied)
    }

    pub fn target_met(&self, observed: &Observed) -> bool {
        self.valid()
            && observed.resolution != Resolution::Unknown
            && observed.resolution.rank() >= self.target_resolution.rank()
            && (!(self.target_subtitle_check || self.minimum_subtitle_check)
                || observed.subtitles_satisfied)
    }

    pub fn status(&self, info: &MediaInfo) -> Status {
        let observed = self.observe(info);
        let mut reasons = Vec::new();
        if observed.resolution == Resolution::Unknown {
            reasons.push("Video resolution could not be verified.".into());
        } else if observed.resolution.rank() < self.minimum_resolution.rank() {
            reasons.push(format!(
                "Video is below the {} minimum.",
                self.minimum_resolution.as_str()
            ));
        } else if observed.resolution.rank() < self.target_resolution.rank() {
            reasons.push(format!(
                "Video is below the {} upgrade target.",
                self.target_resolution.as_str()
            ));
        }
        if !observed.subtitles_satisfied
            && (self.minimum_subtitle_check || self.target_subtitle_check)
        {
            reasons.push("Required selectable subtitle languages could not be verified. Burned-in captions are not detectable by this check.".into());
        }
        Status {
            enabled: self.enabled,
            minimum_met: self.minimum_met(&observed),
            target_met: self.target_met(&observed),
            resolution: observed.resolution.as_str(),
            minimum_resolution: self.minimum_resolution.as_str(),
            target_resolution: self.target_resolution.as_str(),
            selectable_subtitles_satisfied: observed.subtitles_satisfied,
            minimum_subtitle_check: self.minimum_subtitle_check,
            target_subtitle_check: self.target_subtitle_check,
            reasons,
        }
    }

    pub fn validate_minimum(&self, info: &MediaInfo) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !self.valid() {
            return Err(
                "Media requirement: choose a valid minimum and good enough target in Settings."
                    .into(),
            );
        }
        let observed = self.observe(info);
        if observed.resolution == Resolution::Unknown {
            return Err("Media requirement: video resolution could not be verified.".into());
        }
        if observed.resolution.rank() < self.minimum_resolution.rank() {
            return Err(format!(
                "Media requirement: {} is below the {} minimum.",
                observed.resolution.as_str(),
                self.minimum_resolution.as_str()
            ));
        }
        if self.minimum_subtitle_check && !observed.subtitles_satisfied {
            return Err(
                "Media requirement: required selectable subtitle languages could not be verified."
                    .into(),
            );
        }
        Ok(())
    }

    /// Only a successfully inspected file that fails an active content rule
    /// justifies searching another release. Invalid settings and unknown
    /// video facts must stay available for review instead of causing retries.
    pub fn minimum_failure_is_content_rejection(&self, info: &MediaInfo) -> bool {
        if !self.enabled
            || !self.valid()
            || ((self.minimum_subtitle_check || self.target_subtitle_check)
                && (!matches!(self.language_match.as_str(), "all" | "any")
                    || self
                        .required_languages
                        .iter()
                        .any(|language| media_info::normalize_language(language).is_none())))
        {
            return false;
        }
        let observed = self.observe(info);
        if observed.resolution == Resolution::Unknown {
            return false;
        }
        if observed.resolution.rank() < self.minimum_resolution.rank() {
            return true;
        }
        // Sidecar inspection can report an unreadable/changing file as an
        // invalid stream rather than returning an error. That is missing
        // evidence, not proof that this release lacks the required language.
        // Only known text-content failures justify rejecting the release.
        let uncertain_subtitles = info.subtitles.iter().any(|stream| {
            !stream.valid
                && !matches!(
                    stream.validation_status.as_str(),
                    "invalid_encoding" | "empty_subtitle" | "no_valid_timed_text" | "empty_track"
                )
        });
        self.minimum_subtitle_check && !observed.subtitles_satisfied && !uncertain_subtitles
    }

    /// A candidate can justify a download, but its title cannot prove its
    /// subtitles. `validate_replacement` rechecks the downloaded file.
    pub fn judge_candidate(
        &self,
        existing: &Observed,
        incoming: &ClassificationResult,
        title: &str,
    ) -> Result<UpgradeKind, UpgradeRejection> {
        if !self.valid() {
            return Err(UpgradeRejection::RequirementsMinimum);
        }
        if existing.resolution == Resolution::Unknown {
            return Err(UpgradeRejection::RequirementsUnverified);
        }
        if self.target_met(existing) {
            return Err(UpgradeRejection::RequirementsTargetMet);
        }
        if incoming.resolution == Resolution::Unknown
            || incoming.resolution.rank() < self.minimum_resolution.rank()
        {
            return Err(UpgradeRejection::RequirementsMinimum);
        }
        if incoming.resolution.rank() < existing.resolution.rank() {
            return Err(UpgradeRejection::RequirementsDowngrade);
        }
        let resolution_gain = existing.resolution.rank() < self.target_resolution.rank()
            && incoming.resolution.rank() > existing.resolution.rank();
        let title = title.to_ascii_lowercase();
        let claims_hardsub = ["hardsub", "hard sub", "hard-sub", "burned-in", "burnt-in"]
            .iter()
            .any(|word| title.contains(word));
        let possible_subtitle_gain = (self.target_subtitle_check || self.minimum_subtitle_check)
            && !self.required_languages.is_empty()
            && !existing.subtitles_satisfied
            && !claims_hardsub;
        if resolution_gain || possible_subtitle_gain {
            Ok(UpgradeKind::Requirements)
        } else {
            Err(UpgradeRejection::RequirementsNoImprovement)
        }
    }

    /// Preserve known capabilities while improving at least one unmet target.
    pub fn validate_replacement(&self, old: &Observed, new: &Observed) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if old.resolution == Resolution::Unknown {
            return Err(UpgradeRejection::RequirementsUnverified.message());
        }
        if self.target_met(old) {
            return Err(UpgradeRejection::RequirementsTargetMet.message());
        }
        // A resolution/language improvement cannot establish that a much
        // shorter cut still contains the old episode. Ignore small timing
        // changes from credits or disclaimers; hold substantial losses for
        // review. An explicit interactive replacement can still choose a
        // different edition deliberately.
        if let (Some(before), Some(after)) = (old.duration_seconds, new.duration_seconds)
            && before.is_finite()
            && after.is_finite()
            && before > 0.0
            && after > 0.0
            && before - after > 300.0
            && after < before * 0.8
        {
            return Err(format!(
                "Replacement is substantially shorter ({after:.0}s versus {before:.0}s); review the edition before replacing it automatically"
            ));
        }
        if new.resolution == Resolution::Unknown
            || new.resolution.rank() < old.resolution.rank()
            || (old.subtitles_satisfied
                && !new.subtitles_satisfied
                && !self.required_languages.is_empty())
        {
            return Err(UpgradeRejection::RequirementsDowngrade.message());
        }
        let resolution_gain = old.resolution.rank() < self.target_resolution.rank()
            && new.resolution.rank() > old.resolution.rank();
        let subtitle_gain = (self.target_subtitle_check || self.minimum_subtitle_check)
            && !old.subtitles_satisfied
            && new.subtitles_satisfied;
        if !resolution_gain && !subtitle_gain {
            return Err(UpgradeRejection::RequirementsNoImprovement.message());
        }
        Ok(())
    }
}

/// Read actual library files once per series. An unavailable, escaped or
/// unreadable file has no observation and cannot be replaced automatically.
pub async fn observe_files(
    db: &SqlitePool,
    cfg: &Config,
    folder_name: &str,
    files: &[media::EpisodeFile],
) -> HashMap<i32, Observed> {
    let policy = Policy::from_config(cfg);
    if !policy.enabled {
        return HashMap::new();
    }
    let Ok(root) = tokio::fs::canonicalize(&cfg.media_root).await else {
        return HashMap::new();
    };
    let Ok(folder) = tokio::fs::canonicalize(root.join(folder_name)).await else {
        return HashMap::new();
    };
    if folder == root || !folder.starts_with(&root) {
        return HashMap::new();
    }
    let mut observed = HashMap::new();
    let mut unavailable = HashSet::new();
    let mut seen = HashSet::new();
    for file in files {
        if !file.is_own_season() || file.is_special {
            continue;
        }
        let episodes = file.episode_number..=file.episode_last.max(file.episode_number);
        // Multiple files for one slot need an explicit choice. Combining
        // their facts could manufacture a lower resolution or subtitle gain.
        for ep in episodes.clone() {
            if !seen.insert(ep) {
                unavailable.insert(ep);
            }
        }
        let path = match tokio::fs::canonicalize(folder.join(&file.filename)).await {
            Ok(path) if path.starts_with(&folder) => path,
            _ => {
                unavailable.extend(episodes);
                continue;
            }
        };
        match media_info::inspect(db, Path::new(&path)).await {
            Ok(info) => {
                let value = policy.observe(&info);
                for ep in episodes {
                    observed
                        .entry(ep)
                        .and_modify(|old: &mut Observed| {
                            if value.resolution.rank() < old.resolution.rank() {
                                old.resolution = value.resolution;
                            }
                            old.subtitles_satisfied &= value.subtitles_satisfied;
                        })
                        .or_insert(value);
                }
            }
            Err(_) => {
                unavailable.extend(episodes);
            }
        }
    }
    observed.retain(|ep, _| !unavailable.contains(ep));
    observed
}

pub struct SettingsValues {
    pub enabled: bool,
    pub minimum_resolution: String,
    pub target_resolution: String,
    pub minimum_subtitle_check: bool,
    pub target_subtitle_check: bool,
}

/// Optional values preserve old/stale forms. Invalid input never downgrades a
/// user's requirement or partially saves other quality preferences.
pub fn settings_values(
    cfg: &Config,
    enabled: Option<&str>,
    minimum: Option<&str>,
    target: Option<&str>,
    minimum_subtitles: Option<&str>,
    target_subtitles: Option<&str>,
) -> Result<SettingsValues, String> {
    fn flag(raw: Option<&str>, stored: bool) -> Result<bool, String> {
        match raw {
            None => Ok(stored),
            Some("1") => Ok(true),
            Some("0") => Ok(false),
            _ => Err("Choose a valid requirements setting.".into()),
        }
    }
    fn resolution(raw: Option<&str>, stored: &str) -> Result<String, String> {
        let value = raw.unwrap_or(stored);
        if !["480", "576", "720", "1080", "2160"].contains(&value) {
            return Err("Choose a supported minimum and target resolution.".into());
        }
        Ok(value.into())
    }
    let minimum_resolution = resolution(minimum, &cfg.minimum_resolution)?;
    let target_resolution = resolution(target, &cfg.target_resolution)?;
    if Resolution::from_str(&target_resolution).rank()
        < Resolution::from_str(&minimum_resolution).rank()
    {
        return Err("The good enough target must be at least the minimum resolution.".into());
    }
    Ok(SettingsValues {
        enabled: flag(enabled, cfg.media_requirements_enabled)?,
        minimum_resolution,
        target_resolution,
        minimum_subtitle_check: flag(minimum_subtitles, cfg.minimum_subtitle_check)?,
        target_subtitle_check: flag(target_subtitles, cfg.target_subtitle_check)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy::from_config(&Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            ..Default::default()
        })
    }
    fn observed(resolution: Resolution, subtitles_satisfied: bool) -> Observed {
        Observed {
            resolution,
            subtitles_satisfied,
            duration_seconds: None,
        }
    }
    #[test]
    fn minimum_and_target_are_separate() {
        let mut p = policy();
        p.minimum_subtitle_check = false;
        let fallback = observed(Resolution::R720p, false);
        assert!(p.minimum_met(&fallback));
        assert!(!p.target_met(&fallback));
        assert!(!p.target_met(&observed(Resolution::R1080p, false)));
        assert!(p.target_met(&observed(Resolution::R1080p, true)));
        assert!(p.target_met(&observed(Resolution::R2160p, true)));
    }
    #[test]
    fn completed_target_stops_revisions_sources_and_score_churn() {
        let p = policy();
        let mut candidate = super::super::source::classify_release_sync(
            "[Group] Title 01 [2160p BluRay Remux v3].mkv",
            None,
        );
        candidate.resolution = Resolution::R2160p;
        assert_eq!(
            p.judge_candidate(&observed(Resolution::R1080p, true), &candidate, "v3 remux"),
            Err(UpgradeRejection::RequirementsTargetMet)
        );
    }
    #[test]
    fn actual_replacement_must_improve_without_losing_capabilities() {
        let p = policy();
        assert!(
            p.validate_replacement(
                &observed(Resolution::R720p, true),
                &observed(Resolution::R1080p, true)
            )
            .is_ok()
        );
        assert!(
            p.validate_replacement(
                &observed(Resolution::R720p, true),
                &observed(Resolution::R1080p, false)
            )
            .is_err()
        );
        assert!(
            p.validate_replacement(
                &observed(Resolution::R1080p, false),
                &observed(Resolution::R720p, true)
            )
            .is_err()
        );
        assert!(
            p.validate_replacement(
                &observed(Resolution::R1080p, false),
                &observed(Resolution::R1080p, true)
            )
            .is_ok()
        );
        assert!(
            p.validate_replacement(
                &observed(Resolution::R1080p, false),
                &observed(Resolution::R1080p, false)
            )
            .is_err()
        );
    }

    #[test]
    fn automatic_replacement_holds_a_shorter_cut_but_allows_normal_timing_changes() {
        let p = policy();
        let mut old = observed(Resolution::R720p, true);
        old.duration_seconds = Some(1340.694);
        let mut new = observed(Resolution::R1080p, true);
        new.duration_seconds = Some(720.590);
        assert!(
            p.validate_replacement(&old, &new)
                .unwrap_err()
                .contains("review the edition")
        );

        // Credits, disclaimers and container timing must not stop a valid
        // upgrade. The guard requires a large absolute and relative loss.
        for duration in [1369.660, 1300.0, 1150.0, 1500.0] {
            new.duration_seconds = Some(duration);
            assert!(p.validate_replacement(&old, &new).is_ok(), "{duration}");
        }
        old.duration_seconds = Some(3000.0);
        new.duration_seconds = Some(2670.0);
        assert!(p.validate_replacement(&old, &new).is_ok());
    }

    #[test]
    fn missing_or_invalid_runtime_is_not_evidence_of_a_shorter_cut() {
        let p = policy();
        let mut old = observed(Resolution::R720p, true);
        old.duration_seconds = Some(1400.0);
        for duration in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
        ] {
            let mut new = observed(Resolution::R1080p, true);
            new.duration_seconds = duration;
            assert!(p.validate_replacement(&old, &new).is_ok());
        }
    }

    #[test]
    fn only_verified_minimum_content_failures_allow_another_candidate() {
        let identity = || media_info::FileIdentity {
            size: 1,
            mtime_ns: "1".into(),
            device: None,
            inode: None,
        };
        let info = |width, height, subtitle: Option<&str>| {
            let mut streams = vec![serde_json::json!({
                "index": 0, "codec_type": "video", "width": width, "height": height,
            })];
            if let Some(language) = subtitle {
                streams.push(serde_json::json!({
                    "index": 1, "codec_type": "subtitle", "codec_name": "ass",
                    "tags": {"language": language},
                }));
            }
            media_info::parse_probe_json(
                &serde_json::json!({"streams": streams}).to_string(),
                identity(),
                false,
            )
            .unwrap()
        };
        let p = policy();
        assert!(p.minimum_failure_is_content_rejection(&info(1280, 720, None)));
        assert!(p.minimum_failure_is_content_rejection(&info(854, 480, Some("eng"))));
        assert!(!p.minimum_failure_is_content_rejection(&info(1920, 1080, Some("eng"))));
        assert!(!p.minimum_failure_is_content_rejection(&info(0, 0, None)));
        let mut invalid = p.clone();
        invalid.target_resolution = Resolution::Unknown;
        assert!(!invalid.minimum_failure_is_content_rejection(&info(1280, 720, None)));
        invalid = p.clone();
        invalid.language_match = "restored-invalid-mode".into();
        assert!(!invalid.minimum_failure_is_content_rejection(&info(1280, 720, None)));
        invalid = p.clone();
        invalid.required_languages = vec!["invalid-language".into()];
        assert!(!invalid.minimum_failure_is_content_rejection(&info(1280, 720, None)));
        invalid = p.clone();
        invalid.enabled = false;
        assert!(!invalid.minimum_failure_is_content_rejection(&info(1280, 720, None)));

        let mut sidecar = info(1920, 1080, Some("eng"));
        sidecar.subtitles[0].external = true;
        sidecar.subtitles[0].valid = false;
        for status in [
            "unreadable",
            "inspection_timeout",
            "file_changed",
            "validation_unavailable",
            "size_limit_exceeded",
            "unsupported_format",
            "future-unknown-status",
        ] {
            sidecar.subtitles[0].validation_status = status.into();
            assert!(
                !p.minimum_failure_is_content_rejection(&sidecar),
                "{status}"
            );
        }
        for status in [
            "invalid_encoding",
            "empty_subtitle",
            "no_valid_timed_text",
            "empty_track",
        ] {
            sidecar.subtitles[0].validation_status = status.into();
            assert!(p.minimum_failure_is_content_rejection(&sidecar), "{status}");
        }
    }
    #[test]
    fn explicit_hardsub_cannot_justify_a_subtitle_only_download() {
        let p = policy();
        let incoming =
            super::super::source::classify_release_sync("Title [1080p WEB-DL].mkv", None);
        assert!(
            p.judge_candidate(
                &observed(Resolution::R1080p, false),
                &incoming,
                "English Hardsub"
            )
            .is_err()
        );
        assert!(
            p.judge_candidate(
                &observed(Resolution::R1080p, false),
                &incoming,
                "English Softsub"
            )
            .is_ok()
        );
        assert!(
            p.judge_candidate(&observed(Resolution::Unknown, false), &incoming, "Softsub")
                .is_err()
        );
    }
    #[test]
    fn configurable_spanish_4k_and_all_any_are_not_english_1080_constants() {
        let p = Policy::from_config(&Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "es".into(),
            target_resolution: "2160".into(),
            ..Default::default()
        });
        assert_eq!(p.required_languages, ["es"]);
        assert!(!p.target_met(&observed(Resolution::R1080p, true)));
        assert!(p.target_met(&observed(Resolution::R2160p, true)));
    }
    #[test]
    fn old_forms_preserve_choices_and_invalid_target_is_rejected() {
        let cfg = Config {
            media_requirements_enabled: true,
            target_resolution: "2160".into(),
            ..Default::default()
        };
        let v = settings_values(&cfg, None, None, None, None, None).unwrap();
        assert!(v.enabled);
        assert_eq!(v.target_resolution, "2160");
        assert!(settings_values(&cfg, None, Some("1080"), Some("720"), None, None).is_err());
        assert!(settings_values(&cfg, Some("garbage"), None, None, None, None).is_err());
    }
    #[test]
    fn actual_spanish_tracks_and_language_match_control_a_4k_target() {
        let mut p = Policy::from_config(&Config {
            media_requirements_enabled: true,
            target_resolution: "2160".into(),
            required_subtitle_languages: "es,en".into(),
            ..Default::default()
        });
        let identity = media_info::FileIdentity {
            size: 1,
            mtime_ns: "1".into(),
            device: None,
            inode: None,
        };
        let info=media_info::parse_probe_json(r#"{"streams":[{"index":0,"codec_type":"video","width":3840,"height":1600},{"index":1,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"spa"}}]}"#,identity,false).unwrap();
        assert!(!p.target_met(&p.observe(&info)));
        p.language_match = "any".into();
        assert!(p.target_met(&p.observe(&info)));
        p.required_languages = vec!["en".into()];
        assert!(!p.target_met(&p.observe(&info)));
        assert_eq!(p.observe(&info).resolution, Resolution::R2160p);
    }

    #[tokio::test]
    async fn missing_overlapping_file_invalidates_an_otherwise_observed_episode() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("Show");
        tokio::fs::create_dir(&folder).await.unwrap();
        let video = folder.join("Show - S01E01.mkv");
        tokio::fs::write(&video, "video").await.unwrap();
        let identity =
            media_info::FileIdentity::from_metadata(&tokio::fs::metadata(&video).await.unwrap())
                .unwrap();
        crate::models::media_probe_cache::upsert_identified(&db,video.to_str().unwrap(),&identity.cache_key(),identity.size as i64,r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1280,"height":720}]}"#).await;
        let cfg = Config {
            media_requirements_enabled: true,
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let mut files = media::scan_series_folder(&cfg.media_root, "Show").await;
        assert_eq!(files.len(), 1);
        assert!(
            observe_files(&db, &cfg, "Show", &files)
                .await
                .contains_key(&1)
        );
        let mut missing = files[0].clone();
        missing.filename = "missing.mkv".into();
        files.push(missing);
        assert!(
            !observe_files(&db, &cfg, "Show", &files)
                .await
                .contains_key(&1)
        );
    }
    #[test]
    fn incomplete_or_zero_dimensions_never_satisfy_a_gate() {
        let p = policy();
        for dimensions in [
            serde_json::json!({"width":1920}),
            serde_json::json!({"height":1080}),
            serde_json::json!({"width":0,"height":1080}),
            serde_json::json!({"width":1920,"height":0}),
        ] {
            let mut video = dimensions;
            video["codec_type"] = "video".into();
            let json = serde_json::json!({"streams":[video]}).to_string();
            let info = media_info::parse_probe_json(
                &json,
                media_info::FileIdentity {
                    size: 1,
                    mtime_ns: "1".into(),
                    device: None,
                    inode: None,
                },
                false,
            )
            .unwrap();
            let value = p.observe(&info);
            assert_eq!(value.resolution, Resolution::Unknown);
            assert!(!p.minimum_met(&value));
            assert!(!p.target_met(&value));
        }
    }
    #[test]
    fn invalid_restored_policy_never_meets_a_gate_or_authorizes_a_candidate() {
        for (minimum, target) in [("bad", "1080"), ("720", "bad"), ("1080", "720")] {
            let p = Policy::from_config(&Config {
                media_requirements_enabled: true,
                minimum_resolution: minimum.into(),
                target_resolution: target.into(),
                ..Default::default()
            });
            let actual = observed(Resolution::R1080p, true);
            assert!(!p.minimum_met(&actual));
            assert!(!p.target_met(&actual));
            let candidate = super::super::source::classify_release_sync("Show [2160p]", None);
            assert!(p.judge_candidate(&actual, &candidate, "Show").is_err());
        }
    }
    #[tokio::test]
    async fn overlapping_valid_files_require_review_instead_of_combining_capabilities() {
        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("Show");
        tokio::fs::create_dir(&folder).await.unwrap();
        for (name, width, height, subtitles) in [
            ("Show - S01E01 [1080p].mkv", 1920, 1080, false),
            ("Show - S01E01 [720p].mkv", 1280, 720, true),
        ] {
            let video = folder.join(name);
            tokio::fs::write(&video, name).await.unwrap();
            let identity = media_info::FileIdentity::from_metadata(
                &tokio::fs::metadata(&video).await.unwrap(),
            )
            .unwrap();
            let mut streams =
                vec![serde_json::json!({"codec_type":"video","width":width,"height":height})];
            if subtitles {
                streams.push(serde_json::json!({"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng"}}));
            }
            crate::models::media_probe_cache::upsert_identified(
                &db,
                video.to_str().unwrap(),
                &identity.cache_key(),
                identity.size as i64,
                &serde_json::json!({"streams":streams}).to_string(),
            )
            .await;
        }
        let cfg = Config {
            media_requirements_enabled: true,
            required_subtitle_languages: "en".into(),
            media_root: temp.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let files = media::scan_series_folder(&cfg.media_root, "Show").await;
        assert_eq!(files.len(), 2);
        assert!(
            !observe_files(&db, &cfg, "Show", &files)
                .await
                .contains_key(&1)
        );
    }
}

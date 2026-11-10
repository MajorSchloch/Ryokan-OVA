//! Configurable acceptance floor and an observed, finite automatic-upgrade target.
//!
//! Release labels are prospective claims. Only local media facts can satisfy
//! the target or authorize retirement of an existing file.
use std::{collections::HashMap, path::Path};

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
                let width = v.width.unwrap_or(0);
                let height = v.height.unwrap_or(0);
                // Preserve the tier of widescreen crops (1920x800, 3840x1600).
                let by_width = match width {
                    3800.. => Resolution::R2160p,
                    1850.. => Resolution::R1080p,
                    1200.. => Resolution::R720p,
                    _ => Resolution::Unknown,
                };
                let by_height = Resolution::from_dimensions(width, height);
                if by_width.rank() > by_height.rank() {
                    by_width
                } else {
                    by_height
                }
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
        }
    }

    pub fn minimum_met(&self, observed: &Observed) -> bool {
        observed.resolution != Resolution::Unknown
            && observed.resolution.rank() >= self.minimum_resolution.rank()
            && (!self.minimum_subtitle_check || observed.subtitles_satisfied)
    }

    pub fn target_met(&self, observed: &Observed) -> bool {
        observed.resolution != Resolution::Unknown
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

    /// A candidate can justify a download, but its title cannot prove its
    /// subtitles. `validate_replacement` rechecks the downloaded file.
    pub fn judge_candidate(
        &self,
        existing: &Observed,
        incoming: &ClassificationResult,
        title: &str,
    ) -> Result<UpgradeKind, UpgradeRejection> {
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
    for file in files {
        if !file.is_own_season() || file.is_special {
            continue;
        }
        let Ok(path) = tokio::fs::canonicalize(folder.join(&file.filename)).await else {
            continue;
        };
        if !path.starts_with(&folder) {
            continue;
        }
        if let Ok(info) = media_info::inspect(db, Path::new(&path)).await {
            let value = policy.observe(&info);
            for ep in file.episode_number..=file.episode_last.max(file.episode_number) {
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
    }
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
}

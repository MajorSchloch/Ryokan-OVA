//! `SearchTarget` enum + target-set builders.
//!
//! A `SearchTarget` is the per-query granularity the search pipeline
//! works at: either "one entry" (movies, single-episode OVAs, anything
//! AniList marks as one-of) or "episode N" (regular TV). The builders
//! turn a series's on-disk state + monitoring config into the list of
//! targets to search for.

use std::collections::{HashMap, HashSet};

use crate::models::episode_tags::EpisodeQualityTag;
use crate::services::anilist::AnimeDetail;
use crate::services::media;
use crate::services::media_requirements::{Observed, Policy};
use crate::services::source::{self, ClassificationResult, Resolution, Source};

use super::resolve_existing_classification;

#[derive(Debug, Clone)]
pub enum SearchTarget {
    Single,
    Episode(i32),
}

impl SearchTarget {
    /// Build a search target for a user-initiated "search this episode"
    /// action. Collapses to `Single` only when the media is genuinely
    /// single-entry; otherwise stays as `Episode(n)`.
    ///
    /// This exists because the per-episode handlers used to pass
    /// `Episode(n)` unconditionally — and for movies, `matches_target`
    /// then rejected every real release on Nyaa (movie filenames don't
    /// carry episode numbers), leaving Phase 1 empty and triggering the
    /// extended-alias fallback with its looser matching. Collapsing to
    /// `Single` for single-entry media keeps the search on the correct
    /// code path and prevents the fallback from firing in the first
    /// place.
    ///
    /// Rules:
    /// - `MOVIE` → always `Single`. Movies are always single-entry; if
    ///   AniList reports something weird like `episodes: None` or
    ///   `Some(2)`, we still trust the format.
    /// - `SPECIAL` / `OVA` / `ONA` with `episodes == Some(1)` → `Single`.
    ///   These formats are single-entry *in the common case*, but
    ///   multi-episode OVAs (Hellsing Ultimate, LOGH) and multi-episode
    ///   ONAs absolutely exist and their releases DO carry episode
    ///   numbers, so only collapse when AniList explicitly confirms a
    ///   single episode.
    /// - Everything else (TV, TV_SHORT, multi-episode OVA/ONA/SPECIAL,
    ///   or unknown episode count) → `Episode(n)`. TV releases carry
    ///   episode numbers, and for ambiguous formats with unknown episode
    ///   count the safe default is to keep `Episode(n)` — the failure
    ///   mode there is "no results" rather than "wrong series grabbed".
    pub fn for_episode(detail: &AnimeDetail, episode_number: i32) -> Self {
        match detail.format.as_str() {
            "MOVIE" => SearchTarget::Single,
            "SPECIAL" | "OVA" | "ONA" if detail.episodes == Some(1) => SearchTarget::Single,
            _ => SearchTarget::Episode(episode_number),
        }
    }
}

pub fn build_missing_targets(detail: &AnimeDetail, existing_episodes: &[i32]) -> Vec<SearchTarget> {
    if matches!(SearchTarget::for_episode(detail, 1), SearchTarget::Single) {
        return vec![SearchTarget::Single];
    }

    // An unknown total does not make an airing series single-entry media.
    // Without explicit monitored episodes this builder has no range to search.
    let total_eps = detail.episodes.unwrap_or(0);
    let existing: HashSet<i32> = existing_episodes.iter().copied().collect();
    let mut targets = Vec::new();
    for ep in 1..=total_eps.max(0) {
        if !existing.contains(&ep) {
            targets.push(SearchTarget::Episode(ep));
        }
    }
    targets
}

pub fn build_monitored_targets(
    detail: &AnimeDetail,
    existing_episodes: &[i32],
    monitored_episodes: &[i32],
) -> Vec<SearchTarget> {
    if matches!(SearchTarget::for_episode(detail, 1), SearchTarget::Single) {
        if monitored_episodes.is_empty() || monitored_episodes.contains(&1) {
            return vec![SearchTarget::Single];
        }
        return Vec::new();
    }

    let existing: HashSet<i32> = existing_episodes.iter().copied().collect();
    let mut monitored: Vec<i32> = monitored_episodes.to_vec();
    monitored.sort_unstable();
    monitored.dedup();

    monitored
        .into_iter()
        .filter(|ep| !existing.contains(ep))
        .map(SearchTarget::Episode)
        .collect()
}

/// Build upgrade targets: candidate episodes that exist on disk but are below
/// the quality cutoff. These are candidates for automatic quality upgrades.
///
/// Hydration order for each on-disk episode:
/// 1. Structured classification columns on `episode_quality_tags` (written
///    since Phase 1b).
/// 2. Legacy `release_title` column parsed via filename-only classification
///    (for rows grabbed before Phase 1b landed, where the structured cols
///    are empty).
/// 3. On-disk filename + `quality` string, also via filename-only
///    classification (for episodes that have no grab record at all — e.g.
///    pre-existing library files that Ryokan didn't grab itself).
pub fn build_upgrade_targets(
    disk_files: &[media::EpisodeFile],
    candidate_episodes: &[i32],
    cutoff_source: Source,
    cutoff_resolution: Resolution,
    cutoff_is_remux: bool,
    cutoff_is_bdmv: bool,
    quality_tags: &HashMap<i32, EpisodeQualityTag>,
) -> Vec<(SearchTarget, ClassificationResult)> {
    let candidates: HashSet<i32> = candidate_episodes.iter().copied().collect();
    let cutoff = source::cutoff_classification(
        cutoff_source,
        cutoff_resolution,
        cutoff_is_remux,
        cutoff_is_bdmv,
    );
    let cutoff_rank = cutoff.rank();

    let mut targets = Vec::new();
    for file in disk_files {
        if !candidates.contains(&file.episode_number) {
            continue;
        }
        // A multi-episode file (issue #246) is never an upgrade target:
        // a single-episode release replacing `S01E05-E06` would take
        // the other episode with it (the import retires every file
        // whose span overlaps), and the search targets are single
        // episodes. Sonarr's "same episodes" rule; a release covering
        // the exact span would be the way in, and nothing searches for
        // one yet.
        if file.is_multi_episode() {
            continue;
        }
        // manual_override pins must short-circuit before find_best_for_target
        // runs. The downstream SQL guards on record_grab /
        // update_classification drop the tag write, but post-processing has
        // already replaced the on-disk file by the time those guards fire.
        if quality_tags
            .get(&file.episode_number)
            .is_some_and(|t| t.manual_override)
        {
            continue;
        }
        // An upgrade already downloading (the tag row is `grabbed`) is
        // not searched for again: the Wanted cutoff tab hides such a
        // slot, and its "Search all" and the daily sweep used to target
        // it anyway and grab a second release for the same episode.
        if quality_tags
            .get(&file.episode_number)
            .is_some_and(|t| t.state == "grabbed")
        {
            continue;
        }
        let existing =
            resolve_existing_classification(file, quality_tags.get(&file.episode_number));
        // Skip completely unclassified episodes — we have no way to know
        // whether an incoming release would actually be an upgrade.
        if existing.source == Source::Unknown && existing.resolution == Resolution::Unknown {
            continue;
        }
        if existing.rank() < cutoff_rank {
            targets.push((SearchTarget::Episode(file.episode_number), existing));
        }
    }
    targets.sort_by_key(|(t, _)| match t {
        SearchTarget::Episode(n) => *n,
        SearchTarget::Single => 0,
    });
    targets
}

/// Requirements mode uses observed file capabilities, never release-title
/// source/CF rank. Unknown files and overlapping ranges stay out of automatic
/// single-episode searches. All existing pin/downloading safeguards apply.
pub fn build_requirements_upgrade_targets(
    disk_files: &[media::EpisodeFile],
    candidate_episodes: &[i32],
    quality_tags: &HashMap<i32, EpisodeQualityTag>,
    policy: &Policy,
    observed: &HashMap<i32, Observed>,
) -> Vec<(SearchTarget, ClassificationResult)> {
    let candidates: HashSet<i32> = candidate_episodes.iter().copied().collect();
    let ranged: HashSet<i32> = disk_files
        .iter()
        .filter(|file| file.is_multi_episode())
        .flat_map(|file| file.episodes())
        .collect();
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for file in disk_files {
        let ep = file.episode_number;
        if !file.is_own_season()
            || file.is_special
            || !candidates.contains(&ep)
            || ranged.contains(&ep)
            || !seen.insert(ep)
        {
            continue;
        }
        if quality_tags
            .get(&ep)
            .is_some_and(|tag| tag.manual_override || tag.state == "grabbed")
        {
            continue;
        }
        let Some(facts) = observed
            .get(&ep)
            .filter(|facts| facts.resolution != Resolution::Unknown && !policy.target_met(facts))
        else {
            continue;
        };
        let mut existing = resolve_existing_classification(file, quality_tags.get(&ep));
        existing.resolution = facts.resolution;
        targets.push((SearchTarget::Episode(ep), existing));
    }
    targets.sort_by_key(|(target, _)| match target {
        SearchTarget::Episode(ep) => *ep,
        SearchTarget::Single => 0,
    });
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_video_classification_matches_requirements_and_target_stop() {
        use crate::services::{media_info, source_ffprobe};

        let policy = Policy::from_config(&crate::models::config::Config {
            media_requirements_enabled: true,
            target_resolution: "1080".into(),
            required_subtitle_languages: "en".into(),
            ..Default::default()
        });
        let fixture =
            include_str!("../../../tests/fixtures/ffprobe/beast-tamer-e01-primary-video.json");
        for (main_width, main_height, auxiliary_width, auxiliary_height, expected, target_met) in [
            (1920, 1080, 400, 564, Resolution::R1080p, true),
            // A higher-resolution auxiliary stream cannot satisfy the target.
            (1280, 720, 3840, 2160, Resolution::R720p, false),
        ] {
            let mut probe: serde_json::Value = serde_json::from_str(fixture).unwrap();
            probe["streams"][0]["width"] = main_width.into();
            probe["streams"][0]["height"] = main_height.into();
            probe["streams"][5]["width"] = auxiliary_width.into();
            probe["streams"][5]["height"] = auxiliary_height.into();
            let json = probe.to_string();
            let classified = source_ffprobe::scan_ffprobe_json(&json);
            let info = media_info::parse_probe_json(
                &json,
                media_info::FileIdentity {
                    size: 1,
                    mtime_ns: "1".into(),
                    device: None,
                    inode: None,
                },
                true,
            )
            .unwrap();
            let observed = policy.observe(&info);
            assert_eq!(classified.resolution, Some(expected));
            assert_eq!(observed.resolution, expected);
            assert!(observed.subtitles_satisfied);
            assert_eq!(policy.target_met(&observed), target_met);
            let targets = build_requirements_upgrade_targets(
                &[requirement_file(1)],
                &[1],
                &HashMap::new(),
                &policy,
                &HashMap::from([(1, observed)]),
            );
            assert_eq!(targets.is_empty(), target_met);
            if !target_met {
                assert!(matches!(targets[0].0, SearchTarget::Episode(1)));
                assert_eq!(targets[0].1.resolution, Resolution::R720p);
            }
        }
    }

    #[tokio::test]
    async fn cropped_hd_files_stop_at_target_despite_stale_tags() {
        use crate::models::{episode_tags, media_probe_cache};
        use crate::services::{media_info, media_requirements, source_ffprobe};

        let db = crate::test_support::in_memory_pool().await;
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("Show");
        std::fs::create_dir(&folder).unwrap();
        let series_id = crate::test_support::seed_series(&db, 1, "Show").await;
        let cfg = crate::models::config::Config {
            media_root: temp.path().to_str().unwrap().into(),
            media_requirements_enabled: true,
            minimum_resolution: "720".into(),
            target_resolution: "1080".into(),
            minimum_subtitle_check: true,
            target_subtitle_check: true,
            required_subtitle_languages: "en".into(),
            subtitle_language_match: "all".into(),
            ..Default::default()
        };
        let policy = Policy::from_config(&cfg);

        // E1 meets the target despite its old 720p label. E2 is really
        // 720p; E3 is cropped 1080p with signs-only English subtitles.
        for (ep, width, height, subtitle_title) in [
            (1, 1920, 816, "Full English"),
            (2, 1280, 720, "Full English"),
            (3, 1920, 816, "English Signs & Songs"),
        ] {
            let path = folder.join(format!("Show - S01E{ep:02} [1080p BluRay].mkv"));
            std::fs::write(&path, b"probe-cache fixture").unwrap();
            let canonical = std::fs::canonicalize(&path).unwrap();
            let meta = std::fs::metadata(&canonical).unwrap();
            let identity = media_info::FileIdentity::from_metadata(&meta).unwrap();
            let probe = serde_json::json!({
                "streams": [
                    {"codec_type":"video", "codec_name":"hevc", "width":width,
                     "height":height, "pix_fmt":"yuv420p10le", "disposition":{"default":1}},
                    {"codec_type":"audio", "codec_name":"flac", "tags":{"language":"jpn"}},
                    {"codec_type":"subtitle", "codec_name":"ass",
                     "tags":{"language":"eng", "title":subtitle_title},
                     "disposition":{"forced":0}}
                ],
                "format":{"duration":"2880.0"}
            });
            media_probe_cache::upsert_identified(
                &db,
                canonical.to_str().unwrap(),
                &identity.cache_key(),
                meta.len() as i64,
                &probe.to_string(),
            )
            .await;

            // Exercise the same cached-file classifier used after import.
            let classification = source::classify_post_download(
                &db,
                &canonical,
                Some(&folder),
                "[Group] Show (BD 1080p)",
                None,
                true,
            )
            .await;
            assert_eq!(
                classification.resolution,
                if ep == 2 {
                    Resolution::R720p
                } else {
                    Resolution::R1080p
                }
            );
            if ep == 1 {
                assert_eq!(classification.label(), "BD-1080p");
            }
            let mut stored = classification;
            if ep == 1 {
                stored.resolution = Resolution::R720p;
            }
            episode_tags::record_grab(
                &db,
                series_id,
                ep,
                &stored,
                "original release",
                "Group",
                1,
                true,
            )
            .await
            .unwrap();
            // The cache is a real identity-bound probe, not a hand-built Observed.
            assert!(
                source_ffprobe::classify_ffprobe(&db, &canonical)
                    .await
                    .resolution
                    .is_some()
            );
        }
        episode_tags::mark_completed(&db, series_id, &[1, 2, 3])
            .await
            .unwrap();
        let tags = episode_tags::get_for_series(&db, series_id).await.unwrap();
        assert_eq!(tags[&1].quality_tag, "BD-720p");
        let files = media::scan_series_folder(&cfg.media_root, "Show").await;
        let observed = media_requirements::observe_files(&db, &cfg, "Show", &files).await;
        assert!(policy.target_met(&observed[&1]));
        assert!(!policy.target_met(&observed[&2]));
        assert!(!policy.target_met(&observed[&3]));
        let targets =
            build_requirements_upgrade_targets(&files, &[1, 2, 3], &tags, &policy, &observed);
        let episodes: Vec<_> = targets
            .iter()
            .map(|(target, _)| match target {
                SearchTarget::Episode(ep) => *ep,
                SearchTarget::Single => panic!("expected episode target"),
            })
            .collect();
        assert_eq!(episodes, [2, 3]);
    }

    fn requirement_file(ep: i32) -> media::EpisodeFile {
        media::EpisodeFile {
            filename: format!("Show - S01E{ep:02}.mkv"),
            episode_number: ep,
            episode_last: ep,
            season_number: Some(1),
            quality: "BluRay-2160p".into(),
            size_bytes: 1,
            size_display: String::new(),
            modified_secs: None,
            is_special: false,
        }
    }

    #[test]
    fn requirement_targets_use_actual_capabilities_and_preserve_protections() {
        let cfg = crate::models::config::Config {
            media_requirements_enabled: true,
            ..Default::default()
        };
        let policy = Policy::from_config(&cfg);
        let mut files: Vec<_> = (1..=8).map(requirement_file).collect();
        // Any overlapping multi-episode file protects the whole span.
        let mut span = requirement_file(7);
        span.episode_last = 8;
        files.push(span);
        let facts = |resolution, subtitles_satisfied| Observed {
            resolution,
            subtitles_satisfied,
            duration_seconds: None,
        };
        let observations = HashMap::from([
            (1, facts(Resolution::R1080p, true)), // complete despite legacy source quality
            (2, facts(Resolution::R1080p, false)), // subtitle deficit
            (3, facts(Resolution::R720p, true)),  // actual resolution deficit
            (4, facts(Resolution::Unknown, false)),
            (5, facts(Resolution::R720p, false)),
            (6, facts(Resolution::R720p, false)),
            (7, facts(Resolution::R720p, false)),
            (8, facts(Resolution::R720p, false)),
        ]);
        let tag = |ep, manual_override, state: &str| EpisodeQualityTag {
            episode_number: ep,
            quality_tag: String::new(),
            release_title: String::new(),
            release_group: String::new(),
            state: state.into(),
            source: String::new(),
            resolution: String::new(),
            is_remux: false,
            is_bdmv: false,
            web_kind: String::new(),
            classification_confidence: 0.0,
            needs_review: false,
            manual_override,
            classification_evidence: String::new(),
            classification_attempted_at: None,
        };
        let tags = HashMap::from([(5, tag(5, true, "imported")), (6, tag(6, false, "grabbed"))]);
        let targets = build_requirements_upgrade_targets(
            &files,
            &(1..=8).collect::<Vec<_>>(),
            &tags,
            &policy,
            &observations,
        );
        let episodes: Vec<_> = targets
            .iter()
            .map(|(target, existing)| {
                if let SearchTarget::Episode(ep) = target {
                    assert_eq!(existing.resolution, observations[ep].resolution);
                    *ep
                } else {
                    panic!("expected episode")
                }
            })
            .collect();
        assert_eq!(episodes, [2, 3]);
        assert!(
            build_requirements_upgrade_targets(&files, &[2], &tags, &policy, &HashMap::new())
                .is_empty()
        );
    }

    fn detail_with(format: &str, episodes: Option<i32>) -> AnimeDetail {
        AnimeDetail {
            release_date: None,
            is_adult: false,
            id: 1,
            id_mal: None,
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: format.to_string(),
            status: String::new(),
            status_display: String::new(),
            episodes,
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    #[test]
    fn unknown_tv_total_searches_each_missing_monitored_episode() {
        // Currently airing shows can have an unknown final total while
        // monitoring already knows that episodes 1 and 2 have aired.
        // A generic Single target used to select episode 2 and leave 1 missing.
        let detail = detail_with("TV", None);
        assert!(matches!(
            build_monitored_targets(&detail, &[], &[2, 1, 2]).as_slice(),
            [SearchTarget::Episode(1), SearchTarget::Episode(2)]
        ));
        assert!(matches!(
            build_monitored_targets(&detail, &[2], &[1, 2]).as_slice(),
            [SearchTarget::Episode(1)]
        ));
        assert!(matches!(
            build_monitored_targets(&detail, &[1], &[1, 2]).as_slice(),
            [SearchTarget::Episode(2)]
        ));
        assert!(build_monitored_targets(&detail, &[1, 2], &[1, 2]).is_empty());
        assert!(build_monitored_targets(&detail, &[], &[]).is_empty());
    }

    #[test]
    fn unknown_episode_total_does_not_create_a_generic_missing_target() {
        for format in ["TV", "TV_SHORT", "OVA", "ONA", "SPECIAL"] {
            let detail = detail_with(format, None);
            assert!(build_missing_targets(&detail, &[]).is_empty(), "{format}");
            assert!(
                matches!(
                    build_monitored_targets(&detail, &[], &[1]).as_slice(),
                    [SearchTarget::Episode(1)]
                ),
                "{format}"
            );
        }
    }

    #[test]
    fn multi_episode_non_tv_entries_keep_numbered_targets() {
        for format in ["OVA", "ONA", "SPECIAL"] {
            let detail = detail_with(format, Some(3));
            assert!(
                matches!(
                    build_missing_targets(&detail, &[2]).as_slice(),
                    [SearchTarget::Episode(1), SearchTarget::Episode(3)]
                ),
                "{format}"
            );
            assert!(
                matches!(
                    build_monitored_targets(&detail, &[2], &[2, 3]).as_slice(),
                    [SearchTarget::Episode(3)]
                ),
                "{format}"
            );
        }
    }

    #[test]
    fn confirmed_single_entry_media_keeps_a_single_target() {
        for (format, episodes) in [
            ("MOVIE", None),
            ("MOVIE", Some(1)),
            ("MOVIE", Some(4)),
            ("OVA", Some(1)),
            ("ONA", Some(1)),
            ("SPECIAL", Some(1)),
        ] {
            let detail = detail_with(format, episodes);
            assert!(matches!(
                build_missing_targets(&detail, &[]).as_slice(),
                [SearchTarget::Single]
            ));
            assert!(matches!(
                build_monitored_targets(&detail, &[], &[1]).as_slice(),
                [SearchTarget::Single]
            ));
            assert!(matches!(
                build_monitored_targets(&detail, &[], &[]).as_slice(),
                [SearchTarget::Single]
            ));
            assert!(build_monitored_targets(&detail, &[], &[2]).is_empty());
        }
    }

    #[test]
    fn single_episode_tv_still_uses_a_numbered_target() {
        for format in ["TV", "TV_SHORT"] {
            let detail = detail_with(format, Some(1));
            assert!(matches!(
                build_missing_targets(&detail, &[]).as_slice(),
                [SearchTarget::Episode(1)]
            ));
            assert!(build_missing_targets(&detail, &[1]).is_empty());
            assert!(matches!(
                build_monitored_targets(&detail, &[], &[1]).as_slice(),
                [SearchTarget::Episode(1)]
            ));
            assert!(build_monitored_targets(&detail, &[1], &[1]).is_empty());
        }
    }

    // SearchTarget::for_episode collapses to Single for single-entry media
    // so per-episode handlers don't pass Episode(n) for shows that don't
    // have episode numbers in their release filenames.

    #[test]
    fn for_episode_collapses_movie_to_single() {
        let d = detail_with("MOVIE", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_collapses_special_to_single() {
        let d = detail_with("SPECIAL", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_collapses_ova_to_single() {
        let d = detail_with("OVA", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_single_episode_tv() {
        // TV format stays as Episode(n) regardless of episode count — the
        // collapse rule is format-only. A TV release titled "Show - 01" still
        // carries an episode number that Episode(1) can match against.
        let d = detail_with("TV", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Episode(1)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_tv() {
        let d = detail_with("TV", Some(12));
        assert!(matches!(
            SearchTarget::for_episode(&d, 7),
            SearchTarget::Episode(7)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_when_episode_count_unknown() {
        // AniList reports episodes=None for currently-airing shows — the
        // fallback should still be Episode(n) because that's the correct
        // target for an airing weekly release.
        let d = detail_with("TV", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 3),
            SearchTarget::Episode(3)
        ));
    }

    #[test]
    fn for_episode_collapses_movie_even_when_episode_count_is_none() {
        // MOVIE always collapses regardless of AniList's episode count — a
        // film is single-entry even if AniList has weird/missing data.
        let d = detail_with("MOVIE", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_ova() {
        // Multi-episode OVA series (e.g., long-running OVA franchises with
        // 10+ entries) carry episode numbers in their release filenames,
        // so per-episode search must NOT collapse them to Single — that
        // would return a release for any episode or a full batch when the
        // user specifically asked for episode N.
        let d = detail_with("OVA", Some(10));
        assert!(matches!(
            SearchTarget::for_episode(&d, 5),
            SearchTarget::Episode(5)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_ona() {
        let d = detail_with("ONA", Some(24));
        assert!(matches!(
            SearchTarget::for_episode(&d, 12),
            SearchTarget::Episode(12)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_special() {
        let d = detail_with("SPECIAL", Some(4));
        assert!(matches!(
            SearchTarget::for_episode(&d, 2),
            SearchTarget::Episode(2)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_ova_with_unknown_count() {
        // Ambiguous: we don't know whether it's a 1-episode OVA or a
        // 12-episode OVA. Safe default is Episode(n) — the failure mode
        // is "no results" rather than "grabbed the wrong release".
        let d = detail_with("OVA", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Episode(1)
        ));
    }
}

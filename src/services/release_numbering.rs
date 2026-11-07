//! Select episode numbers before considering which library slot is wanted.
//! This is a conservative check of the supported release conventions, not
//! proof of a video's contents. Provider disagreement requires source evidence.

use serde::{Deserialize, Serialize};

use crate::services::anibridge::MalEpisodePlan;
use crate::services::anilist::MAX_PLAUSIBLE_EPISODES;
use crate::services::episode_mapping::{self, MappedFile, MappingRule};
use crate::services::{auto_search, media};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMapping {
    version: u32,
    pub anilist_id: i64,
    pub tv_series: bool,
    source_first: i32,
    source_last: i32,
    target_first: i32,
    source_season: Option<i32>,
    basis: NumberingBasis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum NumberingBasis {
    /// Ordinary entry-relative numbering, with no conflicting known map.
    EntryRelative,
    /// An explicit SxxExx release number, reconciled with known provider maps.
    SeasonRelative,
}

impl ReleaseMapping {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.anilist_id == 0
            || self.source_season.is_none_or(|season| season <= 0)
        {
            return Err("Episode mapping: invalid saved release decision".into());
        }
        let rule = self.rule();
        episode_mapping::resolve_episode(&[rule], self.source_first).map_err(|e| e.to_string())?;
        episode_mapping::resolve_episode(&[rule], self.source_last).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn rule(&self) -> MappingRule {
        MappingRule {
            source_first: self.source_first,
            source_last: self.source_last,
            target_entry: self.anilist_id,
            target_first: self.target_first,
        }
    }

    pub fn episodes(&self) -> Vec<i32> {
        (self.target_first..=self.target_first + self.source_last - self.source_first).collect()
    }

    pub fn contains_episode(&self, number: i32) -> bool {
        number >= self.target_first
            && number <= self.target_first + self.source_last - self.source_first
    }

    /// Never borrow a requested slot when the actual file cannot be parsed.
    pub fn resolve_file(&self, span: media::EpisodeSpan) -> Result<MappedFile, String> {
        self.validate()?;
        if (self.tv_series && span.special)
            || span.season == Some(0)
            || (self.source_season.is_some()
                && span.season.is_some()
                && self.source_season != span.season)
        {
            return Err(
                "Episode mapping: file season/special differs from the release decision".into(),
            );
        }
        episode_mapping::resolve_file(&[self.rule()], span.first, span.last)
            .map_err(|e| e.to_string())
    }

    pub fn offset(&self) -> i32 {
        self.source_first - self.target_first
    }
}

pub struct NumberingContext {
    pub anilist_id: i64,
    pub tv_series: bool,
    pub episode_count: Option<i32>,
    pub cumulative_offset: i32,
    pub expected_season: i32,
    pub provider_plan: Option<MalEpisodePlan>,
}

impl NumberingContext {
    pub async fn load(
        anilist_id: i64,
        mal_id: Option<i64>,
        format: &str,
        episode_count: Option<i32>,
        cumulative_offset: i32,
        expected_season: i32,
    ) -> Result<Self, String> {
        // Entries without a MAL association still use the ordinary release
        // convention. A known provider outage is not an identity mapping.
        let mal_id = mal_id.filter(|id| *id > 0);
        let provider_plan = if mal_id.is_some() {
            crate::services::anibridge::mal_episode_plan(anilist_id, mal_id).await?
        } else {
            None
        };
        Ok(Self {
            anilist_id,
            tv_series: media::is_tv_format(format),
            episode_count,
            cumulative_offset,
            expected_season,
            provider_plan,
        })
    }

    /// No requested episode enters this function. Counts bound a decision;
    /// they never choose between relative and continuous numbering.
    pub fn choose(&self, title: &str, is_batch: bool) -> Result<ReleaseMapping, String> {
        if self.anilist_id == 0 || (self.tv_series && media::is_special_release(title)) {
            return Err("Episode mapping: source episode needs an explicit mapping".into());
        }
        let span = media::parse_episode_span(&title.to_lowercase());
        if span.is_some_and(|s| s.season == Some(0)) {
            return Err("Episode mapping: season zero needs an explicit mapping".into());
        }
        let explicit_season = span.and_then(|s| s.season).filter(|s| *s > 0);
        if explicit_season.is_some_and(|season| season != self.expected_season) {
            return Err("Episode mapping: release names another source season".into());
        }
        if self.cumulative_offset != 0 && explicit_season.is_none() {
            return Err("Episode mapping: entry-relative and continuous numbering are both possible; identify the source scheme".into());
        }
        // The broad pack parser accepts spaced ranges. A single video's
        // strict parser avoids reading `Season 2 - 03` as episodes 2-3.
        let mut numbers: Vec<_> = if is_batch {
            auto_search::parse_release_numbers(title)
                .into_iter()
                .collect()
        } else {
            span.map(|s| (s.first..=s.last).collect())
                .unwrap_or_default()
        };
        numbers.sort_unstable();
        if numbers.is_empty() && is_batch {
            let count = self
                .episode_count
                .filter(|n| (1..=MAX_PLAUSIBLE_EPISODES).contains(n))
                .ok_or_else(|| "Episode mapping: batch has no known episode range".to_string())?;
            numbers.extend(1..=count);
        }
        let (&first, &last) = numbers.first().zip(numbers.last()).ok_or_else(|| {
            "Episode mapping: release has no identifiable episode number".to_string()
        })?;
        if first < 1
            || last > MAX_PLAUSIBLE_EPISODES
            || numbers.windows(2).any(|pair| pair[1] != pair[0] + 1)
        {
            return Err("Episode mapping: release needs an explicit episode range".into());
        }
        if !is_batch && last - first >= media::MAX_FILE_SPAN {
            return Err("Episode mapping: single file has an unsupported episode span".into());
        }
        if let Some(plan) = &self.provider_plan {
            plan.source_slots()?;
            for source in &plan.sources {
                for &number in &numbers {
                    let mapped = episode_mapping::resolve_episode(&source.rules, number)
                        .map_err(|e| e.to_string())?;
                    if mapped.entry != self.anilist_id || mapped.number != number {
                        return Err(format!(
                            "Episode mapping: source episode {number} has conflicting library identities; identify the source scheme"
                        ));
                    }
                }
            }
        }
        if self
            .episode_count
            .is_some_and(|count| count > 0 && last > count)
        {
            return Err(
                "Episode mapping: release exceeds the library entry's known episode count".into(),
            );
        }
        let mapping = ReleaseMapping {
            version: 1,
            anilist_id: self.anilist_id,
            tv_series: self.tv_series,
            source_first: first,
            source_last: last,
            target_first: first,
            source_season: Some(explicit_season.unwrap_or(self.expected_season)),
            basis: if explicit_season.is_some() {
                NumberingBasis::SeasonRelative
            } else {
                NumberingBasis::EntryRelative
            },
        };
        mapping.validate()?;
        Ok(mapping)
    }
}

/// Apply a saved source decision before title, season, or episode heuristics.
/// A known source outside the wanted entry/slot is held, not reinterpreted.
pub(crate) async fn apply_reviewed_source(
    db: &sqlx::SqlitePool,
    anilist_id: i64,
    target: &auto_search::SearchTarget,
    candidate: &mut crate::services::nyaa::SearchResult,
) -> Result<bool, String> {
    let required_replay = candidate.replay_episodes.is_some();
    candidate.replay_episodes = None;
    let source_url = if !candidate.magnet.is_empty() {
        &candidate.magnet
    } else {
        &candidate.torrent
    };
    let episodes = crate::models::grabbed_torrents::source_replay::episodes_for_source(
        db,
        crate::models::grabbed_torrents::source_replay::SourceRef {
            url: source_url,
            guid: &candidate.source_guid,
            indexer_id: candidate.indexer_id,
            ..Default::default()
        },
        anilist_id,
    )
    .await?;
    let Some(episodes) = episodes else {
        if required_replay {
            return Err("Episode mapping: the reviewed source changed during search".into());
        }
        return Ok(false);
    };
    if episodes.is_empty()
        || matches!(target, auto_search::SearchTarget::Episode(n) if !episodes.contains(n))
    {
        return Err(
            "Episode mapping: reviewed source does not contain the requested episode".into(),
        );
    }
    candidate.release_mapping = None;
    candidate.replay_episodes = Some(episodes);
    Ok(true)
}

/// The final automatic gate also covers SeaDex candidates. Curation confirms
/// a title/release association, not the source episode numbering.
pub async fn filter_automatic_candidates(
    db: &sqlx::SqlitePool,
    detail: &crate::services::anilist::AnimeDetail,
    offset: i32,
    target: &auto_search::SearchTarget,
    candidates: Vec<crate::services::nyaa::SearchResult>,
) -> Vec<crate::services::nyaa::SearchResult> {
    let standalone = detail.format == "MOVIE"
        || (matches!(detail.format.as_str(), "SPECIAL" | "OVA" | "ONA")
            && detail.episodes == Some(1));
    if standalone {
        return candidates;
    }
    let wanted = match target {
        auto_search::SearchTarget::Episode(n) => Some(*n),
        auto_search::SearchTarget::Single => None,
    };
    if candidates.is_empty() {
        return candidates;
    }
    let context = NumberingContext::load(
        detail.id,
        detail.id_mal,
        &detail.format,
        detail.episodes,
        offset,
        auto_search::infer_season_from_detail(detail).max(1),
    )
    .await;
    let mut accepted = Vec::new();
    for mut candidate in candidates {
        match apply_reviewed_source(db, detail.id, target, &mut candidate).await {
            Ok(true) => {
                accepted.push(candidate);
                continue;
            }
            Err(reason) => {
                crate::services::logger::warn(
                    db,
                    crate::models::log::LogCategory::Grab,
                    "Automatic grab held: reviewed source",
                    &reason,
                )
                .await;
                continue;
            }
            Ok(false) => {}
        }
        let decision = context
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|ctx| ctx.choose(&candidate.title, candidate.is_batch));
        match decision {
            Ok(mapping) if wanted.is_none_or(|n| mapping.contains_episode(n)) => {
                candidate.release_mapping = Some(mapping);
                accepted.push(candidate);
            }
            Ok(_) => {}
            Err(reason) => {
                crate::services::logger::warn(
                    db,
                    crate::models::log::LogCategory::Grab,
                    &format!("Automatic grab held: {}", candidate.title),
                    &reason,
                )
                .await;
            }
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anibridge::MalSource;

    fn context(offset: i32) -> NumberingContext {
        NumberingContext {
            anilist_id: 10,
            tv_series: true,
            episode_count: Some(13),
            cumulative_offset: offset,
            expected_season: if offset == 0 { 1 } else { 2 },
            provider_plan: None,
        }
    }

    #[tokio::test]
    async fn missing_or_zero_provider_id_does_not_claim_a_source_mapping() {
        for mal_id in [None, Some(0), Some(-1)] {
            let context = NumberingContext::load(10, mal_id, "TV", Some(12), 0, 1)
                .await
                .unwrap();
            assert!(context.provider_plan.is_none());
            assert_eq!(
                context.choose("Show - 06", false).unwrap().episodes(),
                vec![6]
            );
        }
    }

    #[test]
    fn video_parser_distinguishes_a_title_number_and_a_special_entry() {
        let mapping = context(0).choose("Show Season 2 - 03", false).unwrap();
        assert_eq!(mapping.episodes(), vec![3]);
        assert!(context(0).choose("Show OVA 01", false).is_err());
        let mut ova = context(0);
        ova.tv_series = false;
        let mapping = ova.choose("Show OVA 01", false).unwrap();
        let span = media::parse_episode_span("show ova 01.mkv").unwrap();
        assert_eq!(mapping.resolve_file(span).unwrap().first, 1);
        assert!(ova.choose("Show S00E01", false).is_err());
    }

    #[test]
    fn requested_slot_and_count_cannot_choose_continuous_numbering() {
        let ctx = context(1);
        assert!(ctx.choose("Show - 06", false).is_err());
        assert!(ctx.choose("Show - 14", false).is_err());
        assert!(ctx.choose("Show Complete", true).is_err());
        let relative = ctx.choose("Show S02E06", false).unwrap();
        assert_eq!(relative.episodes(), vec![6]);
        assert!(!relative.contains_episode(5));
    }

    #[test]
    fn provider_shift_conflicts_even_with_explicit_season_or_matching_count() {
        let mut ctx = context(0);
        ctx.provider_plan = Some(MalEpisodePlan {
            anilist_id: 10,
            unsupported: false,
            sources: vec![MalSource {
                mal_id: 20,
                rules: vec![MappingRule {
                    source_first: 1,
                    source_last: 12,
                    target_entry: 10,
                    target_first: 2,
                }],
            }],
        });
        assert!(ctx.choose("Show - 06", false).is_err());
        assert!(ctx.choose("Show S01E06", false).is_err());
        assert!(ctx.choose("Show 01-12", true).is_err());
    }

    #[test]
    fn saved_choice_limits_actual_files_and_round_trips() {
        let mapping = context(0).choose("Show - 05-06", false).unwrap();
        let json = serde_json::to_string(&mapping).unwrap();
        let restored: ReleaseMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(mapping, restored);
        let mut incomplete: serde_json::Value = serde_json::from_str(&json).unwrap();
        incomplete.as_object_mut().unwrap().remove("source_season");
        let incomplete: ReleaseMapping = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate().is_err());
        assert_eq!(
            restored
                .resolve_file(media::EpisodeSpan::single(None, 5))
                .unwrap()
                .first,
            5
        );
        assert!(
            restored
                .resolve_file(media::EpisodeSpan::single(None, 7))
                .is_err()
        );
        assert!(
            restored
                .resolve_file(media::EpisodeSpan::single(Some(0), 5))
                .is_err()
        );
        let mut invalid = restored;
        invalid.version = 2;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn whole_pack_is_bounded_without_interpreting_the_wanted_slot() {
        let mapping = context(0).choose("Show Complete", true).unwrap();
        assert_eq!(mapping.episodes(), (1..=13).collect::<Vec<_>>());
        assert!(
            mapping
                .resolve_file(media::EpisodeSpan::single(None, 14))
                .is_err()
        );
        assert!(context(0).choose("Show - 00", false).is_err());
    }
}

//! Select episode numbers before considering which library slot is wanted.
//! This is a conservative check of the supported release conventions, not
//! proof of a video's contents. Provider disagreement requires source evidence.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex_lite::Regex;

use crate::services::anibridge::MalEpisodePlan;
use crate::services::anilist::MAX_PLAUSIBLE_EPISODES;
use crate::services::episode_mapping::{self, MappedFile, MappingRule};
use crate::services::{auto_search, media};

#[path = "release_numbering_publication.rs"]
mod publication;
pub(crate) use publication::PremiereBound;

#[path = "release_numbering_provider.rs"]
mod provider;
pub(crate) use provider::ProviderNumbering;

// Positive source evidence is narrower than the search parser's season
// rejection hints. In particular, bare Romans and Part/Cour are not seasons.
static SOURCE_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._-])(?P<payload>(?:s\d{1,2}e|-\s*|e(?:p\.?)?|episode\s+)(?P<number>\d{1,4})(?:v\d+)?(?:(?:\s*[-~]\s*(?:e(?:p\.?)?)?|e)\d{1,4}(?:v\d+)?)*)\b").unwrap()
});
static SOURCE_PACK_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._])(?P<payload>(?P<number>\d{1,4})(?:v\d+)?\s*[-~]\s*(?:e(?:p\.?)?)?\d{1,4}(?:v\d+)?(?:(?:\s*[-~]\s*(?:e(?:p\.?)?)?|e)\d{1,4}(?:v\d+)?)*)\b").unwrap()
});
static SOURCE_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:s(\d{1,2})|season\s*(\d{1,2})|(\d{1,2})(?:st|nd|rd|th)\s+season)\b")
        .unwrap()
});
static SOURCE_SXXEXX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bs(\d{1,2})e\d{1,4}(?:v\d+)?(?:(?:-e|-|e)\d{1,4}(?:v\d+)?)*\b").unwrap()
});

struct SourceHeading {
    name_prefix: String,
    raw_name_prefix: String,
    season: Option<i32>,
    payload_span: Option<media::EpisodeSpan>,
    batch_payload: Option<String>,
    first_episode: Option<i32>,
}

fn source_heading(title: &str, marked_batch: bool) -> Result<SourceHeading, String> {
    if title.len() > media::MAX_RELEASE_TITLE_BYTES {
        return Err("Episode mapping: source title exceeds the maximum length".into());
    }
    // Preserve byte positions and word boundaries while excluding groups
    // and metadata. Malformed nesting cannot establish source evidence.
    let mut masked = String::with_capacity(title.len());
    let mut brackets = Vec::new();
    for ch in title.chars() {
        match ch {
            '[' | '(' | '{' => {
                brackets.push(ch);
                masked.push(' ');
            }
            ']' | ')' | '}' => {
                let expected = match ch {
                    ']' => '[',
                    ')' => '(',
                    _ => '{',
                };
                if brackets.pop() != Some(expected) {
                    return Err("Episode mapping: malformed source heading brackets".into());
                }
                masked.push(' ');
            }
            _ if !brackets.is_empty() => masked.extend(std::iter::repeat_n(' ', ch.len_utf8())),
            '_' => masked.push(' '),
            _ => masked.push(ch),
        }
    }
    if !brackets.is_empty() {
        return Err("Episode mapping: malformed source heading brackets".into());
    }
    // A caller-recognized pack can use a plain 01-12 range. A marked whole
    // season may omit its range; choose still requires its count and provider map.
    let mut first_payload = SOURCE_EPISODE.captures(&masked);
    let mut plain_pack_range = false;
    {
        for candidate in SOURCE_PACK_RANGE.captures_iter(&masked) {
            let payload = candidate.name("payload").expect("a range has a payload");
            let number = candidate
                .name("number")
                .expect("a range has a first number");
            let season_marker = SOURCE_SEASON.captures_iter(&masked).find_map(|season| {
                (1..=3)
                    .filter_map(|index| season.get(index))
                    .any(|value| value.start() == number.start())
                    .then(|| season.get(0).expect("a season has a whole match"))
            });
            if payload.as_str().contains(char::is_whitespace)
                && let Some(season_marker) = season_marker
            {
                // A spaced range cannot borrow the heading's Season 2 as
                // its left endpoint. English ordinal wording is ambiguous
                // here, so hold "Second Season 01 - 26" rather than keep 26.
                let preceding = masked[..season_marker.start()]
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .rev()
                    .find(|word| !word.is_empty())
                    .unwrap_or("");
                if matches!(
                    preceding.to_ascii_lowercase().as_str(),
                    "first"
                        | "second"
                        | "third"
                        | "fourth"
                        | "fifth"
                        | "sixth"
                        | "seventh"
                        | "eighth"
                        | "ninth"
                        | "tenth"
                ) {
                    return Err("Episode mapping: ambiguous season and pack range boundary".into());
                }
                continue;
            }
            if first_payload.as_ref().is_none_or(|first| {
                first
                    .name("payload")
                    .expect("an episode has a payload")
                    .start()
                    > payload.start()
            }) {
                if !marked_batch {
                    if SOURCE_SEASON.captures_iter(&masked).any(|season| {
                        season
                            .get(0)
                            .is_some_and(|marker| marker.end() <= payload.start())
                    }) {
                        return Err(
                            "Episode mapping: plain pack range requires a marked batch".into()
                        );
                    }
                    // Without an earlier structural season, the left number
                    // can belong to the canonical title: "Adventure 02 - 03".
                    // Its identity is checked upstream; keep the strict marker.
                    continue;
                }
                first_payload = Some(candidate);
                plain_pack_range = true;
            }
            break;
        }
    }
    let boundary = first_payload
        .as_ref()
        .and_then(|captures| captures.name("payload"))
        .map(|payload| payload.start());
    let mut season = None;
    let mut record = |number: i32| -> Result<(), String> {
        if !(1..=99).contains(&number) || season.is_some_and(|prior| prior != number) {
            return Err("Episode mapping: conflicting or invalid source season".into());
        }
        season = Some(number);
        Ok(())
    };
    for captures in SOURCE_SEASON.captures_iter(&masked) {
        let marker = captures.get(0).expect("a season capture has a whole match");
        if boundary.is_some_and(|start| marker.start() < start && marker.end() > start) {
            // In "Second Season 01-26", the range's 01 is not Season 1.
            continue;
        }
        if boundary.map_or(!marked_batch, |start| marker.start() >= start) {
            return Err("Episode mapping: source season is outside the release heading".into());
        }
        let number = (1..=3)
            .find_map(|index| captures.get(index))
            .and_then(|number| number.as_str().parse::<i32>().ok())
            .ok_or_else(|| "Episode mapping: invalid source season".to_string())?;
        record(number)?;
    }
    for captures in SOURCE_SXXEXX.captures_iter(&masked) {
        let marker = captures.get(0).expect("a season capture has a whole match");
        if boundary != Some(marker.start()) {
            return Err("Episode mapping: source season is outside the episode payload".into());
        }
        let number = captures[1]
            .parse::<i32>()
            .map_err(|_| "Episode mapping: invalid source season".to_string())?;
        record(number)?;
    }
    let payload_span = if plain_pack_range {
        // The strict file parser can read a later spaced range endpoint as
        // a single episode. This batch-only range uses the isolated parser.
        None
    } else {
        boundary.and_then(|start| {
            // The strict dash parser needs its leading separator space.
            media::parse_episode_span(&format!(" {}", &masked[start..]).to_ascii_lowercase())
        })
    };
    let number = first_payload
        .as_ref()
        .and_then(|captures| captures.name("number"));
    if let Some(number) = number
        && payload_span.is_some_and(|span| number.as_str().parse::<i32>() != Ok(span.first))
    {
        return Err("Episode mapping: conflicting source episode payloads".into());
    }
    // A safe dummy prefix lets the existing batch parser read a full range.
    // Neither the heading's Season 2 nor the S02E prefix can become a slot.
    let batch_payload = match payload_span {
        Some(span) if span.last > span.first => {
            // Compact E03E04 tails are a strict-file convention. Preserve
            // their already-validated contiguous range for batch parsing.
            Some(format!("Source - {}-{}", span.first, span.last))
        }
        _ => number
            .zip(
                first_payload
                    .as_ref()
                    .and_then(|captures| captures.name("payload")),
            )
            .map(|(number, payload)| {
                format!("Source - {}", &masked[number.start()..payload.end()])
            }),
    };
    let prefix = boundary.map_or(masked.as_str(), |start| &masked[..start]);
    let name_prefix = SOURCE_SEASON.replace_all(prefix, " ").into_owned();
    Ok(SourceHeading {
        name_prefix,
        raw_name_prefix: title[..prefix.len()].to_string(),
        season,
        payload_span,
        batch_payload,
        first_episode: number.and_then(|number| number.as_str().parse().ok()),
    })
}

/// Strict source-file numbering shared by reviewed finite domains and the
/// ordinary release mapping. This does not choose an entry or an offset.
pub(crate) fn validated_source_file_span(
    filename: &str,
) -> Result<(media::EpisodeSpan, Option<i32>), String> {
    let heading = source_heading(filename, false)?;
    let span = media::parse_episode_span(&filename.to_ascii_lowercase())
        .ok_or_else(|| "Episode mapping: file has no identifiable episode number".to_string())?;
    if (heading.season.is_some() && heading.payload_span.is_none())
        || span
            .season
            .is_some_and(|season| heading.season != Some(season))
        || heading
            .payload_span
            .is_some_and(|payload| payload.first != span.first || payload.last != span.last)
    {
        return Err("Episode mapping: source filename has conflicting episode evidence".into());
    }
    Ok((span, heading.season))
}

/// Preserve every title word before the strict episode payload. Anitomy may
/// shorten a foreign installment at a dash; that is insufficient for an
/// automatic canonical identity. Human review keeps its existing contract.
pub(crate) fn source_identity_name(title: &str, require_episode: bool) -> Result<String, String> {
    let heading = source_heading(title, false)?;
    if require_episode && heading.first_episode.is_none() {
        return Err("Episode mapping: source file has no strict title/episode boundary".into());
    }
    // Only the initial square-bracket release group is out of band. Other
    // pre-episode bracket text can name an installment; retain its words even
    // though the episode parser masks them to locate structural numbers.
    let mut qualifiers = Vec::new();
    let mut content = String::new();
    let mut depth = 0;
    let mut release_group = false;
    for (index, ch) in heading.raw_name_prefix.char_indices() {
        match ch {
            '[' | '(' | '{' => {
                if depth == 0 {
                    content.clear();
                    release_group = require_episode
                        && ch == '['
                        && heading.raw_name_prefix[..index].trim().is_empty();
                } else {
                    content.push(' ');
                }
                depth += 1;
            }
            ']' | ')' | '}' => {
                depth -= 1;
                if depth == 0 && !release_group {
                    qualifiers.push(auto_search::normalize_title(&content));
                }
            }
            _ if depth > 0 => content.push(ch),
            _ => {}
        }
    }
    let name = auto_search::normalize_title(&format!(
        "{} {}",
        auto_search::normalize_title(&heading.name_prefix),
        qualifiers.join(" ")
    ));
    if name.is_empty() {
        return Err("Episode mapping: source file has no complete canonical heading".into());
    }
    Ok(name)
}

pub(crate) fn validated_source_folder_season(folder: &str) -> Result<Option<i32>, String> {
    source_heading(folder, true).map(|heading| heading.season)
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_heading: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bound_source: Option<crate::services::source_numbering::BoundSourceProof>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    grouped_upgrade: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum NumberingBasis {
    /// Ordinary entry-relative numbering, with no conflicting known map.
    EntryRelative,
    /// An explicit source season, reconciled with known provider maps.
    SeasonRelative,
    /// One explicitly numbered episode in a confirmed provider season.
    ProviderSeason,
    /// One independently verified aired slot extends a primary MAL identity map.
    AiredEntryTail,
    /// A reviewed exact source selects a finite provider-root domain.
    SourceBound,
}

impl ReleaseMapping {
    pub(crate) fn from_provider(
        anilist_id: i64,
        tv_series: bool,
        season: i32,
        provider_episode: i32,
        local_episode: i32,
        heading: String,
    ) -> Result<Self, String> {
        let mapping = Self {
            version: 1,
            anilist_id,
            tv_series,
            source_first: provider_episode,
            source_last: provider_episode,
            target_first: local_episode,
            source_season: Some(season),
            basis: NumberingBasis::ProviderSeason,
            provider_heading: Some(heading),
            bound_source: None,
            grouped_upgrade: false,
        };
        mapping.validate()?;
        Ok(mapping)
    }

    pub(crate) fn provider_heading(&self) -> Option<&str> {
        self.provider_heading.as_deref()
    }

    pub(crate) fn requires_single_video(&self) -> bool {
        matches!(
            self.basis,
            NumberingBasis::ProviderSeason | NumberingBasis::AiredEntryTail
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.basis == NumberingBasis::ProviderSeason {
            if self.anilist_id <= 0
                || self.source_first != self.source_last
                || self.provider_heading.as_ref().is_none_or(|name| {
                    name.is_empty() || name.len() > media::MAX_RELEASE_TITLE_BYTES
                })
            {
                return Err("Episode mapping: invalid provider episode decision".into());
            }
        } else if self.provider_heading.is_some() {
            return Err("Episode mapping: unexpected provider title".into());
        }
        if self.basis == NumberingBasis::AiredEntryTail
            && (self.anilist_id <= 0
                || self.source_first < 2
                || self.source_first != self.source_last
                || self.source_first != self.target_first)
        {
            return Err("Episode mapping: invalid aired entry episode decision".into());
        }
        if self.basis == NumberingBasis::SourceBound {
            let proof = self
                .bound_source
                .as_ref()
                .ok_or_else(|| "Episode mapping: source-bound decision has no proof".to_string())?;
            proof.validate()?;
            let (first, last, target) = proof.source_span()?;
            let owner = proof
                .domain
                .entries
                .iter()
                .find(|entry| entry.anilist_id == proof.target_anilist_id)
                .ok_or_else(|| {
                    "Episode mapping: source-bound owner metadata is missing".to_string()
                })?;
            let season = proof
                .selected_files
                .iter()
                .filter_map(|file| file.source_season)
                .next();
            if self.version != 2
                || self.anilist_id != proof.target_anilist_id
                || self.tv_series != media::is_tv_format(&owner.format)
                || (self.source_first, self.source_last, self.target_first) != (first, last, target)
                || self.source_season != season
                || (self.grouped_upgrade
                    && (proof.selected_files.len() < 2
                        || proof
                            .selected_files
                            .iter()
                            .any(|file| file.target_first != file.target_last)))
            {
                return Err("Episode mapping: source-bound release decision changed".into());
            }
            return Ok(());
        }
        if self.version != 1
            || self.bound_source.is_some()
            || self.grouped_upgrade
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
        if let Some(proof) = &self.bound_source {
            return proof.episodes();
        }
        (self.target_first..=self.target_first + self.source_last - self.source_first).collect()
    }

    pub fn contains_episode(&self, number: i32) -> bool {
        if let Some(proof) = &self.bound_source {
            return proof.episodes().contains(&number);
        }
        number >= self.target_first
            && number <= self.target_first + self.source_last - self.source_first
    }

    /// Never borrow a requested slot when the actual file cannot be parsed.
    pub fn resolve_file(&self, span: media::EpisodeSpan) -> Result<MappedFile, String> {
        self.validate()?;
        if self.bound_source.is_some() {
            return Err(
                "Episode mapping: source-bound files require their exact relative filenames".into(),
            );
        }
        if ((self.tv_series || self.basis == NumberingBasis::AiredEntryTail) && span.special)
            || span.season == Some(0)
            || (self.provider_heading.is_some() && self.source_season != span.season)
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

    /// Validate the actual filename's source heading as well as its strict
    /// episode span. A saved Season 2 decision cannot import Season 3 - 03.
    pub fn resolve_named_file(&self, filename: &str) -> Result<MappedFile, String> {
        if self.bound_source.is_some() {
            return self.resolve_bound_file(filename);
        }
        let heading = source_heading(filename, false)?;
        let span = media::parse_episode_span(&filename.to_ascii_lowercase()).ok_or_else(|| {
            "Episode mapping: file has no identifiable episode number".to_string()
        })?;
        if (heading.season.is_some() && heading.payload_span.is_none())
            || heading
                .season
                .is_some_and(|season| self.source_season != Some(season))
            || (self.basis == NumberingBasis::AiredEntryTail
                && self.source_season.is_some_and(|season| season > 1)
                && heading.season != self.source_season)
            || span
                .season
                .is_some_and(|season| heading.season != Some(season))
            || heading
                .payload_span
                .is_some_and(|payload| payload.first != span.first || payload.last != span.last)
        {
            return Err("Episode mapping: file heading differs from the release decision".into());
        }
        if let Some(expected) = self.provider_heading()
            && (span.special
                || heading.season != self.source_season
                || source_identity_name(filename, true)? != expected)
        {
            return Err(
                "Episode mapping: file does not match the provider title and season".into(),
            );
        }
        self.resolve_file(span)
    }

    pub fn offset(&self) -> i32 {
        self.source_first - self.target_first
    }

    pub fn bound_source(&self) -> Option<&crate::services::source_numbering::BoundSourceProof> {
        self.bound_source.as_ref()
    }

    pub(crate) fn is_single_episode_pack(&self) -> bool {
        self.bound_source().is_some_and(|proof| {
            proof.origin() == Ok(crate::services::source_numbering::ProofOrigin::AutomaticEntryPack)
        })
    }

    pub fn from_bound_source(
        proof: &crate::services::source_numbering::BoundSourceProof,
    ) -> Result<Self, String> {
        proof.validate()?;
        let (source_first, source_last, target_first) = proof.source_span()?;
        let owner = proof
            .domain
            .entries
            .iter()
            .find(|entry| entry.anilist_id == proof.target_anilist_id)
            .ok_or_else(|| "Episode mapping: source-bound owner metadata is missing".to_string())?;
        let mapping = Self {
            version: 2,
            anilist_id: proof.target_anilist_id,
            tv_series: media::is_tv_format(&owner.format),
            source_first,
            source_last,
            target_first,
            source_season: proof
                .selected_files
                .iter()
                .filter_map(|file| file.source_season)
                .next(),
            basis: NumberingBasis::SourceBound,
            provider_heading: None,
            bound_source: Some(proof.clone()),
            grouped_upgrade: false,
        };
        mapping.validate()?;
        Ok(mapping)
    }

    pub fn validate_bound_files(
        &self,
        info_hash: &str,
        files: &[(&str, bool)],
    ) -> Result<(), String> {
        self.validate()?;
        self.bound_source
            .as_ref()
            .ok_or_else(|| "Episode mapping: no exact source proof is saved".to_string())?
            .validate_files_with_selection(info_hash, files, self.grouped_upgrade)
    }

    pub(crate) fn grouped_upgrade(&self) -> bool {
        self.grouped_upgrade
    }

    pub(crate) fn into_grouped_upgrade(mut self) -> Result<Self, String> {
        self.grouped_upgrade = true;
        self.validate()?;
        Ok(self)
    }

    pub fn resolve_bound_file(&self, relative_name: &str) -> Result<MappedFile, String> {
        self.validate()?;
        self.bound_source
            .as_ref()
            .ok_or_else(|| "Episode mapping: no exact source proof is saved".to_string())?
            .resolve_file(relative_name)
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

#[derive(Clone, Copy)]
pub(crate) struct VerifiedAiredEpisode {
    anilist_id: i64,
    mal_id: i64,
    episode: i32,
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
        self.choose_with_verified_airing(title, is_batch, None)
    }

    /// Before consulting the schedule, limit the exception to the next
    /// unrepresented slot on the primary source's contiguous identity map.
    pub(crate) async fn verified_airing(
        &self,
        db: &sqlx::SqlitePool,
        required_episode: Option<i32>,
        primary_mal: Option<i64>,
    ) -> Option<VerifiedAiredEpisode> {
        let episode = required_episode.filter(|ep| (2..=MAX_PLAUSIBLE_EPISODES).contains(ep))?;
        let mal_id = primary_mal.filter(|id| *id > 0)?;
        let plan = self.provider_plan.as_ref()?;
        if self.anilist_id <= 0
            || plan.anilist_id != self.anilist_id
            || !plan.permits_unmapped_fallback(Some(mal_id))
            || !is_identity_prefix(&plan.source_slots().ok()?, episode)
        {
            return None;
        }
        crate::services::recent_episodes::verified_aired_episode(db, self.anilist_id, episode)
            .await
            .ok()?
            .then_some(VerifiedAiredEpisode {
                anilist_id: self.anilist_id,
                mal_id,
                episode,
            })
    }

    /// A schedule receipt may confirm the number parsed from a single
    /// release. It never supplies the candidate's episode number.
    pub(crate) fn choose_with_verified_airing(
        &self,
        title: &str,
        is_batch: bool,
        aired: Option<VerifiedAiredEpisode>,
    ) -> Result<ReleaseMapping, String> {
        let heading = source_heading(title, is_batch)?;
        if self.anilist_id == 0 || (self.tv_series && media::is_special_release(title)) {
            return Err("Episode mapping: source episode needs an explicit mapping".into());
        }
        let span = media::parse_episode_span(&title.to_lowercase());
        if span.is_some_and(|s| s.season == Some(0)) {
            return Err("Episode mapping: season zero needs an explicit mapping".into());
        }
        let explicit_season = heading.season;
        if !is_batch && explicit_season.is_some() && heading.payload_span.is_none() {
            return Err("Episode mapping: source heading has no strict episode payload".into());
        }
        if span.is_some_and(|span| {
            span.season
                .is_some_and(|season| explicit_season != Some(season))
                || heading
                    .payload_span
                    .is_some_and(|payload| payload.first != span.first || payload.last != span.last)
        }) {
            return Err("Episode mapping: source episode differs from its heading".into());
        }
        if explicit_season.is_some_and(|season| season != self.expected_season) {
            return Err("Episode mapping: release names another source season".into());
        }
        if explicit_season.is_none()
            && (self.cumulative_offset != 0
                || (self.tv_series
                    && crate::services::anime_relations::has_source_namespace(self.anilist_id)))
        {
            return Err("Episode mapping: entry-relative and continuous numbering are both possible; identify the source scheme".into());
        }
        // The broad pack parser accepts spaced ranges. A single video's
        // strict parser avoids reading `Season 2 - 03` as episodes 2-3.
        let mut numbers: Vec<_> = if is_batch && explicit_season.is_some() {
            // The heading's season digit is not a batch episode. Only the
            // isolated payload can supply this qualified source's range.
            heading
                .batch_payload
                .as_deref()
                .map(auto_search::parse_release_numbers)
                .map(|numbers| numbers.into_iter().collect())
                .unwrap_or_default()
        } else if is_batch {
            auto_search::parse_release_numbers(title)
                .into_iter()
                .collect()
        } else {
            span.map(|s| (s.first..=s.last).collect())
                .unwrap_or_default()
        };
        numbers.sort_unstable();
        if is_batch
            && explicit_season.is_some()
            && heading.first_episode.is_some()
            && (numbers.is_empty() || numbers.first().copied() != heading.first_episode)
        {
            return Err("Episode mapping: conflicting source batch payload".into());
        }
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
        let positive_airing = aired.filter(|proof| {
            !is_batch
                && first == last
                && heading.payload_span.is_some()
                && !media::is_special_release(title)
                && proof.anilist_id == self.anilist_id
                && proof.episode == first
        });
        let aired_entry_tail = self.validate_provider_numbers(&numbers, positive_airing)?;
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
            basis: if aired_entry_tail {
                NumberingBasis::AiredEntryTail
            } else if explicit_season.is_some() {
                NumberingBasis::SeasonRelative
            } else {
                NumberingBasis::EntryRelative
            },
            provider_heading: None,
            bound_source: None,
            grouped_upgrade: false,
        };
        mapping.validate()?;
        Ok(mapping)
    }

    /// Called only after a same-hash published file list establishes this
    /// entry's full relative episode set. A catalogue count alone is not
    /// evidence: every distinct source slot must actually be present.
    pub(crate) fn choose_published_entry_pack(
        &self,
        source_episodes: &[i32],
        source_season: Option<i32>,
    ) -> Result<ReleaseMapping, String> {
        if source_season.is_none()
            && self.tv_series
            && crate::services::anime_relations::has_source_namespace(self.anilist_id)
        {
            return Err(
                "Episode mapping: a curated continuous source needs an exact source proof".into(),
            );
        }
        let count = self
            .episode_count
            .filter(|n| (2..=MAX_PLAUSIBLE_EPISODES).contains(n))
            .ok_or_else(|| {
                "Episode mapping: published pack needs a known multi-episode entry".to_string()
            })?;
        if self.anilist_id == 0
            || source_season.is_some_and(|season| season != self.expected_season)
        {
            return Err("Episode mapping: published pack names another entry or season".into());
        }
        let mut numbers = source_episodes.to_vec();
        numbers.sort_unstable();
        if numbers.len() != count as usize || !numbers.iter().copied().eq(1..=count) {
            return Err(
                "Episode mapping: published pack does not prove the complete entry-relative range"
                    .into(),
            );
        }
        self.validate_provider_numbers(&numbers, None)?;
        let mapping = ReleaseMapping {
            version: 1,
            anilist_id: self.anilist_id,
            tv_series: self.tv_series,
            source_first: 1,
            source_last: count,
            target_first: 1,
            source_season: Some(source_season.unwrap_or(self.expected_season)),
            basis: if source_season.is_some() {
                NumberingBasis::SeasonRelative
            } else {
                NumberingBasis::EntryRelative
            },
            provider_heading: None,
            bound_source: None,
            grouped_upgrade: false,
        };
        mapping.validate()?;
        Ok(mapping)
    }

    fn validate_provider_numbers(
        &self,
        numbers: &[i32],
        aired: Option<VerifiedAiredEpisode>,
    ) -> Result<bool, String> {
        if let Some(plan) = &self.provider_plan {
            let source_slots = plan.source_slots()?;
            for source in &plan.sources {
                for &number in numbers {
                    let mapped = match episode_mapping::resolve_episode(&source.rules, number) {
                        Err(episode_mapping::MappingError::Unmapped(_))
                            if aired.is_some_and(|proof| {
                                plan.anilist_id == proof.anilist_id
                                    && plan.permits_unmapped_fallback(Some(proof.mal_id))
                                    && is_identity_prefix(&source_slots, number)
                            }) =>
                        {
                            return Ok(true);
                        }
                        result => result.map_err(|e| e.to_string())?,
                    };
                    if mapped.entry != self.anilist_id || mapped.number != number {
                        return Err(format!(
                            "Episode mapping: source episode {number} has conflicting library identities; identify the source scheme"
                        ));
                    }
                }
            }
        }
        Ok(false)
    }
}

fn is_identity_prefix(slots: &BTreeMap<i32, (i64, i32)>, next: i32) -> bool {
    next > 1 && slots.keys().copied().eq(1..next)
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
    let required_bound = candidate
        .release_mapping
        .as_ref()
        .filter(|mapping| mapping.bound_source().is_some())
        .cloned();
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
        let mapping = bound_mapping_for_source(db, &candidate.info_hash, anilist_id).await?;
        let Some(mapping) = mapping else {
            if required_bound.is_some() {
                return Err(
                    "Episode mapping: the exact reviewed source proof changed during search".into(),
                );
            }
            return Ok(false);
        };
        let automatic = mapping
            .bound_source()
            .is_some_and(|proof| proof.origin().is_ok_and(|origin| origin.is_automatic()));
        let mapping = if let Some(proof) = mapping.bound_source().filter(|_| automatic) {
            let narrowed =
                crate::services::source_numbering::loader::narrow_for_target(proof, target)?;
            ReleaseMapping::from_bound_source(&narrowed)?
        } else {
            mapping
        };
        if required_bound.as_ref().is_some_and(|old| *old != mapping)
            || matches!(target, auto_search::SearchTarget::Episode(number) if !mapping.contains_episode(*number))
        {
            return Err(
                "Episode mapping: exact reviewed source does not contain this entry or episode"
                    .into(),
            );
        }
        let multiple = mapping
            .bound_source()
            .is_some_and(|proof| proof.selected_files.len() > 1);
        candidate.is_batch = if automatic {
            multiple
        } else {
            candidate.is_batch || multiple
        };
        candidate.release_mapping = Some(mapping);
        return Ok(true);
    };
    if required_bound.is_some() {
        return Err(
            "Episode mapping: migration replay conflicts with an exact-source numbering proof"
                .into(),
        );
    }
    if !crate::models::source_numbering::lookup_hash(db, &candidate.info_hash)
        .await?
        .is_empty()
    {
        return Err(
            "Episode mapping: migration replay conflicts with an exact-source numbering review"
                .into(),
        );
    }
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

pub async fn bound_mapping_for_source(
    db: &sqlx::SqlitePool,
    info_hash: &str,
    anilist_id: i64,
) -> Result<Option<ReleaseMapping>, String> {
    let all = crate::models::source_numbering::lookup_hash(db, info_hash).await?;
    let projected = if !all.is_empty()
        && !all.iter().any(|s| s.proof.target_anilist_id == anilist_id)
        && all.iter().all(|s| {
            s.proof.origin() == Ok(crate::services::source_numbering::ProofOrigin::AutomaticUnique)
        }) {
        let proof = crate::services::source_numbering::loader::additional_owner(
            db,
            &all[0].proof,
            anilist_id,
        )
        .await?;
        Some(crate::models::source_numbering::save_automatic(db, &proof).await?)
    } else {
        None
    };
    let saved = match projected {
        Some(saved) => Some(saved),
        None => crate::models::source_numbering::lookup_exact(db, info_hash, anilist_id).await?,
    };
    let Some(saved) = saved else {
        return Ok(None);
    };
    crate::services::source_numbering::loader::refresh(db, &saved.proof).await?;
    ReleaseMapping::from_bound_source(&saved.proof).map(Some)
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
    filter_automatic_candidates_with_pack_mappings(
        db,
        detail,
        offset,
        target,
        candidates,
        &HashMap::new(),
        None,
    )
    .await
}

/// Published-pack decisions are private search evidence, never a field
/// supplied by a client or an indexer. Reviewed source decisions win.
pub(crate) async fn filter_automatic_candidates_with_pack_mappings(
    db: &sqlx::SqlitePool,
    detail: &crate::services::anilist::AnimeDetail,
    offset: i32,
    target: &auto_search::SearchTarget,
    candidates: Vec<crate::services::nyaa::SearchResult>,
    pack_mappings: &HashMap<String, ReleaseMapping>,
    mut upgrade_proofs: Option<(&auto_search::BatchLookups, &mut Vec<String>)>,
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
    let verified_airing = if let Ok(context) = &context {
        context.verified_airing(db, wanted, detail.id_mal).await
    } else {
        None
    };
    let mut accepted = Vec::new();
    let automatic_sources = crate::services::source_numbering::loader::Resolver::default();
    let provider = ProviderNumbering::load(db, detail, target).await;
    for mut candidate in candidates {
        let deferred = upgrade_proofs.is_some()
            && candidate
                .match_provenance
                .as_ref()
                .is_some_and(|p| p.kind == auto_search::MatchKind::Fuzzy);
        let root_pack = deferred
            && candidate
                .match_provenance
                .as_ref()
                .is_some_and(|p| p.phase == auto_search::MatchPhase::Franchise);
        if let Some(bound) = PremiereBound::from_detail(detail)
            && let Err(reason) = bound.check(candidate.published_at)
        {
            crate::services::logger::warn(
                db,
                crate::models::log::LogCategory::Grab,
                &format!("Automatic grab held: {}", candidate.title),
                &reason,
            )
            .await;
            continue;
        }
        if deferred {
            let claimed: Result<bool, _> = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM grabbed_torrents WHERE hash=? COLLATE NOCASE AND state IN ('pending','imported'))",
            )
            .bind(&candidate.info_hash)
            .fetch_one(db)
            .await;
            if !matches!(claimed, Ok(false)) {
                if let Some((_, diagnostics)) = upgrade_proofs.as_mut() {
                    diagnostics
                        .push("Episode mapping: existing source assignment is protected".into());
                }
                continue;
            }
        }
        match apply_reviewed_source(db, detail.id, target, &mut candidate).await {
            Ok(true) => {
                let origin = candidate
                    .release_mapping
                    .as_ref()
                    .and_then(|m| m.bound_source())
                    .and_then(|proof| proof.origin().ok());
                if origin
                    == Some(crate::services::source_numbering::ProofOrigin::AutomaticNamedPack)
                    && wanted.is_none()
                    && upgrade_proofs.is_none()
                {
                    continue;
                }
                if origin.is_some_and(|origin| origin.is_automatic()) {
                    let phase = candidate
                        .match_provenance
                        .as_ref()
                        .map_or(auto_search::MatchPhase::Primary, |p| p.phase);
                    candidate.match_provenance =
                        Some(auto_search::MatchProvenance::automatic_source(phase));
                }
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
        let saved_provider = candidate
            .release_mapping
            .as_ref()
            .filter(|mapping| mapping.provider_heading().is_some());
        let provider_decision = provider.as_ref().map_or(Ok(None), |provider| {
            provider.choose(&candidate.title, candidate.is_batch)
        });
        let provider_decision = provider_decision.and_then(|current| {
            if saved_provider.is_some() && saved_provider != current.as_ref() {
                return Err("Episode mapping: provider decision changed during search".into());
            }
            Ok(current)
        });
        match provider_decision {
            Ok(Some(mapping)) => {
                let phase = candidate
                    .match_provenance
                    .as_ref()
                    .map_or(auto_search::MatchPhase::Primary, |p| p.phase);
                candidate.match_provenance = mapping
                    .provider_heading()
                    .map(|title| auto_search::MatchProvenance::provider_season(phase, title));
                candidate.release_mapping = Some(mapping);
                accepted.push(candidate);
                continue;
            }
            Err(reason) => {
                crate::services::logger::warn(
                    db,
                    crate::models::log::LogCategory::Grab,
                    "Automatic grab held: provider numbering",
                    &reason,
                )
                .await;
                continue;
            }
            Ok(None) => {}
        }
        let proof_resolver = upgrade_proofs
            .as_ref()
            .filter(|_| deferred)
            .map(|(lookups, _)| lookups.upgrade_proofs(root_pack))
            .unwrap_or(&automatic_sources);
        if deferred && !root_pack {
            match proof_resolver
                .prove_named_pack(db, detail, &candidate)
                .await
            {
                Ok(mapping) => {
                    let phase = candidate
                        .match_provenance
                        .as_ref()
                        .map_or(auto_search::MatchPhase::Primary, |p| p.phase);
                    candidate.match_provenance =
                        Some(auto_search::MatchProvenance::automatic_source(phase));
                    candidate.release_mapping = Some(mapping);
                    accepted.push(candidate);
                    continue;
                }
                Err(reason) => {
                    if let Some((_, diagnostics)) = upgrade_proofs.as_mut() {
                        diagnostics.push(reason);
                    }
                }
            }
        }
        match proof_resolver
            .apply(db, detail.id, target, &mut candidate)
            .await
        {
            Ok(true) => {
                let phase = candidate
                    .match_provenance
                    .as_ref()
                    .map_or(auto_search::MatchPhase::Primary, |p| p.phase);
                candidate.match_provenance =
                    Some(auto_search::MatchProvenance::automatic_source(phase));
                accepted.push(candidate);
                continue;
            }
            Err(reason) => {
                crate::services::logger::warn(
                    db,
                    crate::models::log::LogCategory::Grab,
                    "Automatic grab held: source evidence",
                    &reason,
                )
                .await;
                continue;
            }
            Ok(false) => {}
        }
        if let Some((lookups, diagnostics)) = upgrade_proofs.as_mut()
            && wanted.is_none()
            && !root_pack
            && candidate.indexer_id.is_none()
            && candidate.is_batch
            && (deferred
                || candidate
                    .match_provenance
                    .as_ref()
                    .is_some_and(|p| p.kind == auto_search::MatchKind::Verbatim))
            && auto_search::canonical_owner_supports_release(
                &candidate.title,
                &auto_search::collect_aliases(detail),
                &auto_search::SearchTarget::Episode(1),
            )
        {
            match lookups
                .upgrade_proofs(false)
                .prove_complete_entry(db, detail, &candidate)
                .await
                .and_then(|proof| ReleaseMapping::from_bound_source(&proof))
            {
                Ok(mapping) => {
                    candidate.release_mapping = Some(mapping);
                    accepted.push(candidate);
                    continue;
                }
                Err(reason) => diagnostics.push(reason),
            }
        }
        if deferred {
            continue;
        }
        let decision = context.as_ref().map_err(Clone::clone).and_then(|ctx| {
            if let Some(mapping) = pack_mappings.get(&candidate.info_hash.to_ascii_lowercase()) {
                mapping.validate()?;
                let expected = ctx.choose_published_entry_pack(
                    &mapping.episodes(),
                    // Entry-relative files may omit a season. Keep that
                    // distinction when rebuilding the saved decision.
                    match mapping.basis {
                        NumberingBasis::EntryRelative => None,
                        NumberingBasis::SeasonRelative => mapping.source_season,
                        NumberingBasis::SourceBound | NumberingBasis::ProviderSeason | NumberingBasis::AiredEntryTail => return Err("Episode mapping: published-pack evidence cannot replace an exact source decision".into()),
                    },
                )?;
                if !candidate.is_batch || expected != *mapping {
                    return Err(
                        "Episode mapping: published pack decision changed during search".into(),
                    );
                }
                Ok(expected)
            } else {
                let airing = verified_airing.filter(|_| {
                    candidate.match_provenance.as_ref().is_some_and(|proof| {
                        proof.kind == auto_search::MatchKind::Verbatim && !proof.phase.is_fallback()
                    })
                });
                ctx.choose_with_verified_airing(&candidate.title, candidate.is_batch, airing)
            }
        });
        match decision {
            Ok(mapping) if wanted.is_none_or(|n| mapping.contains_episode(n)) => {
                candidate.release_mapping = Some(mapping);
                accepted.push(candidate);
            }
            Ok(_) => {}
            Err(reason) => {
                if let Some((_, diagnostics)) = upgrade_proofs.as_mut() {
                    diagnostics.push(reason.clone());
                }
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
    let mut compatible = Vec::with_capacity(accepted.len());
    for candidate in accepted {
        match crate::models::grabbed_torrents::ensure_automatic_compatibility(
            db,
            &candidate.info_hash,
            detail.id,
            candidate.release_mapping.as_ref(),
        )
        .await
        {
            Ok(()) => compatible.push(candidate),
            Err(reason) => {
                crate::services::logger::warn(
                    db,
                    crate::models::log::LogCategory::Grab,
                    "Automatic candidate held: existing source assignment",
                    &reason,
                )
                .await;
            }
        }
    }
    compatible
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

    fn returner_context() -> NumberingContext {
        NumberingContext {
            anilist_id: 172192,
            tv_series: true,
            episode_count: None,
            cumulative_offset: 12,
            expected_season: 2,
            provider_plan: Some(MalEpisodePlan {
                anilist_id: 172192,
                unsupported: false,
                sources: vec![MalSource {
                    mal_id: 57612,
                    rules: vec![MappingRule {
                        source_first: 1,
                        source_last: 4,
                        target_entry: 172192,
                        target_first: 1,
                    }],
                }],
            }),
        }
    }

    #[test]
    fn explicit_heading_season_maps_real_returner_episode_and_preserves_file_validation() {
        let ctx = returner_context();
        let title = "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu 2nd Season - 03 [1080p CR WEB-DL AVC AAC][MultiSub][B459EA91]";
        let mapping = ctx
            .choose(title, false)
            .expect("the explicit source season identifies relative episode 3");
        assert_eq!(mapping.episodes(), [3]);
        assert_eq!(mapping.offset(), 0);
        assert_eq!(mapping.source_season, Some(2));
        assert_eq!(mapping.basis, NumberingBasis::SeasonRelative);
        assert_eq!(ctx.cumulative_offset, 12);
        let restored: ReleaseMapping =
            serde_json::from_str(&serde_json::to_string(&mapping).unwrap()).unwrap();
        assert_eq!(restored, mapping);
        for season in [None, Some(2)] {
            assert_eq!(
                restored
                    .resolve_file(media::EpisodeSpan::single(season, 3))
                    .unwrap()
                    .first,
                3
            );
        }
        for span in [
            media::EpisodeSpan::single(Some(1), 3),
            media::EpisodeSpan::single(Some(0), 3),
            media::EpisodeSpan::single(None, 4),
        ] {
            assert!(restored.resolve_file(span).is_err());
        }
        assert!(
            ctx.choose(
                "[Erai-raws] Kikansha no Mahou wa Tokubetsu desu 2nd Season - 05 [1080p CR WEB-DL AVC AAC][MultiSub]",
                false,
            )
            .is_err(),
            "the saved provider map proves only episodes 1 through 4"
        );
    }

    #[test]
    fn verified_aired_entry_tail_maps_only_the_next_named_episode() {
        let ctx = returner_context();
        let verified = VerifiedAiredEpisode {
            anilist_id: ctx.anilist_id,
            mal_id: 57612,
            episode: 5,
        };
        let title = "[Fixture] Night Voyage 2nd Season - 05 [1080p]";
        assert!(ctx.choose(title, false).is_err());
        let accepted = ctx
            .choose_with_verified_airing(title, false, Some(verified))
            .unwrap();
        assert_eq!(accepted.episodes(), [5]);
        assert_eq!(accepted.offset(), 0);
        assert_eq!(accepted.basis, NumberingBasis::AiredEntryTail);
        let persisted: ReleaseMapping =
            serde_json::from_str(&serde_json::to_string(&accepted).unwrap()).unwrap();
        assert!(persisted.requires_single_video());
        assert_eq!(
            persisted
                .resolve_named_file(&format!("{title}.mkv"))
                .unwrap()
                .first,
            5
        );
        for wrong_file in [
            "Night Voyage.mkv",
            "Night Voyage - 05.mkv",
            "Night Voyage S02E06.mkv",
            "Night Voyage S03E05.mkv",
            "Night Voyage S02E05E06.mkv",
        ] {
            assert!(
                persisted.resolve_named_file(wrong_file).is_err(),
                "{wrong_file}"
            );
        }
        for (held, is_batch) in [
            (title, true),
            ("Night Voyage 2nd Season - 06 [1080p]", false),
            ("Night Voyage S02E04E05 [1080p]", false),
            ("Night Voyage S03E05 [1080p]", false),
            ("Night Voyage - 05 [1080p]", false),
            ("Night Voyage [1080p]", false),
        ] {
            assert!(
                ctx.choose_with_verified_airing(held, is_batch, Some(verified))
                    .is_err(),
                "{held}"
            );
        }
        for wrong in [
            VerifiedAiredEpisode {
                anilist_id: 123,
                ..verified
            },
            VerifiedAiredEpisode {
                mal_id: 123,
                ..verified
            },
            VerifiedAiredEpisode {
                episode: 6,
                ..verified
            },
        ] {
            assert!(
                ctx.choose_with_verified_airing(title, false, Some(wrong))
                    .is_err()
            );
        }
        assert!(
            ctx.choose_published_entry_pack(&[1, 2, 3, 4, 5], Some(2))
                .is_err()
        );
    }

    #[test]
    fn verified_airing_cannot_replace_conflicting_or_incomplete_provider_evidence() {
        let make_rule = |source_first, source_last, target_entry, target_first| MappingRule {
            source_first,
            source_last,
            target_entry,
            target_first,
        };
        let owner = returner_context().anilist_id;
        let verified = VerifiedAiredEpisode {
            anilist_id: owner,
            mal_id: 57612,
            episode: 5,
        };
        let title = "Night Voyage S02E05 [1080p]";
        for rules in [
            vec![],
            vec![make_rule(2, 4, owner, 2)],
            vec![make_rule(1, 3, owner, 1), make_rule(4, 4, owner, 5)],
            vec![make_rule(1, 2, owner, 1), make_rule(4, 4, owner, 4)],
            vec![make_rule(1, 4, owner, 1), make_rule(5, 6, 123, 1)],
            vec![make_rule(1, 3, owner, 1)],
            vec![make_rule(1, 4, owner, 1), make_rule(6, 7, owner, 6)],
        ] {
            let mut ctx = returner_context();
            ctx.provider_plan.as_mut().unwrap().sources[0].rules = rules.clone();
            assert!(
                ctx.choose_with_verified_airing(title, false, Some(verified))
                    .is_err(),
                "{rules:?}"
            );
        }
        let mut ctx = returner_context();
        let additional_source = MalSource {
            mal_id: 123,
            rules: vec![make_rule(7, 7, owner, 7)],
        };
        ctx.provider_plan
            .as_mut()
            .unwrap()
            .sources
            .push(additional_source);
        assert!(
            ctx.choose_with_verified_airing(title, false, Some(verified))
                .is_err()
        );
    }

    #[test]
    fn a_multi_episode_online_series_can_extend_its_identity_map() {
        let mut ctx = returner_context();
        ctx.tv_series = false;
        ctx.cumulative_offset = 0;
        ctx.expected_season = 1;
        let airing = Some(VerifiedAiredEpisode {
            anilist_id: ctx.anilist_id,
            mal_id: 57612,
            episode: 5,
        });
        let mapping = ctx
            .choose_with_verified_airing("Example Online Series - 05 [1080p]", false, airing)
            .unwrap();
        assert_eq!(mapping.episodes(), [5]);
        assert!(!mapping.tv_series);
        assert!(mapping.requires_single_video());
        let mut special = media::EpisodeSpan::single(None, 5);
        special.special = true;
        assert!(mapping.resolve_file(special).is_err());
        assert!(
            ctx.choose_with_verified_airing("Example Online Series S00E05 [1080p]", false, airing)
                .is_err()
        );
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
        let mut tv = context(0);
        tv.expected_season = 2;
        let mapping = tv.choose("Show Season 2 - 03", false).unwrap();
        assert_eq!(mapping.episodes(), vec![3]);
        assert_eq!(
            context(0)
                .choose("[Keyword] Digimon Adventure 02 - 03 [BD 1080p]", false)
                .unwrap()
                .episodes(),
            [3],
            "a canonical numeric title is not an earlier episode range"
        );
        assert!(context(0).choose("Show OVA 01", false).is_err());
        let mut ova = context(0);
        ova.tv_series = false;
        let mapping = ova.choose("Show OVA 01", false).unwrap();
        let span = media::parse_episode_span("show ova 01.mkv").unwrap();
        assert_eq!(mapping.resolve_file(span).unwrap().first, 1);
        assert!(ova.choose("Show S00E01", false).is_err());
    }

    #[test]
    fn strong_heading_seasons_keep_the_episode_payload_separate() {
        for title in [
            "[G] Show S2 - 03 [1080p]",
            "[G] Show Season 2 - 03 [1080p]",
            "[G] Show 2nd Season - 03 [1080p]",
            "[組 Season 3] Show 2nd Season - 03 [1080p]",
            "[G] Show_2nd_Season_-_03_[1080p]",
            "Show Season 2 S02E03",
        ] {
            let mapping = context(12).choose(title, false).unwrap();
            assert_eq!(mapping.episodes(), [3], "{title}");
            assert_eq!(mapping.source_season, Some(2));
            assert_eq!(mapping.offset(), 0);
            assert_eq!(
                context(12).choose(title, true).unwrap().episodes(),
                [3],
                "a season digit is not a second batch episode: {title}"
            );
        }
        for title in [
            "Show Season 2 - 01-12",
            "Show 2nd Season - 01-12",
            "Show S2 - 01-12",
            "Show S02E01-12",
            "Show Season 2 - 01 - 12",
            "Show Season 2 - 01 ~ 12",
        ] {
            let mapping = context(12).choose(title, true).unwrap();
            assert_eq!(mapping.episodes(), (1..=12).collect::<Vec<_>>(), "{title}");
        }
        for title in ["Show Season 2 - 01-03", "Show S02E01-03"] {
            assert_eq!(
                context(12).choose(title, true).unwrap().episodes(),
                [1, 2, 3],
                "{title}"
            );
        }
        assert_eq!(
            context(12)
                .choose("Show Season 2 - 03 - Episode Title - 04 Batch", true)
                .unwrap()
                .episodes(),
            [3],
            "an episode title cannot manufacture another batch slot"
        );
    }

    #[test]
    fn only_structural_heading_markers_authorize_a_source_season() {
        for title in [
            "Show - 15",
            "[S2] Show - 03",
            "Show - 03 [Season 2]",
            "Show - 03 - 2nd Season",
            "Show - 03 - Episode Title S02E05",
            "Show Season 2 - 03 - Season 3",
            "Show Season 2 - 03 S03E05",
            "Show Season 3 S02E05",
            "Show Season 0 - 03",
            "Show Season 2 3rd Season - 03",
            "Show Season 2 E03 - 04",
            "Show Season 2 - 03 [",
            "Show Season 2 - 03 [)]",
            "Show Season 2 - 03 ]",
            "Show Season 2 01-12",
            "Show Season 2 Complete",
            "Show Part 2 - 03",
            "Show Cour II - 03",
            "Show II - 03",
            "86 - 02",
        ] {
            assert!(context(12).choose(title, false).is_err(), "{title}");
        }
        assert!(context(0).choose("Show Season 2 - 03", false).is_err());
    }

    #[test]
    fn marked_packs_keep_plain_ranges_and_known_complete_seasons() {
        for title in [
            "[Fixture] Different Voyage S02 01-12 (Batch BD 1080p)",
            "[Fixture] Night Voyage Season 2 01-12 (Batch BD 1080p)",
            "[Fixture] Night Voyage 2nd Season 01-12 (Batch BD 1080p)",
            "[Fixture] Different Voyage S02 01 - 12 (Batch BD 1080p)",
            "[Fixture] Night Voyage Season 2 01 - 12 (Batch BD 1080p)",
            "[Fixture] Night Voyage Season 2 01 ~ 12 (Batch BD 1080p)",
        ] {
            assert_eq!(
                context(12).choose(title, true).unwrap().episodes(),
                (1..=12).collect::<Vec<_>>(),
                "{title}"
            );
            assert!(context(12).choose(title, false).is_err(), "{title}");
        }
        let mut monogatari = context(0);
        monogatari.episode_count = Some(26);
        for title in [
            "[Fixture] Monogatari Series Second Season 01-26 (Batch BD 1080p)",
            "[a-S_subs] Monogatari Second Season 1-26 (BDRip 1080p x265 AC3)",
        ] {
            let mapping = monogatari.choose(title, true).unwrap();
            assert_eq!(mapping.episodes(), (1..=26).collect::<Vec<_>>());
            assert_eq!(mapping.basis, NumberingBasis::EntryRelative);
        }
        assert!(
            monogatari
                .choose("Monogatari Second Season 01 - 26 (Batch BD 1080p)", true)
                .is_err(),
            "an ambiguous boundary must not truncate a pack to episode 26"
        );
        for title in [
            "Show Season 2 Complete [BD 1080p]",
            "Show S02 [Batch BD 1080p]",
            "Show 2nd Season [BD 1080p]",
        ] {
            assert_eq!(
                context(12).choose(title, true).unwrap().episodes(),
                (1..=13).collect::<Vec<_>>(),
                "the marked pack's known count remains the existing contract: {title}"
            );
            assert!(context(12).choose(title, false).is_err());
        }
        for title in [
            "Show 3rd Season Complete [BD 1080p]",
            "Show [Season 2] Complete [BD 1080p]",
            "[S2] Show Complete [BD 1080p]",
            "Show [BD 1080p]",
            "Show Season 2 Season 3 Complete [BD 1080p]",
        ] {
            assert!(context(12).choose(title, true).is_err(), "{title}");
        }
        assert!(
            returner_context()
                .choose("Show Season 2 Complete", true)
                .is_err()
        );
        let mut partial_provider = returner_context();
        partial_provider.episode_count = Some(12);
        assert!(
            partial_provider
                .choose("Show Season 2 Complete", true)
                .is_err(),
            "a known count cannot fill gaps in the provider's four-episode map"
        );
    }

    #[test]
    fn named_file_heading_guards_preserve_supported_multi_episode_spans() {
        for (tail, expected) in [
            ("S02E03E04", vec![3, 4]),
            ("S02E03-04", vec![3, 4]),
            ("S02E03-E04", vec![3, 4]),
            ("S02E03E04E05", vec![3, 4, 5]),
            ("S02E03-04-05", vec![3, 4, 5]),
            ("S02E03-E04-E05", vec![3, 4, 5]),
        ] {
            let title = format!("[G] Show {tail} [1080p]");
            let mapping = context(12).choose(&title, false).unwrap();
            assert_eq!(mapping.episodes(), expected, "{title}");
            let mapped = mapping.resolve_named_file(&format!("{title}.mkv")).unwrap();
            assert_eq!((mapped.first, mapped.last), (3, *expected.last().unwrap()));
        }
        let mapping = returner_context()
            .choose("Show 2nd Season - 03", false)
            .unwrap();
        for filename in [
            "Show 2nd Season - 03.mkv",
            "Show S02E03.mkv",
            "Show - 03.mkv",
        ] {
            assert_eq!(mapping.resolve_named_file(filename).unwrap().first, 3);
        }
        for filename in [
            "Show 3rd Season - 03.mkv",
            "Show Season 2 - 03 - Season 3.mkv",
            "Show - 03 - Episode Title S02E05.mkv",
            "Show Season 2 E03 - 04.mkv",
            "Show S01E03.mkv",
            "Show S00E03.mkv",
            "Show - 04.mkv",
            "unknown.mkv",
        ] {
            assert!(mapping.resolve_named_file(filename).is_err(), "{filename}");
        }
        for season in [34, 99] {
            let mut ctx = context(12);
            ctx.expected_season = season;
            let name = format!("Show S{season:02}E03.mkv");
            let mapping = ctx.choose(&name, false).unwrap();
            assert_eq!(mapping.source_season, Some(season));
            assert_eq!(mapping.resolve_named_file(&name).unwrap().first, 3);
        }
    }

    #[test]
    fn missing_source_cache_cannot_restore_ambiguous_root_relative_fallback() {
        let mut ctx = context(0);
        ctx.anilist_id = 889;
        ctx.episode_count = Some(12);
        for title in ["Black Lagoon - 03", "Black Lagoon Complete"] {
            assert!(ctx.choose(title, title.contains("Complete")).is_err());
        }
        assert!(
            ctx.choose_published_entry_pack(&(1..=12).collect::<Vec<_>>(), None)
                .is_err()
        );
        assert_eq!(
            ctx.choose("Black Lagoon S01E03", false).unwrap().episodes(),
            [3]
        );
        assert!(
            ctx.choose_published_entry_pack(&(1..=12).collect::<Vec<_>>(), Some(1))
                .is_ok()
        );
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
    fn published_complete_files_prove_relative_numbering_without_clearing_the_offset() {
        for (offset, count) in [(19, 11), (68, 4), (79, 6)] {
            let mut ctx = context(offset);
            ctx.expected_season = 1;
            ctx.episode_count = Some(count);
            assert!(ctx.choose("[MTBB] Entry (BD 1080p)", true).is_err());
            let mapping = ctx
                .choose_published_entry_pack(&(1..=count).collect::<Vec<_>>(), None)
                .unwrap();
            assert_eq!(mapping.episodes(), (1..=count).collect::<Vec<_>>());
            assert_eq!(ctx.cumulative_offset, offset);
            assert_eq!(mapping.offset(), 0);
            assert_eq!(
                mapping
                    .resolve_file(media::EpisodeSpan::single(None, 1))
                    .unwrap()
                    .first,
                1
            );
            assert!(
                mapping
                    .resolve_file(media::EpisodeSpan::single(None, count + 1))
                    .is_err()
            );
        }
    }

    #[test]
    fn published_pack_evidence_refuses_partial_duplicate_extra_or_unknown_coverage() {
        let ctx = context(24);
        for numbers in [
            vec![],
            vec![1],
            (2..=13).collect(),
            (1..=14).collect(),
            vec![1; 13],
        ] {
            assert!(
                ctx.choose_published_entry_pack(&numbers, None).is_err(),
                "{numbers:?}"
            );
        }
        let full: Vec<_> = (1..=13).collect();
        assert!(ctx.choose_published_entry_pack(&full, Some(1)).is_err());
        assert!(ctx.choose_published_entry_pack(&full, Some(0)).is_err());
        assert!(ctx.choose_published_entry_pack(&full, Some(2)).is_ok());
        for count in [None, Some(0), Some(1), Some(MAX_PLAUSIBLE_EPISODES + 1)] {
            let mut unknown = context(24);
            unknown.episode_count = count;
            assert!(unknown.choose_published_entry_pack(&full, None).is_err());
        }
    }

    #[test]
    fn published_pack_does_not_override_a_conflicting_provider_identity() {
        let mut ctx = context(19);
        ctx.episode_count = Some(12);
        ctx.provider_plan = Some(MalEpisodePlan {
            anilist_id: 10,
            unsupported: false,
            sources: vec![MalSource {
                mal_id: 20,
                rules: vec![MappingRule {
                    source_first: 1,
                    source_last: 12,
                    target_entry: 99,
                    target_first: 1,
                }],
            }],
        });
        assert!(
            ctx.choose_published_entry_pack(&(1..=12).collect::<Vec<_>>(), None)
                .is_err()
        );
        ctx.provider_plan.as_mut().unwrap().sources[0].rules[0].target_entry = 10;
        assert!(
            ctx.choose_published_entry_pack(&(1..=12).collect::<Vec<_>>(), None)
                .is_ok()
        );
        ctx.provider_plan.as_mut().unwrap().sources[0].rules[0].target_first = 2;
        assert!(
            ctx.choose_published_entry_pack(&(1..=12).collect::<Vec<_>>(), None)
                .is_err()
        );
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

    #[test]
    fn source_heading_enforces_the_existing_byte_limit_without_truncation() {
        let prefix = "Show S02E03 [";
        let suffix = "].mkv";
        let title = format!(
            "{prefix}{}{suffix}",
            "x".repeat(media::MAX_RELEASE_TITLE_BYTES - prefix.len() - suffix.len())
        );
        assert_eq!(title.len(), media::MAX_RELEASE_TITLE_BYTES);
        let ctx = context(12);
        let mapping = ctx.choose(&title, false).unwrap();
        assert_eq!(mapping.episodes(), [3]);
        assert_eq!(mapping.resolve_named_file(&title).unwrap().first, 3);
        for oversized in [
            format!("{title}x"),
            "あ".repeat(media::MAX_RELEASE_TITLE_BYTES / 3 + 1),
            "Season 1 - 01 ".repeat(media::MAX_RELEASE_TITLE_BYTES),
        ] {
            assert!(oversized.len() > media::MAX_RELEASE_TITLE_BYTES);
            assert!(
                ctx.choose(&oversized, true)
                    .unwrap_err()
                    .contains("maximum length")
            );
            assert!(
                mapping
                    .resolve_named_file(&oversized)
                    .unwrap_err()
                    .contains("maximum length")
            );
        }
        let repeated = "Season 1 - 01 ".repeat(50);
        assert!(repeated.len() <= media::MAX_RELEASE_TITLE_BYTES);
        assert!(ctx.choose(&repeated, true).is_err());
    }
}

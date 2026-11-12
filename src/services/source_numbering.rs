//! Finite source-numbering domains and exact-source review evidence.
//! A source title or a library offset cannot create a source decision.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use crate::models::{metadata_cache, series};
use crate::services::anilist::{AnimeDetail, MAX_PLAUSIBLE_EPISODES};
use crate::services::episode_mapping::{self, MappedFile, MappingRule};
use crate::services::{
    anibridge, anime_relations, auto_search, library_link, media, post_processing,
};

#[cfg(test)]
#[path = "source_numbering_tests.rs"]
pub(crate) mod tests;

fn fail(message: &str) -> String {
    format!("Episode mapping: {message}")
}

pub(crate) fn normalized_hash(hash: &str) -> Result<String, String> {
    if hash.len() != 40 || !hash.bytes().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(fail("a source decision needs an exact BT v1 infohash"));
    }
    Ok(hash.to_ascii_lowercase())
}

fn fingerprint(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
        .map_err(|error| fail(&format!("cannot encode source evidence: {error}")))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainEntry {
    pub anilist_id: i64,
    pub title: String,
    pub format: String,
    pub episodes: Option<i32>,
    /// Canonical episodes covered by a valid directional provider plan.
    /// None means no provider episode plan is declared for this entry.
    pub provider_episodes: Option<Vec<i32>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceDomain {
    pub version: u32,
    pub root_anilist_id: i64,
    pub root_title: String,
    pub root_aliases: Vec<String>,
    pub source_season: i32,
    pub rules: Vec<MappingRule>,
    pub entries: Vec<DomainEntry>,
    pub rules_fingerprint: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DomainChoice {
    pub root_anilist_id: i64,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundFile {
    pub name: String,
    pub source_first: i32,
    pub source_last: i32,
    pub source_season: Option<i32>,
    pub target_first: i32,
    pub target_last: i32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReviewFile {
    pub name: String,
    pub source_first: Option<i32>,
    pub source_last: Option<i32>,
    pub target_anilist_id: Option<i64>,
    pub target_first: Option<i32>,
    pub target_last: Option<i32>,
    pub eligible: bool,
    pub reason: String,
}

/// Construct this from server-fetched, same-hash published filenames.
/// Confirmation must refetch/rebuild and compare fingerprint; never accept
/// a client-supplied domain or file manifest as review evidence.
#[derive(Clone, Debug, Serialize)]
pub struct ReviewPreview {
    pub info_hash: String,
    pub root_anilist_id: i64,
    pub root_title: String,
    pub target_anilist_id: i64,
    pub target_title: String,
    pub fingerprint: String,
    pub files: Vec<ReviewFile>,
    domain: SourceDomain,
    published_files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundSourceProof {
    pub version: u32,
    pub info_hash: String,
    pub target_anilist_id: i64,
    pub domain: SourceDomain,
    pub publication_fingerprint: String,
    pub published_files: Vec<String>,
    pub selected_files: Vec<BoundFile>,
}

pub async fn domain_choices(
    db: &SqlitePool,
    target_anilist_id: i64,
) -> Result<Vec<DomainChoice>, String> {
    let mut choices = Vec::new();
    for root in anime_relations::finite_source_roots(target_anilist_id) {
        if let Ok(detail) = cached_detail(db, root).await {
            choices.push(DomainChoice {
                root_anilist_id: root,
                title: display_title(&detail),
            });
        }
    }
    Ok(choices)
}

fn display_title(detail: &AnimeDetail) -> String {
    if detail.title_english.is_empty() {
        detail.title_romaji.clone()
    } else {
        detail.title_english.clone()
    }
}

async fn cached_detail(db: &SqlitePool, id: i64) -> Result<AnimeDetail, String> {
    let mut cached = metadata_cache::get_by_provider_id(db, id)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(entry) = series::get_by_anilist_id(db, id)
        .await
        .map_err(|error| error.to_string())?
    {
        let tracked = metadata_cache::get_by_series_id(db, entry.id)
            .await
            .map_err(|error| error.to_string())?;
        if tracked.as_ref().is_some_and(|tracked| {
            cached
                .as_ref()
                .is_none_or(|provider| tracked.cached_at > provider.cached_at)
        }) {
            cached = tracked;
        }
    }
    let cached = cached.ok_or_else(|| fail(&format!("AniList {id} needs cached metadata")))?;
    if cached.provider_id != id || cached.detail.id != id || cached.is_stale {
        return Err(fail(
            "source metadata identity is missing, changed, or stale",
        ));
    }
    Ok(cached.detail)
}

pub async fn load_domain(db: &SqlitePool, root_anilist_id: i64) -> Result<SourceDomain, String> {
    let root = cached_detail(db, root_anilist_id).await?;
    let count = root
        .episodes
        .filter(|count| (1..=MAX_PLAUSIBLE_EPISODES).contains(count))
        .ok_or_else(|| fail("the source root needs a finite authoritative episode count"))?;
    let mut rules = anime_relations::finite_source_rules(root_anilist_id)?;
    rules.push(MappingRule {
        source_first: 1,
        source_last: count,
        target_entry: root_anilist_id,
        target_first: 1,
    });
    rules.sort_by_key(|rule| {
        (
            rule.source_first,
            rule.source_last,
            rule.target_entry,
            rule.target_first,
        )
    });
    rules.dedup();
    let ids: BTreeSet<_> = rules.iter().map(|rule| rule.target_entry).collect();
    let mut entries = Vec::new();
    for id in ids {
        let detail = if id == root_anilist_id {
            root.clone()
        } else {
            cached_detail(db, id).await?
        };
        let provider_episodes = if detail.id_mal.is_some_and(|id| id > 0) {
            anibridge::mal_episode_plan(id, detail.id_mal)
                .await?
                .map(|plan| plan.source_slots().map(|slots| slots.into_keys().collect()))
                .transpose()?
        } else {
            None
        };
        entries.push(DomainEntry {
            anilist_id: id,
            title: display_title(&detail),
            format: detail.format,
            episodes: detail.episodes,
            provider_episodes,
        });
    }
    let mut domain = SourceDomain {
        version: 1,
        root_anilist_id,
        root_title: display_title(&root),
        root_aliases: auto_search::collect_aliases(&root),
        source_season: auto_search::infer_season_from_detail(&root).max(1),
        rules,
        entries,
        rules_fingerprint: String::new(),
    };
    domain.rules_fingerprint = domain.content_fingerprint()?;
    domain.validate()?;
    Ok(domain)
}

pub fn prepare_review(
    domain: SourceDomain,
    info_hash: &str,
    published_files: &[String],
    target_anilist_id: i64,
) -> Result<ReviewPreview, String> {
    domain.validate()?;
    let info_hash = normalized_hash(info_hash)?;
    let target = domain
        .entries
        .iter()
        .find(|entry| entry.anilist_id == target_anilist_id)
        .ok_or_else(|| fail("the selected entry is outside this finite source domain"))?;
    if published_files.is_empty() || published_files.len() > 4096 {
        return Err(fail("source file evidence is empty or exceeds its limit"));
    }
    let mut published_files = published_files.to_vec();
    published_files.sort();
    let mut names = HashSet::new();
    for name in &published_files {
        post_processing::validate_relative_path_fragment(name)?;
        if name.len() > 4096 || !names.insert(name) {
            return Err(fail("source filenames are too long or duplicated"));
        }
    }
    let digest = fingerprint(&(1, &info_hash, &domain, &published_files, target_anilist_id))?;
    let files = published_files
        .iter()
        .map(|name| review_file(&domain, name, target_anilist_id))
        .collect();
    Ok(ReviewPreview {
        info_hash,
        root_anilist_id: domain.root_anilist_id,
        root_title: domain.root_title.clone(),
        target_anilist_id,
        target_title: target.title.clone(),
        fingerprint: digest,
        files,
        domain,
        published_files,
    })
}

impl SourceDomain {
    fn content_fingerprint(&self) -> Result<String, String> {
        fingerprint(&(
            self.version,
            self.root_anilist_id,
            &self.root_title,
            &self.root_aliases,
            self.source_season,
            &self.rules,
            &self.entries,
        ))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.root_anilist_id <= 0
            || !(1..=99).contains(&self.source_season)
            || self.root_aliases.is_empty()
            || self.rules.is_empty()
            || self.rules.len() > 1024
            || self.rules_fingerprint != self.content_fingerprint()?
        {
            return Err(fail("invalid finite source-domain evidence"));
        }
        let mut ids = HashSet::new();
        for entry in &self.entries {
            if entry.anilist_id <= 0
                || !ids.insert(entry.anilist_id)
                || entry
                    .episodes
                    .is_some_and(|count| !(1..=MAX_PLAUSIBLE_EPISODES).contains(&count))
            {
                return Err(fail("invalid source-domain owner metadata"));
            }
            if let Some(episodes) = &entry.provider_episodes
                && (episodes.windows(2).any(|pair| pair[0] >= pair[1])
                    || episodes
                        .iter()
                        .any(|episode| !(1..=MAX_PLAUSIBLE_EPISODES).contains(episode)))
            {
                return Err(fail("invalid canonical provider episode evidence"));
            }
        }
        if !ids.contains(&self.root_anilist_id) {
            return Err(fail("source root metadata is unavailable"));
        }
        for rule in &self.rules {
            episode_mapping::resolve_episode(&[*rule], rule.source_first)
                .map_err(|error| error.to_string())?;
            episode_mapping::resolve_episode(&[*rule], rule.source_last)
                .map_err(|error| error.to_string())?;
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.anilist_id == rule.target_entry)
                .ok_or_else(|| fail("source rule has no verified destination metadata"))?;
            for raw in rule.source_first..=rule.source_last {
                let mapped = episode_mapping::resolve_episode(&self.rules, raw)
                    .map_err(|error| error.to_string())?;
                if entry.episodes.is_some_and(|count| {
                    rule.target_first + rule.source_last - rule.source_first > count
                }) || mapped.entry == 0
                {
                    return Err(fail("source rule exceeds the destination metadata bounds"));
                }
            }
        }
        Ok(())
    }
}

fn review_file(domain: &SourceDomain, name: &str, target: i64) -> ReviewFile {
    let mut row = ReviewFile {
        name: name.into(),
        source_first: None,
        source_last: None,
        target_anilist_id: None,
        target_first: None,
        target_last: None,
        eligible: false,
        reason: String::new(),
    };
    let result = (|| -> Result<MappedFile, String> {
        if !post_processing::is_video_file(name) {
            return Err(fail("not an importable video"));
        }
        let basename = Path::new(name)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| fail("invalid source filename"))?;
        let (span, season) =
            crate::services::release_numbering::validated_source_file_span(basename)?;
        row.source_first = Some(span.first);
        row.source_last = Some(span.last);
        if season.is_some_and(|season| season != domain.source_season) {
            return Err(fail("file names another source season"));
        }
        for component in Path::new(name)
            .parent()
            .into_iter()
            .flat_map(Path::components)
        {
            let folder = component
                .as_os_str()
                .to_str()
                .ok_or_else(|| fail("invalid source folder"))?;
            if crate::services::release_numbering::validated_source_folder_season(folder)?
                .is_some_and(|season| season != domain.source_season)
            {
                return Err(fail("folder names another source season"));
            }
            let parts = auto_search::part_cour_numbers(folder);
            let expected: HashSet<_> = domain
                .root_aliases
                .iter()
                .flat_map(|alias| auto_search::part_cour_numbers(alias))
                .collect();
            if !parts.is_empty() && parts != expected {
                return Err(fail("folder names another Part/Cour source"));
            }
        }
        let root = domain
            .entries
            .iter()
            .find(|entry| entry.anilist_id == domain.root_anilist_id)
            .ok_or_else(|| fail("missing source owner"))?;
        if media::is_non_episodic_file(&basename.to_ascii_lowercase())
            || (media::is_tv_format(&root.format)
                && (span.special || media::path_names_specials(Path::new(name))))
        {
            return Err(fail("extra or special outside the main source domain"));
        }
        let title = library_link::extract_anime_title(basename)
            .map(|title| auto_search::normalize_title(&title))
            .ok_or_else(|| fail("file has no verifiable source title"))?;
        if !domain
            .root_aliases
            .iter()
            .any(|alias| auto_search::normalize_title(alias) == title)
        {
            return Err(fail("file does not name the full canonical source root"));
        }
        let source_parts = auto_search::part_cour_numbers(basename);
        let root_parts: HashSet<_> = domain
            .root_aliases
            .iter()
            .flat_map(|alias| auto_search::part_cour_numbers(alias))
            .collect();
        if !source_parts.is_empty() && source_parts != root_parts {
            return Err(fail("file names another Part/Cour source identity"));
        }
        let mapped = episode_mapping::resolve_file(&domain.rules, span.first, span.last)
            .map_err(|error| error.to_string())?;
        let owner = domain
            .entries
            .iter()
            .find(|entry| entry.anilist_id == mapped.entry)
            .ok_or_else(|| fail("unknown canonical destination"))?;
        if let Some(provider) = &owner.provider_episodes
            && (mapped.first..=mapped.last).any(|episode| !provider.contains(&episode))
        {
            return Err(fail(
                "provider evidence does not cover this canonical episode",
            ));
        }
        Ok(mapped)
    })();
    match result {
        Ok(mapped) => {
            row.target_anilist_id = Some(mapped.entry);
            row.target_first = Some(mapped.first);
            row.target_last = Some(mapped.last);
            row.eligible = mapped.entry == target;
            row.reason = if row.eligible {
                "Verified finite source mapping".into()
            } else {
                "Belongs to another release".into()
            };
        }
        Err(error) => row.reason = error,
    }
    row
}

impl ReviewPreview {
    pub fn confirm(&self, selected_names: &[String]) -> Result<BoundSourceProof, String> {
        if selected_names.is_empty() {
            return Err(fail("select at least one verified source file"));
        }
        let mut selected = Vec::new();
        let mut names = HashSet::new();
        let mut destinations = HashSet::new();
        let mut offsets = HashSet::new();
        let mut seasons = HashSet::new();
        for name in selected_names {
            if !names.insert(name) {
                return Err(fail("duplicate selected source file"));
            }
            let row = self
                .files
                .iter()
                .find(|file| file.name == *name && file.eligible)
                .ok_or_else(|| fail("a selected file has no verified mapping to this entry"))?;
            let basename = Path::new(name)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| fail("invalid source filename"))?;
            let (span, season) =
                crate::services::release_numbering::validated_source_file_span(basename)?;
            if let Some(season) = season {
                seasons.insert(season);
            }
            let (first, last) = row
                .target_first
                .zip(row.target_last)
                .ok_or_else(|| fail("missing target range"))?;
            for episode in first..=last {
                if !destinations.insert(episode) {
                    return Err(fail("selected files overlap a canonical episode"));
                }
            }
            offsets.insert(span.first - first);
            selected.push(BoundFile {
                name: name.clone(),
                source_first: span.first,
                source_last: span.last,
                source_season: season,
                target_first: first,
                target_last: last,
            });
        }
        if offsets.len() != 1 || seasons.len() > 1 {
            return Err(fail("selected files mix source numbering schemes"));
        }
        selected.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(BoundSourceProof {
            version: 1,
            info_hash: self.info_hash.clone(),
            target_anilist_id: self.target_anilist_id,
            domain: self.domain.clone(),
            publication_fingerprint: self.fingerprint.clone(),
            published_files: self.published_files.clone(),
            selected_files: selected,
        })
    }
}

impl BoundSourceProof {
    /// A picker can narrow a reviewed release, never expand its assignment.
    pub fn narrow_to(&self, selected_names: &[String]) -> Result<Self, String> {
        self.validate()?;
        let names: HashSet<_> = selected_names.iter().map(String::as_str).collect();
        if names.is_empty() || names.len() != selected_names.len() {
            return Err(fail("a narrowed source selection is empty or duplicated"));
        }
        let mut narrowed = self.clone();
        narrowed
            .selected_files
            .retain(|file| names.contains(file.name.as_str()));
        if narrowed.selected_files.len() != names.len() {
            return Err(fail("a narrowed selection adds an unreviewed source file"));
        }
        narrowed.validate()?;
        Ok(narrowed)
    }

    pub fn is_within_review(&self, reviewed: &Self) -> bool {
        self.validate().is_ok()
            && reviewed.validate().is_ok()
            && self.info_hash == reviewed.info_hash
            && self.target_anilist_id == reviewed.target_anilist_id
            && self.domain == reviewed.domain
            && self.publication_fingerprint == reviewed.publication_fingerprint
            && self.published_files == reviewed.published_files
            && self
                .selected_files
                .iter()
                .all(|file| reviewed.selected_files.contains(file))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(fail("unsupported source-proof version"));
        }
        let preview = prepare_review(
            self.domain.clone(),
            &self.info_hash,
            &self.published_files,
            self.target_anilist_id,
        )?;
        let names: Vec<_> = self
            .selected_files
            .iter()
            .map(|file| file.name.clone())
            .collect();
        if preview.confirm(&names)? != *self {
            return Err(fail("saved source proof differs from its publication"));
        }
        Ok(())
    }

    pub fn episodes(&self) -> Vec<i32> {
        self.selected_files
            .iter()
            .flat_map(|file| file.target_first..=file.target_last)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn source_span(&self) -> Result<(i32, i32, i32), String> {
        self.validate()?;
        let first = self
            .selected_files
            .iter()
            .map(|file| file.source_first)
            .min()
            .ok_or_else(|| fail("empty source proof"))?;
        let last = self
            .selected_files
            .iter()
            .map(|file| file.source_last)
            .max()
            .ok_or_else(|| fail("empty source proof"))?;
        let offset = self.selected_files[0].source_first - self.selected_files[0].target_first;
        Ok((first, last, first - offset))
    }

    /// Exact relative filenames; caller index ordering has no meaning.
    pub fn validate_files(&self, info_hash: &str, files: &[(&str, bool)]) -> Result<(), String> {
        self.validate()?;
        if normalized_hash(info_hash)? != self.info_hash {
            return Err(fail("downloaded source hash differs from the saved proof"));
        }
        let expected: HashSet<_> = self
            .selected_files
            .iter()
            .map(|file| file.name.as_str())
            .collect();
        let mut seen = HashSet::new();
        let mut actual = HashSet::new();
        for &(name, wanted) in files {
            if !seen.insert(name) {
                return Err(fail("duplicate downloaded source filename"));
            }
            if wanted && post_processing::is_video_file(name) {
                if !expected.contains(name) {
                    return Err(fail("wanted file has no saved source assignment"));
                }
                self.resolve_file(name)?;
                actual.insert(name);
            }
        }
        if actual != expected {
            return Err(fail(
                "downloaded files do not contain the exact reviewed selection",
            ));
        }
        let published: HashSet<_> = self.published_files.iter().map(String::as_str).collect();
        if seen != published {
            return Err(fail(
                "downloaded file manifest differs from the reviewed publication",
            ));
        }
        Ok(())
    }

    pub fn selected_indices(&self, filenames: &[String]) -> Result<Vec<usize>, String> {
        self.validate()?;
        let selected: HashSet<_> = self
            .selected_files
            .iter()
            .map(|file| file.name.as_str())
            .collect();
        let mut seen = HashSet::new();
        let mut indices = Vec::new();
        for (index, name) in filenames.iter().enumerate() {
            if !seen.insert(name.as_str()) {
                return Err(fail("duplicate download filename"));
            }
            if selected.contains(name.as_str()) {
                indices.push(index);
            }
        }
        let published: HashSet<_> = self.published_files.iter().map(String::as_str).collect();
        if indices.len() != selected.len() || seen != published {
            return Err(fail("download file manifest changed before selection"));
        }
        Ok(indices)
    }

    pub fn resolve_file(&self, filename: &str) -> Result<MappedFile, String> {
        let selected = self
            .selected_files
            .iter()
            .find(|file| file.name == filename)
            .ok_or_else(|| fail("file has no saved source assignment"))?;
        let basename = Path::new(filename)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| fail("invalid source filename"))?;
        let (span, season) =
            crate::services::release_numbering::validated_source_file_span(basename)?;
        if (span.first, span.last, season)
            != (
                selected.source_first,
                selected.source_last,
                selected.source_season,
            )
        {
            return Err(fail("source filename numbering changed"));
        }
        let mapped = episode_mapping::resolve_file(&self.domain.rules, span.first, span.last)
            .map_err(|error| error.to_string())?;
        if (mapped.entry, mapped.first, mapped.last)
            != (
                self.target_anilist_id,
                selected.target_first,
                selected.target_last,
            )
        {
            return Err(fail("saved source destination changed"));
        }
        Ok(mapped)
    }
}

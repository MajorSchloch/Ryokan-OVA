//! Exact source evidence for packs with provider episode names in the files.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex_lite::Regex;

use super::*;
use crate::services::anibridge::AcquisitionSeasonMatch;
use crate::services::nyaa::{self, PublishedFiles};
use crate::services::release_numbering::validated_source_file_span;

static EPISODE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^s\d{1,2}e\d{1,4}\s*[-._ ]\s*(.+)$").unwrap());
static CRC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s*\[[0-9a-f]{8}\]$").unwrap());
static SEASON_PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bs(\d{1,2})p(\d{1,2})\b").unwrap());

fn normalized_name(name: &str) -> String {
    auto_search::normalize_title(
        &name
            .replace(['`', '’'], "'")
            .replace(['[', ']', '(', ')', '{', '}'], " "),
    )
}

fn name_from_file(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let title = EPISODE_NAME.captures(stem)?.get(1)?.as_str();
    let title = normalized_name(&CRC.replace(title, ""));
    (!title.is_empty() && !title.starts_with("episode ") && !title.starts_with("ep "))
        .then_some(title)
}

pub(crate) fn names_owner(title: &str, aliases: &[String]) -> bool {
    let title = title.trim_start();
    let heading = title
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map_or(title, |(_, heading)| heading);
    let title = format!(" {} ", normalized_name(heading));
    aliases.iter().any(|alias| {
        let alias = normalized_name(alias);
        !alias.is_empty() && title.contains(&format!(" {alias} "))
    })
}

fn is_main(path: &Path) -> bool {
    !automatic::is_explicit_sample(path) && auto_search::main_video_file(path, true)
}

fn check_part(domain: &SourceDomain, label: &str) -> Result<(), String> {
    let own: BTreeSet<_> = domain
        .root_aliases
        .iter()
        .flat_map(|title| auto_search::part_cour_numbers(title))
        .collect();
    let mut parts: BTreeSet<_> = auto_search::part_cour_numbers(label).into_iter().collect();
    for marker in SEASON_PART.captures_iter(label) {
        if marker[1].parse::<i32>() != Ok(domain.source_season) {
            return Err(fail("source label names another source season"));
        }
        parts.extend(marker[2].parse::<i32>().ok());
    }
    if !parts.is_empty() && parts != own {
        return Err(fail("source label names another part or cour"));
    }
    Ok(())
}

pub(super) fn verify_filename(
    domain: &SourceDomain,
    name: &str,
    span: media::EpisodeSpan,
    season: Option<i32>,
    evidence: &NamedPackEvidence,
) -> Result<(), String> {
    let path = Path::new(name);
    if !is_main(path)
        || span.special
        || span.first != span.last
        || season != Some(domain.source_season)
    {
        return Err(fail(
            "episode-title proof needs one explicit season and episode per file",
        ));
    }
    for component in path.parent().into_iter().flat_map(Path::components) {
        let folder = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| fail("invalid source folder"))?;
        if crate::services::release_numbering::validated_source_folder_season(folder)?
            .is_some_and(|number| number != domain.source_season)
        {
            return Err(fail("folder names another source season"));
        }
        check_part(domain, folder)?;
    }
    let mapped = episode_mapping::resolve_episode(&domain.rules, span.first)
        .map_err(|error| error.to_string())?;
    let title =
        name_from_file(path).ok_or_else(|| fail("source file has no exact episode title"))?;
    if mapped.entry != domain.root_anilist_id
        || evidence.episode_titles.get((mapped.number - 1) as usize) != Some(&title)
    {
        return Err(fail(
            "source filename differs from the provider episode title",
        ));
    }
    Ok(())
}

pub(super) fn complete_selection(
    preview: &ReviewPreview,
    evidence: &NamedPackEvidence,
) -> Result<Vec<String>, String> {
    let domain = &preview.domain;
    let [owner] = domain.entries.as_slice() else {
        return Err(fail("episode-title proof needs one canonical owner"));
    };
    let count = owner.episodes.unwrap_or(0);
    if owner.anilist_id != domain.root_anilist_id
        || preview.target_anilist_id != owner.anilist_id
        || count < 2
        || evidence.episode_titles.len() != count as usize
        || evidence
            .episode_titles
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != count as usize
        || !names_owner(&evidence.publication_title, &domain.root_aliases)
    {
        return Err(fail(
            "episode-title proof does not identify one complete entry",
        ));
    }
    check_part(domain, &evidence.publication_title)?;
    let mut names = Vec::new();
    let mut episodes = BTreeSet::new();
    for file in preview
        .files
        .iter()
        .filter(|file| is_main(Path::new(&file.name)))
    {
        let Some(episode) = file.target_first else {
            return Err(fail("episode-title proof does not cover a main video"));
        };
        if !file.eligible || file.target_last != Some(episode) || !episodes.insert(episode) {
            return Err(fail(
                "episode-title proof has overlapping or conflicting files",
            ));
        }
        names.push(file.name.clone());
    }
    if !episodes.into_iter().eq(1..=count) {
        return Err(fail(
            "episode-title proof does not cover the complete relative range",
        ));
    }
    Ok(names)
}

/// Build a receipt from the complete published manifest, a directional
/// number map, and fresh provider titles independent of the publication.
pub(super) async fn prove(
    db: &SqlitePool,
    detail: &AnimeDetail,
    hash: &str,
    url: &str,
    publication: &PublishedFiles,
) -> Result<BoundSourceProof, String> {
    let count = detail
        .episodes
        .filter(|count| (2..=MAX_PLAUSIBLE_EPISODES).contains(count))
        .ok_or_else(|| fail("episode-title proof needs a finite episode count"))?;
    let aliases = auto_search::collect_aliases(detail);
    if !media::is_tv_format(&detail.format) || !names_owner(&publication.title, &aliases) {
        return Err(fail(
            "publication does not name the complete canonical owner",
        ));
    }
    let mut season = None;
    let mut files = BTreeMap::new();
    for name in &publication.files {
        let path = Path::new(name);
        if !is_main(path) {
            continue;
        }
        let base = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| fail("invalid source filename"))?;
        let (span, file_season) = validated_source_file_span(base)?;
        let file_season =
            file_season.ok_or_else(|| fail("episode-title proof needs explicit source seasons"))?;
        let title =
            name_from_file(path).ok_or_else(|| fail("source file has no exact episode title"))?;
        if span.special
            || span.first != span.last
            || season
                .replace(file_season)
                .is_some_and(|previous| previous != file_season)
            || files.insert(span.first, title).is_some()
        {
            return Err(fail(
                "episode-title proof has mixed seasons or duplicate episodes",
            ));
        }
    }
    let season = season.ok_or_else(|| fail("episode-title proof has no main videos"))?;
    let first = *files
        .first_key_value()
        .ok_or_else(|| fail("episode-title proof is empty"))?
        .0;
    let last = first + count - 1;
    if !files.keys().copied().eq(first..=last) {
        return Err(fail(
            "episode-title proof requires one complete continuous range",
        ));
    }
    let coordinates = anibridge::acquisition_season_map(detail.id).await;
    if !files
        .keys()
        .map(|episode| coordinates.resolve(season, *episode))
        .eq((1..=count).map(AcquisitionSeasonMatch::Own))
    {
        return Err(fail(
            "episode-title proof needs an unambiguous provider numbering map",
        ));
    }
    let titles: Vec<(i32, String, String, String, String)> = sqlx::query_as(
        "SELECT episode_number,title,title_english,title_romaji,title_native FROM provider_episode_metadata
         WHERE provider_id=? AND source IN ('jikan','kitsu') AND cached_at>=datetime('now','-7 days')
         ORDER BY episode_number",
    )
    .bind(detail.id)
    .fetch_all(db)
    .await
    .map_err(|error| fail(&format!("provider episode titles are unavailable: {error}")))?;
    let expected: BTreeMap<_, _> = titles
        .into_iter()
        .map(|(ep, a, b, c, d)| (ep, [a, b, c, d].map(|s| normalized_name(&s))))
        .collect();
    let mut episode_titles = Vec::new();
    for (index, title) in files.into_values().enumerate() {
        let episode = index as i32 + 1;
        if !expected
            .get(&episode)
            .is_some_and(|aliases| aliases.contains(&title))
        {
            return Err(fail(
                "source filename differs from a fresh provider episode title",
            ));
        }
        episode_titles.push(title);
    }
    let mut domain = SourceDomain {
        version: 1,
        root_anilist_id: detail.id,
        root_title: display_title(detail),
        root_aliases: aliases,
        source_season: season,
        rules: vec![MappingRule {
            source_first: first,
            source_last: last,
            target_entry: detail.id,
            target_first: 1,
        }],
        entries: vec![DomainEntry {
            anilist_id: detail.id,
            title: display_title(detail),
            format: detail.format.clone(),
            episodes: Some(count),
            provider_episodes: Some((1..=count).collect()),
        }],
        rules_fingerprint: String::new(),
    };
    domain.rules_fingerprint = domain.content_fingerprint()?;
    let evidence = NamedPackEvidence {
        verifier_version: 1,
        publication_url: url.into(),
        publication_title: publication.title.clone(),
        episode_titles,
    };
    let preview =
        prepare_review_inner(domain, hash, &publication.files, detail.id, Some(&evidence))?;
    let names = complete_selection(&preview, &evidence)?;
    let mut proof = preview.confirm(&names)?;
    proof.version = 2;
    proof.provenance = Some(ProofProvenance::AutomaticNamedPack { evidence });
    proof.validate()?;
    Ok(proof)
}

pub(super) async fn refresh(
    db: &SqlitePool,
    proof: &BoundSourceProof,
    evidence: &NamedPackEvidence,
) -> Result<(), String> {
    let detail = cached_detail(db, proof.target_anilist_id).await?;
    let publication =
        nyaa::fetch_published_files(&evidence.publication_url, &proof.info_hash).await?;
    let current = prove(
        db,
        &detail,
        &proof.info_hash,
        &evidence.publication_url,
        &publication,
    )
    .await?;
    if !proof.is_within_review(&current) {
        return Err(fail("episode-title source evidence changed"));
    }
    Ok(())
}

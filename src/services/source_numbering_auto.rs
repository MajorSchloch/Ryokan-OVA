//! Prove one finite source interpretation before considering a wanted entry.
//! Domain discovery, freshness checks and publication fetching belong to the
//! caller. Missing competing evidence is never treated as a negative match.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{AutomaticEvidence, BoundSourceProof, ProofProvenance, SourceDomain};
use crate::services::anilist::MAX_PLAUSIBLE_EPISODES;
use crate::services::episode_mapping::{self, MappingError};
use crate::services::{media, post_processing, release_numbering};

pub const VERIFIER_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct DomainCandidate {
    pub root_anilist_id: i64,
    /// An error preserves a relevant root that could not be checked fresh.
    pub domain: Result<SourceDomain, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanonicalAssignment {
    pub name: String,
    pub source_first: i32,
    pub source_last: i32,
    pub source_season: Option<i32>,
    pub target_anilist_id: i64,
    pub target_first: i32,
    pub target_last: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionKind {
    NonVideo,
    NonEpisodicExtra,
    OutsideFiniteDomain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExcludedFile {
    pub name: String,
    pub kind: ExclusionKind,
}

#[derive(Clone, Debug)]
pub struct AutomaticInterpretation {
    pub scope_fingerprint: String,
    info_hash: String,
    domain: SourceDomain,
    published_files: Vec<String>,
    assignments: Vec<CanonicalAssignment>,
    excluded_files: Vec<ExcludedFile>,
    expected_roots: Vec<i64>,
    scope_domains: Vec<SourceDomain>,
}

impl AutomaticInterpretation {
    pub fn info_hash(&self) -> &str {
        &self.info_hash
    }

    pub fn domain(&self) -> &SourceDomain {
        &self.domain
    }

    pub fn published_files(&self) -> &[String] {
        &self.published_files
    }

    pub fn assignments(&self) -> &[CanonicalAssignment] {
        &self.assignments
    }

    pub fn excluded_files(&self) -> &[ExcludedFile] {
        &self.excluded_files
    }

    /// Projection follows uniqueness. It cannot select a source scheme or
    /// turn an excluded file into an assignment to the requested owner.
    pub fn build_proof_for_owner(
        &self,
        owner: i64,
        publication_url: &str,
    ) -> Result<Option<BoundSourceProof>, String> {
        if owner <= 0 {
            return Err(hold("needs a positive canonical owner"));
        }
        let selected: Vec<_> = self
            .assignments
            .iter()
            .filter(|file| file.target_anilist_id == owner)
            .map(|file| file.name.clone())
            .collect();
        if selected.is_empty() {
            return Ok(None);
        }
        if self.scope_fingerprint
            != scope_fingerprint(
                &self.info_hash,
                &self.published_files,
                &self.expected_roots,
                &self.scope_domains,
            )?
        {
            return Err(hold("scope evidence changed before proof construction"));
        }
        let preview = super::prepare_review(
            self.domain.clone(),
            &self.info_hash,
            &self.published_files,
            owner,
        )?;
        let mut proof = preview.confirm(&selected)?;
        proof.version = 2;
        proof.provenance = Some(ProofProvenance::AutomaticUnique {
            evidence: AutomaticEvidence {
                verifier_version: VERIFIER_VERSION,
                scope_fingerprint: self.scope_fingerprint.clone(),
                publication_url: publication_url.to_string(),
            },
        });
        proof.validate()?;
        Ok(Some(proof))
    }
}

fn hold(message: &str) -> String {
    format!("Episode mapping: automatic source {message}")
}

fn scope_fingerprint(
    hash: &str,
    files: &[String],
    roots: &[i64],
    domains: &[SourceDomain],
) -> Result<String, String> {
    // Lists are canonicalized before this call. Every validated competing
    // domain is included, not just the domain that eventually won.
    let bytes = serde_json::to_vec(&(
        "ryokan-automatic-source-interpretation",
        VERIFIER_VERSION,
        hash,
        files,
        roots,
        domains,
    ))
    .map_err(|error| hold(&format!("evidence cannot be encoded: {error}")))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// The caller must enumerate all relevant roots from source identity, not
/// from a wanted AniList entry or episode. A truncated or partly unavailable
/// set cannot establish uniqueness.
pub fn resolve_unique(
    info_hash: &str,
    published_files: &[String],
    expected_root_ids: &[i64],
    candidates: &[DomainCandidate],
) -> Result<AutomaticInterpretation, String> {
    let hash = super::normalized_hash(info_hash)?;
    let files = validate_manifest(published_files)?;
    let domains = validate_scope(expected_root_ids, candidates)?;
    let roots: Vec<_> = domains
        .iter()
        .map(|domain| domain.root_anilist_id)
        .collect();
    let mut witnessed = BTreeSet::new();
    for domain in &domains {
        witnessed.extend(
            super::prepare_review(domain.clone(), &hash, &files, domain.root_anilist_id)?
                .files
                .into_iter()
                .filter(|file| file.target_anilist_id.is_some())
                .map(|file| file.name),
        );
    }
    let mut interpretations = Vec::new();
    let mut rejected = Vec::new();
    for domain in &domains {
        match interpret(domain, &hash, &files) {
            Ok(interpretation) => interpretations.push(interpretation),
            Err(reason) => rejected.push((domain.root_anilist_id, reason)),
        }
    }
    if interpretations.is_empty() {
        let reasons = rejected
            .iter()
            .map(|(root, reason)| format!("root {root}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(hold(&format!(
            "has no complete consistent interpretation; {reasons}"
        )));
    }
    if interpretations.len() != 1 {
        let roots = interpretations
            .iter()
            .map(|interpretation| interpretation.domain.root_anilist_id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(hold(&format!("has multiple valid source roots: {roots}")));
    }
    let interpretation = interpretations.pop().expect("one interpretation remains");
    let resets: Vec<_> = domains
        .iter()
        .flat_map(|domain| reset_alternatives(domain, &files, &witnessed, &interpretation))
        .collect();
    if !resets.is_empty() {
        let owners = resets
            .iter()
            .map(|(root, owner)| format!("{owner} (source root {root})"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(hold(&format!(
            "is ambiguous: relative/reset numbering also fits canonical entries {owners}"
        )));
    }
    let fingerprint = scope_fingerprint(&hash, &files, &roots, &domains)?;
    Ok(AutomaticInterpretation {
        scope_fingerprint: fingerprint,
        info_hash: hash,
        domain: interpretation.domain,
        published_files: files,
        assignments: interpretation.assignments,
        excluded_files: interpretation.excluded_files,
        expected_roots: roots,
        scope_domains: domains,
    })
}

fn validate_manifest(files: &[String]) -> Result<Vec<String>, String> {
    if files.is_empty() || files.len() > 4096 {
        return Err(hold("publication is empty or exceeds the file limit"));
    }
    let mut names = HashSet::new();
    for name in files {
        post_processing::validate_relative_path_fragment(name)?;
        if name.len() > 4096 || !names.insert(name.as_str()) {
            return Err(hold("publication has a long or duplicate filename"));
        }
    }
    let mut files = files.to_vec();
    files.sort();
    Ok(files)
}

fn validate_scope(
    expected_roots: &[i64],
    candidates: &[DomainCandidate],
) -> Result<Vec<SourceDomain>, String> {
    let roots: BTreeSet<_> = expected_roots.iter().copied().collect();
    if roots.is_empty()
        || roots.len() != expected_roots.len()
        || roots.iter().any(|root| *root <= 0)
    {
        return Err(hold("candidate root scope is empty, repeated or invalid"));
    }
    let mut supplied = BTreeMap::new();
    for candidate in candidates {
        if !roots.contains(&candidate.root_anilist_id)
            || supplied
                .insert(candidate.root_anilist_id, candidate)
                .is_some()
        {
            return Err(hold(
                "candidate evidence has an unexpected or duplicate root",
            ));
        }
    }
    if supplied.keys().copied().collect::<BTreeSet<_>>() != roots {
        return Err(hold(
            "candidate evidence is incomplete; a relevant root is missing",
        ));
    }
    let mut domains = Vec::new();
    for (root, candidate) in supplied {
        let domain = candidate.domain.as_ref().map_err(|reason| {
            hold(&format!(
                "metadata for relevant root {root} is unavailable: {reason}"
            ))
        })?;
        if domain.root_anilist_id != root {
            return Err(hold("candidate evidence identifies another root"));
        }
        domain.validate().map_err(|reason| {
            hold(&format!(
                "domain for relevant root {root} is invalid: {reason}"
            ))
        })?;
        for entry in &domain.entries {
            if !entry
                .episodes
                .is_some_and(|count| (1..=MAX_PLAUSIBLE_EPISODES).contains(&count))
            {
                return Err(hold(&format!(
                    "finite reset bounds are unavailable for canonical entry {}",
                    entry.anilist_id
                )));
            }
        }
        let root_entry = domain
            .entries
            .iter()
            .find(|entry| entry.anilist_id == root)
            .ok_or_else(|| hold("canonical root metadata is missing"))?;
        for number in 1..=root_entry.episodes.expect("finite entries checked") {
            if episode_mapping::resolve_episode(&domain.rules, number).map_or(true, |mapped| {
                mapped.entry != root || mapped.number != number
            }) {
                return Err(hold(
                    "finite domain lacks its canonical root identity range",
                ));
            }
        }
        domains.push(domain.clone());
    }
    Ok(domains)
}

struct Interpretation {
    domain: SourceDomain,
    assignments: Vec<CanonicalAssignment>,
    excluded_files: Vec<ExcludedFile>,
}

fn interpret(
    domain: &SourceDomain,
    hash: &str,
    files: &[String],
) -> Result<Interpretation, String> {
    let root = domain
        .entries
        .iter()
        .find(|entry| entry.anilist_id == domain.root_anilist_id)
        .ok_or_else(|| hold("canonical root metadata is missing"))?;
    let mut assignments = Vec::new();
    let mut excluded_files = Vec::new();
    for name in files {
        if let Some(kind) = known_exclusion(name, media::is_tv_format(&root.format)) {
            excluded_files.push(ExcludedFile {
                name: name.clone(),
                kind,
            });
            continue;
        }
        let basename = Path::new(name)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| hold("has an invalid video filename"))?;
        let (span, season) = release_numbering::validated_source_file_span(basename)?;
        super::validate_source_file_identity(domain, name, span, season)?;
        validate_automatic_source_identity(domain, basename)?;
        let mapped: Vec<_> = (span.first..=span.last)
            .map(|number| episode_mapping::resolve_episode(&domain.rules, number))
            .collect();
        if mapped
            .iter()
            .all(|mapped| matches!(mapped, Err(MappingError::Unmapped(_))))
        {
            // Retain all names in the proof. An out-of-domain tail is not a
            // reset witness and never becomes a wanted file by projection.
            excluded_files.push(ExcludedFile {
                name: name.clone(),
                kind: ExclusionKind::OutsideFiniteDomain,
            });
            continue;
        }
        let mapped = episode_mapping::resolve_file(&domain.rules, span.first, span.last).map_err(
            |reason| {
                hold(&format!(
                    "contains a conflicting or partial file range: {reason}"
                ))
            },
        )?;
        assignments.push(CanonicalAssignment {
            name: name.clone(),
            source_first: span.first,
            source_last: span.last,
            source_season: season,
            target_anilist_id: mapped.entry,
            target_first: mapped.first,
            target_last: mapped.last,
        });
    }
    if assignments.is_empty() {
        return Err(hold("has no main file inside this finite domain"));
    }
    // Reuse the existing per-owner proof constructor for provider coverage,
    // duplicate canonical destinations and a consistent affine selection.
    let owners: BTreeSet<_> = assignments
        .iter()
        .map(|file| file.target_anilist_id)
        .collect();
    for owner in owners {
        let selected: Vec<_> = assignments
            .iter()
            .filter(|file| file.target_anilist_id == owner)
            .map(|file| file.name.clone())
            .collect();
        super::prepare_review(domain.clone(), hash, files, owner)?
            .confirm(&selected)?
            .validate()?;
    }
    Ok(Interpretation {
        domain: domain.clone(),
        assignments,
        excluded_files,
    })
}

fn validate_automatic_source_identity(domain: &SourceDomain, name: &str) -> Result<(), String> {
    let source = release_numbering::source_identity_name(name, true)?;
    if domain.root_aliases.iter().any(|alias| {
        release_numbering::source_identity_name(alias, false)
            .is_ok_and(|canonical| canonical == source)
    }) {
        Ok(())
    } else {
        Err(hold(
            "full source title does not match the canonical root identity",
        ))
    }
}

fn known_exclusion(name: &str, tv: bool) -> Option<ExclusionKind> {
    if !post_processing::is_video_file(name) {
        return Some(ExclusionKind::NonVideo);
    }
    let basename = Path::new(name).file_name()?.to_str()?;
    let lower = basename.to_ascii_lowercase();
    if media::is_non_episodic_file(&lower)
        || (tv
            && (media::path_names_specials(Path::new(name))
                || media::parse_episode_span(&lower).is_some_and(|span| span.special)))
    {
        return Some(ExclusionKind::NonEpisodicExtra);
    }
    None
}

fn reset_alternatives(
    domain: &SourceDomain,
    files: &[String],
    witnessed: &BTreeSet<String>,
    chosen: &Interpretation,
) -> Vec<(i64, i64)> {
    let Some(root) = domain
        .entries
        .iter()
        .find(|entry| entry.anilist_id == domain.root_anilist_id)
    else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    for name in files {
        if known_exclusion(name, media::is_tv_format(&root.format)).is_some() {
            continue;
        }
        let Some(basename) = Path::new(name).file_name().and_then(|name| name.to_str()) else {
            return Vec::new();
        };
        let Ok((span, season)) = release_numbering::validated_source_file_span(basename) else {
            return Vec::new();
        };
        if super::validate_source_file_identity(domain, name, span, season).is_err()
            || validate_automatic_source_identity(domain, basename).is_err()
        {
            // A fresh different canonical namespace is a genuine negative
            // match. It cannot claim a reset interpretation of these files.
            return Vec::new();
        }
        if witnessed.contains(name) {
            spans.push((name, span));
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }
    // A root with no continuous hit can still supply a competing reset.
    // Use the union of finite witnesses across the complete candidate set;
    // never let a root discard another root's high-number witness as a tail.
    domain
        .entries
        .iter()
        .filter_map(|entry| {
            let count = entry.episodes?;
            let fits_every_file = spans.iter().all(|(_, span)| {
                span.first >= 1
                    && span.last <= count
                    && entry.provider_episodes.as_ref().is_none_or(|numbers| {
                        (span.first..=span.last).all(|number| numbers.contains(&number))
                    })
            });
            let same_assignment = domain.root_anilist_id == chosen.domain.root_anilist_id
                && spans.iter().all(|(name, span)| {
                    chosen.assignments.iter().any(|file| {
                        file.name == **name
                            && file.target_anilist_id == entry.anilist_id
                            && file.target_first == span.first
                            && file.target_last == span.last
                    })
                });
            (fits_every_file && !same_assignment)
                .then_some((domain.root_anilist_id, entry.anilist_id))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
#[path = "source_numbering_auto_tests.rs"]
mod tests;

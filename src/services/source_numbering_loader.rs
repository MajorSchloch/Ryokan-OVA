//! Fresh, target-independent source evidence for automatic acquisition.
use super::{
    automatic::{AutomaticInterpretation, DomainCandidate},
    *,
};
use crate::services::{nyaa, release_numbering::ReleaseMapping};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio::sync::{Mutex, OnceCell};

const MAX_PUBLICATIONS: usize = 3;
type InterpretationCache = HashMap<String, Result<AutomaticInterpretation, String>>;
#[derive(Default)]
pub(crate) struct Resolver {
    catalogue: OnceCell<Result<BTreeMap<i64, AnimeDetail>, String>>,
    evidence: Mutex<InterpretationCache>,
}

async fn catalogue(db: &SqlitePool) -> Result<BTreeMap<i64, AnimeDetail>, String> {
    let ids: Vec<i64> = sqlx::query_scalar("SELECT provider_id FROM provider_metadata_cache UNION SELECT provider_id FROM series_metadata_cache")
        .fetch_all(db).await.map_err(|e| fail(&format!("cannot read source identities: {e}")))?;
    let mut result = BTreeMap::new();
    // Keep stale identities in discovery. Freshness is required after the full
    // connected scope is known; an unavailable competitor must not disappear.
    for id in ids.into_iter().filter(|id| *id > 0) {
        let a = metadata_cache::get_by_provider_id(db, id)
            .await
            .map_err(|e| fail(&e.to_string()))?;
        let b = match series::get_by_anilist_id(db, id)
            .await
            .map_err(|e| fail(&e.to_string()))?
        {
            Some(s) => metadata_cache::get_by_series_id(db, s.id)
                .await
                .map_err(|e| fail(&e.to_string()))?,
            None => None,
        };
        let entry = match (a, b) {
            (Some(a), Some(b)) => Some(if a.cached_at >= b.cached_at { a } else { b }),
            (a, b) => a.or(b),
        };
        if let Some(entry) = entry {
            if entry.provider_id != id || entry.detail.id != id {
                return Err(fail("conflicting cached canonical source identity"));
            }
            result.insert(id, entry.detail);
        }
    }
    Ok(result)
}

fn title_matches(detail: &AnimeDetail, title: &str) -> bool {
    auto_search::collect_aliases(detail)
        .iter()
        .any(|alias| auto_search::normalize_title(alias) == title)
}

fn relevant_scope(
    catalogue: &BTreeMap<i64, AnimeDetail>,
    titles: &BTreeSet<String>,
) -> BTreeSet<i64> {
    let rules = anime_relations::parse(anime_relations::RULES_TEXT).rules;
    let mut scope: BTreeSet<_> = catalogue
        .iter()
        .filter(|(_, d)| titles.iter().any(|t| title_matches(d, t)))
        .map(|(id, _)| *id)
        .collect();
    loop {
        let before = scope.len();
        for rule in &rules {
            if let (Some(a), Some(b)) = (rule.src.anilist, rule.dst.anilist)
                && (scope.contains(&a) || scope.contains(&b))
            {
                scope.extend([a, b]);
            }
        }
        if before == scope.len() {
            break;
        }
    }
    scope
}

async fn resolve(
    db: &SqlitePool,
    catalogue: &BTreeMap<i64, AnimeDetail>,
    url: &str,
    hash: &str,
) -> Result<AutomaticInterpretation, String> {
    let published = nyaa::fetch_published_files(url, hash).await.map_err(|e| {
        fail(&format!(
            "continuous numbering source publication unavailable: {e}"
        ))
    })?;
    let titles: BTreeSet<_> = published
        .files
        .iter()
        .filter(|f| post_processing::is_video_file(f))
        .filter_map(|f| Path::new(f).file_name()?.to_str())
        .filter(|f| !media::is_non_episodic_file(&f.to_ascii_lowercase()))
        .filter_map(library_link::extract_anime_title)
        .map(|t| auto_search::normalize_title(&t))
        .collect();
    let scope = relevant_scope(catalogue, &titles);
    if scope.is_empty() {
        return Err(fail("source has no cached canonical identity"));
    }
    let mut expected = Vec::new();
    let mut candidates = Vec::new();
    for id in scope {
        let fresh = cached_detail(db, id).await?;
        if !titles.iter().any(|t| title_matches(&fresh, t)) {
            continue;
        }
        expected.push(id);
        candidates.push(DomainCandidate {
            root_anilist_id: id,
            domain: load_domain(db, id).await,
        });
    }
    let hash = hash.to_string();
    tokio::task::spawn_blocking(move || {
        automatic::resolve_unique(&hash, &published.files, &expected, &candidates)
    })
    .await
    .map_err(|error| fail(&format!("source verifier failed: {error}")))?
}

/// Hashing and canonical projection of a large manifest run off the async
/// worker, once for the complete set of relevant tracked owners.
pub(crate) async fn project(
    interpretation: AutomaticInterpretation,
    owners: Vec<i64>,
    previous: Vec<BoundSourceProof>,
    url: String,
) -> Result<Vec<BoundSourceProof>, String> {
    tokio::task::spawn_blocking(move || {
        for proof in previous {
            let rebuilt = interpretation
                .build_proof_for_owner(proof.target_anilist_id, &url)?
                .ok_or_else(|| fail("saved automatic owner disappeared"))?;
            if !proof.is_within_review(&rebuilt) {
                return Err(fail(
                    "automatic source evidence changed; source remains on hold",
                ));
            }
        }
        owners
            .into_iter()
            .filter_map(|owner| {
                interpretation
                    .build_proof_for_owner(owner, &url)
                    .transpose()
            })
            .collect()
    })
    .await
    .map_err(|error| fail(&format!("source projection failed: {error}")))?
}

impl Resolver {
    pub(crate) async fn interpretation(
        &self,
        db: &SqlitePool,
        title: &str,
        link: &str,
        info_hash: &str,
        guid: &str,
        advertised_pack: bool,
    ) -> Result<Option<(AutomaticInterpretation, String)>, String> {
        // Ordinary explicitly qualified single episodes keep their existing
        // canonical title/season/provider gates. This does not claim proof for
        // an unqualified source or an advertised pack.
        if !advertised_pack
            && crate::services::release_numbering::validated_source_file_span(title)
                .is_ok_and(|(span, season)| season.is_some() && span.first == span.last)
        {
            return Ok(None);
        }
        let catalogue = self
            .catalogue
            .get_or_init(|| catalogue(db))
            .await
            .as_ref()
            .map_err(Clone::clone)?;
        let raw_title = title;
        let title = auto_search::normalize_title(title);
        let advertised_pack =
            advertised_pack || title.split_whitespace().any(|word| word == "seasons");
        let exact_title =
            library_link::extract_anime_title(raw_title).map(|t| auto_search::normalize_title(&t));
        // This service handles finite curated numbering only. Ordinary entries
        // retain their existing explicit/entry-relative acquisition checks.
        // This broad heading check only budgets a publication fetch. Admission
        // still requires exact canonical identities in the actual manifest.
        let roots: Vec<_> = catalogue
            .iter()
            .filter(|(id, d)| {
                let episodic = media::is_tv_format(&d.format)
                    || (matches!(d.format.as_str(), "OVA" | "ONA" | "SPECIAL")
                        && d.episodes != Some(1));
                episodic
                    && auto_search::collect_aliases(d).iter().any(|a| {
                        let alias = auto_search::normalize_title(a);
                        !alias.is_empty()
                            && (exact_title.as_ref().is_some_and(|title| *title == alias)
                                || (advertised_pack && title.contains(&alias)))
                    })
                    && anime_relations::has_source_namespace(**id)
            })
            .map(|(_, d)| d)
            .collect();
        let relevant = !roots.is_empty();
        if !relevant {
            return Ok(None);
        }
        let hash = normalized_hash(info_hash)?;
        let url = nyaa::view_page_url(link)
            .or_else(|| nyaa::view_page_url(guid))
            .ok_or_else(|| {
                fail("automatic source proof needs a same-hash published file manifest")
            })?;
        let mut evidence = self.evidence.lock().await;
        if !evidence.contains_key(&hash) {
            if evidence.len() >= MAX_PUBLICATIONS {
                return Err(fail(
                    "automatic source evidence budget reached; source remains on hold",
                ));
            }
            let result = resolve(db, catalogue, &url, &hash).await;
            evidence.insert(hash.clone(), result);
        }
        let interpretation = evidence
            .get(&hash)
            .expect("inserted evidence")
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(Some((interpretation.clone(), url)))
    }

    pub(crate) async fn apply(
        &self,
        db: &SqlitePool,
        owner: i64,
        target: &auto_search::SearchTarget,
        candidate: &mut nyaa::SearchResult,
    ) -> Result<bool, String> {
        // Newznab releases have no BT identity and retain the ordinary
        // canonical numbering path. No finite torrent proof is claimed.
        if candidate.info_hash.is_empty() && candidate.indexer_id.is_some() {
            return Ok(false);
        }
        let Some((interpretation, url)) = self
            .interpretation(
                db,
                &candidate.title,
                &candidate.link,
                &candidate.info_hash,
                &candidate.source_guid,
                candidate.is_batch,
            )
            .await?
        else {
            return Ok(false);
        };
        let proof = project(interpretation, vec![owner], Vec::new(), url)
            .await?
            .pop()
            .ok_or_else(|| fail("unique source interpretation belongs to another release"))?;
        let saved = crate::models::source_numbering::save_automatic(db, &proof).await?;
        let proof = narrow_for_target(&saved.proof, target)?;
        candidate.is_batch = proof.selected_files.len() > 1;
        candidate.release_mapping = Some(ReleaseMapping::from_bound_source(&proof)?);
        Ok(true)
    }
}

pub(crate) fn narrow_for_target(
    proof: &BoundSourceProof,
    target: &auto_search::SearchTarget,
) -> Result<BoundSourceProof, String> {
    if let auto_search::SearchTarget::Episode(number) = target {
        let names: Vec<_> = proof
            .selected_files
            .iter()
            .filter(|f| f.target_first <= *number && *number <= f.target_last)
            .map(|f| f.name.clone())
            .collect();
        let narrowed = proof.narrow_to(&names)?;
        if narrowed.episodes() != [*number] {
            return Err(fail("source file spans more than the wanted episode"));
        }
        Ok(narrowed)
    } else {
        Ok(proof.clone())
    }
}

pub(crate) async fn refresh(db: &SqlitePool, proof: &BoundSourceProof) -> Result<(), String> {
    if let Some(ProofProvenance::AutomaticUnique { evidence }) = &proof.provenance {
        let interpretation = resolve(
            db,
            &catalogue(db).await?,
            &evidence.publication_url,
            &proof.info_hash,
        )
        .await?;
        project(
            interpretation,
            Vec::new(),
            vec![proof.clone()],
            evidence.publication_url.clone(),
        )
        .await?;
    }
    Ok(())
}

/// A second owner may be projected from the same fresh automatic namespace.
/// This does not grant permission to reuse or change an existing client job.
pub(crate) async fn additional_owner(
    db: &SqlitePool,
    proof: &BoundSourceProof,
    owner: i64,
) -> Result<BoundSourceProof, String> {
    let Some(ProofProvenance::AutomaticUnique { evidence }) = &proof.provenance else {
        return Err(fail(
            "human source decisions cannot be expanded automatically",
        ));
    };
    let interpretation = resolve(
        db,
        &catalogue(db).await?,
        &evidence.publication_url,
        &proof.info_hash,
    )
    .await?;
    project(
        interpretation,
        vec![owner],
        vec![proof.clone()],
        evidence.publication_url.clone(),
    )
    .await?
    .pop()
    .ok_or_else(|| fail("source has no assignment for this release"))
}

#[cfg(test)]
#[path = "source_numbering_loader_tests.rs"]
mod tests;

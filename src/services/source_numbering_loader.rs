//! Fresh, target-independent source evidence for automatic acquisition.
use super::{
    automatic::{AutomaticInterpretation, DomainCandidate},
    *,
};
use crate::services::{
    nyaa,
    release_numbering::{NumberingContext, ReleaseMapping},
};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio::sync::{Mutex, OnceCell};

const MAX_PUBLICATIONS: usize = 3;
type InterpretationCache = HashMap<String, Result<AutomaticInterpretation, String>>;
type PublicationCache = HashMap<(String, String), Result<nyaa::PublishedFiles, String>>;
#[derive(Default)]
pub(crate) struct Resolver {
    catalogue: OnceCell<Result<BTreeMap<i64, AnimeDetail>, String>>,
    evidence: Mutex<InterpretationCache>,
    publications: Mutex<PublicationCache>,
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
    let published = fetch_source_publication(url, hash).await?;
    resolve_published(db, catalogue, published, hash).await
}

async fn fetch_source_publication(url: &str, hash: &str) -> Result<nyaa::PublishedFiles, String> {
    nyaa::fetch_published_files(url, hash)
        .await
        .map_err(|error| {
            fail(&format!(
                "continuous numbering source publication unavailable: {error}"
            ))
        })
}

async fn resolve_published(
    db: &SqlitePool,
    catalogue: &BTreeMap<i64, AnimeDetail>,
    published: nyaa::PublishedFiles,
    hash: &str,
) -> Result<AutomaticInterpretation, String> {
    let titles: BTreeSet<_> = published
        .files
        .iter()
        .filter(|f| post_processing::is_video_file(f))
        .filter(|f| !automatic::is_explicit_sample(Path::new(f)))
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
    pub(crate) async fn prove_named_pack(
        &self,
        db: &SqlitePool,
        detail: &AnimeDetail,
        candidate: &nyaa::SearchResult,
    ) -> Result<ReleaseMapping, String> {
        let hash = normalized_hash(&candidate.info_hash)?;
        let url = nyaa::view_page_url(&candidate.link)
            .or_else(|| nyaa::view_page_url(&candidate.source_guid))
            .ok_or_else(|| fail("episode-title proof needs a same-hash published file manifest"))?;
        let detail = cached_detail(db, detail.id).await?;
        let publication = self.publication(&hash, &url).await?;
        let proof = named_pack::prove(db, &detail, &hash, &url, &publication).await?;
        let saved = crate::models::source_numbering::save_automatic(db, &proof).await?;
        ReleaseMapping::from_bound_source(&saved.proof)
    }

    /// Prove one complete, owner-specific entry before projecting any subset.
    /// This also covers later seasons whose exact filenames use relative numbers.
    pub(crate) async fn prove_complete_entry(
        &self,
        db: &SqlitePool,
        detail: &AnimeDetail,
        candidate: &nyaa::SearchResult,
    ) -> Result<BoundSourceProof, String> {
        if anime_relations::has_source_namespace(detail.id) {
            return Err(fail(
                "curated episode numbering requires its own source proof",
            ));
        }
        let hash = normalized_hash(&candidate.info_hash)?;
        let url = nyaa::view_page_url(&candidate.link)
            .ok_or_else(|| fail("entry pack needs a same-hash published file manifest"))?;
        let interpretation = self.load_interpretation(db, &hash, &url).await?;
        verify_complete_entry(detail, &interpretation).await?;
        let proof = interpretation
            .build_proof_for_owner(detail.id, &url)?
            .ok_or_else(|| fail("entry pack does not contain this release"))?;
        let saved = crate::models::source_numbering::save_automatic(db, &proof).await?;
        Ok(saved.proof)
    }

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
        let catalogue = self.catalogue(db).await?;
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
        let interpretation = self.load_interpretation(db, &hash, &url).await?;
        Ok(Some((interpretation, url)))
    }

    async fn catalogue(&self, db: &SqlitePool) -> Result<&BTreeMap<i64, AnimeDetail>, String> {
        self.catalogue
            .get_or_init(|| catalogue(db))
            .await
            .as_ref()
            .map_err(Clone::clone)
    }

    async fn load_interpretation(
        &self,
        db: &SqlitePool,
        hash: &str,
        url: &str,
    ) -> Result<AutomaticInterpretation, String> {
        let catalogue = self.catalogue(db).await?;
        let mut evidence = self.evidence.lock().await;
        if !evidence.contains_key(hash) {
            let published = self.publication(hash, url).await?;
            let result = resolve_published(db, catalogue, published, hash).await;
            evidence.insert(hash.to_string(), result);
        }
        evidence.get(hash).expect("inserted evidence").clone()
    }

    async fn publication(&self, hash: &str, url: &str) -> Result<nyaa::PublishedFiles, String> {
        let key = (hash.to_owned(), url.to_owned());
        let mut publications = self.publications.lock().await;
        if let Some(result) = publications.get(&key) {
            return result.clone();
        }
        if publications.len() >= MAX_PUBLICATIONS {
            return Err(fail(
                "automatic source evidence budget reached; source remains on hold",
            ));
        }
        let result = fetch_source_publication(url, hash).await;
        publications.insert(key, result.clone());
        result
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

async fn verify_complete_entry(
    detail: &AnimeDetail,
    interpretation: &AutomaticInterpretation,
) -> Result<(), String> {
    if interpretation.domain().root_anilist_id != detail.id
        || interpretation.domain().entries.len() != 1
        || interpretation
            .excluded_files()
            .iter()
            .any(|file| file.kind == automatic::ExclusionKind::OutsideFiniteDomain)
        || interpretation.assignments().iter().any(|file| {
            file.target_anilist_id != detail.id
                || file.source_first != file.target_first
                || file.target_first != file.target_last
        })
    {
        return Err(fail("entry pack has conflicting owners or episode files"));
    }
    let episodes: Vec<_> = interpretation
        .assignments()
        .iter()
        .map(|file| file.source_first)
        .collect();
    let season = interpretation
        .assignments()
        .iter()
        .find_map(|file| file.source_season);
    NumberingContext::load(
        detail.id,
        detail.id_mal,
        &detail.format,
        detail.episodes,
        0,
        interpretation.domain().source_season,
    )
    .await?
    .choose_published_entry_pack(&episodes, season)?;
    Ok(())
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
    match &proof.provenance {
        Some(ProofProvenance::AutomaticUnique { evidence }) => {
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
        Some(ProofProvenance::AutomaticEntryPack {
            publication_url, ..
        }) => {
            proof.validate()?;
            let detail = cached_detail(db, proof.target_anilist_id).await?;
            let rebuilt = auto_search::entry_pack_proof(
                db,
                &detail,
                &proof.info_hash,
                publication_url,
                proof.selected_files[0].target_first,
            )
            .await?;
            if rebuilt != *proof {
                return Err(fail(
                    "automatic entry-pack evidence changed; source remains on hold",
                ));
            }
        }
        Some(ProofProvenance::AutomaticNamedPack { evidence }) => {
            named_pack::refresh(db, proof, evidence).await?;
        }
        None => {}
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

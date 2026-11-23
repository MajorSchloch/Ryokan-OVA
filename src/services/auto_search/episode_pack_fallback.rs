use sqlx::SqlitePool;

use super::{
    BatchLookups, BatchSearchMode, CandidateFilter, MatchPhase, MatchProvenance, SearchTarget,
    collect_scored_batches_for_target_filtered, entry_pack_proof, published_packs, source,
};
use crate::models::{config::Config, grabbed_torrents};
use crate::services::anilist::AnimeDetail;
use crate::services::custom_formats::CompiledCustomFormat;
use crate::services::nyaa::{self, SearchResult};
use crate::services::release_numbering::ReleaseMapping;
use crate::services::source_numbering::{BoundSourceProof, loader::narrow_for_target};

const PUBLICATION_LIMIT: usize = 3;

/// Search with the batch query shape but return only one proven video.
/// Call after the eligible standalone search misses. The returned mapping
/// requires the strict metadata-only torrent dispatch and import checks.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn find_single_episode_batch_fallback(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    cfs: &[CompiledCustomFormat],
    indexers_cache: &crate::IndexerCache,
    eligible: Option<CandidateFilter<'_>>,
    lookups: Option<&BatchLookups>,
) -> (Option<SearchResult>, Vec<String>) {
    let SearchTarget::Episode(episode) = *target else {
        return (None, Vec::new());
    };
    if episode <= 0 || !published_packs::supports(detail) {
        return (None, Vec::new());
    }
    let (candidates, mut diagnostics) = collect_scored_batches_for_target_filtered(
        db,
        detail,
        config,
        &SearchTarget::Single,
        cfs,
        indexers_cache,
        BatchSearchMode::EpisodeFallback,
        None,
        lookups,
    )
    .await;

    let mut fetched = 0;
    for mut candidate in candidates {
        let existing = candidate
            .release_mapping
            .as_ref()
            .and_then(ReleaseMapping::bound_source);
        let proof = match existing {
            Some(proof) => narrow_for_target(proof, target),
            None => {
                if candidate.indexer_id.is_some()
                    || candidate.replay_episodes.is_some()
                    || !candidate.is_batch
                    || fetched == PUBLICATION_LIMIT
                {
                    continue;
                }
                let Some(url) = nyaa::view_page_url(&candidate.link) else {
                    continue;
                };
                fetched += 1;
                entry_pack_proof(db, detail, &candidate.info_hash, &url, episode).await
            }
        };
        let proof = match proof {
            Ok(proof) if exact_wanted_file(&proof, detail.id, episode) => proof,
            Ok(_) => continue,
            Err(reason) => {
                diagnostics.push(reason);
                continue;
            }
        };
        let mapping = match ReleaseMapping::from_bound_source(&proof) {
            Ok(mapping) => mapping,
            Err(reason) => {
                diagnostics.push(reason);
                continue;
            }
        };
        let incoming = source::classify_release(
            db,
            &candidate.title,
            Some(&candidate.resolution),
            Some(source::NyaaContext {
                info_hash: &candidate.info_hash,
                view_url: &candidate.link,
                is_batch: true,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;
        candidate.is_batch = false;
        candidate.release_mapping = Some(mapping);
        if eligible.is_some_and(|allows| !allows(&candidate, &incoming)) {
            continue;
        }
        if let Err(reason) = grabbed_torrents::ensure_automatic_compatibility(
            db,
            &candidate.info_hash,
            detail.id,
            candidate.release_mapping.as_ref(),
        )
        .await
        {
            diagnostics.push(reason);
            continue;
        }
        let phase = candidate
            .match_provenance
            .as_ref()
            .map_or(MatchPhase::BatchProbe, |provenance| provenance.phase);
        candidate.match_provenance = Some(
            if proof.origin().is_ok_and(|origin| origin.is_automatic()) {
                MatchProvenance::automatic_source(phase)
            } else {
                MatchProvenance::reviewed_source(phase)
            },
        );
        return (Some(candidate), super::dedupe_strings(diagnostics));
    }
    (None, super::dedupe_strings(diagnostics))
}

fn exact_wanted_file(proof: &BoundSourceProof, owner: i64, episode: i32) -> bool {
    proof.target_anilist_id == owner
        && proof.selected_files.len() == 1
        && proof.episodes() == [episode]
}

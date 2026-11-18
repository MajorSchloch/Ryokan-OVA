//! Source-file proof for entry packs whose release title has no batch marker.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use sqlx::SqlitePool;

use super::aliases::{SiblingRejectPrecompute, sibling_match_rejects};
use super::{
    MatchKind, SearchTarget, classify_match, infer_season_from_detail, is_media_filename,
    normalize_title, part_cour_numbers, season_mismatch, token_set,
};
use crate::services::anilist::{AnimeDetail, MAX_PLAUSIBLE_EPISODES};
use crate::services::nyaa::SearchResult;
use crate::services::release_numbering::{NumberingContext, ReleaseMapping};
use crate::services::source_numbering::{self, BoundSourceProof};
use crate::services::{anime_relations, media, nyaa};

const VIEW_FETCH_CAP: usize = 3;

fn episodic(detail: &AnimeDetail) -> bool {
    media::is_tv_format(&detail.format)
        || (matches!(detail.format.as_str(), "OVA" | "ONA" | "SPECIAL")
            && detail.episodes != Some(1))
}

pub(super) fn supports(detail: &AnimeDetail) -> bool {
    episodic(detail)
        && detail
            .episodes
            .is_some_and(|n| (2..=MAX_PLAUSIBLE_EPISODES).contains(&n))
}

/// Single is a search shape, not permission for a TV pack to name another
/// season/cour. A reviewed file decision remains the explicit exception.
pub(super) fn identity_allows(
    detail: &AnimeDetail,
    candidate: &SearchResult,
    aliases: &[String],
    siblings: &SiblingRejectPrecompute,
) -> bool {
    if !episodic(detail)
        || candidate.replay_episodes.is_some()
        || candidate
            .release_mapping
            .as_ref()
            .is_some_and(|mapping| mapping.bound_source().is_some())
    {
        return true;
    }
    if !super::canonical_owner_supports_release(
        &candidate.title,
        &super::collect_aliases(detail),
        &SearchTarget::Episode(1),
    ) {
        return false;
    }
    if season_mismatch(&candidate.title, infer_season_from_detail(detail).max(1)) {
        return false;
    }
    let own_parts: HashSet<_> = aliases.iter().flat_map(|s| part_cour_numbers(s)).collect();
    let parts = part_cour_numbers(&candidate.title);
    if !parts.is_empty() && parts != own_parts {
        return false;
    }
    let normalized = normalize_title(&candidate.title);
    !sibling_match_rejects(&normalized, &token_set(&normalized), siblings)
}

fn full_own_title(
    title: &str,
    aliases: &[String],
    siblings: &SiblingRejectPrecompute,
    season: i32,
) -> bool {
    classify_match(
        title,
        aliases,
        siblings,
        &SearchTarget::Single,
        season,
        false,
        0,
    )
    .is_some_and(|matched| matched.kind == MatchKind::Verbatim)
        && !season_mismatch(title, season)
}

pub(crate) fn main_video_file(path: &Path, tv_series: bool) -> bool {
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    (is_media_filename(filename) || crate::services::post_processing::is_video_file(filename))
        && !media::is_non_episodic_file(&filename.to_ascii_lowercase())
        && !(tv_series && (media::path_names_specials(path) || media::is_special_release(filename)))
}

/// Every non-extra video must identify this entry and a distinct relative
/// episode. Unknown/foreign files, ranges, duplicates and season disagreement
/// do not establish a source numbering decision.
fn complete_coverage(
    files: &[String],
    detail: &AnimeDetail,
    aliases: &[String],
    siblings: &SiblingRejectPrecompute,
    offset: i32,
) -> Option<(Vec<i32>, Option<i32>, bool)> {
    let count = detail
        .episodes
        .filter(|n| (2..=MAX_PLAUSIBLE_EPISODES).contains(n))?;
    let expected_season = infer_season_from_detail(detail).max(1);
    let canonical = super::collect_aliases(detail);
    let own_parts: HashSet<_> = canonical
        .iter()
        .flat_map(|title| part_cour_numbers(title))
        .collect();
    let mut numbers = BTreeSet::new();
    let mut seasons = HashSet::new();
    let mut picker_compatible = true;
    for name in files {
        if !is_media_filename(name) {
            continue;
        }
        let path = Path::new(name);
        let filename = path.file_name()?.to_str()?;
        let lower = filename.to_ascii_lowercase();
        if !main_video_file(path, media::is_tv_format(&detail.format)) {
            continue;
        }
        let span = media::parse_episode_span(&lower)?;
        if span.first != span.last || span.first < 1 || span.first > count {
            return None;
        }
        if span.season.is_some_and(|s| s != expected_season) {
            return None;
        }
        if let Some(season) = span.season {
            seasons.insert(season);
        }
        picker_compatible &= crate::services::grab_commit::file_episodes_for_series(
            name,
            offset,
            media::is_tv_format(&detail.format),
        )
        .as_deref()
            == Some(std::slice::from_ref(&span.first));
        for folder in path.parent()?.components() {
            let folder = folder.as_os_str().to_str()?;
            if season_mismatch(folder, expected_season) {
                return None;
            }
            let folder_parts = part_cour_numbers(folder);
            if !folder_parts.is_empty() && folder_parts != own_parts {
                return None;
            }
        }
        if !classify_match(
            filename,
            aliases,
            siblings,
            &SearchTarget::Episode(span.first),
            expected_season,
            false,
            0,
        )
        .is_some_and(|matched| matched.kind == MatchKind::Verbatim)
            || !super::canonical_owner_supports_release(
                filename,
                &canonical,
                &SearchTarget::Episode(span.first),
            )
        {
            return None;
        }
        if !numbers.insert(span.first) {
            return None;
        }
    }
    if numbers.iter().copied().ne(1..=count) || seasons.len() > 1 {
        return None;
    }
    Some((
        numbers.into_iter().collect(),
        seasons.into_iter().next(),
        picker_compatible,
    ))
}

/// Establish a single episode from a complete, ordinary entry-relative
/// Nyaa pack. Any competing numbering domain continues through the
/// existing finite-source resolver.
pub(crate) async fn entry_pack_proof(
    db: &SqlitePool,
    detail: &AnimeDetail,
    info_hash: &str,
    publication_url: &str,
    episode: i32,
) -> Result<BoundSourceProof, String> {
    let tracked = crate::models::series::get_by_anilist_id(db, detail.id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Episode mapping: the entry pack has no tracked release")?;
    if !supports(detail)
        || detail.id <= 0
        || tracked.cumulative_prior_episodes != 0
        || anime_relations::has_source_namespace(detail.id)
        || anime_relations::offset_for(detail.id, detail.id_mal).is_some()
    {
        return Err(
            "Episode mapping: entry pack needs unambiguous entry-relative numbering".into(),
        );
    }
    let count = detail.episodes.unwrap_or_default();
    if !(1..=count).contains(&episode) {
        return Err("Episode mapping: the wanted episode is outside the entry pack".into());
    }
    let aliases = super::collect_aliases(detail);
    let siblings = SiblingRejectPrecompute::build(
        &aliases,
        &super::collect_related_identity_aliases(detail, &aliases),
    );
    let season = infer_season_from_detail(detail).max(1);
    let published = nyaa::fetch_published_files(publication_url, info_hash).await?;
    if !full_own_title(&published.title, &aliases, &siblings, season)
        || !super::canonical_owner_supports_release(
            &published.title,
            &aliases,
            &SearchTarget::Episode(1),
        )
    {
        return Err("Episode mapping: published pack does not name this exact release".into());
    }
    let (episodes, source_season, _) =
        complete_coverage(&published.files, detail, &aliases, &siblings, 0).ok_or(
            "Episode mapping: published pack has missing, duplicate, or conflicting episode files",
        )?;
    NumberingContext::load(
        detail.id,
        detail.id_mal,
        &detail.format,
        detail.episodes,
        0,
        season,
    )
    .await?
    .choose_published_entry_pack(&episodes, source_season)?;
    let domain = source_numbering::load_domain(db, detail.id).await?;
    BoundSourceProof::for_entry_pack(
        domain,
        info_hash,
        &published.files,
        publication_url,
        episode,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn promote(
    db: &SqlitePool,
    detail: &AnimeDetail,
    aliases: &[String],
    siblings: &SiblingRejectPrecompute,
    offset: i32,
    candidates: &mut [SearchResult],
    eligibility: Option<super::CandidateFilter<'_>>,
    automatic: bool,
) -> HashMap<String, ReleaseMapping> {
    let mut mappings = HashMap::new();
    if !supports(detail) {
        return mappings;
    }
    let season = infer_season_from_detail(detail).max(1);
    let canonical = super::collect_aliases(detail);
    let mut eligible: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            !c.is_batch
                && c.release_mapping
                    .as_ref()
                    .is_none_or(|mapping| !mapping.is_single_episode_pack())
                && c.indexer_id.is_none()
                && c.info_hash.len() == 40
                && c.info_hash.bytes().all(|b| b.is_ascii_hexdigit())
                && nyaa::view_page_url(&c.link).is_some()
                && media::parse_episode_span(&c.title.to_ascii_lowercase()).is_none()
                && full_own_title(&c.title, aliases, siblings, season)
                && super::canonical_owner_supports_release(
                    &c.title,
                    &canonical,
                    &SearchTarget::Episode(1),
                )
                && {
                    let mut prospective = (**c).clone();
                    prospective.is_batch = true;
                    super::passes_presearch_requirements(&prospective, None, eligibility)
                }
        })
        .map(|(index, _)| index)
        .collect();
    eligible.sort_by(|a, b| {
        candidates[*b]
            .score
            .cmp(&candidates[*a].score)
            .then(candidates[*b].seeders.cmp(&candidates[*a].seeders))
    });
    if eligible.is_empty() {
        return mappings;
    }
    let context = NumberingContext::load(
        detail.id,
        detail.id_mal,
        &detail.format,
        detail.episodes,
        offset,
        season,
    )
    .await;
    for index in eligible.into_iter().take(VIEW_FETCH_CAP) {
        let candidate = &mut candidates[index];
        let Ok(published) =
            nyaa::fetch_published_files(&candidate.link, &candidate.info_hash).await
        else {
            continue;
        };
        if !full_own_title(&published.title, aliases, siblings, season) {
            continue;
        }
        if !super::canonical_owner_supports_release(
            &published.title,
            &canonical,
            &SearchTarget::Episode(1),
        ) {
            continue;
        }
        let files = published.files;
        let owned_detail = detail.clone();
        let owned_aliases = aliases.to_vec();
        let owned_siblings = siblings.clone();
        let proof = tokio::task::spawn_blocking(move || {
            complete_coverage(
                &files,
                &owned_detail,
                &owned_aliases,
                &owned_siblings,
                offset,
            )
        })
        .await;
        let Ok(Some((episodes, source_season, picker_compatible))) = proof else {
            continue;
        };
        // Fresh pickers still use their existing source-number projection.
        // Automatic grabs carry the explicit proven mapping instead.
        if !automatic && !picker_compatible {
            continue;
        }
        candidate.is_batch = true;
        candidate
            .score_breakdown
            .push(crate::services::scoring::ScoreComponent {
                label: "Published episode files".into(),
                delta: 0,
                detail: Some(format!(
                    "{} distinct episodes in the listing's file list",
                    episodes.len()
                )),
            });
        // Reuse the exact fetched page for classification instead of making
        // a second view request. Existing TTL/hash-keyed cache rules remain.
        if crate::models::nyaa_description_cache::get(db, &candidate.info_hash)
            .await
            .is_none()
        {
            crate::models::nyaa_description_cache::upsert(
                db,
                &candidate.info_hash,
                &published.description,
            )
            .await;
        }
        if let Ok(mapping) = context
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|ctx| ctx.choose_published_entry_pack(&episodes, source_season))
        {
            mappings.insert(candidate.info_hash.to_ascii_lowercase(), mapping);
        }
    }
    mappings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail() -> AnimeDetail {
        serde_json::from_value(serde_json::json!({"id":1,"title_romaji":"Quiet Horizon","title_english":"Quiet Horizon","title_native":"","cover_url":"","banner_url":"","format":"TV","status":"FINISHED","status_display":"Finished","episodes":3,"season":"","description":"","genres":[],"score_is_ten_point":false,"score_class":"","synonyms":[],"streaming_episodes":[],"relations":[]})).unwrap()
    }

    #[test]
    fn published_pack_coverage_rejects_incomplete_duplicate_foreign_and_unknown_video() {
        let detail = detail();
        let aliases = super::super::collect_aliases(&detail);
        let siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let valid: Vec<_> = (1..=3)
            .map(|ep| format!("Quiet Horizon - {ep:02}.mkv"))
            .collect();
        assert_eq!(
            complete_coverage(&valid, &detail, &aliases, &siblings, 0)
                .unwrap()
                .0,
            [1, 2, 3]
        );
        for invalid in [
            valid[..2].to_vec(),
            [valid.clone(), vec![valid[0].clone()]].concat(),
            [valid.clone(), vec!["Different Show - 01.mkv".into()]].concat(),
            [valid.clone(), vec!["unverifiable.mkv".into()]].concat(),
            (1..=3)
                .map(|ep| format!("Quiet Horizon S02E{ep:02}.mkv"))
                .collect(),
        ] {
            assert!(
                complete_coverage(&invalid, &detail, &aliases, &siblings, 0).is_none(),
                "{invalid:?}"
            );
        }
        let extras = [
            valid,
            vec![
                "Quiet Horizon - NCOP 1.mkv".into(),
                "Specials/Quiet Horizon - OVA 01.mkv".into(),
                "cover.jpg".into(),
            ],
        ]
        .concat();
        assert!(complete_coverage(&extras, &detail, &aliases, &siblings, 0).is_some());
    }

    #[test]
    fn published_files_cannot_take_a_canonical_title_number_from_the_episode() {
        let mut detail = detail();
        detail.title_romaji = "Digimon Adventure 02".into();
        detail.title_english.clone_from(&detail.title_romaji);
        let aliases = [
            super::super::collect_aliases(&detail),
            vec!["Digimon Adventure".into()],
        ]
        .concat();
        let siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let foreign: Vec<_> = (1..=3)
            .map(|ep| format!("Digimon Adventure - {ep:02}.mkv"))
            .collect();
        assert!(complete_coverage(&foreign, &detail, &aliases, &siblings, 0).is_none());
        let own: Vec<_> = (1..=3)
            .map(|ep| format!("Digimon Adventure 02 - {ep:02}.mkv"))
            .collect();
        assert!(complete_coverage(&own, &detail, &aliases, &siblings, 0).is_some());
    }

    #[test]
    fn published_pack_folders_cannot_contradict_the_entry_part_or_cour() {
        let mut detail = detail();
        detail.title_romaji = "Quiet Horizon Part 2".into();
        detail.title_english.clone_from(&detail.title_romaji);
        let aliases = super::super::collect_aliases(&detail);
        let siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        for (folder, valid) in [
            ("Quiet Horizon Part 1", false),
            ("Quiet Horizon Cour 1", false),
            ("Quiet Horizon Part 2", true),
            ("Quiet Horizon Cour 2", true),
        ] {
            let names: Vec<_> = (1..=3)
                .map(|ep| format!("{folder}/Quiet Horizon Part 2 - {ep:02}.mkv"))
                .collect();
            assert_eq!(
                complete_coverage(&names, &detail, &aliases, &siblings, 0).is_some(),
                valid,
                "{folder}"
            );
        }
    }
}

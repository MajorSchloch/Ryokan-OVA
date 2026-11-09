//! TMDB-season resolution for the manual-import matcher (#122).
//!
//! The AniList title search finds a franchise; this module decides
//! which entry of it a season's files belong to, and what those files'
//! episode numbers are in that entry's own numbering. It reads the
//! anibridge mappings (`services::anibridge`, ID-only) which link every
//! AniList entry to a TMDB show + season with episode ranges. That is
//! the numbering a `Season 3` folder in a Jellyfin / Plex library uses,
//! so `Show/Season 3/Show - 18.mkv` resolves by id rather than by how
//! AniList happens to name the sequel (`II`, `3rd Season`, `San no
//! Shou`).
//!
//! The ranges also fix a correctness gap the title-only path has: a
//! TMDB season that AniList lists as two entries (split cours) sends
//! episodes 13 to 24 to the second entry as its 1 to 12, and an AniList
//! entry that TMDB splits across seasons takes `S02E05` as its E17.
//! Files with a missing, conflicting or cross-entry mapping lose their
//! automatic match. Keeping the title search's pick would silently apply
//! source episode numbers as library episode identities.
//!
//! [`apply_absolute_numbering`] covers the other common library shape:
//! a folder with no season at all and absolute episode numbers
//! (`Jujutsu Kaisen - 55.mkv`). The title search lands on the first
//! entry; when file numbers run past its episode count, the walk
//! follows the TV `SEQUEL` chain from that entry (the same relation
//! chain #30's `cumulative_prior_episodes` is built from, in the other
//! direction) and routes each file to the entry whose cumulative range
//! holds it, renumbered relative to that entry.
//!
//! Both run on the automatic pass only. A candidate the user picked, or
//! a title they typed, is taken as given.

use std::collections::BTreeMap;

use super::{CandidateFile, SeriesGroup};
use crate::services::anibridge;
use crate::services::anilist::{self, AnimeDetail, AnimeEntry};
use crate::services::episode_mapping::{self, MappingError, MappingRule};

fn source_file_span(file: &CandidateFile) -> Result<(i32, i32), MappingError> {
    let first = file.episode.ok_or(MappingError::InvalidSource)?;
    let last = first
        .checked_add(file.episode_count.max(1) - 1)
        .ok_or(MappingError::InvalidSource)?;
    Ok((first, last))
}

fn unresolved_group(mut group: SeriesGroup, note: String) -> SeriesGroup {
    group.pick = None;
    group.existing = None;
    group.low_confidence = true;
    group.resolved_by_id = true;
    group.mapping_note = Some(note);
    group
}

/// A search-style entry from a by-id detail fetch, for a mapped
/// AniList id the title search didn't return.
pub fn entry_from_detail(d: &AnimeDetail) -> AnimeEntry {
    AnimeEntry {
        id: d.id,
        id_mal: d.id_mal,
        title_romaji: d.title_romaji.clone(),
        title_english: d.title_english.clone(),
        title_native: d.title_native.clone(),
        cover_url: d.cover_url.clone(),
        format: d.format.clone(),
        status: d.status.clone(),
        status_display: d.status_display.clone(),
        episodes: d.episodes,
        season_year: d.season_year,
        source: "anilist".to_string(),
        average_score: d.average_score,
    }
}

/// TMDB show for the group: the first ranked candidate the mappings
/// know. Any entry of a franchise resolves to the same show.
async fn tmdb_show_for(group: &SeriesGroup) -> Option<i64> {
    for e in group.candidates.iter().filter(|e| e.id > 0) {
        if let Some(tmdb) = anibridge::lookup_tmdb_by_anilist(e.id).await {
            return Some(tmdb);
        }
    }
    None
}

/// Resolve `group` through the TMDB mapping. Returns the group(s) that
/// replace it, in order: one per AniList entry the season's files map
/// to (split cours make two), then the files no span covered under
/// no automatic pick. A group the mapping can't help
/// (no season, show unknown, no mapping available) comes back as it was.
pub async fn apply_season_mapping(group: SeriesGroup) -> Vec<SeriesGroup> {
    let Some(season) = group.tmdb_season else {
        return vec![group];
    };
    let Some(tmdb) = tmdb_show_for(&group).await else {
        return vec![group];
    };
    let spans = anibridge::tmdb_episode_spans(tmdb, season).await;
    if spans.is_empty() {
        return vec![group];
    }

    // Bucket files by the AniList entry their TMDB episode maps to,
    // in span order; renumber into that entry's numbering.
    let mut order: Vec<i64> = Vec::new();
    for s in &spans {
        if !order.contains(&s.anilist_id) {
            order.push(s.anilist_id);
        }
    }
    let mut mapped: BTreeMap<usize, Vec<CandidateFile>> = BTreeMap::new();
    let mut unmapped: Vec<CandidateFile> = Vec::new();
    let mut failures = std::collections::BTreeSet::new();
    for f in &group.files {
        match source_file_span(f)
            .and_then(|(first, last)| anibridge::map_tmdb_file(&spans, first, last))
        {
            Ok(result) => {
                let mut nf = f.clone();
                nf.episode = Some(result.first);
                nf.source_episode = f.episode.filter(|e| *e != result.first);
                // A season-zero crosswalk can identify a regular episode of
                // an AniList entry. A stray OVA in another season does not
                // establish that source identity; retain its special marker.
                if season == 0 {
                    nf.special = false;
                }
                let idx = order
                    .iter()
                    .position(|id| *id == result.entry)
                    .expect("mapped entry is in spans");
                mapped.entry(idx).or_default().push(nf);
            }
            Err(error) => {
                failures.insert(error.to_string());
                unmapped.push(f.clone());
            }
        }
    }
    let unresolved_note = format!(
        "Cannot map every episode in these files for season {season}. Automatic match cleared. {}",
        failures.into_iter().collect::<Vec<_>>().join("; ")
    );
    if mapped.is_empty() {
        return vec![unresolved_group(group, unresolved_note)];
    }

    // Every mapped entry needs a candidate to pick. Fetch the ones the
    // title search left out; if AniList can't answer, keep the group
    // as the search left it rather than renumber against the wrong
    // entry.
    let mut candidates = group.candidates.clone();
    for idx in mapped.keys() {
        let al = order[*idx];
        if candidates.iter().any(|e| e.id == al) {
            continue;
        }
        match anilist::get_anime_detail(al).await {
            Ok(d) => candidates.push(entry_from_detail(&d)),
            Err(e) => {
                return vec![unresolved_group(
                    group,
                    format!(
                        "TMDB mapping names AniList entry {al} for season {season}, but the lookup failed: {e}"
                    ),
                )];
            }
        }
    }

    let split = mapped.len() > 1 || !unmapped.is_empty();
    let mut out: Vec<SeriesGroup> = Vec::new();
    for (idx, files) in mapped {
        let al = order[idx];
        let pick = candidates.iter().position(|e| e.id == al);
        let (lo, hi) = files
            .iter()
            .filter_map(|f| f.source_episode.or(f.episode))
            .fold((i32::MAX, i32::MIN), |(lo, hi), e| (lo.min(e), hi.max(e)));
        let mut g = group.clone();
        g.files = files;
        g.candidates = candidates.clone();
        g.pick = pick;
        g.low_confidence = false;
        g.search_error = None;
        g.resolved_by_id = true;
        g.mapping_note = Some(if split && lo <= hi {
            format!("Season {season}, episodes {lo} to {hi}, through the TMDB mapping")
        } else {
            format!("Season {season} through the TMDB mapping")
        });
        out.push(g);
    }
    if !unmapped.is_empty() {
        let mut g = group;
        g.files = unmapped;
        out.push(unresolved_group(g, unresolved_note));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anibridge::{clear_cache_for_tests, seed_tmdb_episode_spans_for_tests};
    use crate::services::manual_import::parse::TitleSource;
    use std::path::PathBuf;

    fn entry(id: i64, english: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: english.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    fn file(ep: Option<i32>) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from("x"),
            rel_path: format!("Show/Season 2/Show - {:?}.mkv", ep),
            file_name: "x.mkv".into(),
            size_bytes: 1,
            parsed_title: Some("Show".into()),
            title_source: TitleSource::Filename,
            season: Some(2),
            episode: ep,
            year: None,
            group: None,
            quality_label: "Unknown".into(),
            selected: true,
            episode_count: 1,
            special: false,
            source_episode: None,
        }
    }

    fn group(files: Vec<CandidateFile>, candidates: Vec<AnimeEntry>) -> SeriesGroup {
        SeriesGroup {
            key: "show|s2".into(),
            parsed_title: "Show".into(),
            season: Some(2),
            tmdb_season: Some(2),
            year: None,
            query: "Show season 2".into(),
            files,
            candidates,
            pick: Some(0),
            low_confidence: true,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        }
    }

    #[tokio::test]
    async fn merged_entry_renumbers_into_anilist_numbering() {
        // AniList 55 covers TMDB s1 (1-12) and s2 (1-12) as its 1-24.
        seed_tmdb_episode_spans_for_tests(&[(9, 1, 1, 12, 55, 1), (9, 2, 1, 12, 55, 13)]).await;
        let g = group(vec![file(Some(5)), file(Some(12))], vec![entry(55, "Show")]);
        let out = apply_season_mapping(g).await;
        assert_eq!(out.len(), 1);
        let g = &out[0];
        assert_eq!(g.picked().map(|e| e.id), Some(55));
        assert_eq!(g.files[0].episode, Some(17));
        assert_eq!(g.files[0].source_episode, Some(5));
        assert_eq!(g.files[1].episode, Some(24));
        assert!(!g.low_confidence, "a mapped pick is not a guess");
        assert_eq!(
            g.mapping_note.as_deref(),
            Some("Season 2 through the TMDB mapping")
        );
        clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn split_cour_season_splits_the_group_and_keeps_unmapped_files() {
        // TMDB s2: 1-12 are AniList 55 e1-12, 13-24 are AniList 56 e1-12.
        seed_tmdb_episode_spans_for_tests(&[(9, 2, 1, 12, 55, 1), (9, 2, 13, 24, 56, 1)]).await;
        let g = group(
            vec![file(Some(20)), file(Some(3)), file(Some(30)), file(None)],
            vec![entry(56, "Show Part 2"), entry(55, "Show")],
        );
        let out = apply_season_mapping(g).await;
        assert_eq!(
            out.len(),
            3,
            "{:?}",
            out.iter().map(|g| &g.mapping_note).collect::<Vec<_>>()
        );
        // Span order: entry 55 first, then 56, then the leftovers.
        assert_eq!(out[0].picked().map(|e| e.id), Some(55));
        assert_eq!(out[0].files.len(), 1);
        assert_eq!(out[0].files[0].episode, Some(3));
        assert_eq!(
            out[0].files[0].source_episode, None,
            "identity numbering carries no note"
        );
        assert_eq!(
            out[0].mapping_note.as_deref(),
            Some("Season 2, episodes 3 to 3, through the TMDB mapping")
        );
        assert_eq!(out[1].picked().map(|e| e.id), Some(56));
        assert_eq!(out[1].files[0].episode, Some(8));
        assert_eq!(out[1].files[0].source_episode, Some(20));
        assert_eq!(out[2].files.len(), 2, "episode 30 and the unnumbered file");
        assert_eq!(
            out[2].picked().map(|e| e.id),
            None,
            "unmapped files cannot silently inherit the search's pick"
        );
        assert!(
            out[2]
                .mapping_note
                .as_deref()
                .unwrap()
                .starts_with("Cannot map every episode")
        );
        clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn untouched_without_a_season_or_a_known_show() {
        seed_tmdb_episode_spans_for_tests(&[(9, 2, 1, 12, 55, 1)]).await;
        let mut g = group(vec![file(Some(5))], vec![entry(55, "Show")]);
        g.tmdb_season = None;
        let out = apply_season_mapping(g.clone()).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].files[0].episode, Some(5));
        assert!(out[0].mapping_note.is_none());

        let g = group(vec![file(Some(5))], vec![entry(999, "Unknown Show")]);
        let out = apply_season_mapping(g).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].mapping_note.is_none());
        assert!(out[0].low_confidence, "the search's own verdict stands");
        clear_cache_for_tests().await;
    }

    #[test]
    fn entry_from_detail_carries_the_search_fields() {
        let d = AnimeDetail {
            release_date: None,
            is_adult: false,
            id: 7,
            id_mal: Some(8),
            title_romaji: "R".into(),
            title_english: "E".into(),
            title_native: "N".into(),
            cover_url: "c".into(),
            banner_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: "Finished".into(),
            episodes: Some(12),
            duration: None,
            season: String::new(),
            season_year: Some(2021),
            end_year: None,
            description: String::new(),
            genres: Vec::new(),
            average_score: Some(80),
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        };
        let e = entry_from_detail(&d);
        assert_eq!(
            (e.id, e.id_mal, e.title_english.as_str(), e.episodes),
            (7, Some(8), "E", Some(12))
        );
        assert_eq!(e.source, "anilist");
    }

    #[tokio::test]
    async fn combined_file_crossing_entries_has_no_automatic_match() {
        seed_tmdb_episode_spans_for_tests(&[(9, 2, 1, 12, 55, 1), (9, 2, 13, 24, 56, 1)]).await;
        let mut combined = file(Some(12));
        combined.episode_count = 2;
        let out = apply_season_mapping(group(
            vec![combined],
            vec![entry(55, "Show"), entry(56, "Part 2")],
        ))
        .await;
        assert_eq!(out.len(), 1);
        assert!(
            out[0].picked().is_none(),
            "a file crossing two entries must not be assigned to the first entry"
        );
        let cfg = crate::models::config::Config::default();
        let folders = std::collections::HashSet::new();
        let preview = super::super::preview::project_group(
            &out[0],
            &super::super::preview::ProjectionContext {
                layout: None,
                media_root: "/library",
                owned_folders: &folders,
                disk_folders: &folders,
                title_pref: "english",
                series_folder_format: &cfg.series_folder_format,
                season_folder_format: &cfg.season_folder_format,
                episode_file_format: &cfg.episode_file_format,
            },
        );
        assert_eq!(
            preview.counts.writes(),
            0,
            "the import execution uses this same projection"
        );
        assert_eq!(preview.kind, super::super::preview::GroupKind::NoMatch);
        clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn only_a_season_zero_crosswalk_clears_a_special_marker() {
        seed_tmdb_episode_spans_for_tests(&[(9, 0, 1, 1, 55, 1), (9, 2, 1, 12, 55, 1)]).await;
        let mut special = file(Some(1));
        special.special = true;
        let mut g = group(vec![special], vec![entry(55, "Show")]);
        let ordinary_season = apply_season_mapping(g.clone()).await;
        assert!(
            ordinary_season[0].files[0].special,
            "an unrelated extra is still a special"
        );
        g.tmdb_season = Some(0);
        let mapped_special = apply_season_mapping(g).await;
        assert!(!mapped_special[0].files[0].special);
        assert_eq!(mapped_special[0].files[0].episode, Some(1));
        clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn explicit_zero_mapping_assigns_regular_episode_one() {
        seed_tmdb_episode_spans_for_tests(&[(9, 2, 0, 12, 55, 1)]).await;
        let prologue = file(Some(0));
        let out = apply_season_mapping(group(
            vec![prologue, file(Some(12))],
            vec![entry(55, "Show")],
        ))
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].picked().map(|e| e.id), Some(55));
        assert_eq!(out[0].files[0].episode, Some(1));
        assert_eq!(out[0].files[0].source_episode, Some(0));
        assert!(
            !out[0].files[0].special,
            "a mapped episode zero is a regular library episode"
        );
        assert_eq!(out[0].files[1].episode, Some(13));
        clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn conflicts_and_partially_mapped_files_have_no_automatic_match() {
        seed_tmdb_episode_spans_for_tests(&[(9, 2, 1, 12, 55, 1), (9, 2, 6, 6, 56, 1)]).await;
        let out = apply_season_mapping(group(
            vec![file(Some(6))],
            vec![entry(55, "Show"), entry(56, "Other")],
        ))
        .await;
        assert!(out[0].picked().is_none());
        assert!(
            out[0]
                .mapping_note
                .as_deref()
                .unwrap()
                .contains("conflicting mappings")
        );
        clear_cache_for_tests().await;

        seed_tmdb_episode_spans_for_tests(&[(9, 2, 1, 12, 55, 1)]).await;
        let mut combined = file(Some(12));
        combined.episode_count = 2;
        let out = apply_season_mapping(group(vec![combined], vec![entry(55, "Show")])).await;
        assert!(out[0].picked().is_none());
        assert!(
            out[0]
                .mapping_note
                .as_deref()
                .unwrap()
                .contains("no verified mapping for episode 13")
        );
        clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn complete_combined_file_maps_to_consecutive_target_episodes() {
        seed_tmdb_episode_spans_for_tests(&[(9, 2, 13, 24, 55, 1)]).await;
        let mut combined = file(Some(13));
        combined.episode_count = 2;
        let out = apply_season_mapping(group(vec![combined], vec![entry(55, "Part 2")])).await;
        assert_eq!(out[0].picked().map(|e| e.id), Some(55));
        assert_eq!(out[0].files[0].episode, Some(1));
        assert_eq!(out[0].files[0].episode_count, 2);
        assert_eq!(out[0].files[0].source_episode, Some(13));
        clear_cache_for_tests().await;
    }
}

/// Longest sequel chain the absolute walk follows. Long-running
/// franchises (Gintama, JoJo) are cut into a handful of AniList
/// entries; eight hops covers every one with a plausible single
/// absolute numbering.
pub const MAX_SEQUEL_HOPS: usize = 8;

/// Forward chain from `start_id`: the entry itself, then each TV
/// `SEQUEL` in turn, until an entry has none, a fetch fails, or the
/// hop cap. Detail fetches memoize in AniList's `DETAIL_CACHE`.
async fn sequel_chain(start_id: i64) -> Vec<AnimeEntry> {
    let mut chain: Vec<AnimeEntry> = Vec::new();
    let mut next = Some(start_id);
    while let Some(id) = next {
        if chain.len() > MAX_SEQUEL_HOPS || chain.iter().any(|e| e.id == id) {
            break;
        }
        let Ok(d) = anilist::get_anime_detail(id).await else {
            break;
        };
        let start_format = chain
            .first()
            .map(|e| e.format.clone())
            .unwrap_or_else(|| d.format.clone());
        next = d
            .relations
            .iter()
            .filter(|r| {
                r.relation_type == "SEQUEL"
                    && r.media_type == "ANIME"
                    && (r.format == "TV" || r.format == start_format)
            })
            .min_by_key(|r| r.season_year.unwrap_or(i32::MAX))
            .map(|r| r.id);
        chain.push(entry_from_detail(&d));
    }
    chain
}

/// Route absolute-numbered files along `chain` (first entry first).
/// Returns `(chain index, files)` buckets in chain order, each file
/// renumbered relative to its entry (`source_episode` keeps the
/// absolute number when it changed), plus the files no entry holds:
/// unnumbered ones and numbers past the chain's end. An entry with an
/// unknown episode count (still airing) is open-ended and takes every
/// remaining number.
pub fn route_absolute(
    files: &[CandidateFile],
    chain: &[AnimeEntry],
) -> (Vec<(usize, Vec<CandidateFile>)>, Vec<CandidateFile>) {
    // The existing sequel-chain decision supplies the source scheme. Apply
    // it through the same complete-file resolver as the explicit TMDB map.
    let mut rules = Vec::new();
    let mut cum = 0i32;
    for e in chain {
        let count = e
            .episodes
            .and_then(|n| anilist::plausible_episode_count(i64::from(n)));
        if e.episodes.is_some() && count.is_none() {
            break;
        }
        let end = match count {
            Some(n) => cum.checked_add(n),
            None => Some(anilist::MAX_PLAUSIBLE_EPISODES),
        };
        let Some(end) = end.filter(|n| *n > cum && *n <= anilist::MAX_PLAUSIBLE_EPISODES) else {
            break;
        };
        rules.push(MappingRule {
            source_first: cum + 1,
            source_last: end,
            target_entry: e.id,
            target_first: 1,
        });
        cum = end;
        if count.is_none() {
            break;
        }
    }
    let mut buckets: BTreeMap<usize, Vec<CandidateFile>> = BTreeMap::new();
    let mut leftovers: Vec<CandidateFile> = Vec::new();
    for f in files {
        match source_file_span(f)
            .and_then(|(first, last)| episode_mapping::resolve_file(&rules, first, last))
        {
            Ok(result) => {
                let idx = chain
                    .iter()
                    .position(|entry| entry.id == result.entry)
                    .expect("mapping entry comes from this chain");
                let mut nf = f.clone();
                nf.episode = Some(result.first);
                nf.source_episode = f.episode.filter(|e| *e != result.first);
                buckets.entry(idx).or_default().push(nf);
            }
            Err(_) => leftovers.push(f.clone()),
        }
    }
    (buckets.into_iter().collect(), leftovers)
}

/// Resolve absolute numbering for a group with no season whose file
/// numbers run past the matched entry's episode count. Groups the
/// TMDB mapping already shaped, groups with a season, and groups
/// whose numbers fit the entry come back unchanged.
pub async fn apply_absolute_numbering(group: SeriesGroup) -> Vec<SeriesGroup> {
    if group.tmdb_season.is_some() || group.resolved_by_id {
        return vec![group];
    }
    let Some(pick) = group.picked().cloned() else {
        return vec![group];
    };
    let Some(count) = pick.episodes.filter(|n| *n > 0) else {
        return vec![group];
    };
    if pick.id <= 0
        || !group
            .files
            .iter()
            .any(|f| source_file_span(f).is_ok_and(|(_, last)| last > count))
    {
        return vec![group];
    }
    let chain = sequel_chain(pick.id).await;
    if chain.len() < 2 {
        return vec![group];
    }
    let (buckets, leftovers) = route_absolute(&group.files, &chain);
    if leftovers.is_empty() && !buckets.iter().any(|(idx, _)| *idx > 0) {
        return vec![group];
    }

    let mut candidates = group.candidates.clone();
    for e in &chain {
        if !candidates.iter().any(|c| c.id == e.id) {
            candidates.push(e.clone());
        }
    }
    let mut out: Vec<SeriesGroup> = Vec::new();
    for (idx, files) in buckets {
        let entry = &chain[idx];
        let (lo, hi) = files
            .iter()
            .filter_map(|f| f.source_episode.or(f.episode))
            .fold((i32::MAX, i32::MIN), |(lo, hi), e| (lo.min(e), hi.max(e)));
        let mut g = group.clone();
        g.files = files;
        g.candidates = candidates.clone();
        g.pick = candidates.iter().position(|c| c.id == entry.id);
        g.low_confidence = false;
        g.search_error = None;
        g.resolved_by_id = true;
        g.mapping_note = Some(if idx == 0 {
            "Absolute numbering; first entry of the sequel chain".to_string()
        } else {
            format!("Absolute numbering; episodes {lo} to {hi} through the sequel chain")
        });
        out.push(g);
    }
    if !leftovers.is_empty() {
        let mut g = group;
        g.files = leftovers;
        out.push(unresolved_group(g,
            "Cannot map every episode in these files to one entry of the sequel chain. Automatic match cleared.".to_string()));
    }
    out
}

#[cfg(test)]
mod absolute_tests {
    use super::*;
    use crate::services::manual_import::parse::TitleSource;
    use std::path::PathBuf;

    fn entry(id: i64, episodes: Option<i32>) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("T{id}"),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes,
            season_year: None,
            source: "anilist".into(),
            average_score: None,
        }
    }

    fn file(ep: Option<i32>) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from("x"),
            rel_path: "x".into(),
            file_name: "x.mkv".into(),
            size_bytes: 1,
            parsed_title: Some("Show".into()),
            title_source: TitleSource::Filename,
            season: None,
            episode: ep,
            year: None,
            group: None,
            quality_label: "Unknown".into(),
            selected: true,
            episode_count: 1,
            special: false,
            source_episode: None,
        }
    }

    #[test]
    fn routes_absolute_numbers_along_the_chain() {
        // JJK: S1 24, S2 23, S3 24 (the motivating case for #30).
        let chain = vec![entry(1, Some(24)), entry(2, Some(23)), entry(3, Some(24))];
        let files = vec![
            file(Some(10)),
            file(Some(55)),
            file(Some(30)),
            file(Some(200)),
            file(None),
        ];
        let (buckets, leftovers) = route_absolute(&files, &chain);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].0, 0);
        assert_eq!(buckets[0].1[0].episode, Some(10));
        assert_eq!(
            buckets[0].1[0].source_episode, None,
            "in range: no renumbering"
        );
        assert_eq!(buckets[1].0, 1);
        assert_eq!(
            (buckets[1].1[0].episode, buckets[1].1[0].source_episode),
            (Some(6), Some(30))
        );
        assert_eq!(buckets[2].0, 2);
        assert_eq!(
            (buckets[2].1[0].episode, buckets[2].1[0].source_episode),
            (Some(8), Some(55))
        );
        assert_eq!(
            leftovers.len(),
            2,
            "200 is past the chain; None has no number"
        );
    }

    #[test]
    fn airing_entry_is_open_ended() {
        let chain = vec![entry(1, Some(12)), entry(2, None)];
        let files = vec![file(Some(13)), file(Some(99))];
        let (buckets, leftovers) = route_absolute(&files, &chain);
        assert!(leftovers.is_empty());
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].0, 1);
        let eps: Vec<Option<i32>> = buckets[0].1.iter().map(|f| f.episode).collect();
        assert_eq!(eps, vec![Some(1), Some(87)]);
    }

    #[test]
    fn combined_absolute_file_cannot_cross_an_entry_boundary() {
        let mut crossing = file(Some(12));
        crossing.episode_count = 2;
        let mut valid = file(Some(13));
        valid.episode_count = 2;
        let (buckets, leftovers) = route_absolute(
            &[crossing, valid],
            &[entry(1, Some(12)), entry(2, Some(12))],
        );
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].0, 1);
        assert_eq!(buckets[0].1[0].episode, Some(1));
        assert_eq!(buckets[0].1[0].episode_count, 2);
        assert_eq!(leftovers.len(), 1);
        assert_eq!(leftovers[0].episode, Some(12));
    }

    #[test]
    fn malformed_absolute_chain_does_not_overflow_or_become_open_ended() {
        let (buckets, leftovers) = route_absolute(&[file(Some(5))], &[entry(1, Some(i32::MAX))]);
        assert!(buckets.is_empty());
        assert_eq!(leftovers.len(), 1);
    }

    #[tokio::test]
    async fn untouched_when_numbers_fit_or_a_season_is_known() {
        let mut g = SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files: vec![file(Some(5))],
            candidates: vec![entry(1, Some(24))],
            pick: Some(0),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        };
        // Fits: no fetch, no change (a fetch here would hit the
        // network, so this doubles as a guard on the trigger).
        let out = apply_absolute_numbering(g.clone()).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].mapping_note.is_none());
        g.files = vec![file(Some(55))];
        g.tmdb_season = Some(1);
        let out = apply_absolute_numbering(g).await;
        assert!(
            out[0].mapping_note.is_none(),
            "season groups belong to the TMDB path"
        );
    }
}

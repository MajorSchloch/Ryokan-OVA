//! Exact provider-season coordinates, separate from catalogue season inference.
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex_lite::Regex;
use sqlx::SqlitePool;

use super::{ReleaseMapping, SOURCE_SXXEXX, source_heading, source_identity_name};
use crate::models::metadata_cache;
use crate::services::anibridge::{self, AcquisitionSeasonMap, AcquisitionSeasonMatch};
use crate::services::anilist::{AnimeDetail, MAX_PLAUSIBLE_EPISODES};
use crate::services::auto_search::{
    SearchTarget, collect_aliases, collect_synonym_aliases, infer_season_from_detail,
    normalize_title,
};
use crate::services::media;

const QUERY_CAP: usize = 4;
static SEASON_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(.*?)(?:[\s._:-]+)(?:s(\d{1,2})|season\s*(\d{1,2})|(\d{1,2})(?:st|nd|rd|th)\s+season|(\d{1,2}))\s*$").unwrap()
});
static NATIVE_SEASON_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*第([一二三四五六七八九十]|\d{1,2})[季期]\s*$").unwrap());

struct Alias {
    raw: String,
    name: String,
}

struct Stem {
    name: String,
    coordinate: (i32, i32),
    confirm_owner: bool,
    priority: u8,
}

pub(crate) struct ProviderNumbering {
    owner_id: i64,
    local_episode: i32,
    tv_series: bool,
    map: AcquisitionSeasonMap,
    coordinates: Vec<(i32, i32)>,
    stems: Vec<Stem>,
    own_names: Vec<String>,
    owner_aliases: Vec<String>,
}

fn aliases(detail: &AnimeDetail) -> Vec<Alias> {
    let mut seen = HashSet::new();
    collect_aliases(detail)
        .into_iter()
        .chain(collect_synonym_aliases(detail))
        .filter_map(|raw| {
            if raw.len() > media::MAX_RELEASE_TITLE_BYTES {
                return None;
            }
            let name = normalize_title(&raw);
            (!name.is_empty() && seen.insert(name.clone())).then_some(Alias { raw, name })
        })
        .collect()
}

fn stripped_season(title: &str, season: i32) -> Option<String> {
    if let Some(captures) = SEASON_SUFFIX.captures(title) {
        let number = (2..=5)
            .find_map(|index| captures.get(index))?
            .as_str()
            .parse::<i32>()
            .ok()?;
        if number == season {
            return Some(captures[1].trim().to_string());
        }
    }
    let captures = NATIVE_SEASON_SUFFIX.captures(title)?;
    let number = captures[2].parse::<i32>().ok().or_else(|| {
        "一二三四五六七八九十"
            .chars()
            .position(|ch| captures[2].starts_with(ch))
            .map(|index| index as i32 + 1)
    })?;
    (number == season).then(|| captures[1].trim().to_string())
}

fn add_stem(
    stems: &mut Vec<Stem>,
    name: String,
    coordinate: (i32, i32),
    confirm_owner: bool,
    priority: u8,
) {
    if name.is_empty() {
        return;
    }
    if let Some(stem) = stems
        .iter_mut()
        .find(|stem| stem.name == name && stem.coordinate == coordinate)
    {
        // A season-stripped own title is stronger than a shared source
        // root. A source root is stronger than an unqualified own alias.
        if priority < stem.priority {
            stem.confirm_owner = confirm_owner;
            stem.priority = priority;
        }
    } else {
        stems.push(Stem {
            name,
            coordinate,
            confirm_owner,
            priority,
        });
    }
}

impl ProviderNumbering {
    pub(crate) async fn load(
        db: &SqlitePool,
        detail: &AnimeDetail,
        target: &SearchTarget,
    ) -> Option<Self> {
        let SearchTarget::Episode(episode) = *target else {
            return None;
        };
        let format = detail.format.trim().to_ascii_uppercase();
        if detail.id <= 0
            || !(1..=MAX_PLAUSIBLE_EPISODES).contains(&episode)
            || format == "MOVIE"
            || (matches!(format.as_str(), "OVA" | "ONA" | "SPECIAL") && detail.episodes == Some(1))
        {
            return None;
        }
        let map = anibridge::acquisition_season_map(detail.id).await;
        if !map.has_spans() {
            return None;
        }
        let inferred_season = infer_season_from_detail(detail).max(1);
        // An agreeing canonical SxxExx keeps its ordinary file validation.
        // Only changed coordinates need a provider receipt.
        let coordinates: Vec<_> = map
            .coordinates_for(episode)
            .into_iter()
            .filter(|&(season, number)| season != inferred_season || number != episode)
            .collect();
        let own = aliases(detail);
        let own_names: Vec<_> = own.iter().map(|alias| alias.name.clone()).collect();
        let mut stems = Vec::new();
        for &coordinate in &coordinates {
            let (season, _) = coordinate;
            for alias in &own {
                add_stem(&mut stems, alias.name.clone(), coordinate, false, 3);
                if let Some(stripped) = stripped_season(&alias.raw, season) {
                    let name = normalize_title(&stripped);
                    add_stem(&mut stems, name, coordinate, false, 0);
                }
            }
        }
        let mut roots = HashMap::new();
        let mut root_names = HashSet::new();
        for &coordinate in coordinates.iter().take(QUERY_CAP) {
            let (season, provider_episode) = coordinate;
            let Some(root_id) = map.source_root_id(season, provider_episode) else {
                continue;
            };
            if root_id == detail.id {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = roots.entry(root_id) {
                let root_aliases = match metadata_cache::get_by_provider_id(db, root_id).await {
                    Ok(Some(cached))
                        if cached.provider_id == root_id
                            && cached.detail.id == root_id
                            && !cached.is_stale =>
                    {
                        aliases(&cached.detail)
                    }
                    _ => Vec::new(),
                };
                entry.insert(root_aliases);
            }
            for alias in &roots[&root_id] {
                root_names.insert(alias.name.clone());
                add_stem(&mut stems, alias.name.clone(), coordinate, true, 1);
            }
        }
        stems.sort_by_key(|stem| stem.priority);
        // A bare shared alias cannot confirm itself as the complete owner.
        let owner_aliases = own_names
            .iter()
            .filter(|name| !root_names.contains(*name))
            .cloned()
            .collect();
        Some(Self {
            owner_id: detail.id,
            local_episode: episode,
            tv_series: media::is_tv_format(&detail.format),
            map,
            coordinates,
            stems,
            own_names,
            owner_aliases,
        })
    }

    pub(crate) fn queries(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut queries = Vec::new();
        for &coordinate in &self.coordinates {
            let (season, episode) = coordinate;
            for stem in self
                .stems
                .iter()
                .filter(|stem| stem.coordinate == coordinate)
            {
                // Normalized root names remove provider-only `(TV)` and
                // punctuation, without adding a fuzzy or shortened alias.
                let query = format!("{} S{season:02}E{episode:02}", stem.name);
                if seen.insert(query.to_lowercase()) {
                    queries.push(query);
                    if queries.len() == QUERY_CAP {
                        return queries;
                    }
                }
            }
        }
        queries
    }

    pub(crate) fn choose(
        &self,
        title: &str,
        is_batch: bool,
    ) -> Result<Option<ReleaseMapping>, String> {
        if is_batch {
            return Ok(None);
        }
        if title.len() > media::MAX_RELEASE_TITLE_BYTES {
            return Err("Episode mapping: source title exceeds the maximum length".into());
        }
        if !SOURCE_SXXEXX.is_match(title) {
            return Ok(None);
        }
        let heading = source_heading(title, false)?;
        // This path cannot replace ordinary relative/reset decisions or
        // turn a trailing metadata marker into provider-season evidence.
        let Some(span) = heading.payload_span.filter(|span| span.season.is_some()) else {
            return Ok(None);
        };
        if !SOURCE_SXXEXX
            .find_iter(title)
            .any(|marker| marker.start() == heading.raw_name_prefix.len())
        {
            return Ok(None);
        }
        let name = source_identity_name(title, true)?;
        let season = span.season.expect("the explicit season was checked");
        let coordinate = (season, span.first);
        let recognized =
            self.own_names.contains(&name) || self.stems.iter().any(|stem| stem.name == name);
        if !recognized {
            return Ok(None);
        }
        if span.special || span.first != span.last || span.season != heading.season {
            return Err(
                "Episode mapping: provider source needs one explicit SxxExx episode".into(),
            );
        }
        let (raw_span, _) = super::validated_source_file_span(title)?;
        if raw_span.special {
            return Err("Episode mapping: provider source cannot name a special".into());
        }
        let stem = self
            .stems
            .iter()
            .find(|stem| stem.name == name && stem.coordinate == coordinate);
        match self.map.resolve(season, span.first) {
            AcquisitionSeasonMatch::Own(episode) if episode == self.local_episode => {
                let Some(stem) = stem else {
                    // No changed coordinate, or this name belongs to a
                    // different provider root. Never apply it cross-root.
                    return if self.coordinates.contains(&coordinate) {
                        Err(
                            "Episode mapping: provider name does not belong to this coordinate"
                                .into(),
                        )
                    } else {
                        Ok(None)
                    };
                };
                if stem.confirm_owner
                    && !self.owner_confirmation(title, heading.raw_name_prefix.len())
                {
                    return Err("Episode mapping: provider source root does not confirm the complete owner title".into());
                }
                ReleaseMapping::from_provider(
                    self.owner_id,
                    self.tv_series,
                    season,
                    span.first,
                    episode,
                    name,
                )
                .map(Some)
            }
            AcquisitionSeasonMatch::Own(_) => {
                Err("Episode mapping: provider source names another episode".into())
            }
            AcquisitionSeasonMatch::Other => {
                Err("Episode mapping: provider source belongs to another entry".into())
            }
            AcquisitionSeasonMatch::Ambiguous => {
                Err("Episode mapping: provider source has conflicting ownership".into())
            }
            AcquisitionSeasonMatch::Unknown => Ok(None),
        }
    }

    fn owner_confirmation(&self, title: &str, payload_start: usize) -> bool {
        let Some(payload_end) = SOURCE_SXXEXX
            .find_iter(title)
            .find(|marker| marker.start() == payload_start)
            .map(|marker| marker.end())
        else {
            return false;
        };
        let mut brackets = Vec::new();
        let mut start = None;
        let mut nested = false;
        for (index, ch) in title[payload_end..].char_indices() {
            match ch {
                '(' | '[' | '{' => {
                    if brackets.is_empty() && ch == '(' {
                        start = Some(payload_end + index + 1);
                        nested = false;
                    } else if start.is_some() {
                        nested = true;
                    }
                    brackets.push(ch);
                }
                ')' | ']' | '}' => {
                    brackets.pop();
                    if brackets.is_empty()
                        && let Some(from) = start.take()
                        && !nested
                    {
                        let segment = &title[from..payload_end + index];
                        // Semicolons can separate provider aliases. Commas
                        // can belong to an owner title, so test complete
                        // prefixes rather than splitting the title itself.
                        for alias in segment.split(';') {
                            if alias
                                .split(',')
                                .next()
                                .is_some_and(|first| normalize_title(first).is_empty())
                            {
                                continue;
                            }
                            for end in alias
                                .match_indices(',')
                                .map(|(index, _)| index)
                                .chain(std::iter::once(alias.len()))
                            {
                                if self.owner_aliases.contains(&normalize_title(&alias[..end])) {
                                    return true;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail(id: i64, romaji: &str, english: &str) -> AnimeDetail {
        serde_json::from_value(json!({
            "id":id, "id_mal":null, "title_romaji":romaji,
            "title_english":english, "title_native":"", "cover_url":"", "banner_url":"",
            "format":"TV", "status":"RELEASING", "status_display":"Releasing",
            "episodes":16, "duration":24, "season":"", "season_year":2026,
            "description":"", "genres":[], "average_score":null,
            "average_score_display":null, "score_is_ten_point":false, "score_class":"",
            "next_airing_episode":null, "next_airing_at":null, "synonyms":[],
            "streaming_episodes":[], "relations":[]
        }))
        .unwrap()
    }

    fn steel() -> AnimeDetail {
        detail(
            210482,
            "JoJo no Kimyou na Bouken: Steel Ball Run - 2nd & 3rd STAGE",
            "STEEL BALL RUN JoJo's Bizarre Adventure 2nd - 3rd STAGE",
        )
    }

    fn dawang() -> AnimeDetail {
        let mut owner = detail(199353, "Dawang Raoming 3", "");
        owner.title_native = "大王饶命 第三季".into();
        owner.synonyms = vec![
            "Spare Me, Great Lord! 3".into(),
            "Spare My Life, King 3".into(),
        ];
        owner
    }

    async fn real_roots(db: &SqlitePool) {
        let jojo = detail(
            14719,
            "JoJo no Kimyou na Bouken (TV)",
            "JoJo's Bizarre Adventure (TV)",
        );
        metadata_cache::upsert_provider(db, jojo.id, None, &jojo)
            .await
            .unwrap();
        let mut dawang = detail(120220, "Dawang Raoming", "");
        dawang.title_native = "大王饶命".into();
        dawang.synonyms = vec!["Spare Me, Great Lord!".into(), "Spare My Life, King".into()];
        metadata_cache::upsert_provider(db, dawang.id, None, &dawang)
            .await
            .unwrap();
    }

    async fn real_maps() {
        anibridge::seed_mapping_data_for_tests(&json!({
            "anilist:14719": {
                "tmdb_show:45790:s1":{"1-26":"1-26"},
                "tvdb_show:262954:s1":{"1-26":"1-26"}
            },
            "anilist:190327": {
                "tmdb_show:45790:s6":{"1":"1"},
                "tvdb_show:262954:s6":{"1":"1"}
            },
            "anilist:210482": {
                "tmdb_show:45790:s6":{"1-11":"2-12"},
                "tvdb_show:262954:s6":{"1-11":"2-12"}
            },
            "tvdb_show:413480:s1":{"anilist:120220":{"1-12":"1-12"}},
            "tvdb_show:413480:s2":{"anilist:146543":{"1-12":"1-12"}},
            "tvdb_show:413480:s3":{"anilist:199353":{"1-16":"1-16"}}
        }))
        .await;
    }

    const STEEL_SOURCE: &str = "JoJos Bizarre Adventure S06E09 Frightening Monsters Part 1 1080p NF WEB-DL DUAL AAC2.0 H.264-VARYG (JoJo no Kimyou na Bouken: Steel Ball Run - 2nd & 3rd STAGE, Dual-Audio, Multi-Subs)";

    #[tokio::test]
    async fn provider_steel_maps_exact_sixth_season_episode_and_freezes_heading() {
        let _lock = anibridge::ENV_LOCK.lock().await;
        real_maps().await;
        let db = crate::test_support::in_memory_pool().await;
        real_roots(&db).await;
        let numbering = ProviderNumbering::load(&db, &steel(), &SearchTarget::Episode(8))
            .await
            .unwrap();
        let mapping = numbering.choose(STEEL_SOURCE, false).unwrap().unwrap();
        assert_eq!(mapping.offset(), 1);
        assert_eq!(mapping.provider_heading(), Some("jojos bizarre adventure"));
        assert_eq!(
            mapping
                .resolve_named_file("JoJos.Bizarre.Adventure.S06E09.1080p.WEB-DL.mkv")
                .unwrap()
                .first,
            8
        );
        for filename in [
            "JoJos Bizarre Adventure S06E08 1080p.mkv",
            "JoJos Bizarre Adventure S05E09 1080p.mkv",
            "JoJo no Kimyou na Bouken S06E09 1080p.mkv",
            "JoJos Bizarre Adventure - 09.mkv",
        ] {
            assert!(mapping.resolve_named_file(filename).is_err(), "{filename}");
        }
        let queries = numbering.queries();
        assert!(!queries.is_empty() && queries.len() <= QUERY_CAP);
        assert!(
            queries
                .iter()
                .any(|query| query == "jojos bizarre adventure S06E09")
        );
        assert!(!queries.iter().any(|query| query.contains("(TV)")));
        assert!(queries.iter().all(|query| query.ends_with("S06E09")));
    }

    #[tokio::test]
    async fn provider_shared_root_needs_complete_post_episode_owner_confirmation() {
        let _lock = anibridge::ENV_LOCK.lock().await;
        real_maps().await;
        let db = crate::test_support::in_memory_pool().await;
        real_roots(&db).await;
        let numbering = ProviderNumbering::load(&db, &steel(), &SearchTarget::Episode(8))
            .await
            .unwrap();
        let owner = &steel().title_romaji;
        for title in [
            "JoJos Bizarre Adventure S06E09 1080p".to_string(),
            format!("({owner}) JoJos Bizarre Adventure S06E09 1080p"),
            format!("JoJos Bizarre Adventure S06E09 1080p [metadata ({owner})]"),
            format!("JoJos Bizarre Adventure S06E09 1080p (Dual-Audio, {owner})"),
            format!("JoJos Bizarre Adventure S06E09 1080p ({owner} Extra Story, Multi-Subs)"),
            format!("JoJos Bizarre Adventure S06E09 1080p (({owner}))"),
        ] {
            assert!(
                !matches!(numbering.choose(&title, false), Ok(Some(_))),
                "{title}"
            );
        }
        assert!(
            numbering
                .choose(
                    &format!("JoJos Bizarre Adventure S06E09 ({owner}; Multi-Subs)"),
                    false
                )
                .unwrap()
                .is_some()
        );
        for title in [
            STEEL_SOURCE.replace("S06E09", "S06E08"),
            STEEL_SOURCE.replace("S06E09", "S06E01"),
            STEEL_SOURCE.replace("S06E09", "S06E09-E10"),
            STEEL_SOURCE.replace("S06E09", "S00E09"),
        ] {
            assert!(numbering.choose(&title, false).is_err(), "{title}");
        }
        assert!(numbering.choose(STEEL_SOURCE, true).unwrap().is_none());
        assert!(
            numbering
                .choose("Unrelated Show S06E09", false)
                .unwrap()
                .is_none()
        );
        assert!(
            numbering
                .choose("[Group] JoJo no Kimyou na Bouken - 08 [1080p]", false)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn provider_dawang_stripped_own_alias_beats_identical_shared_root() {
        let _lock = anibridge::ENV_LOCK.lock().await;
        real_maps().await;
        let db = crate::test_support::in_memory_pool().await;
        real_roots(&db).await;
        let numbering = ProviderNumbering::load(&db, &dawang(), &SearchTarget::Episode(9))
            .await
            .unwrap();
        let gecko = "[Gecko] Spare Me, Great Lord! - S03E09 (大王饶命; Da Wang Rao Ming 3; Master's Mercy 3) [WETV.WEB-DL 1080P AVC, AAC, M-SUB][1A865949]";
        let toons = "[ToonsHub] Spare Me Great Lord S03E09 1080p IVI WEB-DL ZHO AAC2.0 H.264 (Dawang Raoming, Multi-Subs)";
        let filename =
            "Spare.Me.Great.Lord.S03E09.1080p.IVI.WEB-DL.ZHO.AAC2.0.H.264.MSubs-ToonsHub.mkv";
        for title in [gecko, toons, filename] {
            let mapping = numbering.choose(title, false).unwrap().unwrap();
            assert_eq!(mapping.offset(), 0);
            assert_eq!(mapping.resolve_named_file(filename).unwrap().first, 9);
        }
        assert!(
            numbering
                .choose("大王饶命 S03E09 [1080p]", false)
                .unwrap()
                .is_some()
        );
        assert!(numbering.queries().len() <= QUERY_CAP);
        assert!(
            numbering
                .queries()
                .iter()
                .any(|query| query == "spare me great lord S03E09")
        );
        assert!(
            numbering
                .choose(&gecko.replace("S03E09", "S02E09"), false)
                .is_err()
        );

        assert!(
            numbering
                .choose("Dawang Raoming S03E09 (Dawang Raoming)", false)
                .unwrap()
                .is_some()
        );

        // This artificial owner has no numbered own form that derives
        // Dawang Raoming. Its bare alias is shared only with the root.
        let mut bare = dawang();
        bare.title_romaji = "Specific Complete Owner".into();
        bare.title_english = "Dawang Raoming".into();
        let numbering = ProviderNumbering::load(&db, &bare, &SearchTarget::Episode(9))
            .await
            .unwrap();
        assert!(
            numbering
                .choose("Dawang Raoming S03E09 (Dawang Raoming)", false)
                .is_err()
        );
        assert!(
            numbering
                .choose("Dawang Raoming S03E09 (Specific Complete Owner)", false)
                .unwrap()
                .is_some()
        );
        assert!(
            numbering
                .choose(
                    "Dawang Raoming S03E09 (metadata; Spare Me, Great Lord! 3; Multi-Subs)",
                    false
                )
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn provider_roots_are_coordinate_scoped_and_missing_cache_cannot_invent_a_colon_root() {
        let _lock = anibridge::ENV_LOCK.lock().await;
        anibridge::seed_mapping_data_for_tests(&json!({
            "tmdb_show:200:s1":{"anilist:101":{"1-3":"1-3"}},
            "tmdb_show:200:s2":{"anilist:100":{"1-3":"1-3"}},
            "tvdb_show:300:s1":{"anilist:102":{"1-3":"1-3"}},
            "tvdb_show:300:s3":{"anilist:100":{"1-3":"1-3"}}
        }))
        .await;
        let db = crate::test_support::in_memory_pool().await;
        for (id, title) in [(101, "First Source Root"), (102, "Second Source Root")] {
            let root = detail(id, title, "");
            metadata_cache::upsert_provider(&db, id, None, &root)
                .await
                .unwrap();
        }
        let owner = detail(100, "Own Complete Title", "");
        let numbering = ProviderNumbering::load(&db, &owner, &SearchTarget::Episode(1))
            .await
            .unwrap();
        for (root, season) in [("First Source Root", 2), ("Second Source Root", 3)] {
            assert!(
                numbering
                    .choose(
                        &format!("{root} S{season:02}E01 (Own Complete Title)"),
                        false
                    )
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            numbering
                .choose("First Source Root S03E01 (Own Complete Title)", false)
                .is_err()
        );
        assert!(
            numbering
                .choose("Second Source Root S02E01 (Own Complete Title)", false)
                .is_err()
        );

        real_maps().await;
        let empty_db = crate::test_support::in_memory_pool().await;
        let numbering = ProviderNumbering::load(&empty_db, &steel(), &SearchTarget::Episode(8))
            .await
            .unwrap();
        assert!(numbering.choose(STEEL_SOURCE, false).unwrap().is_none());
        assert!(numbering.choose("JoJo no Kimyou na Bouken S06E09 (JoJo no Kimyou na Bouken: Steel Ball Run - 2nd & 3rd STAGE)", false).unwrap().is_none());
    }

    #[tokio::test]
    async fn provider_conflicts_stay_held_without_reverse_queries_and_canonical_paths_stay_ordinary()
     {
        let _lock = anibridge::ENV_LOCK.lock().await;
        let db = crate::test_support::in_memory_pool().await;
        anibridge::seed_mapping_data_for_tests(&json!({
            "anilist:100": {
                "tmdb_show:200:s2":{"1-3":"1-3"},
                "tvdb_show:300:s2":{"1-3":"2-4"}
            }
        }))
        .await;
        let owner = detail(100, "Complete Owner", "");
        let numbering = ProviderNumbering::load(&db, &owner, &SearchTarget::Episode(2))
            .await
            .unwrap();
        assert!(numbering.queries().is_empty());
        assert!(numbering.choose("Complete Owner S02E02", false).is_err());
        assert!(
            numbering
                .choose("Complete Owner - 02", false)
                .unwrap()
                .is_none()
        );
        assert!(
            numbering
                .choose("Complete Owner S03E02", false)
                .unwrap()
                .is_none()
        );

        anibridge::seed_mapping_data_for_tests(&json!({
            "tmdb_show:200:s2":{"anilist:100":{"1-3":"1-3"}}
        }))
        .await;
        let owner = detail(100, "Complete Owner Season 2", "Complete Owner");
        let numbering = ProviderNumbering::load(&db, &owner, &SearchTarget::Episode(2))
            .await
            .unwrap();
        assert!(numbering.queries().is_empty());
        assert!(
            numbering
                .choose("Complete Owner S02E02", false)
                .unwrap()
                .is_none()
        );
        assert!(numbering.choose("Complete Owner S02E03", false).is_err());
        for format in ["MOVIE", "OVA", "ONA", "SPECIAL"] {
            let mut standalone = owner.clone();
            standalone.format = format.into();
            standalone.episodes = Some(1);
            assert!(
                ProviderNumbering::load(&db, &standalone, &SearchTarget::Episode(1))
                    .await
                    .is_none()
            );
        }
        assert!(
            ProviderNumbering::load(&db, &owner, &SearchTarget::Single)
                .await
                .is_none()
        );
        anibridge::clear_cache_for_tests().await;
        assert!(
            ProviderNumbering::load(&db, &owner, &SearchTarget::Episode(2))
                .await
                .is_none()
        );
    }
}

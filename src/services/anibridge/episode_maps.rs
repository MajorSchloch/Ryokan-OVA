//! Direct MAL → AniList episode maps from the published AniBridge dataset.
//!
//! These maps are directional. An ID association, reverse edge, count difference,
//! or common pivot does not establish a MAL episode mapping.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::services::anilist::MAX_PLAUSIBLE_EPISODES;
use crate::services::episode_mapping::{self, MappingRule};

#[derive(Clone, Debug)]
pub struct MalSource {
    pub mal_id: i64,
    /// Includes all destination entries, so conflicting edges stay visible.
    pub rules: Vec<MappingRule>,
}

#[derive(Clone, Debug)]
pub struct MalEpisodePlan {
    pub anilist_id: i64,
    pub sources: Vec<MalSource>,
    pub unsupported: bool,
}

impl MalEpisodePlan {
    /// A different source episode for the same destination is a conflict even
    /// when both providers happen to give it the same title.
    pub fn source_slots(&self) -> Result<BTreeMap<i32, (i64, i32)>, String> {
        if self.unsupported {
            return Err("Episode mapping: unsupported provider episode ranges".to_string());
        }
        let mut slots = BTreeMap::new();
        for source in &self.sources {
            for rule in &source.rules {
                for number in rule.source_first..=rule.source_last {
                    let identity = episode_mapping::resolve_episode(&source.rules, number)
                        .map_err(|e| e.to_string())?;
                    if identity.entry != self.anilist_id {
                        continue;
                    }
                    // Validate the reverse direction too: two source episodes
                    // cannot silently collapse into one library episode.
                    episode_mapping::source_episode(&source.rules, identity)
                        .map_err(|e| e.to_string())?;
                    let origin = (source.mal_id, number);
                    if let Some(previous) = slots.insert(identity.number, origin)
                        && previous != origin
                    {
                        return Err(format!(
                            "Episode mapping: conflicting metadata sources for episode {}",
                            identity.number
                        ));
                    }
                }
            }
        }
        Ok(slots)
    }

    /// Raw Kitsu numbers must not fill holes in a shifted or split MAL list.
    pub fn permits_unmapped_fallback(&self, primary_mal: Option<i64>) -> bool {
        !self.unsupported
            && self.sources.len() == 1
            && Some(self.sources[0].mal_id) == primary_mal
            && self.sources[0]
                .rules
                .iter()
                .all(|r| r.target_entry == self.anilist_id && r.source_first == r.target_first)
    }
}

fn exact_id(key: &str, provider: &str) -> Option<i64> {
    key.strip_prefix(provider)?
        .strip_prefix(':')?
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
}

/// Single numbers are singleton ranges; a trailing dash is open-ended.
fn range(s: &str) -> Option<(i32, i32, bool)> {
    let (first, last, open) = if let Some((a, b)) = s.split_once('-') {
        (
            a.parse().ok()?,
            if b.is_empty() {
                MAX_PLAUSIBLE_EPISODES
            } else {
                b.parse().ok()?
            },
            b.is_empty(),
        )
    } else {
        let n = s.parse().ok()?;
        (n, n, false)
    };
    (first >= 1 && first <= last && last <= MAX_PLAUSIBLE_EPISODES).then_some((first, last, open))
}

fn rule(source: &str, target: &str, entry: i64) -> Option<MappingRule> {
    let (a, mut b, source_open) = range(source)?;
    let (c, mut d, target_open) = range(target)?;
    if source_open || target_open {
        // An open side may be bounded by a finite side. Two open sides are
        // bounded by the application's provider-episode cap, never i32::MAX.
        let length = (b - a).min(d - c);
        if source_open {
            b = a + length;
        }
        if target_open {
            d = c + length;
        }
    }
    if b - a != d - c {
        // Ratios, missing episodes and combined broadcast episodes require
        // explicit support. Do not clamp unequal finite ranges.
        return None;
    }
    let out = MappingRule {
        source_first: a,
        source_last: b,
        target_entry: entry,
        target_first: c,
    };
    episode_mapping::resolve_episode(&[out], b).ok()?;
    Some(out)
}

pub(super) fn build_mal_plans(data: &serde_json::Value) -> HashMap<i64, MalEpisodePlan> {
    let mut plans: HashMap<i64, MalEpisodePlan> = HashMap::new();
    let Some(entries) = data.as_object() else {
        return plans;
    };
    for (source_key, targets) in entries {
        let Some(mal_id) = exact_id(source_key, "mal") else {
            continue;
        };
        let Some(targets) = targets.as_object() else {
            continue;
        };
        let mut rules = Vec::new();
        let mut entries = HashSet::new();
        let mut unsupported = false;
        for (key, ranges) in targets {
            let Some(anilist_id) = exact_id(key, "anilist") else {
                continue;
            };
            let Some(ranges) = ranges.as_object() else {
                entries.insert(anilist_id);
                unsupported = true;
                continue;
            };
            // An empty object only associates IDs; it specifies no numbering.
            if ranges.is_empty() {
                continue;
            }
            entries.insert(anilist_id);
            for (from, to) in ranges {
                match to.as_str().and_then(|s| rule(from, s, anilist_id)) {
                    Some(r) => rules.push(r),
                    None => unsupported = true,
                }
            }
        }
        rules.sort_by_key(|r| {
            (
                r.source_first,
                r.source_last,
                r.target_entry,
                r.target_first,
            )
        });
        rules.dedup();
        for anilist_id in entries {
            let plan = plans.entry(anilist_id).or_insert_with(|| MalEpisodePlan {
                anilist_id,
                sources: Vec::new(),
                unsupported: false,
            });
            plan.unsupported |= unsupported;
            plan.sources.push(MalSource {
                mal_id,
                rules: rules.clone(),
            });
        }
    }
    for plan in plans.values_mut() {
        plan.sources.sort_by_key(|s| s.mal_id);
    }
    plans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::episode_mapping::EpisodeIdentity;
    use serde_json::json;

    #[test]
    fn combines_separate_provider_entries_without_title_specific_code() {
        let plans = build_mal_plans(&json!({
            "mal:10": {"anilist:80": {"1-12": "2-13"}},
            "mal:11": {"anilist:80": {"1": "1"}}
        }));
        let plan = &plans[&80];
        let slots = plan.source_slots().unwrap();
        assert_eq!(slots.len(), 13);
        assert_eq!(slots[&1], (11, 1));
        assert_eq!(slots[&2], (10, 1));
        assert_eq!(slots[&13], (10, 12));
        assert!(!plan.permits_unmapped_fallback(Some(10)));
    }

    #[test]
    fn shared_mal_entry_is_split_by_explicit_episode_ranges() {
        let plans = build_mal_plans(&json!({"mal:10": {
            "anilist:80": {"1": "1"}, "anilist:81": {"2-12": "1-11"}
        }}));
        assert_eq!(plans[&80].source_slots().unwrap().len(), 1);
        assert_eq!(plans[&81].source_slots().unwrap()[&6], (10, 7));
        assert!(!plans[&81].permits_unmapped_fallback(Some(10)));
    }

    #[test]
    fn reverse_edges_and_id_only_associations_do_not_invent_rules() {
        assert!(
            build_mal_plans(&json!({
                "anilist:80": {"mal:10": {"1-12": "2-13"}},
                "mal:11": {"anilist:81": {}},
                "mal:12:s1": {"anilist:82": {"1": "1"}}
            }))
            .is_empty()
        );
    }

    #[test]
    fn unsupported_ranges_do_not_fall_back_to_identity() {
        for to in ["1-6", "1-12|2", "1,3-13", "0-11", "1-2147483647"] {
            let plans = build_mal_plans(&json!({"mal:10": {"anilist:80": {"1-12": to}}}));
            assert!(plans[&80].source_slots().is_err(), "{to}");
            assert!(!plans[&80].permits_unmapped_fallback(Some(10)));
        }
        let plans = build_mal_plans(&json!({"mal:10": {"anilist:80": "invalid"}}));
        assert!(plans[&80].source_slots().is_err());
    }

    #[test]
    fn conflicts_are_rejected_in_both_directions() {
        for data in [
            json!({"mal:10": {"anilist:80": {"1-12": "1-12"}, "anilist:81": {"6": "1"}}}),
            json!({"mal:10": {"anilist:80": {"1-12": "1-12", "13": "12"}}}),
            json!({"mal:10": {"anilist:80": {"1": "1"}}, "mal:11": {"anilist:80": {"1": "1"}}}),
        ] {
            assert!(build_mal_plans(&data)[&80].source_slots().is_err());
        }
    }

    #[test]
    fn open_ranges_are_bounded_and_identity_allows_existing_fallback() {
        let plans = build_mal_plans(&json!({"mal:10": {"anilist:80": {"1-": "1-"}}}));
        assert_eq!(
            plans[&80].source_slots().unwrap().len(),
            MAX_PLAUSIBLE_EPISODES as usize
        );
        assert!(plans[&80].permits_unmapped_fallback(Some(10)));
        assert_eq!(rule("2-", "1-11", 80).unwrap().source_last, 12);
        assert_eq!(rule("2-12", "1-", 80).unwrap().source_last, 12);
        assert_eq!(rule("2-", "1-", 80).unwrap().source_last, 5000);
        assert_eq!(
            episode_mapping::source_episode(
                &[rule("2-", "1-", 80).unwrap()],
                EpisodeIdentity {
                    entry: 80,
                    number: 6
                }
            )
            .unwrap(),
            7
        );
    }
}

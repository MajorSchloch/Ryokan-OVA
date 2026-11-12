//! Resolve a known source numbering scheme into Ryokan episode identities.
//!
//! Callers must first identify the source entry and its numbering scheme.
//! Counts, filename order and a requested episode are not mapping evidence.
//! These functions never fall back to identity numbering after a mapping miss.
//! A file is accepted only when every held episode maps to consecutive episodes
//! of one entry. Otherwise importing it would require splitting the video.

use std::collections::BTreeMap;
use std::fmt;

use crate::services::anilist::MAX_PLAUSIBLE_EPISODES;
use crate::services::media::MAX_FILE_SPAN;

/// One verified, inclusive range in a single source numbering scheme.
/// Source episode zero is valid. Library episodes always start at one.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MappingRule {
    pub source_first: i32,
    pub source_last: i32,
    pub target_entry: i64,
    pub target_first: i32,
}

impl MappingRule {
    /// Adapt an already-selected affine mapping (target = source - offset).
    /// This validates the complete file span; it does not choose a numbering
    /// scheme or infer an offset from the requested destination episode.
    pub fn for_offset(
        target_entry: i64,
        source_first: i32,
        source_last: i32,
        offset: i32,
    ) -> Result<Self, MappingError> {
        let rule = Self {
            source_first,
            source_last,
            target_entry,
            target_first: source_first
                .checked_sub(offset)
                .ok_or(MappingError::InvalidRule)?,
        };
        rule.target_last()?;
        Ok(rule)
    }

    fn target_last(self) -> Result<i32, MappingError> {
        let length = self.source_last.checked_sub(self.source_first);
        let end = length.and_then(|n| self.target_first.checked_add(n));
        if self.target_entry == 0
            || !(0..=MAX_PLAUSIBLE_EPISODES).contains(&self.source_first)
            || !(self.source_first..=MAX_PLAUSIBLE_EPISODES).contains(&self.source_last)
            || !(1..=MAX_PLAUSIBLE_EPISODES).contains(&self.target_first)
            || !end.is_some_and(|n| (self.target_first..=MAX_PLAUSIBLE_EPISODES).contains(&n))
        {
            return Err(MappingError::InvalidRule);
        }
        Ok(end.expect("validated target end"))
    }

    fn resolve(self, episode: i32) -> Result<Option<EpisodeIdentity>, MappingError> {
        self.target_last()?;
        Ok((self.source_first..=self.source_last)
            .contains(&episode)
            .then(|| EpisodeIdentity {
                entry: self.target_entry,
                number: self.target_first + (episode - self.source_first),
            }))
    }
}

/// Keep the entry identity with the number: episode 1 of two entries differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EpisodeIdentity {
    pub entry: i64,
    pub number: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedFile {
    pub entry: i64,
    pub first: i32,
    pub last: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingError {
    InvalidSource,
    InvalidRule,
    Unmapped(i32),
    Ambiguous(i32),
    CrossesEntries,
    NonConsecutive,
}

impl fmt::Display for MappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Episode mapping: ")?;
        match self {
            Self::InvalidSource => write!(f, "invalid source episode range"),
            Self::InvalidRule => write!(f, "invalid mapping rule"),
            Self::Unmapped(ep) => write!(f, "no verified mapping for episode {ep}"),
            Self::Ambiguous(ep) => write!(f, "conflicting mappings for episode {ep}"),
            Self::CrossesEntries => write!(f, "one file covers more than one series entry"),
            Self::NonConsecutive => write!(f, "one file maps to non-consecutive episodes"),
        }
    }
}

/// Resolve all applicable rules. Input order must never decide the identity.
pub fn resolve_episode(
    rules: &[MappingRule],
    episode: i32,
) -> Result<EpisodeIdentity, MappingError> {
    if !(0..=MAX_PLAUSIBLE_EPISODES).contains(&episode) {
        return Err(MappingError::InvalidSource);
    }
    let mut result = None;
    for rule in rules {
        if let Some(candidate) = rule.resolve(episode)? {
            if result.is_some_and(|old| old != candidate) {
                return Err(MappingError::Ambiguous(episode));
            }
            result = Some(candidate);
        }
    }
    result.ok_or(MappingError::Unmapped(episode))
}

/// Resolve a complete file before naming it or writing any library state.
pub fn resolve_file(
    rules: &[MappingRule],
    first: i32,
    last: i32,
) -> Result<MappedFile, MappingError> {
    if first < 0
        || last > MAX_PLAUSIBLE_EPISODES
        || !last
            .checked_sub(first)
            .is_some_and(|n| (0..MAX_FILE_SPAN).contains(&n))
    {
        return Err(MappingError::InvalidSource);
    }
    let start = resolve_episode(rules, first)?;
    for source in first + 1..=last {
        let mapped = resolve_episode(rules, source)?;
        if mapped.entry != start.entry {
            return Err(MappingError::CrossesEntries);
        }
        if mapped.number != start.number + (source - first) {
            return Err(MappingError::NonConsecutive);
        }
    }
    Ok(MappedFile {
        entry: start.entry,
        first: start.number,
        last: start.number + (last - first),
    })
}

/// Reverse lookup for a search using the same verified numbering scheme.
/// Several source numbers for one target need a scheme choice, never a guess.
pub fn source_episode(rules: &[MappingRule], target: EpisodeIdentity) -> Result<i32, MappingError> {
    if target.entry == 0 || !(1..=MAX_PLAUSIBLE_EPISODES).contains(&target.number) {
        return Err(MappingError::InvalidSource);
    }
    let mut result = None;
    for rule in rules {
        let end = rule.target_last()?;
        if target.entry == rule.target_entry && (rule.target_first..=end).contains(&target.number) {
            let source = rule.source_first + (target.number - rule.target_first);
            if result.is_some_and(|old| old != source) {
                return Err(MappingError::Ambiguous(target.number));
            }
            // A reverse hit must also be unambiguous in the forward direction.
            if resolve_episode(rules, source)? != target {
                return Err(MappingError::Ambiguous(target.number));
            }
            result = Some(source);
        }
    }
    result.ok_or(MappingError::Unmapped(target.number))
}

/// Carry titles/dates with episode identities; never pad a short list from a
/// sequel or shift numbers while leaving the old episode titles in place.
pub fn map_metadata<T>(
    rules: &[MappingRule],
    source: impl IntoIterator<Item = (i32, T)>,
) -> Result<BTreeMap<EpisodeIdentity, T>, MappingError> {
    let mut mapped = BTreeMap::new();
    for (number, value) in source {
        let identity = resolve_episode(rules, number)?;
        if mapped.insert(identity, value).is_some() {
            return Err(MappingError::Ambiguous(number));
        }
    }
    Ok(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(first: i32, last: i32, entry: i64, target: i32) -> MappingRule {
        MappingRule {
            source_first: first,
            source_last: last,
            target_entry: entry,
            target_first: target,
        }
    }

    #[test]
    fn fate_prologue_order_round_trips_without_a_title_specific_branch() {
        let rules = [rule(0, 12, 19603, 1), rule(13, 25, 20792, 1)];
        for source in 0..=25 {
            let target = resolve_episode(&rules, source).unwrap();
            let expected = if source <= 12 {
                (19603, source + 1)
            } else {
                (20792, source - 12)
            };
            assert_eq!((target.entry, target.number), expected);
            assert_eq!(source_episode(&rules, target), Ok(source));
        }
        // The mechanism is data-driven, including for an unrelated entry.
        assert_eq!(
            resolve_episode(&[rule(0, 3, 77, 1)], 0),
            Ok(EpisodeIdentity {
                entry: 77,
                number: 1
            })
        );
    }

    #[test]
    fn metadata_titles_follow_the_same_mapping_as_files() {
        let rules = [rule(0, 12, 19603, 1)];
        let titles = map_metadata(
            &rules,
            [
                (0, "Prologue"),
                (1, "Winter Day, Fateful Night"),
                (12, "The Final Choice"),
            ],
        )
        .unwrap();
        assert_eq!(
            titles[&EpisodeIdentity {
                entry: 19603,
                number: 1
            }],
            "Prologue"
        );
        assert_eq!(
            titles[&EpisodeIdentity {
                entry: 19603,
                number: 2
            }],
            "Winter Day, Fateful Night"
        );
        assert_eq!(
            titles[&EpisodeIdentity {
                entry: 19603,
                number: 13
            }],
            "The Final Choice"
        );
        assert_eq!(titles.len(), 3, "missing metadata is not invented");
    }

    #[test]
    fn overlapping_absolute_and_relative_schemes_are_ambiguous() {
        let rules = [rule(1, 11, 210482, 1), rule(2, 12, 210482, 1)];
        assert_eq!(resolve_episode(&rules, 6), Err(MappingError::Ambiguous(6)));
        assert_eq!(
            resolve_episode(&[rules[1], rules[0]], 6),
            Err(MappingError::Ambiguous(6))
        );
        assert!(
            source_episode(
                &rules,
                EpisodeIdentity {
                    entry: 210482,
                    number: 6
                }
            )
            .is_err()
        );
        // A verified source scheme resolves the reported one-episode offset.
        assert_eq!(
            resolve_episode(&[rules[1]], 7),
            Ok(EpisodeIdentity {
                entry: 210482,
                number: 6
            })
        );
        assert_eq!(
            resolve_episode(&[rules[1]], 1),
            Err(MappingError::Unmapped(1))
        );
    }

    #[test]
    fn equal_numbers_of_different_entries_are_not_the_same_episode() {
        assert_eq!(
            resolve_episode(&[rule(1, 12, 10, 1), rule(1, 12, 20, 1)], 1),
            Err(MappingError::Ambiguous(1))
        );
    }

    #[test]
    fn identical_rules_do_not_create_false_conflicts() {
        let r = rule(1, 12, 10, 1);
        assert_eq!(
            resolve_file(&[r, r], 5, 6),
            Ok(MappedFile {
                entry: 10,
                first: 5,
                last: 6
            })
        );
        assert_eq!(
            source_episode(
                &[r, r],
                EpisodeIdentity {
                    entry: 10,
                    number: 5
                }
            ),
            Ok(5)
        );
    }

    #[test]
    fn a_combined_file_must_stay_in_one_entry() {
        let rules = [rule(1, 12, 10, 1), rule(13, 24, 20, 1)];
        assert_eq!(
            resolve_file(&rules, 12, 13),
            Err(MappingError::CrossesEntries)
        );
        assert_eq!(
            resolve_file(&rules, 13, 14),
            Ok(MappedFile {
                entry: 20,
                first: 1,
                last: 2
            })
        );
    }

    #[test]
    fn every_episode_of_a_file_must_have_a_consecutive_mapping() {
        assert_eq!(
            resolve_file(&[rule(1, 2, 10, 1), rule(4, 5, 10, 4)], 2, 4),
            Err(MappingError::Unmapped(3))
        );
        assert_eq!(
            resolve_file(&[rule(1, 1, 10, 1), rule(2, 2, 10, 3)], 1, 2),
            Err(MappingError::NonConsecutive)
        );
        assert_eq!(
            resolve_file(&[rule(1, 1, 10, 2), rule(2, 2, 10, 1)], 1, 2),
            Err(MappingError::NonConsecutive)
        );
    }

    #[test]
    fn mapping_misses_never_fall_back_to_source_numbers() {
        assert_eq!(
            resolve_episode(&[rule(13, 24, 20, 1)], 12),
            Err(MappingError::Unmapped(12))
        );
        assert_eq!(resolve_episode(&[], 1), Err(MappingError::Unmapped(1)));
    }

    #[test]
    fn invalid_and_overflowing_rules_are_rejected() {
        for invalid in [
            rule(-1, 2, 10, 1),
            rule(3, 2, 10, 1),
            rule(0, 12, 0, 1),
            rule(0, 12, 10, 0),
            rule(0, i32::MAX, 10, 1),
            rule(1, 2, 10, i32::MAX),
        ] {
            assert_eq!(
                resolve_episode(&[invalid], 1),
                Err(MappingError::InvalidRule)
            );
        }
        assert_eq!(
            resolve_file(&[rule(0, 12, 10, 1)], i32::MIN, i32::MAX),
            Err(MappingError::InvalidSource)
        );
        assert_eq!(
            resolve_file(&[rule(0, 12, 10, 1)], 4, 3),
            Err(MappingError::InvalidSource)
        );
        assert_eq!(
            resolve_file(&[rule(0, 12, 10, 1)], 0, MAX_FILE_SPAN),
            Err(MappingError::InvalidSource)
        );
    }

    #[test]
    fn duplicate_metadata_and_nonunique_reverse_maps_are_rejected() {
        let rules = [rule(0, 0, 10, 1), rule(1, 1, 10, 1)];
        assert_eq!(
            map_metadata(&rules, [(0, "Prologue"), (1, "Other")]),
            Err(MappingError::Ambiguous(1))
        );
        assert_eq!(
            source_episode(
                &rules,
                EpisodeIdentity {
                    entry: 10,
                    number: 1
                }
            ),
            Err(MappingError::Ambiguous(1))
        );
    }
}

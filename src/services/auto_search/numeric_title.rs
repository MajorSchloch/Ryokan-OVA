//! Keep numbers that identify a title separate from its episode payload.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashSet;
use std::sync::LazyLock;

use regex_lite::Regex;

use super::aliases::normalize_title;

// These numbers describe a season or part. Their own guards compare that
// identity; they are not numeric words in the title itself.
static STRUCTURAL_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:(?:season|part|cour)\s*\d{1,2}|\d{1,2}(?:st|nd|rd|th)\s+(?:season|part|cour)|s\d{1,2})\b",
    )
    .unwrap()
});

fn digit_words(value: &str) -> HashSet<String> {
    value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn identity_numbers(alias: &str) -> HashSet<String> {
    if !alias.chars().any(|ch| ch.is_ascii_digit()) {
        return HashSet::new();
    }
    let normalized = normalize_title(alias);
    let core = STRUCTURAL_NUMBER.replace_all(&normalized, " ");
    digit_words(&core)
        .into_iter()
        // A year can name a reboot, an encode, or a title itself. This
        // check does not infer identities from those ambiguous years.
        .filter(|word| {
            word.len() != 4
                || word
                    .parse::<u32>()
                    .is_ok_and(|number| !(1900..=2099).contains(&number))
        })
        .collect()
}

/// Parsed numeric title evidence, shared across all aliases for one release.
/// Parsing is lazy: ordinary titles and structural markers need no extra
/// anitomy call. Callers keep standalone/movie searches on their existing path.
pub(crate) struct TitleNumberEvidence<'a> {
    release_title: &'a str,
    parsed_numbers: OnceCell<Option<HashSet<String>>>,
}

impl<'a> TitleNumberEvidence<'a> {
    pub(crate) fn new(release_title: &'a str) -> Self {
        Self {
            release_title,
            parsed_numbers: OnceCell::new(),
        }
    }

    fn supports_alias(&self, alias: &str) -> bool {
        let required = identity_numbers(alias);
        required.is_empty()
            || self
                .parsed_numbers
                .get_or_init(|| {
                    crate::services::library_link::extract_anime_title(self.release_title)
                        .map(|title| digit_words(&normalize_title(&title)))
                })
                .as_ref()
                .is_some_and(|claimed| required.is_subset(claimed))
    }

    /// Remove aliases whose title number appears only in the episode,
    /// resolution, CRC, or another release field. Keep a borrowed slice when
    /// no alias changes, including every title without numeric identity.
    pub(crate) fn filter_aliases<'b>(&self, aliases: &'b [String]) -> Cow<'b, [String]> {
        let allowed: Vec<bool> = aliases
            .iter()
            .map(|alias| self.supports_alias(alias))
            .collect();
        if allowed.iter().all(|allowed| *allowed) {
            Cow::Borrowed(aliases)
        } else {
            Cow::Owned(
                aliases
                    .iter()
                    .zip(allowed)
                    .filter(|(_, allowed)| *allowed)
                    .map(|(alias, _)| alias.clone())
                    .collect(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digimon_title_number_cannot_come_from_the_episode() {
        let aliases = vec!["Digimon Adventure 02".to_owned()];
        let wrong = TitleNumberEvidence::new(
            "[YakuboEncodes] Digimon Adventure (2020) - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]",
        );
        assert!(wrong.filter_aliases(&aliases).is_empty());
        for title in [
            "[Keyword] Digimon Adventure 02 - 01 [BD][1080p]",
            "[Keyword] Digimon Adventure 02 - 02 [BD][1080p]",
        ] {
            assert_eq!(
                TitleNumberEvidence::new(title)
                    .filter_aliases(&aliases)
                    .as_ref(),
                aliases.as_slice()
            );
        }
    }

    #[test]
    fn numeric_titles_keep_their_actual_title_fields() {
        for (alias, title) in [
            ("86", "[G] 86 - 02 [1080p]"),
            ("Mob Psycho 100", "[G] Mob Psycho 100 - 02 [1080p]"),
            (
                "2.5-jigen no Ririsa",
                "[G] 2.5-jigen no Ririsa - 02 [1080p]",
            ),
            (
                "16bit Sensation",
                "[G] 16bit Sensation - Another Layer - 02 [1080p]",
            ),
        ] {
            assert!(
                TitleNumberEvidence::new(title).supports_alias(alias),
                "{title}"
            );
        }
    }

    #[test]
    fn episode_and_encode_fields_do_not_supply_title_numbers() {
        for (alias, title) in [
            ("86", "[G] Another Story - 86 [1080p]"),
            ("Another Story 1080", "[G] Another Story - 02 [1080p]"),
            ("Another Story 100", "[G] Another Story - 02 [ABC00100]"),
        ] {
            assert!(
                !TitleNumberEvidence::new(title).supports_alias(alias),
                "{title}"
            );
        }
    }

    #[test]
    fn season_and_part_numbers_stay_with_their_separate_guards() {
        let evidence = TitleNumberEvidence::new("[G] Some Story - 11 [1080p]");
        for alias in [
            "Some Story Season 2",
            "Some Story 2nd Season",
            "Some Story S02",
            "Some Story Part 2",
            "Some Story Cour 2",
            "Some Story Season 2 Part 2",
        ] {
            assert!(evidence.supports_alias(alias), "{alias}");
        }
    }

    #[test]
    fn ambiguous_years_do_not_become_a_release_year_filter() {
        assert!(
            TitleNumberEvidence::new("[G] Japan Sinks 2020 - 02 [BD 2024]")
                .supports_alias("Japan Sinks 2020")
        );
        assert!(
            TitleNumberEvidence::new("[G] Some Story (2020) - 02 [BD 2024]")
                .supports_alias("Some Story (2000)")
        );
    }

    #[test]
    fn ordinary_aliases_avoid_parsing_and_keep_the_borrowed_view() {
        let evidence = TitleNumberEvidence::new("[G] Some Story - 02 [1080p]");
        let aliases = vec!["Some Story".to_owned(), "Some Story Season 2".to_owned()];
        assert!(matches!(
            evidence.filter_aliases(&aliases),
            Cow::Borrowed(_)
        ));
        assert!(evidence.parsed_numbers.get().is_none());
    }
}

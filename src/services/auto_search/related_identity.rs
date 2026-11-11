//! Negative evidence from known related entries, never positive aliases.

use std::collections::HashSet;

use super::aliases::{dedupe_strings, normalize_title};
use crate::services::misgrab::verdict::content_tokens;

/// A related movie can have an installment subtitle that the uploader
/// omits. Keep a head only when it adds two distinctive words beyond the
/// target's own titles. This preserves a subseries name without turning
/// the shared franchise name into evidence against its parent.
pub(super) fn subseries_heads(
    title: &str,
    own_content: &HashSet<String>,
    movie: bool,
) -> Vec<String> {
    let separators = title.replace(['–', '—'], " - ");
    let mut heads = Vec::new();
    for (end, _) in separators.match_indices(" - ") {
        let mut head = separators[..end].trim();
        // Strip a numbered movie installment only before its explicit
        // subtitle. Zero and numeric TV titles retain their identity.
        if movie
            && let Some((base, number)) = head.rsplit_once(char::is_whitespace)
            && number.chars().all(|ch| ch.is_ascii_digit())
            && number.parse::<u32>().is_ok_and(|number| number > 0)
        {
            head = base.trim_end();
        }
        let tokens = content_tokens(&normalize_title(head));
        if tokens.difference(own_content).count() >= 2 {
            heads.push(head.to_string());
        }
    }
    dedupe_strings(heads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(titles: &[&str]) -> HashSet<String> {
        titles
            .iter()
            .flat_map(|title| content_tokens(&normalize_title(title)))
            .collect()
    }

    #[test]
    fn related_identity_keeps_specific_movie_heads_without_installment_subtitles() {
        let own = own(&[
            "Code Geass: Hangyaku no Lelouch",
            "Code Geass: Lelouch of the Rebellion",
        ]);
        assert_eq!(
            subseries_heads(
                "Code Geass: Boukoku no Akito 1 - Yokuryuu wa Maiorita",
                &own,
                true
            ),
            ["Code Geass: Boukoku no Akito"]
        );
        assert_eq!(
            subseries_heads(
                "Code Geass: Akito the Exiled - The Wyvern Arrives",
                &own,
                true
            ),
            ["Code Geass: Akito the Exiled"]
        );
    }

    #[test]
    fn related_identity_does_not_derive_generic_or_weak_parent_heads() {
        let own = own(&["Long Universe: Main Story"]);
        for title in [
            "Long Universe - Different Movie",
            "Long Universe Silver - Different Movie",
            "Long Universe: Main Story - Different Movie",
        ] {
            assert!(subseries_heads(title, &own, true).is_empty(), "{title}");
        }
    }

    #[test]
    fn related_identity_preserves_zero_tv_numbers_and_hyphenated_words() {
        let own = own(&["Long Universe: Main Story"]);
        for (title, movie, expected) in [
            (
                "Long Universe: Silver Dragon 0 - Subtitle",
                true,
                "Long Universe: Silver Dragon 0",
            ),
            (
                "Long Universe: Silver Dragon 86 - Subtitle",
                false,
                "Long Universe: Silver Dragon 86",
            ),
            (
                "Long Universe: Silver-Dragon - Subtitle",
                true,
                "Long Universe: Silver-Dragon",
            ),
        ] {
            assert_eq!(subseries_heads(title, &own, movie), [expected]);
        }
    }

    #[test]
    fn related_identity_does_not_expand_other_relations_into_packs() {
        for kind in ["OTHER", "CHARACTER", "ADAPTATION", "SOURCE"] {
            assert!(!super::super::pack_detection::is_pack_candidate_relation(
                kind
            ));
        }
        assert!(super::super::pack_detection::is_pack_candidate_relation(
            "SEQUEL"
        ));
    }
}

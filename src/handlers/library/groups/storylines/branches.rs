//! Project explicit metadata relations between established storyline sections.

use std::collections::{BTreeSet, HashMap};

use super::{Entry, Storyline};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::handlers::library) enum BranchKind {
    Sequence,
    SpinOff,
    Recap,
    SideStory,
    Alternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::handlers::library) struct BranchEdge {
    pub from: i64,
    pub to: i64,
    pub kind: BranchKind,
}

pub(in crate::handlers::library) fn edges(
    entries: &[Entry],
    sections: &[Storyline],
) -> Vec<BranchEdge> {
    // `sections` has already sent cyclic and conflicting sequence components
    // to Related, so their releases have no endpoints here.
    let heads: HashMap<_, _> = sections
        .iter()
        .filter(|section| section.kind.can_select())
        .flat_map(|section| section.releases.iter().map(|&id| (id, section.head())))
        .collect();
    let mut result = BTreeSet::new();
    for entry in entries {
        let Some(&to) = heads.get(&entry.provider_id) else {
            continue;
        };
        for predecessor in &entry.watch_predecessors {
            if let Some(&from) = heads.get(predecessor)
                && from != to
                && !entry.has_conflicting_predecessor(*predecessor)
            {
                result.insert(BranchEdge {
                    from,
                    to,
                    kind: BranchKind::Sequence,
                });
            }
        }

        // Use the normalized relations on each release. A section heading
        // keeps only one display label per peer and can omit other link kinds.
        for link in entry.links.iter().filter(|link| link.within_group) {
            let Some(&from) = heads.get(&link.peer_id) else {
                continue;
            };
            if from == to {
                continue;
            }
            let kind = match link.label.as_str() {
                "Spin-off of" => BranchKind::SpinOff,
                "Summary of" => BranchKind::Recap,
                "Side story of" => BranchKind::SideStory,
                "Alternative to" => BranchKind::Alternative,
                _ => continue,
            };
            let (from, to) = if kind == BranchKind::Alternative && from > to {
                (to, from)
            } else {
                (from, to)
            };
            result.insert(BranchEdge { from, to, kind });
        }
    }
    result.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use crate::handlers::library::groups::{RelationLabel, storylines::sections};

    use super::*;

    fn entry(id: i64, predecessors: &[i64], relations: &[(&str, i64)]) -> Entry {
        Entry {
            provider_id: id,
            title: format!("Release {id}"),
            watch_predecessors: predecessors.to_vec(),
            links: relations
                .iter()
                .map(|&(label, peer_id)| RelationLabel {
                    label: label.into(),
                    peer: format!("Release {peer_id}"),
                    peer_id,
                    outside: false,
                    within_group: peer_id != 99,
                })
                .collect(),
            ..Entry::default()
        }
    }

    fn edge(from: i64, to: i64, kind: BranchKind) -> BranchEdge {
        BranchEdge { from, to, kind }
    }

    #[test]
    fn preserves_split_join_and_distinct_relations_between_exact_storyline_heads() {
        use BranchKind::*;

        let entries = [
            entry(10, &[], &[]),
            entry(11, &[10], &[]),
            entry(30, &[11], &[("Alternative to", 20)]),
            entry(20, &[11], &[("Alternative to", 30)]),
            entry(40, &[20, 30, 30], &[]),
            entry(41, &[40], &[("Spin-off of", 20)]),
            entry(50, &[], &[("Summary of", 20)]),
            entry(60, &[], &[("Summary of", 11)]),
            entry(61, &[60], &[("Summary of", 10)]),
            entry(70, &[], &[("Side story of", 11), ("Spin-off of", 99)]),
            entry(71, &[70], &[]),
        ];
        let projected = sections(&entries);
        let expected = [
            edge(10, 20, Sequence),
            edge(10, 30, Sequence),
            edge(10, 60, Recap),
            edge(10, 70, SideStory),
            edge(20, 30, Alternative),
            edge(20, 40, Sequence),
            edge(20, 40, SpinOff),
            edge(30, 40, Sequence),
        ];
        assert_eq!(edges(&entries, &projected), expected);
        let mut entries = entries;
        entries.reverse();
        assert_eq!(edges(&entries, &projected), expected);
    }

    #[test]
    fn excludes_links_to_cyclic_and_conflicting_sequences() {
        let entries = [
            entry(10, &[], &[]),
            entry(11, &[10], &[]),
            entry(20, &[], &[("Alternative to", 10)]),
            entry(21, &[20], &[("Alternative to", 20)]),
            entry(30, &[31], &[("Alternative to", 10)]),
            entry(31, &[30], &[]),
        ];
        assert!(edges(&entries, &sections(&entries)).is_empty());
    }
}

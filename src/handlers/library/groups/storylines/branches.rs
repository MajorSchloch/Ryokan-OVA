//! Project explicit metadata relations between established storyline sections.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::{Entry, Storyline};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::handlers::library) enum BranchKind {
    Sequence,
    SpinOff,
    Recap,
    SideStory,
    Alternative,
    Related,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::handlers::library) struct BranchEdge {
    pub from: i64,
    pub to: i64,
    pub kind: BranchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::handlers::library) struct TimelineEdge {
    pub edge: BranchEdge,
    pub from_release: i64,
    pub to_release: i64,
}

fn standalone_releases(entries: &[Entry], sections: &[Storyline]) -> HashSet<i64> {
    let members: HashSet<_> = sections
        .iter()
        .flat_map(|section| section.releases.iter().copied())
        .collect();
    let mut ordered = HashSet::new();
    for entry in entries
        .iter()
        .filter(|entry| members.contains(&entry.provider_id))
    {
        for &before in entry
            .watch_predecessors
            .iter()
            .filter(|id| members.contains(id))
        {
            ordered.extend([before, entry.provider_id]);
        }
    }
    // A non-selectable release with a sequence edge belongs to a conflicted
    // or cyclic component. Keep it out of the timeline rather than reviving
    // its relationships through a standalone card.
    members.difference(&ordered).copied().collect()
}

pub(in crate::handlers::library) fn timeline_edges(
    entries: &[Entry],
    sections: &[Storyline],
) -> Vec<TimelineEdge> {
    let mut heads: HashMap<_, _> = sections
        .iter()
        .filter(|section| section.kind.can_select())
        .flat_map(|section| section.releases.iter().map(|&id| (id, section.head())))
        .collect();
    for id in standalone_releases(entries, sections) {
        heads.entry(id).or_insert(id);
    }
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
                result.insert(TimelineEdge {
                    edge: BranchEdge {
                        from,
                        to,
                        kind: BranchKind::Sequence,
                    },
                    from_release: *predecessor,
                    to_release: entry.provider_id,
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
                "Related to" => BranchKind::Related,
                _ => continue,
            };
            let mut timeline_edge = TimelineEdge {
                edge: BranchEdge { from, to, kind },
                from_release: link.peer_id,
                to_release: entry.provider_id,
            };
            if matches!(kind, BranchKind::Alternative | BranchKind::Related) && from > to {
                std::mem::swap(&mut timeline_edge.edge.from, &mut timeline_edge.edge.to);
                std::mem::swap(
                    &mut timeline_edge.from_release,
                    &mut timeline_edge.to_release,
                );
            }
            result.insert(timeline_edge);
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

    fn exact(edge: BranchEdge, from_release: i64, to_release: i64) -> TimelineEdge {
        TimelineEdge {
            edge,
            from_release,
            to_release,
        }
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
            exact(edge(10, 20, Sequence), 11, 20),
            exact(edge(10, 30, Sequence), 11, 30),
            exact(edge(10, 60, Recap), 10, 61),
            exact(edge(10, 60, Recap), 11, 60),
            exact(edge(10, 70, SideStory), 11, 70),
            exact(edge(20, 30, Alternative), 20, 30),
            exact(edge(20, 40, Sequence), 20, 40),
            exact(edge(20, 40, SpinOff), 20, 41),
            exact(edge(20, 50, Recap), 20, 50),
            exact(edge(30, 40, Sequence), 30, 40),
        ];
        assert_eq!(timeline_edges(&entries, &projected), expected);
        let mut entries = entries;
        entries.reverse();
        assert_eq!(timeline_edges(&entries, &projected), expected);
    }

    #[test]
    fn adds_only_explicit_context_for_exact_standalone_releases() {
        use BranchKind::*;

        let entries = [
            entry(10, &[], &[]),
            entry(11, &[10], &[("Related to", 20), ("Related to", 31)]),
            entry(12, &[11], &[]),
            entry(20, &[], &[("Related to", 11)]),
            entry(21, &[], &[("Side story of", 12)]),
            entry(22, &[], &[("Summary of", 11)]),
            entry(30, &[], &[]),
            entry(31, &[30], &[("Related to", 11)]),
            entry(40, &[], &[]),
            entry(41, &[], &[("Side story of", 40)]),
            entry(42, &[], &[("Parent of", 40)]),
            entry(43, &[], &[("Side story of", 99)]),
            entry(50, &[51], &[("Side story of", 12)]),
            entry(51, &[50], &[]),
            entry(60, &[], &[]),
            entry(61, &[60], &[("Alternative to", 60), ("Related to", 11)]),
            entry(62, &[], &[("Summary of", 61)]),
        ];
        let projected = sections(&entries);
        assert_eq!(
            timeline_edges(&entries, &projected),
            [
                exact(edge(10, 20, Related), 11, 20),
                exact(edge(10, 21, SideStory), 12, 21),
                exact(edge(10, 22, Recap), 11, 22),
                exact(edge(10, 30, Related), 11, 31),
                exact(edge(40, 41, SideStory), 40, 41),
            ]
        );
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
        assert!(timeline_edges(&entries, &sections(&entries)).is_empty());
    }
}

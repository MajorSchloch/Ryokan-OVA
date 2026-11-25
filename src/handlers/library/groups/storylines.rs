//! Project provider relationships into viewing sections without changing franchise
//! membership or inferring an intended watch order.

pub(in crate::handlers::library) mod branches;

use std::collections::{BTreeSet, HashMap};

use super::Entry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::handlers::library) enum StorylineKind {
    Sequence,
    Continuation,
    SpinOff,
    Alternative,
    Extras,
    Related,
}

impl StorylineKind {
    pub(in crate::handlers::library) fn slug(self) -> &'static str {
        match self {
            Self::Sequence => "sequence",
            Self::Continuation => "continuation",
            Self::SpinOff => "spin-off",
            Self::Alternative => "alternative",
            Self::Extras => "extras",
            Self::Related => "related",
        }
    }

    pub(in crate::handlers::library) fn can_select(self) -> bool {
        matches!(
            self,
            Self::Sequence | Self::Continuation | Self::SpinOff | Self::Alternative
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::handlers::library) struct Storyline {
    pub title: String,
    pub releases: Vec<i64>,
    pub extras: bool,
    pub kind: StorylineKind,
    pub links: Vec<StorylineLink>,
}

impl Storyline {
    pub(in crate::handlers::library) fn head(&self) -> i64 {
        self.releases[0]
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::handlers::library) struct StorylineLink {
    pub peer_id: i64,
    pub peer_title: String,
    pub label: &'static str,
}

#[derive(Default)]
struct Links {
    before: Vec<usize>,
    after: Vec<usize>,
    conflicting: bool,
}

pub(in crate::handlers::library) fn sections(entries: &[Entry]) -> Vec<Storyline> {
    let positions: HashMap<_, _> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.provider_id, index))
        .collect();
    let mut roles: Vec<_> = entries
        .iter()
        .map(|entry| {
            let labels = entry
                .links
                .iter()
                .filter(|link| link.within_group && positions.contains_key(&link.peer_id));
            let extras = labels.clone().any(|link| {
                matches!(link.label.as_str(), "Side story of" | "Summary of")
                    && !labels.clone().any(|other| {
                        other.peer_id == link.peer_id
                            && matches!(
                                other.label.as_str(),
                                "Prequel to" | "Sequel to" | "Alternative to"
                            )
                    })
            });
            let peers = |label| {
                labels
                    .clone()
                    .filter(|link| link.label == label)
                    .map(|link| positions[&link.peer_id])
                    .collect()
            };
            Roles {
                extras,
                spin_offs: peers("Spin-off of"),
                recaps: peers("Summary of"),
                side_stories: peers("Side story of"),
                alternatives: peers("Alternative to"),
            }
        })
        .collect();
    let mut links: Vec<Links> = (0..entries.len()).map(|_| Links::default()).collect();
    for (index, entry) in entries.iter().enumerate() {
        let predecessors: BTreeSet<_> = entry.watch_predecessors.iter().copied().collect();
        for id in predecessors {
            let Some(&before) = positions.get(&id) else {
                continue;
            };
            links[index].conflicting |= entry.has_conflicting_predecessor(id);
            links[index].before.push(before);
            links[before].after.push(index);
        }
    }

    // A side story or recap with a sequence relationship belongs to that
    // sequence. The extras section contains only standalone releases.
    for (role, link) in roles.iter_mut().zip(&links) {
        role.extras &= !link.ordered();
    }

    // Input indices already follow premiere order. Use that order between
    // separate components and when more than one continuation is possible.
    let mut seen = vec![false; entries.len()];
    let mut placed = seen.clone();
    let mut pending: Vec<_> = links.iter().map(|link| link.before.len()).collect();
    let mut related = Vec::new();
    let mut result = Vec::new();
    for index in 0..entries.len() {
        if roles[index].extras || seen[index] {
            continue;
        }
        let component = collect_component(index, &links, &mut seen);
        let Some(order) = component_order(&component, &links, &mut pending) else {
            related.extend(component);
            continue;
        };
        for start in order {
            if placed[start] {
                continue;
            }
            let mut path = vec![start];
            placed[start] = true;
            let mut tail = start;
            while let [next] = links[tail].after.as_slice() {
                if links[*next].before.len() != 1 {
                    break;
                }
                placed[*next] = true;
                path.push(*next);
                tail = *next;
            }

            let kind = if path
                .iter()
                .any(|&pos| roles[pos].spin_offs.iter().any(|peer| !path.contains(peer)))
            {
                StorylineKind::SpinOff
            } else if path.iter().any(|&pos| {
                roles[pos].alternatives.iter().any(|&peer| {
                    !path.contains(&peer) && (!links[pos].ordered() || links[peer].ordered())
                })
            }) {
                // An isolated alternative does not relabel its peer's known sequence.
                StorylineKind::Alternative
            } else if !links[start].before.is_empty() {
                StorylineKind::Continuation
            } else if links[start].ordered() {
                StorylineKind::Sequence
            } else {
                StorylineKind::Related
            };
            let heading = match kind {
                StorylineKind::Sequence => "Story sequence",
                StorylineKind::Continuation => "Continuation",
                StorylineKind::SpinOff => "Spin-off",
                StorylineKind::Alternative => "Alternative version",
                StorylineKind::Related => {
                    related.extend(path);
                    continue;
                }
                StorylineKind::Extras => unreachable!("extra releases do not enter the graph"),
            };
            result.push(Storyline {
                title: format!("{heading} · {}", entries[start].title),
                releases: path.iter().map(|&pos| entries[pos].provider_id).collect(),
                extras: false,
                kind,
                links: storyline_links(&path, entries, &links, &roles),
            });
        }
    }

    if !related.is_empty() {
        related.sort_unstable();
        result.push(Storyline {
            title: "Related releases".into(),
            releases: related
                .iter()
                .map(|&pos| entries[pos].provider_id)
                .collect(),
            extras: false,
            kind: StorylineKind::Related,
            links: Vec::new(),
        });
    }
    let extras: Vec<_> = entries
        .iter()
        .zip(roles)
        .filter_map(|(entry, role)| role.extras.then_some(entry.provider_id))
        .collect();
    if !extras.is_empty() {
        result.push(Storyline {
            title: "Side stories and recaps".into(),
            releases: extras,
            extras: true,
            kind: StorylineKind::Extras,
            links: Vec::new(),
        });
    }
    result
}

fn storyline_links(
    path: &[usize],
    entries: &[Entry],
    links: &[Links],
    roles: &[Roles],
) -> Vec<StorylineLink> {
    let mut seen: BTreeSet<_> = path.iter().copied().collect();
    let predecessors = &links[path[0]].before;
    let predecessor_label = if predecessors.len() == 1 {
        "Continues from"
    } else {
        "Linked prequel"
    };
    // One reference per destination. A verified direct predecessor at the
    // start is more useful here than a version relationship later in the path.
    predecessors
        .iter()
        .map(|&peer| (peer, predecessor_label))
        .chain(path.iter().flat_map(|&pos| {
            roles[pos]
                .spin_offs
                .iter()
                .map(|&peer| (peer, "Spin-off of"))
        }))
        .chain(
            path.iter()
                .flat_map(|&pos| roles[pos].recaps.iter().map(|&peer| (peer, "Recaps"))),
        )
        .chain(path.iter().flat_map(|&pos| {
            roles[pos]
                .side_stories
                .iter()
                .map(|&peer| (peer, "Side story of"))
        }))
        .chain(path.iter().flat_map(|&pos| {
            roles[pos]
                .alternatives
                .iter()
                .map(|&peer| (peer, "Alternative to"))
        }))
        .filter(|(peer, _)| seen.insert(*peer) && !entries[*peer].title.trim().is_empty())
        .map(|(peer, label)| StorylineLink {
            peer_id: entries[peer].provider_id,
            peer_title: entries[peer].title.clone(),
            label,
        })
        .collect()
}

struct Roles {
    extras: bool,
    spin_offs: Vec<usize>,
    recaps: Vec<usize>,
    side_stories: Vec<usize>,
    alternatives: Vec<usize>,
}

impl Links {
    fn ordered(&self) -> bool {
        !self.before.is_empty() || !self.after.is_empty()
    }
}

fn collect_component(index: usize, links: &[Links], seen: &mut [bool]) -> Vec<usize> {
    let mut queue = vec![index];
    seen[index] = true;
    let mut cursor = 0;
    while let Some(&current) = queue.get(cursor) {
        for &peer in links[current].before.iter().chain(&links[current].after) {
            if !seen[peer] {
                seen[peer] = true;
                queue.push(peer);
            }
        }
        cursor += 1;
    }
    queue
}

fn component_order(
    component: &[usize],
    links: &[Links],
    pending: &mut [usize],
) -> Option<Vec<usize>> {
    if component.iter().any(|&index| links[index].conflicting) {
        return None;
    }
    let mut available: BTreeSet<_> = component
        .iter()
        .copied()
        .filter(|&index| pending[index] == 0)
        .collect();
    let mut order = Vec::with_capacity(component.len());
    while let Some(index) = available.pop_first() {
        order.push(index);
        for &next in &links[index].after {
            pending[next] -= 1;
            if pending[next] == 0 {
                available.insert(next);
            }
        }
    }
    (order.len() == component.len()).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::super::RelationLabel;
    use super::*;

    fn entry(id: i64, predecessors: &[i64], links: &[(&str, i64)]) -> Entry {
        Entry {
            provider_id: id,
            title: format!("Release {id}"),
            watch_predecessors: predecessors.to_vec(),
            links: links
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

    fn projection(entries: &[Entry]) -> Vec<(StorylineKind, Vec<i64>)> {
        let result = sections(entries);
        assert_eq!(
            result
                .iter()
                .flat_map(|section| section.releases.iter())
                .copied()
                .collect::<BTreeSet<_>>(),
            entries.iter().map(|entry| entry.provider_id).collect(),
        );
        assert_eq!(
            result
                .iter()
                .map(|section| section.releases.len())
                .sum::<usize>(),
            entries.len(),
        );
        assert!(
            result
                .iter()
                .all(|section| section.extras == (section.kind == StorylineKind::Extras))
        );
        result
            .into_iter()
            .map(|section| (section.kind, section.releases))
            .collect()
    }

    #[test]
    fn follows_explicit_order_even_when_a_prequel_premieres_later() {
        assert_eq!(
            projection(&[entry(2, &[1], &[]), entry(1, &[], &[])]),
            [(StorylineKind::Sequence, vec![1, 2])],
        );
    }

    #[test]
    fn shares_the_opening_once_and_keeps_branches_and_convergence_distinct() {
        assert_eq!(
            projection(&[
                entry(1, &[], &[]),
                entry(2, &[1], &[]),
                entry(4, &[2], &[]),
                entry(3, &[2], &[]),
                entry(5, &[3], &[]),
                entry(6, &[4, 5], &[]),
                entry(7, &[6], &[]),
            ]),
            [
                (StorylineKind::Sequence, vec![1, 2]),
                (StorylineKind::Continuation, vec![4]),
                (StorylineKind::Continuation, vec![3, 5]),
                (StorylineKind::Continuation, vec![6, 7]),
            ],
        );
    }

    #[test]
    fn links_sections_to_exact_releases_without_repeating_shared_setup() {
        let entries = [
            entry(10, &[], &[]),
            entry(11, &[10], &[]),
            entry(12, &[11], &[]),
            entry(13, &[11], &[]),
            entry(14, &[12, 13, 13], &[]),
            entry(20, &[], &[("Spin-off of", 10), ("Spin-off of", 99)]),
            entry(21, &[20], &[("Spin-off of", 10)]),
            entry(30, &[], &[("Alternative to", 13), ("Alternative to", 99)]),
            entry(40, &[], &[("Summary of", 11)]),
            entry(50, &[], &[("Related to", 11)]),
            entry(60, &[], &[("Summary of", 10)]),
            entry(61, &[60], &[("Summary of", 10)]),
            entry(70, &[], &[("Side story of", 11)]),
            entry(71, &[70], &[]),
        ];
        let sections = sections(&entries);
        let context = |head| {
            let section = sections
                .iter()
                .find(|section| section.head() == head)
                .unwrap();
            section
                .links
                .iter()
                .map(|link| (link.label, link.peer_id, link.peer_title.as_str()))
                .collect::<Vec<_>>()
        };
        assert!(context(10).is_empty());
        assert_eq!(context(12), [("Continues from", 11, "Release 11")]);
        assert_eq!(context(13), [("Continues from", 11, "Release 11")]);
        assert_eq!(
            context(14),
            [
                ("Linked prequel", 12, "Release 12"),
                ("Linked prequel", 13, "Release 13"),
            ]
        );
        assert_eq!(context(20), [("Spin-off of", 10, "Release 10")]);
        assert_eq!(context(30), [("Alternative to", 13, "Release 13")]);
        assert!(context(40).is_empty());
        assert!(context(50).is_empty());
        assert_eq!(context(60), [("Recaps", 10, "Release 10")]);
        assert_eq!(context(70), [("Side story of", 11, "Release 11")]);
        assert_eq!(
            sections
                .iter()
                .filter(|section| section.kind.can_select())
                .map(Storyline::head)
                .collect::<Vec<_>>(),
            [10, 12, 13, 14, 20, 30, 60, 70]
        );
    }

    #[test]
    fn separates_spin_offs_alternatives_extras_and_unclassified_parents() {
        assert_eq!(
            projection(&[
                entry(1, &[], &[]),
                entry(2, &[1], &[("Alternative to", 5)]),
                entry(3, &[], &[("Spin-off of", 1)]),
                entry(4, &[3], &[]),
                entry(5, &[], &[("Alternative to", 2)]),
                entry(6, &[], &[("Summary of", 2)]),
                entry(7, &[], &[("Side story of", 1)]),
                entry(8, &[], &[("Related to", 1)]),
                entry(9, &[99], &[("Side story of", 99)]),
            ]),
            [
                (StorylineKind::Sequence, vec![1, 2]),
                (StorylineKind::SpinOff, vec![3, 4]),
                (StorylineKind::Alternative, vec![5]),
                (StorylineKind::Related, vec![8, 9]),
                (StorylineKind::Extras, vec![6, 7]),
            ],
        );
    }

    #[test]
    fn keeps_side_stories_and_summaries_with_known_sequences() {
        assert_eq!(
            projection(&[
                entry(1, &[], &[]),
                entry(2, &[1], &[]),
                entry(3, &[2], &[("Side story of", 1)]),
                entry(4, &[], &[("Summary of", 1)]),
                entry(5, &[4], &[]),
                entry(6, &[], &[("Side story of", 1)]),
                entry(7, &[], &[("Summary of", 1)]),
            ]),
            [
                (StorylineKind::Sequence, vec![1, 2, 3]),
                (StorylineKind::Sequence, vec![4, 5]),
                (StorylineKind::Extras, vec![6, 7]),
            ],
        );
    }

    #[test]
    fn does_not_label_a_sequence_as_a_spin_off_of_its_own_member() {
        assert_eq!(
            projection(&[
                entry(1, &[], &[("Parent of", 3)]),
                entry(2, &[1], &[]),
                entry(3, &[2], &[("Spin-off of", 1)]),
            ]),
            [(StorylineKind::Sequence, vec![1, 2, 3])],
        );
    }

    #[test]
    fn confines_conflicts_and_cycles_to_their_own_components() {
        assert_eq!(
            projection(&[
                entry(2, &[1], &[("Alternative to", 1)]),
                entry(1, &[], &[]),
                entry(4, &[3], &[]),
                entry(3, &[4], &[]),
                entry(6, &[5], &[]),
                entry(5, &[], &[]),
            ]),
            [
                (StorylineKind::Sequence, vec![5, 6]),
                (StorylineKind::Related, vec![2, 1, 4, 3]),
            ],
        );
    }
}

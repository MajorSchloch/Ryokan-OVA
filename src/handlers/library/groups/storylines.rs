//! Project provider relationships into viewing sections without changing franchise
//! membership or inferring an intended watch order.

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
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::handlers::library) struct Storyline {
    pub title: String,
    pub releases: Vec<i64>,
    pub extras: bool,
    pub kind: StorylineKind,
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
            Roles {
                extras,
                spin_offs: labels
                    .clone()
                    .filter(|link| link.label == "Spin-off of")
                    .map(|link| positions[&link.peer_id])
                    .collect(),
                alternatives: labels
                    .filter(|link| link.label == "Alternative to")
                    .map(|link| positions[&link.peer_id])
                    .collect(),
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
        });
    }
    result
}

struct Roles {
    extras: bool,
    spin_offs: Vec<usize>,
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

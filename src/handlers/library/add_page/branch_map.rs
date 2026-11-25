use std::collections::{HashMap, HashSet};

use crate::handlers::library::{
    availability::premiere_label,
    groups::{
        Entry,
        storylines::{
            Storyline,
            branches::{BranchKind, TimelineEdge},
        },
    },
};

use super::{
    AddPage,
    discovery::{cover, release_href},
    facts,
};

pub(super) struct BranchMap {
    pub choices: Vec<BranchOption>,
    pub focus: i64,
    pub stops: Vec<TimelineStop>,
}

pub(super) struct BranchOption {
    pub id: i64,
    pub title: String,
}

pub(super) struct TimelineStop {
    pub story: BranchCard,
    pub branches: Vec<BranchCard>,
}

pub(super) struct BranchCard {
    pub id: i64,
    pub title: String,
    pub href: String,
    pub solid: bool,
    pub active: bool,
    pub releases: Vec<TimelineRelease>,
}

pub(super) struct TimelineRelease {
    pub title: String,
    pub cover: String,
    pub facts: String,
    pub date: String,
    pub href: String,
    pub labels: Vec<&'static str>,
}

#[derive(Default)]
struct Connection {
    labels: HashMap<i64, Vec<&'static str>>,
    solid: bool,
}

impl Connection {
    fn record(&mut self, kind: BranchKind, outgoing: bool, peer: i64) {
        use BranchKind::*;
        let label = match (kind, outgoing) {
            (Sequence, true) => "Sequel",
            (Sequence, false) => "Prequel",
            (SpinOff, true) => "Spin-off",
            (SpinOff, false) => "Parent of this spin-off",
            (SideStory, true) => "Side story",
            (SideStory, false) => "Parent of this side story",
            (Recap, true) => "Recap",
            (Recap, false) => "Original release",
            (Alternative, _) => "Alternative version",
            (Related, _) => "Related release",
        };
        self.solid |= kind == Sequence;
        let labels = self.labels.entry(peer).or_default();
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
}

pub(super) fn build(
    entries: &[Entry],
    sections: &[Storyline],
    edges: &[TimelineEdge],
    view: &AddPage,
    seed: i64,
    representative: i64,
) -> Option<BranchMap> {
    let sections: HashMap<_, _> = sections
        .iter()
        .filter(|section| section.kind.can_select())
        .map(|section| (section.head(), section))
        .collect();
    let connected: HashSet<_> = edges
        .iter()
        .flat_map(|link| [link.edge.from, link.edge.to])
        .collect();
    // Keep the same stable order as the release list, including standalone
    // releases that have a real connection but no ordered storyline.
    let choices: Vec<_> = entries
        .iter()
        .filter(|entry| connected.contains(&entry.provider_id))
        .map(|entry| BranchOption {
            id: entry.provider_id,
            title: entry.title.clone(),
        })
        .collect();
    let focus = view
        .branch
        .filter(|id| connected.contains(id))
        .or_else(|| {
            sections
                .values()
                .find(|section| {
                    connected.contains(&section.head())
                        && section.releases.contains(&representative)
                })
                .map(|section| section.head())
        })
        .or_else(|| {
            connected
                .contains(&representative)
                .then_some(representative)
        })
        .or_else(|| choices.first().map(|choice| choice.id))?;

    let release_ids = |id| {
        sections
            .get(&id)
            .map_or_else(|| vec![id], |section| section.releases.clone())
    };
    let entries_by_id: HashMap<_, _> = entries
        .iter()
        .map(|entry| (entry.provider_id, entry))
        .collect();
    let release = |id, labels| {
        let entry = entries_by_id[&id];
        let date = premiere_label(&entry.date);
        TimelineRelease {
            title: entry.title.clone(),
            cover: cover(entry),
            facts: facts(&entry.format, entry.episodes, None),
            date: if date == "TBA" { String::new() } else { date },
            href: release_href(view, seed, entry),
            labels,
        }
    };
    let mut branches: HashMap<i64, HashMap<i64, Connection>> = HashMap::new();
    for link in edges {
        if link.edge.from == focus {
            branches
                .entry(link.from_release)
                .or_default()
                .entry(link.edge.to)
                .or_default()
                .record(link.edge.kind, true, link.to_release);
        } else if link.edge.to == focus {
            branches
                .entry(link.to_release)
                .or_default()
                .entry(link.edge.from)
                .or_default()
                .record(link.edge.kind, false, link.from_release);
        }
    }
    let stops = release_ids(focus)
        .into_iter()
        .map(|id| {
            let story = release(id, Vec::new());
            let branches = if let Some(mut attached) = branches.remove(&id) {
                choices
                    .iter()
                    .filter_map(|choice| {
                        let mut connection = attached.remove(&choice.id)?;
                        Some(BranchCard {
                            id: choice.id,
                            title: choice.title.clone(),
                            href: format!(
                                "{}#discover-branch-map",
                                view.branch_path(seed, choice.id)
                            ),
                            solid: connection.solid,
                            active: false,
                            releases: release_ids(choice.id)
                                .into_iter()
                                .filter_map(|id| {
                                    let mut labels = connection.labels.remove(&id)?;
                                    if labels.len() > 1 {
                                        labels.retain(|label| *label != "Related release");
                                    }
                                    Some(release(id, labels))
                                })
                                .collect(),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            TimelineStop {
                story: BranchCard {
                    id,
                    title: story.title.clone(),
                    href: story.href.clone(),
                    solid: true,
                    active: id == focus,
                    releases: vec![story],
                },
                branches,
            }
        })
        .collect();

    Some(BranchMap {
        choices,
        focus,
        stops,
    })
}

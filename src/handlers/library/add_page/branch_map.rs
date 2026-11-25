use std::collections::{HashMap, HashSet};

use crate::handlers::library::groups::storylines::{
    Storyline,
    branches::{BranchEdge, BranchKind},
};

use super::AddPage;

pub(super) struct BranchMap {
    pub choices: Vec<BranchOption>,
    pub focus: i64,
    pub title: String,
    pub before: Vec<BranchCard>,
    pub after: Vec<BranchCard>,
    pub related: Vec<BranchCard>,
}

pub(super) struct BranchOption {
    pub id: i64,
    pub title: String,
}

pub(super) struct BranchCard {
    pub title: String,
    pub href: String,
    pub labels: Vec<&'static str>,
}

#[derive(Default)]
struct Connections {
    incoming: bool,
    outgoing: bool,
    labels: Vec<&'static str>,
}

impl Connections {
    fn record(&mut self, kind: BranchKind, at_origin: bool) {
        let label = match (kind, at_origin) {
            (BranchKind::Sequence, true) => {
                self.outgoing = true;
                "Sequel"
            }
            (BranchKind::Sequence, false) => {
                self.incoming = true;
                "Prequel"
            }
            (BranchKind::SpinOff, true) => "Spin-off",
            (BranchKind::SpinOff, false) => "Parent of this spin-off",
            (BranchKind::Recap, true) => "Recap",
            (BranchKind::Recap, false) => "Original storyline",
            (BranchKind::SideStory, true) => "Side story",
            (BranchKind::SideStory, false) => "Parent of this side story",
            (BranchKind::Alternative, _) => "Alternative version",
        };
        if !self.labels.contains(&label) {
            self.labels.push(label);
        }
    }
}

pub(super) fn build(
    sections: &[Storyline],
    edges: &[BranchEdge],
    view: &AddPage,
    seed: i64,
) -> Option<BranchMap> {
    let connected: HashSet<_> = edges.iter().flat_map(|edge| [edge.from, edge.to]).collect();
    let choices: Vec<_> = sections
        .iter()
        .filter(|section| connected.contains(&section.head()))
        .collect();
    let focused = view
        .branch
        .and_then(|head| choices.iter().find(|section| section.head() == head))
        .or_else(|| {
            choices
                .iter()
                .find(|section| section.releases.contains(&seed))
        })
        .or_else(|| choices.first())?;
    let focus = focused.head();
    let mut connections: HashMap<i64, Connections> = HashMap::new();
    for edge in edges {
        if edge.from == focus {
            connections
                .entry(edge.to)
                .or_default()
                .record(edge.kind, true);
        } else if edge.to == focus {
            connections
                .entry(edge.from)
                .or_default()
                .record(edge.kind, false);
        }
    }
    let mut result = BranchMap {
        choices: choices
            .iter()
            .map(|section| BranchOption {
                id: section.head(),
                title: section.title.clone(),
            })
            .collect(),
        focus,
        title: focused.title.clone(),
        before: Vec::new(),
        after: Vec::new(),
        related: Vec::new(),
    };
    for section in choices {
        let id = section.head();
        let Some(connection) = connections.remove(&id) else {
            continue;
        };
        let (cards, implied_label) = match (connection.incoming, connection.outgoing) {
            (true, false) => (&mut result.before, Some("Prequel")),
            (false, true) => (&mut result.after, Some("Sequel")),
            _ => (&mut result.related, None),
        };
        cards.push(BranchCard {
            title: section.title.clone(),
            href: format!("{}#add-anime-releases-title", view.branch_path(seed, id)),
            labels: connection
                .labels
                .into_iter()
                .filter(|label| Some(*label) != implied_label)
                .collect(),
        });
    }
    Some(result)
}

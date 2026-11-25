//! A pasted viewing guide is a display and selection aid, with exact release IDs.
use std::collections::{HashMap, HashSet};

use super::super::groups::Group;

const MAX_NAME_CHARS: usize = 80;
const MAX_GUIDE_BYTES: usize = 24 * 1024;
const MAX_LINE_BYTES: usize = 512;
const MAX_RELEASES: usize = 100;

#[derive(Debug)]
pub(super) struct GuideDraft {
    name: String,
    releases: Vec<GuideRelease>,
}

#[derive(Debug)]
pub(super) struct Guide {
    pub name: String,
    pub releases: Vec<GuideRelease>,
    pub selected: HashSet<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GuideRelease {
    pub id: i64,
    pub optional: bool,
}

impl GuideDraft {
    pub fn parse(name: &str, entries: &str) -> Result<Self, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Enter a name for the guide.".into());
        }
        if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(format!(
                "Use a guide name of up to {MAX_NAME_CHARS} characters on one line."
            ));
        }
        if entries.len() > MAX_GUIDE_BYTES {
            return Err("The guide is too long. Remove extra text or shorten the links.".into());
        }

        let mut releases = Vec::new();
        let mut seen = HashSet::new();
        for (index, line) in entries.lines().enumerate() {
            let line_number = index + 1;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if releases.len() == MAX_RELEASES {
                return Err(format!("Use up to {MAX_RELEASES} releases in one guide."));
            }
            if line.len() > MAX_LINE_BYTES {
                return Err(format!(
                    "Line {line_number} is too long. Use the AniList ID or shorten the link."
                ));
            }
            let optional = line.starts_with('?');
            let value = line.strip_prefix('?').unwrap_or(line).trim_start();
            let id = parse_id(value).ok_or_else(|| {
                format!("Line {line_number} needs a positive AniList ID or a link like https://anilist.co/anime/123.")
            })?;
            if !seen.insert(id) {
                return Err(format!(
                    "Line {line_number} repeats AniList ID {id}. Keep one entry per release."
                ));
            }
            releases.push(GuideRelease { id, optional });
        }
        if releases.is_empty() {
            return Err("Add at least one AniList release to the guide.".into());
        }
        Ok(Self {
            name: name.into(),
            releases,
        })
    }

    pub fn resolve(self, group: &Group) -> Result<Guide, String> {
        let members: HashMap<_, _> = group
            .entries
            .iter()
            .map(|entry| (entry.provider_id, entry.series_id.is_some()))
            .collect();
        let mut selected = HashSet::new();
        for release in &self.releases {
            let Some(&tracked) = members.get(&release.id) else {
                return Err(format!(
                    "AniList ID {} is outside this franchise. Remove that release or open the correct franchise.",
                    release.id
                ));
            };
            if !release.optional && !tracked {
                selected.insert(release.id);
            }
        }
        Ok(Guide {
            name: self.name,
            releases: self.releases,
            selected,
        })
    }
}

fn parse_id(value: &str) -> Option<i64> {
    let id = if let Some(tail) = value.strip_prefix("https://anilist.co/anime/") {
        let path = tail.strip_suffix('/').unwrap_or(tail);
        if let Some((id, slug)) = path.split_once('/') {
            if matches!(slug, "" | "." | "..")
                || slug.contains(['/', '?', '#', '\\'])
                || slug.chars().any(|ch| ch.is_control() || ch.is_whitespace())
            {
                return None;
            }
            id
        } else {
            path
        }
    } else {
        value
    };
    if !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // AniList accepts signed 32-bit GraphQL Int values for media IDs.
    id.parse::<i32>().ok().filter(|id| *id > 0).map(i64::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::library::groups::Entry;

    #[test]
    fn accepts_exact_ids_and_links_in_order_with_optional_rows() {
        let draft = GuideDraft::parse(
            "  Viewing guide  ",
            "12\r\n\r\n? https://anilist.co/anime/18/Title-Name/\r\nhttps://anilist.co/anime/14\r\n",
        )
        .unwrap();
        assert_eq!(draft.name, "Viewing guide");
        assert_eq!(
            draft.releases,
            [
                GuideRelease {
                    id: 12,
                    optional: false
                },
                GuideRelease {
                    id: 18,
                    optional: true
                },
                GuideRelease {
                    id: 14,
                    optional: false
                },
            ]
        );
    }

    #[test]
    fn rejects_unsupported_entries_and_duplicate_ids_with_line_numbers() {
        for invalid in [
            "0",
            "-123",
            "+123",
            "2147483648",
            "123 124",
            "?",
            "http://anilist.co/anime/123",
            "https://anilist.co.example/anime/123",
            "https://anilist.co/manga/123",
            "https://anilist.co/anime/123/Title/extra",
            "https://anilist.co/anime/123/Title?extra=1",
        ] {
            let error = GuideDraft::parse("Guide", &format!("42\n{invalid}")).unwrap_err();
            assert!(error.contains("Line 2"), "{invalid}: {error}");
        }
        let error =
            GuideDraft::parse("Guide", "123\n? https://anilist.co/anime/123/Title").unwrap_err();
        assert!(error.contains("Line 2 repeats AniList ID 123"));
    }

    #[test]
    fn bounds_name_lines_size_and_release_count() {
        assert!(GuideDraft::parse("  ", "1").is_err());
        assert!(GuideDraft::parse("Two\nlines", "1").is_err());
        assert!(GuideDraft::parse(&"あ".repeat(MAX_NAME_CHARS + 1), "1").is_err());
        assert!(GuideDraft::parse("Guide", "\n").is_err());
        assert!(GuideDraft::parse("Guide", &"1".repeat(MAX_GUIDE_BYTES + 1)).is_err());
        assert!(GuideDraft::parse("Guide", &"1".repeat(MAX_LINE_BYTES + 1)).is_err());
        let releases = (1..=MAX_RELEASES + 1)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            GuideDraft::parse("Guide", &releases)
                .unwrap_err()
                .contains("up to 100 releases")
        );
    }

    #[test]
    fn resolves_only_exact_group_members_and_selects_required_untracked_releases() {
        let entries = [(1, None), (2, Some(17)), (3, None)].map(|(id, series_id)| {
            let mut entry = Entry::default();
            entry.provider_id = id;
            entry.series_id = series_id;
            entry.library_provider_id = series_id.map(|_| 37);
            entry
        });
        let group = Group {
            page_id: None,
            owner: None,
            title: "Fixture".into(),
            tracked: 1,
            outside: 2,
            entries: entries.into(),
        };
        let guide = GuideDraft::parse("Guide", "?3\n2\n1")
            .unwrap()
            .resolve(&group)
            .unwrap();
        assert_eq!(guide.name, "Guide");
        assert_eq!(
            guide
                .releases
                .iter()
                .map(|release| release.id)
                .collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert!(guide.releases[0].optional);
        assert_eq!(guide.selected, HashSet::from([1]));
        let error = GuideDraft::parse("Guide", "1\n37")
            .unwrap()
            .resolve(&group)
            .unwrap_err();
        assert!(error.contains("AniList ID 37 is outside this franchise"));
    }
}

use super::*;

fn entry(id: i64, title: &str, count: i32) -> DomainEntry {
    DomainEntry {
        anilist_id: id,
        title: title.into(),
        format: "TV".into(),
        episodes: Some(count),
        provider_episodes: Some((1..=count).collect()),
    }
}

fn domain(root: i64, title: &str, count: i32, entries: Vec<DomainEntry>) -> SourceDomain {
    let mut rules = anime_relations::finite_source_rules(root).unwrap();
    rules.push(MappingRule {
        source_first: 1,
        source_last: count,
        target_entry: root,
        target_first: 1,
    });
    rules.sort_by_key(|rule| {
        (
            rule.source_first,
            rule.source_last,
            rule.target_entry,
            rule.target_first,
        )
    });
    let mut domain = SourceDomain {
        version: 1,
        root_anilist_id: root,
        root_title: title.into(),
        root_aliases: vec![title.into()],
        source_season: 1,
        rules,
        entries,
        rules_fingerprint: String::new(),
    };
    domain.rules_fingerprint = domain.content_fingerprint().unwrap();
    domain.validate().unwrap();
    domain
}

fn stone_domain() -> SourceDomain {
    domain(
        172019,
        "Dr. Stone: Science Future",
        12,
        vec![
            entry(172019, "Dr. Stone: Science Future", 12),
            entry(189117, "Dr. Stone: Science Future Part 2", 12),
            entry(199221, "Dr. Stone: Science Future Part 3", 13),
        ],
    )
}

pub(crate) fn lagoon_domain() -> SourceDomain {
    domain(
        889,
        "Black Lagoon",
        12,
        vec![
            entry(889, "Black Lagoon", 12),
            entry(1519, "Black Lagoon: The Second Barrage", 12),
        ],
    )
}

fn stone_files() -> Vec<String> {
    (1..=38)
        .map(|episode| format!("[Erai-raws] Dr. Stone: Science Future - {episode:02} [1080p].mkv"))
        .collect()
}

pub(crate) fn lagoon_files() -> Vec<String> {
    (1..=29)
        .map(|episode| {
            format!(
                "[Legion] Black Lagoon - {episode:02} [BD x264 1080p 10bit FLAC][Dual Audio].mkv"
            )
        })
        .collect()
}

pub(crate) fn lagoon_proof() -> BoundSourceProof {
    let files = lagoon_files();
    let preview = prepare_review(
        lagoon_domain(),
        "91a564c6d0f605344b67cef81eeab3199d61872b",
        &files,
        1519,
    )
    .unwrap();
    preview.confirm(&files[12..24]).unwrap()
}

#[test]
fn finite_domains_project_source_roots_without_graph_offsets_or_wanted_slots() {
    let files = stone_files();
    for (raw, target, own) in [
        (12, 172019, 12),
        (13, 189117, 1),
        (24, 189117, 12),
        (25, 199221, 1),
        (37, 199221, 13),
    ] {
        let preview = prepare_review(
            stone_domain(),
            "e4d62dcf1963a4a7022840c8e935c54fbd0cbcc3",
            &files,
            target,
        )
        .unwrap();
        let proof = preview.confirm(&[files[raw - 1].clone()]).unwrap();
        assert_eq!(proof.episodes(), [own]);
        assert_eq!(proof.resolve_file(&files[raw - 1]).unwrap().entry, target);
        assert_eq!(proof.resolve_file(&files[raw - 1]).unwrap().first, own);
    }
    let preview = prepare_review(stone_domain(), &"a".repeat(40), &files, 172019).unwrap();
    assert!(
        preview.confirm(&[files[12].clone()]).is_err(),
        "source13 belongs to Part2, never the requested first cour"
    );
    assert!(
        preview.confirm(&[files[37].clone()]).is_err(),
        "source38 is outside the finite source domain"
    );
    let new_world = vec!["[Erai-raws] Dr. Stone: New World - 34 [1080p].mkv".into()];
    let preview = prepare_review(stone_domain(), &"b".repeat(40), &new_world, 172019).unwrap();
    assert!(
        preview.confirm(&new_world).is_err(),
        "a graph-derived22 cannot create a NewWorld34 mapping"
    );
}

#[test]
fn reviewed_lagoon_selection_preserves_gaps_and_provider_namespaces() {
    let proof = lagoon_proof();
    assert_eq!(proof.episodes(), (1..=12).collect::<Vec<_>>());
    assert_eq!(proof.source_span().unwrap(), (13, 24, 1));
    let files = lagoon_files();
    let selected = vec![files[12].clone(), files[14].clone()];
    let narrowed = proof.narrow_to(&selected).unwrap();
    assert_eq!(narrowed.episodes(), [1, 3]);
    assert!(narrowed.is_within_review(&proof));
    assert!(!proof.is_within_review(&narrowed));
    assert!(narrowed.resolve_file(&files[13]).is_err());
    assert_eq!(
        narrowed.resolve_file(&files[14]).unwrap().first,
        3,
        "source15 is translated before validating canonical provider episode3"
    );
    assert!(proof.narrow_to(&[files[11].clone()]).is_err());
    assert!(proof.narrow_to(&[files[24].clone()]).is_err());
    assert!(proof.narrow_to(&[]).is_err());
    let mut partial = lagoon_domain();
    partial
        .entries
        .iter_mut()
        .find(|entry| entry.anilist_id == 1519)
        .unwrap()
        .provider_episodes = Some(vec![1, 2]);
    partial.rules_fingerprint = partial.content_fingerprint().unwrap();
    let preview = prepare_review(partial, &proof.info_hash, &files, 1519).unwrap();
    assert!(
        preview.confirm(&[files[14].clone()]).is_err(),
        "a provider gap cannot become an identity fallback"
    );
}

#[test]
fn exact_hash_and_full_relative_manifest_are_required_after_restart() {
    let proof = lagoon_proof();
    let restored: BoundSourceProof =
        serde_json::from_str(&serde_json::to_string(&proof).unwrap()).unwrap();
    restored.validate().unwrap();
    let mut files = lagoon_files();
    files.reverse();
    let selected = restored.selected_indices(&files).unwrap();
    assert_eq!(selected.len(), 12);
    let wanted: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), selected.contains(&index)))
        .collect();
    restored
        .validate_files(&restored.info_hash, &wanted)
        .unwrap();
    assert!(restored.validate_files(&"f".repeat(40), &wanted).is_err());
    let mut changed = files.clone();
    changed[0] = format!("renamed/{}", changed[0]);
    assert!(
        restored.selected_indices(&changed).is_err(),
        "unwanted files are still part of the exact source manifest"
    );
    let all_wanted: Vec<_> = files.iter().map(|name| (name.as_str(), true)).collect();
    assert!(
        restored
            .validate_files(&restored.info_hash, &all_wanted)
            .is_err()
    );
    let mut tampered = restored.clone();
    tampered.selected_files[0].target_first = 2;
    assert!(tampered.validate().is_err());
    let mut wrong_root = restored.clone();
    wrong_root.domain.root_anilist_id = 172019;
    assert!(wrong_root.validate().is_err());
}

#[test]
fn file_ownership_conflicts_and_cross_entry_spans_cannot_be_reviewed_away() {
    for name in [
        "[G] Black Lagoon: Roberta's Blood Trail - 15 [1080p].mkv",
        "[G] Black Lagoon Part 2 - 15 [1080p].mkv",
        "[G] Black Lagoon - 12-13 [1080p].mkv",
        "[G] Foreign Story - 15 [1080p].mkv",
        "[G] Black Lagoon - 15 - Episode Title S02E16.mkv",
        "[G] Black Lagoon S02E15 [1080p].mkv",
        "[G] Black Lagoon Season 2 - 15 [1080p].mkv",
        "Season 2/[G] Black Lagoon - 15 [1080p].mkv",
        "Part 2/[G] Black Lagoon - 15 [1080p].mkv",
    ] {
        let names = vec![name.into()];
        let preview = prepare_review(lagoon_domain(), &"c".repeat(40), &names, 1519).unwrap();
        assert!(preview.confirm(&names).is_err(), "{name}");
    }
    let mut conflict = lagoon_domain();
    conflict.rules.push(MappingRule {
        source_first: 15,
        source_last: 15,
        target_entry: 889,
        target_first: 1,
    });
    conflict.rules_fingerprint = conflict.content_fingerprint().unwrap();
    assert!(conflict.validate().is_err());
    let mut open = lagoon_domain();
    open.rules[0].source_last = MAX_PLAUSIBLE_EPISODES + 1;
    open.rules_fingerprint = open.content_fingerprint().unwrap();
    assert!(open.validate().is_err());
}

#[test]
fn v2_mapping_keeps_exact_coverage_and_legacy_serialization_unchanged() {
    use crate::services::release_numbering::ReleaseMapping;
    let files = lagoon_files();
    let proof = lagoon_proof().narrow_to(&[files[14].clone()]).unwrap();
    let mapping = ReleaseMapping::from_bound_source(&proof).unwrap();
    assert_eq!(mapping.episodes(), [3]);
    assert_eq!(mapping.offset(), 12);
    assert!(!mapping.contains_episode(2));
    assert_eq!(mapping.resolve_bound_file(&files[14]).unwrap().first, 3);
    let json = serde_json::to_string(&mapping).unwrap();
    let restored: ReleaseMapping = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, mapping);
    let legacy = r#"{"version":1,"anilist_id":1519,"tv_series":true,"source_first":3,"source_last":3,"target_first":3,"source_season":2,"basis":"season_relative"}"#;
    let old: ReleaseMapping = serde_json::from_str(legacy).unwrap();
    old.validate().unwrap();
    assert_eq!(serde_json::to_string(&old).unwrap(), legacy);
    let mut forged: serde_json::Value = serde_json::from_str(&json).unwrap();
    forged["version"] = 1.into();
    assert!(
        serde_json::from_value::<ReleaseMapping>(forged)
            .unwrap()
            .validate()
            .is_err()
    );
}

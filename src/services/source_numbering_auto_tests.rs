use super::*;
use crate::services::episode_mapping::MappingRule;
use crate::services::source_numbering::{DomainEntry, ProofProvenance, prepare_review};

const HASH: &str = "91a564c6d0f605344b67cef81eeab3199d61872b";
const URL: &str = "https://nyaa.si/view/1428087";

fn domain(
    root: i64,
    title: &str,
    root_count: i32,
    redirects: &[(i32, i32, i64, i32)],
) -> SourceDomain {
    let mut rules = vec![MappingRule {
        source_first: 1,
        source_last: root_count,
        target_entry: root,
        target_first: 1,
    }];
    rules.extend(
        redirects
            .iter()
            .map(|&(first, last, owner, target)| MappingRule {
                source_first: first,
                source_last: last,
                target_entry: owner,
                target_first: target,
            }),
    );
    let mut counts = BTreeMap::from([(root, root_count)]);
    for rule in &rules {
        let end = rule.target_first + rule.source_last - rule.source_first;
        counts
            .entry(rule.target_entry)
            .and_modify(|count| *count = (*count).max(end))
            .or_insert(end);
    }
    let entries = counts
        .into_iter()
        .map(|(id, count)| DomainEntry {
            anilist_id: id,
            title: if id == root {
                title.into()
            } else {
                format!("Entry {id}")
            },
            format: "TV".into(),
            episodes: Some(count),
            provider_episodes: Some((1..=count).collect()),
        })
        .collect();
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
    seal(&mut domain);
    domain
}

fn seal(domain: &mut SourceDomain) {
    domain.rules_fingerprint = domain.content_fingerprint().unwrap();
    domain.validate().unwrap();
}

fn stone() -> SourceDomain {
    domain(
        172019,
        "Dr. Stone: Science Future",
        12,
        &[(13, 24, 189117, 1), (25, 37, 199221, 1)],
    )
}

fn lagoon() -> SourceDomain {
    domain(889, "Black Lagoon", 12, &[(13, 24, 1519, 1)])
}

fn file(title: &str, number: impl std::fmt::Display) -> String {
    format!("[Fixture] {title} - {number} [1080p].mkv")
}

fn files(title: &str, range: std::ops::RangeInclusive<i32>) -> Vec<String> {
    range
        .map(|number| file(title, format!("{number:02}")))
        .collect()
}

fn resolve(domain: SourceDomain, files: &[String]) -> Result<AutomaticInterpretation, String> {
    let root = domain.root_anilist_id;
    resolve_unique(
        HASH,
        files,
        &[root],
        &[DomainCandidate {
            root_anilist_id: root,
            domain: Ok(domain),
        }],
    )
}

#[test]
fn continuous_complete_manifest_is_resolved_without_any_requested_owner_or_episode() {
    let names = files("Dr. Stone: Science Future", 1..=37);
    let result = resolve(stone(), &names).unwrap();
    assert_eq!(result.assignments().len(), 37);
    let root = result.build_proof_for_owner(172019, URL).unwrap().unwrap();
    let part2 = result.build_proof_for_owner(189117, URL).unwrap().unwrap();
    let part3 = result.build_proof_for_owner(199221, URL).unwrap().unwrap();
    assert_eq!(root.episodes(), (1..=12).collect::<Vec<_>>());
    assert_eq!(part2.episodes(), (1..=12).collect::<Vec<_>>());
    assert_eq!(part3.episodes(), (1..=13).collect::<Vec<_>>());
    assert_eq!(part3.selected_files[0].source_first, 25);
    assert_eq!(part3.selected_files[0].target_first, 1);
    for proof in [root, part2, part3] {
        assert_eq!(proof.version, 2);
        let Some(ProofProvenance::AutomaticUnique { evidence }) = proof.provenance else {
            panic!("automatic proof provenance required")
        };
        assert_eq!(evidence.verifier_version, VERIFIER_VERSION);
        assert_eq!(evidence.scope_fingerprint, result.scope_fingerprint);
        assert_eq!(evidence.publication_url, URL);
    }
    assert!(result.build_proof_for_owner(99999, URL).unwrap().is_none());
}

#[test]
fn a_single_high_number_is_a_continuity_witness_but_an_unqualified_twelve_is_not() {
    let result = resolve(stone(), &[file("Dr. Stone: Science Future", "25")]).unwrap();
    assert_eq!(result.assignments()[0].target_anilist_id, 199221);
    assert_eq!(result.assignments()[0].target_first, 1);
    let reason = resolve(stone(), &[file("Dr. Stone: Science Future", "12")]).unwrap_err();
    assert!(reason.contains("relative/reset"), "{reason}");
    assert!(
        reason.contains("189117") && reason.contains("199221"),
        "{reason}"
    );
}

#[test]
fn a_later_cour_count_cannot_be_ignored_to_make_an_early_file_unique() {
    let mut d = stone();
    d.entries
        .iter_mut()
        .find(|entry| entry.anilist_id == 199221)
        .unwrap()
        .episodes = None;
    d.rules_fingerprint = d.content_fingerprint().unwrap();
    let reason = resolve(d, &[file("Dr. Stone: Science Future", "25")]).unwrap_err();
    assert!(reason.contains("finite reset bounds"), "{reason}");
}

#[test]
fn legion_unrequested_tail_stays_excluded_in_every_owner_projection() {
    let names = files("Black Lagoon", 1..=29);
    let result = resolve(lagoon(), &names).unwrap();
    assert_eq!(result.assignments().len(), 24);
    assert_eq!(result.excluded_files().len(), 5);
    assert!(
        result
            .excluded_files()
            .iter()
            .all(|file| file.kind == ExclusionKind::OutsideFiniteDomain)
    );
    let root = result.build_proof_for_owner(889, URL).unwrap().unwrap();
    let second = result.build_proof_for_owner(1519, URL).unwrap().unwrap();
    assert_eq!(root.selected_files.len(), 12);
    assert_eq!(second.selected_files.len(), 12);
    assert_eq!(second.published_files.len(), 29);
    assert!(
        second
            .selected_files
            .iter()
            .all(|file| (13..=24).contains(&file.source_first))
    );
    assert!(result.build_proof_for_owner(4901, URL).unwrap().is_none());
}

#[test]
fn an_excluded_high_number_is_not_a_reset_witness() {
    let mut names = files("Black Lagoon", 1..=12);
    names.push(file("Black Lagoon", "29"));
    let reason = resolve(lagoon(), &names).unwrap_err();
    assert!(reason.contains("relative/reset"), "{reason}");
}

#[test]
fn the_mechanism_is_data_driven_for_an_unrelated_catalogue_domain() {
    let d = domain(91001, "Fixture Saga", 4, &[(5, 8, 91002, 1)]);
    let result = resolve(d, &[file("Fixture Saga", "07")]).unwrap();
    assert_eq!(result.assignments()[0].target_anilist_id, 91002);
    assert_eq!(result.assignments()[0].target_first, 3);
}

#[test]
fn a_shared_root_prefix_cannot_hide_a_foreign_installment_with_a_continuity_witness() {
    for foreign in [
        "[LNS] Code Geass - Boukoku no Akito (Akito the Exiled) - 26 [BD 1080p].mkv",
        "[LNS] Code Geass (Boukoku no Akito) - 26 [BD 1080p].mkv",
        "[LNS] Code Geass [Boukoku no Akito] - 26 [BD 1080p].mkv",
    ] {
        let d = domain(1575, "Code Geass", 25, &[(26, 50, 2904, 1)]);
        assert_eq!(
            release_numbering::validated_source_file_span(foreign)
                .unwrap()
                .0
                .first,
            26,
            "{foreign}"
        );
        let reason = resolve(d, &[foreign.to_string()]).unwrap_err();
        assert!(
            reason.contains("canonical source root") || reason.contains("full source title"),
            "{foreign}: {reason}"
        );
    }
}

#[test]
fn post_episode_title_text_does_not_change_the_canonical_source_identity() {
    let d = domain(1575, "Code Geass", 25, &[(26, 50, 2904, 1)]);
    let name = "[Fixture] Code Geass - 26 - The New Season [1080p].mkv".to_string();
    let result = resolve(d, std::slice::from_ref(&name)).unwrap();
    assert_eq!(result.assignments().len(), 1);
    assert_eq!(result.assignments()[0].name, name);
    assert_eq!(result.assignments()[0].target_anilist_id, 2904);
    assert_eq!(result.assignments()[0].target_first, 1);
}

#[test]
fn numeric_canonical_root_titles_remain_part_of_the_source_identity() {
    for title in ["86 EIGHTY-SIX", "Digimon Adventure 02"] {
        let d = domain(91001, title, 12, &[(13, 24, 91002, 1)]);
        let result = resolve(d, &[file(title, "23")]).unwrap();
        assert_eq!(result.assignments().len(), 1, "{title}");
        assert_eq!(result.assignments()[0].target_anilist_id, 91002, "{title}");
        assert_eq!(result.assignments()[0].target_first, 11, "{title}");
    }
}

#[test]
fn a_single_entry_relative_interpretation_does_not_create_false_reset_ambiguity() {
    let d = domain(91001, "Fixture Saga", 4, &[]);
    let result = resolve(d, &[file("Fixture Saga", "03")]).unwrap();
    assert_eq!(result.assignments()[0].target_anilist_id, 91001);
    assert_eq!(result.assignments()[0].target_first, 3);
}

#[test]
fn a_fresh_nonmatching_root_is_zero_interpretations_not_missing_evidence() {
    let other = domain(91001, "Different Source", 4, &[]);
    let inputs = [
        DomainCandidate {
            root_anilist_id: 172019,
            domain: Ok(stone()),
        },
        DomainCandidate {
            root_anilist_id: 91001,
            domain: Ok(other),
        },
    ];
    let result = resolve_unique(
        HASH,
        &[file("Dr. Stone: Science Future", "25")],
        &[172019, 91001],
        &inputs,
    )
    .unwrap();
    assert_eq!(result.domain().root_anilist_id, 172019);
}

#[test]
fn two_valid_roots_hold_even_when_their_current_assignments_coincide() {
    for second_owner in [91002, 92002] {
        let a = domain(91001, "Shared Source", 4, &[(5, 8, 91002, 1)]);
        let b = domain(92001, "Shared Source", 4, &[(5, 8, second_owner, 1)]);
        let inputs = [
            DomainCandidate {
                root_anilist_id: 91001,
                domain: Ok(a),
            },
            DomainCandidate {
                root_anilist_id: 92001,
                domain: Ok(b),
            },
        ];
        let reason = resolve_unique(
            HASH,
            &[file("Shared Source", "05")],
            &[91001, 92001],
            &inputs,
        )
        .unwrap_err();
        assert!(reason.contains("multiple valid source roots"), "{reason}");
    }
}

#[test]
fn a_second_root_with_only_a_reset_interpretation_cannot_disappear() {
    let a = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    let b = DomainCandidate {
        root_anilist_id: 91001,
        domain: Ok(domain(
            91001,
            "Dr. Stone: Science Future",
            12,
            &[(100, 129, 91002, 1)],
        )),
    };
    let reason = resolve_unique(
        HASH,
        &[file("Dr. Stone: Science Future", "25")],
        &[172019, 91001],
        &[a, b],
    )
    .unwrap_err();
    assert!(
        reason.contains("relative/reset") && reason.contains("91002"),
        "{reason}"
    );
}

#[test]
fn an_incompatible_alias_cannot_claim_a_reset_even_when_its_count_fits() {
    let a = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    let b = DomainCandidate {
        root_anilist_id: 91001,
        domain: Ok(domain(
            91001,
            "Different Source",
            12,
            &[(100, 129, 91002, 1)],
        )),
    };
    let result = resolve_unique(
        HASH,
        &[file("Dr. Stone: Science Future", "25")],
        &[172019, 91001],
        &[a, b],
    )
    .unwrap();
    assert_eq!(result.domain().root_anilist_id, 172019);
}

#[test]
fn a_cross_entry_continuous_failure_can_still_be_a_competing_reset() {
    let a = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    let b = DomainCandidate {
        root_anilist_id: 91001,
        domain: Ok(domain(91001, "Dr. Stone: Science Future", 13, &[])),
    };
    let reason = resolve_unique(
        HASH,
        &[file("Dr. Stone: Science Future", "12-13")],
        &[172019, 91001],
        &[a, b],
    )
    .unwrap_err();
    assert!(reason.contains("relative/reset"), "{reason}");
}

#[test]
fn missing_stale_or_open_relevant_evidence_cannot_disappear_from_the_scope() {
    let names = [file("Dr. Stone: Science Future", "25")];
    let available = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    let reason = resolve_unique(
        HASH,
        &names,
        &[172019, 91001],
        std::slice::from_ref(&available),
    )
    .unwrap_err();
    assert!(reason.contains("incomplete"), "{reason}");
    for reason in [
        "stale metadata",
        "missing canonical identity",
        "unknown or open range",
    ] {
        let missing = DomainCandidate {
            root_anilist_id: 91001,
            domain: Err(reason.into()),
        };
        let failure = resolve_unique(
            HASH,
            &names,
            &[172019, 91001],
            &[available.clone(), missing],
        )
        .unwrap_err();
        assert!(
            failure.contains("unavailable") && failure.contains(reason),
            "{failure}"
        );
    }
}

#[test]
fn unexpected_duplicate_and_misidentified_roots_hold() {
    let names = [file("Dr. Stone: Science Future", "25")];
    let d = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    for roots in [vec![], vec![0], vec![172019, 172019], vec![91001]] {
        assert!(resolve_unique(HASH, &names, &roots, std::slice::from_ref(&d)).is_err());
    }
    assert!(resolve_unique(HASH, &names, &[172019], &[d.clone(), d]).is_err());
    let wrong = DomainCandidate {
        root_anilist_id: 91001,
        domain: Ok(stone()),
    };
    assert!(
        resolve_unique(HASH, &names, &[91001], &[wrong])
            .unwrap_err()
            .contains("another root")
    );
}

#[test]
fn invalid_domain_fingerprint_and_missing_root_identity_are_not_negative_matches() {
    let names = [file("Dr. Stone: Science Future", "25")];
    let mut d = stone();
    d.rules_fingerprint = "0".repeat(64);
    assert!(resolve(d, &names).unwrap_err().contains("is invalid"));
    let mut d = stone();
    d.rules.retain(|rule| rule.target_entry != 172019);
    seal(&mut d);
    assert!(
        resolve(d, &names)
            .unwrap_err()
            .contains("root identity range")
    );
}

#[test]
fn root_and_file_order_cannot_choose_an_interpretation_or_change_its_scope() {
    let a = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    let b = DomainCandidate {
        root_anilist_id: 91001,
        domain: Ok(domain(91001, "Different Source", 4, &[])),
    };
    let mut names = files("Dr. Stone: Science Future", 1..=37);
    let one = resolve_unique(HASH, &names, &[172019, 91001], &[a.clone(), b.clone()]).unwrap();
    names.reverse();
    let two = resolve_unique(HASH, &names, &[91001, 172019], &[b, a]).unwrap();
    assert_eq!(one.scope_fingerprint, two.scope_fingerprint);
    assert_eq!(one.assignments(), two.assignments());
    assert_eq!(one.published_files(), two.published_files());
}

#[test]
fn a_changed_competing_domain_or_manifest_changes_the_scope_receipt() {
    let a = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    let b = DomainCandidate {
        root_anilist_id: 91001,
        domain: Ok(domain(91001, "Different Source", 4, &[])),
    };
    let mut names = vec![file("Dr. Stone: Science Future", "25")];
    let first = resolve_unique(HASH, &names, &[172019, 91001], &[a.clone(), b.clone()]).unwrap();
    let mut changed = b;
    changed
        .domain
        .as_mut()
        .unwrap()
        .root_aliases
        .push("Different Alias".into());
    seal(changed.domain.as_mut().unwrap());
    let second = resolve_unique(HASH, &names, &[172019, 91001], &[a.clone(), changed]).unwrap();
    assert_ne!(first.scope_fingerprint, second.scope_fingerprint);
    names.push("checksums.sha256".into());
    let third = resolve_unique(
        HASH,
        &names,
        &[172019, 91001],
        &[
            a,
            DomainCandidate {
                root_anilist_id: 91001,
                domain: Ok(domain(91001, "Different Source", 4, &[])),
            },
        ],
    )
    .unwrap();
    assert_ne!(first.scope_fingerprint, third.scope_fingerprint);
    assert_eq!(first.assignments(), third.assignments());
}

#[test]
fn a_new_hash_never_reuses_the_old_automatic_receipt() {
    let names = [file("Dr. Stone: Science Future", "25")];
    let first = resolve(stone(), &names).unwrap();
    let second = resolve_unique(
        &"a".repeat(40),
        &names,
        &[172019],
        &[DomainCandidate {
            root_anilist_id: 172019,
            domain: Ok(stone()),
        }],
    )
    .unwrap();
    assert_ne!(first.scope_fingerprint, second.scope_fingerprint);
    assert_eq!(second.info_hash(), "a".repeat(40));
}

#[test]
fn ignored_extras_remain_in_the_manifest_and_never_become_owner_files() {
    let mut names = vec![file("Dr. Stone: Science Future", "25")];
    names.extend([
        "notes.txt".into(),
        "NCOP.mkv".into(),
        "Specials/Bonus S00E01.mkv".into(),
    ]);
    let result = resolve(stone(), &names).unwrap();
    assert_eq!(result.assignments().len(), 1);
    assert_eq!(result.excluded_files().len(), 3);
    let proof = result.build_proof_for_owner(199221, URL).unwrap().unwrap();
    assert_eq!(proof.published_files.len(), 4);
    assert_eq!(proof.selected_files.len(), 1);
}

#[test]
fn foreign_or_unverifiable_main_files_prevent_a_complete_interpretation() {
    for foreign in [
        "[Fixture] Foreign Story - 25.mkv",
        "unknown.mkv",
        "[Fixture] Dr. Stone: Science Future Part 2 - 25.mkv",
    ] {
        let names = [file("Dr. Stone: Science Future", "25"), foreign.into()];
        assert!(
            resolve(stone(), &names)
                .unwrap_err()
                .contains("no complete consistent interpretation")
        );
    }
}

#[test]
fn explicit_season_or_cour_disagreement_cannot_supply_a_witness() {
    for wrong in [
        "[Fixture] Dr. Stone: Science Future S02E25.mkv",
        "Season 2/[Fixture] Dr. Stone: Science Future - 25.mkv",
        "Part 2/[Fixture] Dr. Stone: Science Future - 25.mkv",
    ] {
        assert!(resolve(stone(), &[wrong.into()]).is_err(), "{wrong}");
    }
}

#[test]
fn wholly_unmapped_files_can_be_excluded_but_not_partial_or_cross_entry_ranges() {
    assert!(
        resolve(stone(), &[file("Dr. Stone: Science Future", "38")])
            .unwrap_err()
            .contains("no main file")
    );
    for number in ["37-38", "12-13"] {
        let reason = resolve(stone(), &[file("Dr. Stone: Science Future", number)]).unwrap_err();
        assert!(
            reason.contains("conflicting or partial file range"),
            "{reason}"
        );
    }
}

#[test]
fn duplicate_versions_cannot_choose_the_canonical_episode_automatically() {
    let names = [
        file("Dr. Stone: Science Future", "25"),
        file("Dr. Stone: Science Future", "25v2"),
    ];
    assert!(resolve(stone(), &names).unwrap_err().contains("overlap"));
}

#[test]
fn provider_evidence_cannot_be_overridden_by_the_published_number() {
    let mut d = stone();
    d.entries
        .iter_mut()
        .find(|entry| entry.anilist_id == 199221)
        .unwrap()
        .provider_episodes = Some((2..=13).collect());
    seal(&mut d);
    assert!(resolve(d, &[file("Dr. Stone: Science Future", "25")]).is_err());
}

#[test]
fn bad_hashes_duplicate_paths_and_unsafe_publications_hold_before_projection() {
    let names = [file("Dr. Stone: Science Future", "25")];
    let candidate = DomainCandidate {
        root_anilist_id: 172019,
        domain: Ok(stone()),
    };
    for hash in ["", "all", "a|b", &"z".repeat(40)] {
        assert!(resolve_unique(hash, &names, &[172019], std::slice::from_ref(&candidate)).is_err());
    }
    for names in [
        vec![],
        vec![names[0].clone(), names[0].clone()],
        vec!["../source.mkv".into()],
        vec!["/source.mkv".into()],
        vec!["x".repeat(4097)],
    ] {
        assert!(resolve_unique(HASH, &names, &[172019], std::slice::from_ref(&candidate)).is_err());
    }
}

#[test]
fn a_modified_scope_receipt_cannot_be_stamped_as_an_automatic_proof() {
    let mut result = resolve(stone(), &[file("Dr. Stone: Science Future", "25")]).unwrap();
    result.scope_fingerprint = "0".repeat(64);
    assert!(
        result
            .build_proof_for_owner(199221, URL)
            .unwrap_err()
            .contains("scope evidence changed")
    );
}

#[test]
fn automatic_projection_never_changes_the_existing_human_proof_format() {
    let d = stone();
    let names = [file("Dr. Stone: Science Future", "25")];
    let human = prepare_review(d.clone(), HASH, &names, 199221)
        .unwrap()
        .confirm(&names)
        .unwrap();
    let before = serde_json::to_vec(&human).unwrap();
    assert_eq!(human.version, 1);
    assert!(human.provenance.is_none());
    let automatic = resolve(d, &names)
        .unwrap()
        .build_proof_for_owner(199221, URL)
        .unwrap()
        .unwrap();
    assert_eq!(automatic.version, 2);
    assert_eq!(serde_json::to_vec(&human).unwrap(), before);
    assert!(
        serde_json::to_value(&human)
            .unwrap()
            .get("provenance")
            .is_none()
    );
    assert!(
        resolve(stone(), &names)
            .unwrap()
            .build_proof_for_owner(199221, "file:///untrusted")
            .is_err()
    );
}

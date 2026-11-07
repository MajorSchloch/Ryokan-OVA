use super::*;

fn file(path: &str) -> ObservedFile {
    ObservedFile {
        path: path.into(),
        kind: "file".into(),
        size: 123,
        mtime_ns: 456,
        device: 1,
        inode: 2,
        links: 1,
        text: None,
        sha256: None,
    }
}

fn fixture() -> (Snapshot, Manifest) {
    let mut tables = BTreeMap::new();
    for name in [
        "config",
        "series",
        "episode_monitor_state",
        "episode_quality_tags",
        "episode_grab_history",
        "series_episode_metadata",
        "provider_episode_metadata",
        "episode_airings",
        "episode_cache",
        "grabbed_torrents",
        "grabbed_torrent_series",
        "grabbed_torrent_numbering",
        "pending_grabs",
    ] {
        tables.insert(name.into(), Vec::new());
    }
    tables.get_mut("config").unwrap().push(json!({"id":1,"title_language":"english",
        "series_folder_format":"{series.title} ({series.year})","season_folder_format":"Season {season.number:00}",
        "episode_file_format":"{series.title} - S{season.number:00}E{episode.number:00} - {episode.title} [{quality.full}] [{group}]{ext}"}));
    tables.get_mut("series").unwrap().push(json!({"id":1,"anilist_id":101,"episodes":2,"title":"Example",
        "title_english":"Example","season_year":2020,"folder_name":"Old Example","cumulative_prior_episodes":0}));
    tables
        .get_mut("episode_monitor_state")
        .unwrap()
        .push(json!({"series_id":1,"episode_number":1,"monitored":0}));
    tables.get_mut("episode_quality_tags").unwrap().push(json!({"series_id":1,"episode_number":1,
        "state":"completed","release_group":"Incorrect","manual_override":1,"quality_tag":"BD-1080p",
        "resolution":"1080p","source":"BluRay","is_remux":0,"web_kind":""}));
    tables.get_mut("episode_grab_history").unwrap().extend([
        json!({"id":10,"series_id":1,"episode_number":1,"state":"completed","file_name":"Example - S01E01.mkv","release_title":"Original release"}),
        json!({"id":11,"series_id":1,"episode_number":1,"state":"removed","file_name":"Original source 01.mkv","release_title":"Original release"}),
    ]);
    tables
        .get_mut("provider_episode_metadata")
        .unwrap()
        .push(json!({"provider_id":101,"episode_number":3,"title":"Wrong sequel title"}));
    tables.get_mut("grabbed_torrents").unwrap().push(json!({"id":9,"series_id":1,"state":"imported","episode_numbers":"[1,2]","imported_episode_coverage":null}));
    let mut folder = file("Old Example");
    folder.kind = "directory".into();
    let mut files = vec![
        folder,
        file("Old Example/Season 01/Example - S01E01.mkv"),
        file("Old Example/Season 01/Example - S01E02.mkv"),
        file("Old Example/Specials/Example - S00E01.mkv"),
        file("Old Example/Season 01/Example - S01E01.en.forced.ass"),
    ];
    let mut nfo = file("Old Example/Season 01/Example - S01E01.nfo");
    nfo.text =
        Some("<episodedetails><episode>1</episode><custom>Keep</custom></episodedetails>".into());
    nfo.sha256 = Some(hex::encode(Sha256::digest(
        nfo.text.as_ref().unwrap().as_bytes(),
    )));
    files.push(nfo);
    let file_evidence = files
        .iter()
        .filter(|f| is_video(&f.path))
        .map(|f| FileEvidence {
            path: f.path.clone(),
            resolution: "1080p".into(),
            source: "BluRay".into(),
            release_group: "Group".into(),
            evidence: "Reviewed original source path and release identity".into(),
            grab_id: Some(9),
        })
        .collect();
    let snapshot = Snapshot {
        version: 1,
        collected_at: "2026-11-06T00:00:00Z".into(),
        tables,
        columns: BTreeMap::from([
            ("series".into(), vec!["cumulative_offset_reviewed".into()]),
            (
                "grabbed_torrents".into(),
                vec![
                    "imported_episode_coverage".into(),
                    "library_migration_bridge".into(),
                ],
            ),
        ]),
        root_children: vec!["Old Example".into()],
        files,
        issues: Vec::new(),
    };
    let manifest = Manifest {
        version: 1,
        replay_decisions: Vec::new(),
        snapshot_sha256: String::new(),
        entries: vec![EntryPlan {
            series_id: 1,
            anilist_id: 101,
            old_count: 2,
            target_count: 3,
            canonical_prior_episodes: 0,
            reset_monitoring: None,
            evidence: "Verified provider mapping".into(),
            rules: vec![MappingRule {
                source_first: 1,
                source_last: 2,
                target_entry: 1,
                target_first: 2,
            }],
        }],
        special_files: vec![SpecialFile {
            path: "Old Example/Specials/Example - S00E01.mkv".into(),
            series_id: 1,
            season: 1,
            episode: 1,
            title: "Prologue".into(),
            evidence: "Separate provider prologue maps to this entry's episode 1".into(),
            monitored: true,
        }],
        file_evidence,
        metadata: (1..=3)
            .map(|ep| EpisodeMetadata {
                series_id: 1,
                episode: ep,
                title: format!("Canonical {ep}"),
                aired: String::new(),
                evidence: "Verified metadata identity".into(),
            })
            .collect(),
    };
    (snapshot, manifest)
}

fn review(snapshot: &Snapshot, mut manifest: Manifest) -> Result<Preview, String> {
    let bytes = serde_json::to_vec(snapshot).unwrap();
    manifest.snapshot_sha256 = hex::encode(Sha256::digest(&bytes));
    build(&bytes, &manifest)
}

#[test]
fn explicit_source_bindings_create_a_complete_replay_without_rewriting_old_routes() {
    let (mut s, mut m) = fixture();
    let videos: Vec<_> = m.file_evidence.iter().map(|f| f.path.clone()).collect();
    let names = [
        "Season 1/Example - 01.mkv",
        "Season 1/Example - 02.mkv",
        "Specials/Example - 00.mkv",
    ];
    s.tables.get_mut("grabbed_torrents").unwrap()[0]["imported_source_paths"] = json!(
        serde_json::to_string(
            &names
                .iter()
                .map(|n| format!("/source/{n}"))
                .collect::<Vec<_>>()
        )
        .unwrap()
    );
    m.replay_decisions.push(ReplayDecision {
        grab_id: 9,
        source_root: "/source".into(),
        files: names
            .iter()
            .zip(&videos)
            .map(|(name, path)| ReplayBinding {
                source_name: (*name).into(),
                library_path: path.clone(),
                evidence: "Reviewed source identity".into(),
            })
            .collect(),
    });
    let p = review(&s, m.clone()).unwrap();
    let row = p
        .records
        .iter()
        .find(|r| r.table == "grabbed_torrents")
        .unwrap();
    let bridge = row.after["library_migration_bridge"].as_str().unwrap();
    let plan = crate::models::grabbed_torrents::migration_replay::decode_bridge(bridge).unwrap();
    assert_eq!(
        plan.files
            .iter()
            .find(|f| f.name.ends_with("00.mkv"))
            .unwrap()
            .first,
        1
    );
    assert_eq!(
        plan.files
            .iter()
            .find(|f| f.name.ends_with("01.mkv"))
            .unwrap()
            .first,
        2
    );
    assert!(row.after.get("episode_numbers").is_none());
    assert!(row.after.get("imported_source_paths").is_none());
    m.replay_decisions[0].files.pop();
    assert!(
        review(&s, m.clone())
            .unwrap_err()
            .contains("complete imported file set")
    );
    m.replay_decisions[0].files[0].source_name = "../escape.mkv".into();
    assert!(review(&s, m).is_err());
}

#[test]
fn preview_coordinates_files_nfo_monitoring_history_and_ownership_without_mutation() {
    let (snapshot, manifest) = fixture();
    let original = serde_json::to_value(&snapshot).unwrap();
    let p = review(&snapshot, manifest).unwrap();
    assert!(p.preview_only);
    assert_eq!(p.folders[0].to, "Example (2020)");
    let first = p
        .files
        .iter()
        .find(|f| f.from.ends_with("S01E01.mkv"))
        .unwrap();
    assert_eq!(
        first.to,
        "Example (2020)/Season 01/Example - S01E02 - Canonical 2 [1080p BluRay] [Group].mkv"
    );
    let prologue = p
        .files
        .iter()
        .find(|f| f.from.ends_with("S00E01.mkv"))
        .unwrap();
    assert_eq!(prologue.first, Some(1));
    assert_eq!(prologue.season, Some(1));
    let subtitle = p.files.iter().find(|f| f.from.ends_with(".ass")).unwrap();
    assert_eq!(subtitle.to, first.to.replace(".mkv", ".en.forced.ass"));
    let nfo = p.files.iter().find(|f| f.kind == "episode_nfo").unwrap();
    assert_eq!(nfo.nfo_fields[0]["episode"], 2);
    assert!(
        nfo.observed
            .text
            .as_ref()
            .unwrap()
            .contains("<custom>Keep</custom>")
    );
    let monitor = p
        .records
        .iter()
        .find(|r| r.table == "episode_monitor_state" && r.action == "update")
        .unwrap();
    assert_eq!(monitor.before["monitored"], 0);
    assert_eq!(monitor.after["episode_number"], 2);
    assert!(monitor.after.get("monitored").is_none());
    let history = p
        .records
        .iter()
        .find(|r| r.table == "episode_grab_history" && r.key["id"] == 11)
        .unwrap();
    assert_eq!(history.after["episode_number"], 2);
    assert!(history.after.get("file_name").is_none());
    assert!(history.after.get("release_title").is_none());
    let cache = p
        .records
        .iter()
        .find(|r| r.table == "provider_episode_metadata" && r.key["episode_number"] == 3)
        .unwrap();
    assert_eq!(cache.before["title"], "Wrong sequel title");
    assert_eq!(cache.after["title"], "Canonical 3");
    let coverage = p
        .records
        .iter()
        .find(|r| r.table == "grabbed_torrents")
        .unwrap();
    assert_eq!(
        coverage.after["imported_episode_coverage"],
        "{\"1\":[1,2,3]}"
    );
    assert!(coverage.after.get("episode_numbers").is_none());
    assert_eq!(serde_json::to_value(&snapshot).unwrap(), original);
    let promoted = p
        .records
        .iter()
        .find(|r| r.table == "episode_quality_tags" && r.action == "insert")
        .unwrap();
    assert_eq!(promoted.after["episode_number"], 1);
    assert_eq!(promoted.after["manual_override"], 0);
    assert_eq!(promoted.after["needs_review"], 1);
    assert_eq!(promoted.after["quality_tag"], "BD-1080p");
    let history = p
        .records
        .iter()
        .find(|r| r.table == "episode_grab_history" && r.action == "insert")
        .unwrap();
    assert_eq!(history.after["size_bytes"], 123);
    assert!(history.after.get("grabbed_at").is_none());
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
}

#[test]
fn changed_snapshot_or_identity_is_rejected() {
    let (snapshot, mut m) = fixture();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    m.snapshot_sha256 = hex::encode(Sha256::digest(&bytes));
    let mut changed = bytes.clone();
    changed.push(b' ');
    assert!(build(&changed, &m).unwrap_err().contains("digest"));
    m.entries[0].anilist_id = 102;
    assert!(build(&bytes, &m).unwrap_err().contains("precondition"));
}

#[test]
fn incomplete_snapshot_symlink_traversal_and_duplicate_paths_fail_closed() {
    for variant in 0..4 {
        let (mut s, m) = fixture();
        match variant {
            0 => s.issues.push("Database changed during collection".into()),
            1 => s.files[1].kind = "unsupported".into(),
            2 => s.files[1].path = "Old Example/../outside.mkv".into(),
            _ => s.files.push(s.files[1].clone()),
        }
        assert!(review(&s, m).is_err(), "variant {variant}");
    }
}

#[test]
fn existing_target_folder_and_file_collisions_are_rejected() {
    let (mut s, m) = fixture();
    s.root_children.push("EXAMPLE (2020)".into());
    assert!(review(&s, m).unwrap_err().contains("folder already exists"));
    let (mut s, mut m) = fixture();
    let duplicate = "Old Example/Season 01/Other - S01E01.mp4";
    s.files.push(file(duplicate));
    let mut fact = m.file_evidence[0].clone();
    fact.path = duplicate.into();
    m.file_evidence.push(fact);
    assert!(review(&s, m).unwrap_err().contains("two videos"));
}

#[test]
fn aliases_that_collapse_two_library_episodes_are_rejected() {
    let (s, mut m) = fixture();
    m.entries[0].rules = vec![
        MappingRule {
            source_first: 1,
            source_last: 1,
            target_entry: 1,
            target_first: 2,
        },
        MappingRule {
            source_first: 2,
            source_last: 2,
            target_entry: 1,
            target_first: 2,
        },
    ];
    assert!(review(&s, m).unwrap_err().contains("conflicting"));
}

#[test]
fn unselected_special_never_becomes_a_regular_episode_from_a_missing_slot() {
    let (s, mut m) = fixture();
    m.special_files.clear();
    assert!(review(&s, m).unwrap_err().contains("explicit keep/promote"));
}

#[test]
fn special_can_remain_special_with_the_same_naming_template() {
    let (s, mut m) = fixture();
    m.special_files[0].season = 0;
    let p = review(&s, m).unwrap();
    let extra = p
        .files
        .iter()
        .find(|f| f.from.ends_with("S00E01.mkv"))
        .unwrap();
    assert!(
        extra
            .to
            .contains("/Specials/Example - S00E01 - Prologue [1080p BluRay] [Group]")
    );
    assert!(
        !p.blockers
            .iter()
            .any(|b| b.contains("classification/history"))
    );
}

#[test]
fn missing_metadata_or_evidence_and_invalid_templates_are_rejected() {
    for variant in 0..4 {
        let (mut s, mut m) = fixture();
        match variant {
            0 => {
                m.metadata.pop();
            }
            1 => m.entries[0].evidence.clear(),
            2 => {
                m.file_evidence.pop();
            }
            _ => {
                s.tables.get_mut("config").unwrap()[0]["episode_file_format"] =
                    json!("../{episode.number}{ext}")
            }
        }
        assert!(review(&s, m).is_err(), "variant {variant}");
    }
}

#[test]
fn combined_file_maps_all_episodes_and_keeps_subtitles_and_multi_nfo_fields() {
    let (mut s, mut m) = fixture();
    let first = "Old Example/Season 01/Example - S01E01.mkv";
    let second = "Old Example/Season 01/Example - S01E02.mkv";
    s.files.retain(|f| f.path != second);
    m.file_evidence.retain(|f| f.path != second);
    for f in &mut s.files {
        f.path = f.path.replace("S01E01", "S01E01-E02");
    }
    for f in &mut m.file_evidence {
        f.path = f.path.replace("S01E01", "S01E01-E02");
    }
    s.tables.get_mut("episode_grab_history").unwrap()[0]["file_name"] =
        json!("Example - S01E01-E02.mkv");
    let p = review(&s, m).unwrap();
    let video = p
        .files
        .iter()
        .find(|f| f.from == first.replace("S01E01", "S01E01-E02"))
        .unwrap();
    assert_eq!(video.first, Some(2));
    assert_eq!(video.last, Some(3));
    assert!(video.to.contains("S01E02-E03"));
    let nfo = p.files.iter().find(|f| f.kind == "episode_nfo").unwrap();
    assert_eq!(nfo.nfo_fields.len(), 2);
    assert_eq!(nfo.nfo_fields[1]["episode"], 3);
}

#[test]
fn nonconsecutive_combined_file_is_rejected() {
    let (mut s, mut m) = fixture();
    m.entries[0].target_count = 4;
    m.entries[0].rules = vec![
        MappingRule {
            source_first: 1,
            source_last: 1,
            target_entry: 1,
            target_first: 2,
        },
        MappingRule {
            source_first: 2,
            source_last: 2,
            target_entry: 1,
            target_first: 4,
        },
    ];
    m.metadata.push(EpisodeMetadata {
        series_id: 1,
        episode: 4,
        title: "Four".into(),
        aired: String::new(),
        evidence: "Verified".into(),
    });
    for f in &mut s.files {
        f.path = f.path.replace("S01E01", "S01E01-E02");
    }
    for f in &mut m.file_evidence {
        f.path = f.path.replace("S01E01", "S01E01-E02");
    }
    assert!(review(&s, m).is_err());
}

#[test]
fn active_downloads_and_legacy_routes_are_explicit_blockers() {
    let (mut s, m) = fixture();
    s.tables
        .get_mut("pending_grabs")
        .unwrap()
        .push(json!({"series_id":1}));
    s.tables
        .get_mut("grabbed_torrent_series")
        .unwrap()
        .push(json!({"grab_id":9,"series_id":1,"episode_offset":0}));
    s.columns.get_mut("grabbed_torrents").unwrap().clear();
    s.columns.remove("series");
    let p = review(&s, m).unwrap();
    for phrase in [
        "active download",
        "lacks the migration replay guard",
        "lacks canonical import coverage",
        "cannot protect reviewed episode offsets",
    ] {
        assert!(p.blockers.iter().any(|b| b.contains(phrase)), "{phrase}");
    }
    assert!(
        !p.records
            .iter()
            .any(|r| r.table == "grabbed_torrent_series")
    );
}

#[test]
fn provider_airings_and_raw_metadata_do_not_get_the_library_offset() {
    let (mut s, m) = fixture();
    s.tables
        .get_mut("episode_airings")
        .unwrap()
        .push(json!({"series_id":1,"episode":1,"airing_at":123}));
    s.tables
        .get_mut("episode_cache")
        .unwrap()
        .push(json!({"mal_id":202,"episode_number":1,"title":"Raw one"}));
    let p = review(&s, m).unwrap();
    assert!(
        !p.records
            .iter()
            .any(|r| matches!(r.table.as_str(), "episode_airings" | "episode_cache"))
    );
}

#[test]
fn verified_zero_based_library_numbers_move_records_and_files_together() {
    let (mut s, mut m) = fixture();
    m.entries[0].target_count = 2;
    m.entries[0].rules = vec![MappingRule {
        source_first: 0,
        source_last: 1,
        target_entry: 1,
        target_first: 1,
    }];
    m.metadata.retain(|r| r.episode <= 2);
    m.special_files[0].season = 0;
    let renumber = |path: &str| path.replace("S01E01", "S01E00").replace("S01E02", "S01E01");
    for f in &mut s.files {
        f.path = renumber(&f.path);
    }
    for f in &mut m.file_evidence {
        f.path = renumber(&f.path);
    }
    for table in [
        "episode_monitor_state",
        "episode_quality_tags",
        "episode_grab_history",
    ] {
        for row in s.tables.get_mut(table).unwrap() {
            row["episode_number"] = json!(0);
            if row["state"] == "completed" && table == "episode_grab_history" {
                row["file_name"] = json!("Example - S01E00.mkv");
            }
        }
    }
    let p = review(&s, m).unwrap();
    let zero = p
        .files
        .iter()
        .find(|f| f.from.ends_with("S01E00.mkv"))
        .unwrap();
    assert_eq!(zero.first, Some(1));
    assert!(p.records.iter().any(|r| r.table == "episode_monitor_state"
        && r.key["episode_number"] == 0
        && r.after["episode_number"] == 1));
}

#[test]
fn a_combined_file_cannot_cross_entry_boundaries() {
    let (mut s, mut m) = fixture();
    let mut second = s.tables["series"][0].clone();
    second["id"] = json!(2);
    second["anilist_id"] = json!(102);
    second["episodes"] = json!(1);
    second["folder_name"] = json!("Other");
    second["title_english"] = json!("Other");
    s.tables.get_mut("series").unwrap().push(second);
    let mut dir = file("Other");
    dir.kind = "directory".into();
    s.files.push(dir);
    s.root_children.push("Other".into());
    m.entries[0].rules = vec![
        MappingRule {
            source_first: 1,
            source_last: 1,
            target_entry: 1,
            target_first: 2,
        },
        MappingRule {
            source_first: 2,
            source_last: 2,
            target_entry: 2,
            target_first: 1,
        },
    ];
    m.entries.push(EntryPlan {
        series_id: 2,
        anilist_id: 102,
        old_count: 1,
        target_count: 2,
        canonical_prior_episodes: 3,
        reset_monitoring: None,
        evidence: "Verified split".into(),
        rules: vec![MappingRule {
            source_first: 1,
            source_last: 1,
            target_entry: 2,
            target_first: 2,
        }],
    });
    for ep in 1..=2 {
        m.metadata.push(EpisodeMetadata {
            series_id: 2,
            episode: ep,
            title: format!("Other {ep}"),
            aired: String::new(),
            evidence: "Verified split".into(),
        });
    }
    for f in &mut s.files {
        f.path = f.path.replace("S01E01", "S01E01-E02");
    }
    for f in &mut m.file_evidence {
        f.path = f.path.replace("S01E01", "S01E01-E02");
    }
    assert!(
        review(&s, m)
            .unwrap_err()
            .contains("more than one series entry")
    );
}

#[test]
fn conflicting_classification_and_unattributed_imports_are_not_hidden() {
    let (mut s, m) = fixture();
    s.tables.get_mut("episode_quality_tags").unwrap()[0]["resolution"] = json!("720p");
    s.tables
        .get_mut("grabbed_torrents")
        .unwrap()
        .push(json!({"id":10,"series_id":1,"state":"imported","episode_numbers":"[1]"}));
    let p = review(&s, m).unwrap();
    assert!(
        p.blockers
            .iter()
            .any(|b| b.contains("Naming classification differs"))
    );
    assert!(
        p.blockers
            .iter()
            .any(|b| b.contains("grab 10 has no reviewed"))
    );
}

#[test]
fn damaged_monitor_numbers_require_an_explicit_reset_and_do_not_duplicate_prologue() {
    let (mut s, mut m) = fixture();
    s.tables
        .get_mut("episode_monitor_state")
        .unwrap()
        .push(json!({"series_id":1,"episode_number":0,"monitored":1}));
    assert!(
        review(&s, m.clone())
            .unwrap_err()
            .contains("unmapped episode_monitor_state")
    );
    m.entries[0].reset_monitoring = Some(true);
    let p = review(&s, m).unwrap();
    let changes: Vec<_> = p
        .records
        .iter()
        .filter(|r| r.table == "episode_monitor_state")
        .collect();
    assert_eq!(
        changes
            .iter()
            .filter(|r| r.key["episode_number"] == 1)
            .count(),
        1
    );
    assert!(
        changes
            .iter()
            .any(|r| r.key["episode_number"] == 0 && r.action == "delete_monitor_row")
    );
    for ep in 1..=3 {
        assert!(
            changes
                .iter()
                .any(|r| r.key["episode_number"] == ep && r.after["monitored"] == 1)
        );
    }
}

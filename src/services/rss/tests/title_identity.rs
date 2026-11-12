//! Provider identity survives RSS core and collapsed alias expansion.
use super::super::*;

fn series_with(id: i64, romaji: &str, english: &str, total: i32, offset: i32) -> SeriesMeta {
    SeriesMeta::from_series(&series::Series {
        is_adult: false,
        id,
        anilist_id: id,
        mal_id: None,
        title: english.into(),
        title_romaji: romaji.into(),
        title_english: english.into(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        episodes: Some(total),
        season_year: None,
        end_year: None,
        folder_name: english.into(),
        monitor_mode: "all".into(),
        allow_upgrades: true,
        allow_pt_upgrades: false,
        custom_query_tokens: String::new(),
        restrict_to_uploader: String::new(),
        alternate_titles: String::new(),
        cumulative_prior_episodes: offset,
        monitor_mode_manual_override: false,
        user_score: None,
        added_at: String::new(),
    })
}

fn item(title: &str) -> RssItem {
    RssItem {
        seeders: None,
        title: title.into(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: extract_group(title),
        resolution: extract_resolution(title),
        is_batch: detect_batch(title),
        source: RssSource::Nyaa,
    }
}

#[test]
fn numeric_identity_rejects_actual_digimon_reboot_episode_and_accepts_02_title() {
    let meta = vec![series_with(
        1313,
        "Digimon Adventure 02",
        "Digimon: Digital Monsters 02",
        50,
        0,
    )];
    let wrong = item(
        "[YakuboEncodes] Digimon Adventure (2020) - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]",
    );
    assert!(best_series_match(&wrong, &meta).is_none());
    let right =
        item("[Keyword] Digimon Adventure 02 - 02 [1080p][10bit][x265 HEVC][Opus][Multi-Subs]");
    let found = best_series_match(&right, &meta).expect("02 belongs to the actual parsed title");
    assert_eq!(found.series.anilist_id, 1313);
    assert!(found.alias_score >= 1.0);
    assert_eq!(found.resolved_eps, HashSet::from([2]));
    let mut with_reboot = meta;
    with_reboot.push(series_with(
        114811,
        "Digimon Adventure:",
        "Digimon Adventure:",
        67,
        0,
    ));
    assert_eq!(
        best_series_match(&wrong, &with_reboot)
            .unwrap()
            .series
            .anilist_id,
        114811
    );
}

const BASE: &str = "Mushoku Tensei: Isekai Ittara Honki Dasu";
fn cour() -> SeriesMeta {
    series_with(
        127720,
        &format!("{BASE} Part 2"),
        "Mushoku Tensei: Jobless Reincarnation Cour 2",
        12,
        11,
    )
}
fn season() -> SeriesMeta {
    series_with(
        146065,
        "Mushoku Tensei II: Isekai Ittara Honki Dasu",
        "Mushoku Tensei: Jobless Reincarnation Season 2",
        13,
        23,
    )
}

#[test]
fn cour_identity_does_not_turn_part_2_into_season_2() {
    let target = cour();
    assert_eq!(target.season_num, None);
    assert_eq!(target.target_parts, HashSet::from([2]));
    let wrong = item(
        "[DKB] Mushoku Tensei - Isekai Ittara Honki Dasu - S02E11 [1080p][HEVC x265 10bit][Multi-Subs][weekly]",
    );
    assert!(best_series_match(&wrong, std::slice::from_ref(&target)).is_none());
    assert_eq!(
        best_series_match(&wrong, &[target, season()])
            .unwrap()
            .series
            .anilist_id,
        146065
    );
}

#[test]
fn cour_identity_accepts_own_part_and_proven_absolute_slots() {
    for (title, episode) in [
        (format!("[G] {BASE} Part 2 - S01E11 [1080p]"), 11),
        (
            "[G] Mushoku Tensei: Jobless Reincarnation Cour 2 - 12 [1080p]".into(),
            12,
        ),
        (format!("[G] {BASE} - 22 [1080p]"), 11),
    ] {
        let found = best_series_match(&item(&title), &[cour()]).expect(&title);
        assert_eq!(found.resolved_eps, HashSet::from([episode]), "{title}");
        assert_eq!(
            found.canonical_abs_eps,
            HashSet::from([episode + 11]),
            "{title}"
        );
    }
    for title in [
        format!("[G] {BASE} - 11 [1080p]"),
        format!("[G] {BASE} - S01E11 [1080p]"),
        format!("[G] {BASE} Part 1 - S01E11 [1080p]"),
    ] {
        assert!(
            best_series_match(&item(&title), &[cour()]).is_none(),
            "{title}"
        );
    }
}

#[test]
fn compound_season_and_part_identity_remain_separate() {
    let target = series_with(
        1,
        "Some Story Season 2 Part 2",
        "Some Story Season 2 Cour 2",
        12,
        24,
    );
    for title in [
        "[G] Some Story Part 2 - S02E03 [1080p]",
        "[G] Some Story Cour 2 - S02E03 [1080p]",
    ] {
        assert_eq!(
            best_series_match(&item(title), std::slice::from_ref(&target))
                .unwrap()
                .resolved_eps,
            HashSet::from([3])
        );
    }
    for title in [
        "[G] Some Story Part 1 - S02E03 [1080p]",
        "[G] Some Story Part 2 - S01E03 [1080p]",
        "[G] Some Story - S02E03 [1080p]",
    ] {
        assert!(
            best_series_match(&item(title), std::slice::from_ref(&target)).is_none(),
            "{title}"
        );
    }
}

#[test]
fn part_ranges_and_dashes_never_supply_a_season_hint() {
    for title in [
        "[G] Some Story Part 2 - 11 [1080p]",
        "[G] Some Story Cour 2 - 01-12 [1080p]",
    ] {
        let parsed = parse_release(&item(title));
        assert_eq!(parsed.season_hint, None, "{title}");
        let expected: HashSet<_> = if title.contains("01-12") {
            (1..=12).collect()
        } else {
            HashSet::from([11])
        };
        assert_eq!(
            parsed.absolute_eps, expected,
            "part digit must not become episode"
        );
    }
    assert_eq!(
        parse_release(&item("[G] Some Story Part II - S02E03 [1080p]")).season_hint,
        Some(2)
    );
}

#[test]
fn numeric_title_checks_preserve_title_only_movies_and_batches() {
    let mut movie = series_with(9260, "Kizumonogatari Part 2", "Kizumonogatari Part 2", 1, 0);
    movie.series.format = "MOVIE".into();
    assert!(best_series_match(&item("[MTBB] Kizumonogatari Part 2 [1080p]"), &[movie]).is_some());
    let title = series_with(
        1313,
        "Digimon Adventure 02",
        "Digimon: Digital Monsters 02",
        50,
        0,
    );
    let batch = item("[Group] Digimon Adventure 02 Complete [BD 1080p]");
    assert!(batch.is_batch);
    assert!(best_series_match(&batch, &[title]).is_some());
}

#[test]
fn different_provider_parts_do_not_share_history_or_sweep_buckets() {
    let later = series_with(
        166873,
        "Mushoku Tensei II: Isekai Ittara Honki Dasu Part 2",
        "Mushoku Tensei: Jobless Reincarnation Season 2 Part 2",
        12,
        12,
    );
    let meta = vec![cour(), later];
    let earlier_title = "[Group] Mushoku Tensei: Jobless Reincarnation Cour 2 - S01E02 [1080p]";
    let later_title =
        "[Group] Mushoku Tensei: Jobless Reincarnation Season 2 Part 2 - S02E01 [1080p]";
    let earlier = best_series_match(&item(earlier_title), &meta).unwrap();
    let later = best_series_match(&item(later_title), &meta).unwrap();
    assert_eq!(earlier.series.anilist_id, 127720);
    assert_eq!(later.series.anilist_id, 166873);
    assert_eq!(earlier.family_key, later.family_key);
    assert_eq!(earlier.canonical_abs_eps, HashSet::from([13]));
    assert_eq!(later.canonical_abs_eps, HashSet::from([13]));
    let absolute_title = "[Group] Mushoku Tensei: Jobless Reincarnation - 13v2 [1080p]";
    let absolute = best_series_match(&item(absolute_title), &meta).unwrap();
    assert_eq!(absolute.series.id, earlier.series.id);
    assert_eq!(absolute.resolved_eps, HashSet::from([2]));
    assert_eq!(
        logical_episode_key(&earlier, false),
        logical_episode_key(&absolute, false)
    );
    assert_ne!(
        logical_episode_key(&earlier, false),
        logical_episode_key(&later, false)
    );
    let mut history = CanonicalHistory::default();
    history.insert_title(earlier_title, &meta);
    assert!(history.contains(&earlier, false, 1));
    assert!(!history.contains(&later, false, 1));
    assert!(history.lower_revision(&later, false, 2).is_none());
    assert_eq!(
        history
            .lower_revision(&absolute, false, 2)
            .unwrap()
            .revision,
        1
    );
    history.insert_title(absolute_title, &meta);
    assert_eq!(history.items.len(), 1);
    assert!(history.contains(&earlier, false, 2));
    history.insert_title(later_title, &meta);
    assert_eq!(history.items.len(), 2);
    assert!(history.contains(&later, false, 1));
    assert_eq!(
        history.lower_revision(&earlier, false, 2).unwrap().revision,
        1
    );
}

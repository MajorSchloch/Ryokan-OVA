use super::*;

const NOW: i64 = 1_800_000_000;

fn target(age_seconds: i64) -> Target {
    Target {
        series_id: 1,
        provider_id: 101,
        episode: 6,
        airing_at: NOW - age_seconds,
        folder_name: "Example".into(),
        title: "Example".into(),
    }
}

fn finished(detail: &CheckDetail, reason: &str) -> CheckHistory {
    CheckHistory {
        series_id: 1,
        episode: 6,
        occurred_at: String::new(),
        action: FINISHED.into(),
        reason: reason.into(),
        started_airing_at: None,
        finished_reason: Some(reason.into()),
        finished_detail: Some(serde_json::to_string(detail).unwrap()),
    }
}

#[test]
fn repeats_delay_after_the_first_hold_with_separate_recent_and_older_caps() {
    let reason = "Episode 6: no matching release found";
    for (age, expected) in [
        (120, &[0, 1800, 3600, 3600, 3600, 3600, 3600][..]),
        (
            FRESH_FOR + 1,
            &[0, 1800, 3600, 7200, 14400, 28800, 28800][..],
        ),
    ] {
        let target = target(age);
        let mut last = None;
        for delay in expected {
            let detail = check_detail(&target, "held", reason, last.as_ref(), NOW);
            assert_eq!(detail.retry_at, (*delay > 0).then(|| NOW + delay));
            last = Some(finished(&detail, reason));
        }
    }
}

#[test]
fn near_match_samples_do_not_reset_a_hold_but_new_outcomes_and_airing_times_do() {
    let mut target = target(120);
    let earlier =
        "Episode 6: 2 release(s) looked close but none named the series exactly, for example One";
    let newer =
        "Episode 6: 1 release(s) looked close but none named the series exactly, for example Two";
    let old = finished(&check_detail(&target, "held", earlier, None, NOW), earlier);
    let repeat = check_detail(&target, "held", newer, Some(&old), NOW);
    assert_eq!(repeat.retry_at, Some(NOW + SHORT_DELAY));
    let previous = finished(&repeat, newer);
    assert_eq!(
        check_detail(&target, "held", "Needs review", Some(&previous), NOW).hold_streak,
        1
    );
    assert_eq!(
        check_detail(&target, "queued", "Queued", Some(&previous), NOW).hold_streak,
        0
    );
    target.airing_at += 30;
    assert_eq!(previous.retry_at(target.airing_at), None);
    assert_eq!(
        check_detail(&target, "held", newer, Some(&previous), NOW).hold_streak,
        1
    );
}

#[test]
fn source_identity_refusal_backs_off_without_treating_connectivity_errors_as_holds() {
    let target = target(120);
    let identity = finished(
        &check_detail(&target, "error", IDENTITY_UNAVAILABLE, None, NOW),
        IDENTITY_UNAVAILABLE,
    );
    assert_eq!(
        check_detail(&target, "error", IDENTITY_UNAVAILABLE, Some(&identity), NOW).retry_at,
        Some(NOW + SHORT_DELAY)
    );
    let reason = "Download client is currently unreachable";
    let previous = finished(&check_detail(&target, "error", reason, None, NOW), reason);
    assert_eq!(
        check_detail(&target, "error", reason, Some(&previous), NOW).retry_at,
        None
    );
}

#[test]
fn labelled_transient_holds_do_not_delay_a_fresh_check() {
    let target = target(120);
    for reason in [
        "Episode 6: Episode identity, airing, or monitoring changed",
        "Single: A download, import, or file selection is already pending",
    ] {
        let previous = finished(&check_detail(&target, "held", reason, None, NOW), reason);
        assert_eq!(
            check_detail(&target, "held", reason, Some(&previous), NOW).retry_at,
            None
        );
    }
}

use super::*;

fn timestamp(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value).unwrap().timestamp()
}

#[test]
fn publication_date_actual_old_release_contradicts_the_new_premiere() {
    let bound = PremiereBound::from_date("2026-10-03").unwrap();
    let reason = bound
        .check(Some(timestamp("2019-11-06T18:34:30Z")))
        .unwrap_err();
    assert!(
        reason.contains("predates canonical premiere 2026-10-03"),
        "{reason}"
    );
    assert!(reason.contains("2019-11-06"), "{reason}");
}

#[test]
fn publication_date_uses_the_earliest_global_utc_boundary_for_date_precision() {
    for (premiere, boundary) in [
        ("2026-10-03", "2026-10-02T10:00:00Z"),
        ("2026-10", "2026-09-30T10:00:00Z"),
        ("2026", "2025-12-31T10:00:00Z"),
    ] {
        let bound = PremiereBound::from_date(premiere).unwrap();
        let boundary = timestamp(boundary);
        assert!(bound.check(Some(boundary - 1)).is_err(), "{premiere}");
        assert!(bound.check(Some(boundary)).is_ok(), "{premiere}");
        assert!(bound.check(Some(boundary + 1)).is_ok(), "{premiere}");
    }
}

#[test]
fn publication_date_missing_invalid_or_recent_values_make_no_positive_identity_claim() {
    let bound = PremiereBound::from_date("2026-10-03").unwrap();
    for publication in [
        None,
        Some(0),
        Some(-1),
        Some(i64::MAX),
        Some(timestamp("2026-11-07T00:00:00Z")),
    ] {
        assert!(bound.check(publication).is_ok());
    }
    for invalid in [
        "",
        "2026-02-30",
        "2026-13",
        "2026-00-01",
        "2026-10-00",
        "2026-1-03",
        "2026-10-03-extra",
        "year2026",
        "0000",
    ] {
        assert!(PremiereBound::from_date(invalid).is_none(), "{invalid}");
    }
    assert!(PremiereBound::from_year(None).is_none());
    assert!(PremiereBound::from_year(Some(0)).is_none());
    assert!(PremiereBound::from_date("2024-02-29").is_some());
}

#[test]
fn publication_date_detail_prefers_precise_premiere_and_keeps_known_year_fallback() {
    let mut detail: AnimeDetail = serde_json::from_value(serde_json::json!({
        "id":1,"id_mal":null,"release_date":"2026-10-03","title_romaji":"Fixture",
        "title_english":"Fixture","title_native":"","cover_url":"","banner_url":"",
        "format":"TV","status":"RELEASING","status_display":"Airing","episodes":null,
        "season":"","season_year":2026,"description":"","genres":[],"score_is_ten_point":false,
        "score_class":"","synonyms":[],"streaming_episodes":[],"relations":[]
    }))
    .unwrap();
    assert_eq!(
        PremiereBound::from_detail(&detail),
        PremiereBound::from_date("2026-10-03")
    );
    detail.release_date = Some("2026-02-30".into());
    assert_eq!(
        PremiereBound::from_detail(&detail),
        PremiereBound::from_year(Some(2026))
    );
    detail.release_date = None;
    assert_eq!(
        PremiereBound::from_detail(&detail),
        PremiereBound::from_year(Some(2026))
    );
    detail.season_year = None;
    assert!(PremiereBound::from_detail(&detail).is_none());
}

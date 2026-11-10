use super::CalendarPageQuery;
use chrono::{Datelike, Duration, Months, NaiveDate, Utc};

const MIN_YEAR: i32 = 1970;
const MAX_YEAR: i32 = 2100;

pub(super) struct Parameter {
    pub name: String,
    pub value: String,
}

pub(super) struct Navigation {
    pub range: &'static str,
    pub date: NaiveDate,
    pub from: i64,
    pub to: i64,
    pub label: String,
    pub previous: Option<String>,
    pub today: String,
    pub next: Option<String>,
    pub day: String,
    pub week: String,
    pub month: String,
    pub parameters: Vec<Parameter>,
    pub notice: Option<&'static str>,
}

fn valid_date(value: &str) -> Option<NaiveDate> {
    if value.len() != 10 || !value.is_ascii() {
        return None;
    }
    value
        .parse::<NaiveDate>()
        .ok()
        .filter(|date| (MIN_YEAR..=MAX_YEAR).contains(&date.year()))
}

fn in_bounds(date: NaiveDate) -> Option<NaiveDate> {
    (MIN_YEAR..=MAX_YEAR).contains(&date.year()).then_some(date)
}

impl Navigation {
    pub fn new(query: &CalendarPageQuery, now: chrono::DateTime<Utc>) -> Self {
        let range = match query.range.as_deref() {
            Some("day") => "day",
            Some("month") => "month",
            _ => "this_week",
        };
        let parsed = query.date.as_deref().and_then(valid_date);
        let legacy_next = query.date.is_none()
            && matches!(query.range.as_deref(), Some("next_week" | "next-week"));
        let date = parsed
            .unwrap_or_else(|| now.date_naive() + Duration::days(if legacy_next { 7 } else { 0 }));
        let start = date.and_hms_opt(0, 0, 0).unwrap().and_utc();
        let (from, to) = if range == "month" {
            super::month_grid_window(start)
        } else {
            (
                start.timestamp(),
                (start + Duration::days(if range == "day" { 1 } else { 7 })).timestamp(),
            )
        };
        let parameters: Vec<Parameter> = query
            .context
            .iter()
            .filter(|(name, value)| name.len() <= 64 && value.len() <= 1024)
            .take(24)
            .map(|(name, value)| Parameter {
                name: name.clone(),
                value: value.clone(),
            })
            .chain(query.monitored.unwrap_or(false).then(|| Parameter {
                name: "monitored".into(),
                value: "true".into(),
            }))
            .chain((!query.search_text().is_empty()).then(|| Parameter {
                name: "search".into(),
                value: query.search_text(),
            }))
            .collect();
        let url = |range: &str, date: NaiveDate| {
            let mut query = format!("range={range}&date={date}");
            for parameter in &parameters {
                query.push_str(&format!(
                    "&{}={}",
                    urlencoding::encode(&parameter.name),
                    urlencoding::encode(&parameter.value)
                ));
            }
            format!("/calendar?{query}")
        };
        let shift = |forward: bool| {
            let candidate = if range == "month" {
                let first = date.with_day(1).unwrap();
                if forward {
                    first.checked_add_months(Months::new(1))
                } else {
                    first.checked_sub_months(Months::new(1))
                }
            } else {
                date.checked_add_signed(Duration::days(
                    (if range == "day" { 1 } else { 7 }) * if forward { 1 } else { -1 },
                ))
            };
            candidate.and_then(in_bounds).map(|date| url(range, date))
        };
        let label = match range {
            "day" => date.format("%A, %B %-d, %Y").to_string(),
            "month" => date.format("%B %Y").to_string(),
            _ => format!(
                "{} – {}",
                date.format("%b %-d, %Y"),
                (date + Duration::days(6)).format("%b %-d, %Y")
            ),
        };
        Self {
            range,
            date,
            from,
            to,
            label,
            previous: shift(false),
            next: shift(true),
            today: url(range, now.date_naive()),
            day: url("day", date),
            week: url("this_week", date),
            month: url("month", date),
            notice: (query.date.is_some() && parsed.is_none())
                .then_some("Choose a valid date from 1970 to 2100. Showing today."),
            parameters,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 15, 30, 0).unwrap()
    }

    fn query(range: &str, date: &str) -> CalendarPageQuery {
        CalendarPageQuery {
            range: Some(range.into()),
            date: Some(date.into()),
            ..Default::default()
        }
    }

    fn parameters(url: &str) -> std::collections::BTreeMap<String, String> {
        reqwest::Url::parse(&format!("http://localhost{url}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn day_previous_reaches_yesterday_across_a_year_boundary() {
        let nav = Navigation::new(
            &CalendarPageQuery {
                range: Some("day".into()),
                ..Default::default()
            },
            now(),
        );
        assert_eq!(
            parameters(nav.previous.as_ref().unwrap())["date"],
            "2025-12-31"
        );
        assert_eq!(nav.to - nav.from, 86_400);
        assert_eq!(
            chrono::DateTime::from_timestamp(nav.from, 0)
                .unwrap()
                .hour(),
            0
        );
        assert_eq!(parameters(&nav.today)["range"], "day");
    }

    #[test]
    fn leap_day_and_month_navigation_keep_real_calendar_boundaries() {
        let day = Navigation::new(&query("day", "2024-02-29"), now());
        assert_eq!(parameters(day.next.as_ref().unwrap())["date"], "2024-03-01");
        let month = Navigation::new(&query("month", "2024-03-31"), now());
        assert_eq!(
            parameters(month.previous.as_ref().unwrap())["date"],
            "2024-02-01"
        );
        assert_eq!(
            parameters(month.next.as_ref().unwrap())["date"],
            "2024-04-01"
        );
        assert_eq!(month.label, "March 2024");
        assert_eq!(
            chrono::DateTime::from_timestamp(month.from, 0)
                .unwrap()
                .date_naive()
                .to_string(),
            "2024-02-25"
        );
        assert_eq!((month.to - month.from) % (7 * 86_400), 0);
    }

    #[test]
    fn week_crosses_year_boundary_without_changing_its_length() {
        let nav = Navigation::new(&query("this_week", "2025-12-29"), now());
        assert_eq!(nav.to - nav.from, 7 * 86_400);
        assert_eq!(parameters(nav.next.as_ref().unwrap())["date"], "2026-01-05");
        assert!(nav.label.contains("2025") && nav.label.contains("2026"));
    }

    #[test]
    fn legacy_next_week_and_invalid_or_extreme_dates_are_bounded() {
        for range in ["next_week", "next-week"] {
            let nav = Navigation::new(
                &CalendarPageQuery {
                    range: Some(range.into()),
                    ..Default::default()
                },
                now(),
            );
            assert_eq!(nav.date.to_string(), "2026-01-08");
            assert_eq!(nav.range, "this_week");
            assert_eq!(
                Navigation::new(&query(range, "2025-12-31"), now())
                    .date
                    .to_string(),
                "2025-12-31"
            );
        }
        for value in [
            "2026-02-30",
            "1969-12-31",
            "2101-01-01",
            "999999-01-01",
            "２０２６-01-01",
            "not-a-date",
        ] {
            let nav = Navigation::new(&query("day", value), now());
            assert_eq!(nav.date, now().date_naive());
            assert!(nav.notice.is_some(), "{value}");
        }
        assert!(
            Navigation::new(&query("day", "1970-01-01"), now())
                .previous
                .is_none()
        );
        assert!(
            Navigation::new(&query("month", "2100-12-01"), now())
                .next
                .is_none()
        );
    }

    #[test]
    fn every_navigation_link_preserves_filters_timezone_and_discovery_context() {
        let mut q = query("this_week", "2026-01-01");
        q.monitored = Some(true);
        q.search = Some("86 & Frieren".into());
        q.context.insert("timezone".into(), "Europe/Vienna".into());
        q.context.insert("external".into(), "true".into());
        let nav = Navigation::new(&q, now());
        for url in [
            nav.previous.as_ref().unwrap(),
            &nav.today,
            nav.next.as_ref().unwrap(),
            &nav.day,
            &nav.week,
            &nav.month,
        ] {
            let params = parameters(url);
            assert_eq!(params["monitored"], "true");
            assert_eq!(params["search"], "86 & Frieren");
            assert_eq!(params["timezone"], "Europe/Vienna");
            assert_eq!(params["external"], "true");
        }
    }
}

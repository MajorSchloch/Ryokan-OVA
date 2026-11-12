//! Publication dates can contradict an identity, never establish one.
use chrono::{DateTime, NaiveDate, Utc};

use crate::services::anilist::AnimeDetail;

/// A calendar date can start fourteen hours before UTC midnight. Use that
/// earliest possible instant, including for month/year-only provider dates.
const UTC_DATE_GRACE_SECONDS: i64 = 14 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PremiereBound {
    earliest_utc: i64,
    year: i32,
    month: Option<u32>,
    day: Option<u32>,
}

impl PremiereBound {
    pub(crate) fn from_detail(detail: &AnimeDetail) -> Option<Self> {
        detail
            .release_date
            .as_deref()
            .and_then(Self::from_date)
            .or_else(|| Self::from_year(detail.season_year))
    }

    pub(crate) fn from_year(year: Option<i32>) -> Option<Self> {
        Self::new(year?, None, None)
    }

    fn from_date(raw: &str) -> Option<Self> {
        let parts: Vec<_> = raw.trim().split('-').collect();
        if !(1..=3).contains(&parts.len())
            || parts[0].len() != 4
            || parts
                .iter()
                .any(|part| !part.bytes().all(|byte| byte.is_ascii_digit()))
            || parts.iter().skip(1).any(|part| part.len() != 2)
        {
            return None;
        }
        Self::new(
            parts[0].parse().ok()?,
            parts.get(1).map(|part| part.parse()).transpose().ok()?,
            parts.get(2).map(|part| part.parse()).transpose().ok()?,
        )
    }

    fn new(year: i32, month: Option<u32>, day: Option<u32>) -> Option<Self> {
        if !(1..=9999).contains(&year) {
            return None;
        }
        let date = NaiveDate::from_ymd_opt(year, month.unwrap_or(1), day.unwrap_or(1))?;
        Some(Self {
            earliest_utc: date.and_hms_opt(0, 0, 0)?.and_utc().timestamp() - UTC_DATE_GRACE_SECONDS,
            year,
            month,
            day,
        })
    }

    pub(crate) fn check(&self, published_at: Option<i64>) -> Result<(), String> {
        let Some(published) = published_at
            .filter(|timestamp| *timestamp > 0)
            .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        else {
            return Ok(());
        };
        if published.timestamp() < self.earliest_utc {
            let premiere = match (self.month, self.day) {
                (Some(month), Some(day)) => format!("{:04}-{month:02}-{day:02}", self.year),
                (Some(month), None) => format!("{:04}-{month:02}", self.year),
                _ => format!("{:04}", self.year),
            };
            return Err(format!(
                "Episode mapping: release publication {} predates canonical premiere {premiere}; source identity needs review",
                published.format("%Y-%m-%d %H:%M UTC")
            ));
        }
        // A recent publication or an unknown date proves no episode identity.
        Ok(())
    }
}

#[cfg(test)]
#[path = "release_numbering_publication_tests.rs"]
mod tests;

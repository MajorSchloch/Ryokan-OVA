//! Shared HTTP 429 cooldown for each Torznab/Newznab endpoint and API key.
//! A client reconstructed from the same configuration observes the same deadline.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};

const DEFAULT_WAIT_SECONDS: u64 = 60;
pub(crate) const MAX_WAIT_SECONDS: u64 = 24 * 60 * 60;
const RATE_LIMIT: &str = "Indexer rate-limited ";
const DEFERRED: &str = "Indexer rate-limited (cooldown)";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Endpoint(u64);

impl Endpoint {
    pub(super) fn new(url: &str, api_key: &str) -> Self {
        // The keyed hash stays in memory. The shared table does not retain keys or URLs.
        static ROUTE_KEYS: LazyLock<RandomState> = LazyLock::new(RandomState::new);
        Self(ROUTE_KEYS.hash_one((url, api_key)))
    }
}

#[derive(Clone, Copy)]
struct Window {
    until: Instant,
    retry_at: DateTime<Utc>,
}

static WINDOWS: LazyLock<Mutex<HashMap<Endpoint, Window>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn is_rate_limit(error: &str) -> bool {
    error.starts_with(RATE_LIMIT)
}

pub(crate) fn is_deferred(error: &str) -> bool {
    error.starts_with(DEFERRED)
}

/// Retry-After permits delay seconds or an HTTP date. A zero, past, or
/// malformed value uses the default wait to avoid a tight retry loop.
pub(super) fn retry_after_seconds(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    let seconds = value.parse::<u64>().ok().or_else(|| {
        let date = httpdate::parse_http_date(value).ok()?;
        let delay = date.duration_since(now).ok()?;
        Some(
            delay
                .as_secs()
                .saturating_add(u64::from(delay.subsec_nanos() > 0)),
        )
    })?;
    (seconds > 0).then_some(seconds)
}

pub(super) fn deferred(endpoint: Endpoint) -> Option<String> {
    active_until(endpoint, Instant::now()).map(|retry_at| {
        format!(
            "{DEFERRED}. Request deferred. Retry after {}.",
            utc_stamp(retry_at)
        )
    })
}

fn active_until(endpoint: Endpoint, now: Instant) -> Option<DateTime<Utc>> {
    let mut windows = WINDOWS.lock().ok()?;
    let window = *windows.get(&endpoint)?;
    if now < window.until {
        Some(window.retry_at)
    } else {
        windows.remove(&endpoint);
        None
    }
}

pub(super) fn record_429(endpoint: Endpoint, retry_after: Option<u64>) -> String {
    let requested = retry_after
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WAIT_SECONDS);
    let delay = requested.min(MAX_WAIT_SECONDS);
    let now = Instant::now();
    let window = Window {
        until: now + Duration::from_secs(delay),
        retry_at: Utc::now() + chrono::TimeDelta::seconds(delay as i64),
    };
    let retry_at = if let Ok(mut windows) = WINDOWS.lock() {
        windows.retain(|_, existing| existing.until > now);
        let active = windows.entry(endpoint).or_insert(window);
        // Overlapping queries can return 429 out of order. Keep the later deadline.
        if window.until > active.until {
            *active = window;
        }
        active.retry_at
    } else {
        window.retry_at
    };

    let hint = match retry_after {
        Some(seconds) if seconds > MAX_WAIT_SECONDS => {
            format!("Retry-After requested {seconds}s, capped at {MAX_WAIT_SECONDS}s")
        }
        Some(seconds) if seconds > 0 => format!("Retry-After requested {seconds}s"),
        _ => format!("No usable Retry-After, default wait {DEFAULT_WAIT_SECONDS}s"),
    };
    format!(
        "{RATE_LIMIT}(429). Retry after {}. {hint}.",
        utc_stamp(retry_at)
    )
}

fn utc_stamp(retry_at: DateTime<Utc>) -> String {
    retry_at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn retry_after_accepts_delay_and_http_date() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let date = httpdate::fmt_http_date(now + Duration::from_secs(71_018));
        assert_eq!(retry_after_seconds(&date, now), Some(71_018));
        assert_eq!(retry_after_seconds("71018", now), Some(71_018));
        assert_eq!(retry_after_seconds("0", now), None);
        assert_eq!(retry_after_seconds("malformed", now), None);
        assert_eq!(
            retry_after_seconds(&date, now + Duration::from_secs(71_019)),
            None
        );
    }

    #[test]
    fn honors_multi_hour_retry_and_keeps_the_later_deadline() {
        let route = Endpoint::new("https://long.example.invalid/api", "key");
        let before = Instant::now();
        let first = record_429(route, Some(71_018));
        assert!(first.contains("71018s"));
        assert!(deferred(Endpoint::new("https://other.example.invalid/api", "key")).is_none());
        let deadline = active_until(route, before + Duration::from_secs(300)).unwrap();
        let smaller = record_429(route, Some(60));
        assert!(smaller.contains(&utc_stamp(deadline)), "{smaller}");
        assert!(active_until(route, before + Duration::from_secs(71_017)).is_some());
        assert!(active_until(route, Instant::now() + Duration::from_secs(71_018)).is_none());
    }

    #[test]
    fn bounds_long_values_and_uses_a_default_for_missing_dates() {
        let long = Endpoint::new("https://capped.example.invalid/api", "");
        let before = Instant::now();
        assert!(record_429(long, Some(u64::MAX)).contains("capped at 86400s"));
        assert!(active_until(long, before + Duration::from_secs(MAX_WAIT_SECONDS - 1)).is_some());
        assert!(
            active_until(long, Instant::now() + Duration::from_secs(MAX_WAIT_SECONDS)).is_none()
        );
        let default = Endpoint::new("https://default.example.invalid/api", "");
        assert!(record_429(default, None).contains("default wait 60s"));
        assert!(deferred(default).is_some());
        assert!(active_until(default, Instant::now() + Duration::from_secs(60)).is_none());
    }
}

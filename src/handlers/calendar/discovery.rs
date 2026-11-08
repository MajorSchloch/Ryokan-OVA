//! Calendar premiere list. All reads are local, including library membership.
use crate::{
    models::config,
    services::{anilist::discovery::Premiere, calendar::discovery, library_link::pick_title},
};
use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Html,
};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Filters {
    format: String,
    hide_tracked: bool,
}

struct Card {
    id: i64,
    title: String,
    format: String,
    cover: String,
    date: String,
    library_id: Option<i64>,
    add_json: String,
}

struct Day {
    date: NaiveDate,
    cards: Vec<Card>,
}

#[derive(Template)]
#[template(path = "calendar_discovery.html")]
struct DiscoveryPage {
    page: &'static str,
    title_language: String,
    days: Vec<Day>,
    announced: Vec<Card>,
    formats: BTreeSet<String>,
    filters: Filters,
    refreshed: Option<String>,
    failed: bool,
}

fn announced_date(item: &Premiere) -> String {
    let date = item.date.as_deref().unwrap_or_default();
    if date.len() == 7 {
        // No day is known. The YYYY-MM value is itself a complete label.
        return date.to_owned();
    }
    let year = date.parse::<i32>().ok().or(item.entry.season_year);
    let season = match item.season.as_str() {
        "WINTER" => "Winter",
        "SPRING" => "Spring",
        "SUMMER" => "Summer",
        "FALL" => "Fall",
        _ => "",
    };
    match year {
        Some(y) if !season.is_empty() && item.entry.season_year.is_none_or(|sy| sy == y) => {
            format!("{season} {y}")
        }
        Some(y) => y.to_string(),
        None => "Date TBA".into(),
    }
}

async fn load(db: &SqlitePool, filters: Filters) -> Result<DiscoveryPage, String> {
    let snapshot = discovery::read(db).await?;
    let lang = config::get_naming_prefs(db).await.title_language;
    let library = sqlx::query_as::<_, (i64, i64, Option<i64>)>(
        "SELECT id, anilist_id, mal_id FROM series ORDER BY id",
    )
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    let mut tracked = HashMap::new();
    let mut legacy = HashMap::new();
    let mut parts = HashMap::<i64, HashSet<i64>>::new();
    for (id, al, mal) in library {
        if al > 0 {
            tracked.insert(al, id);
            if let Some(mal) = mal {
                parts.entry(mal).or_default().insert(al);
            }
        } else if let Some(mal) = mal {
            legacy.entry(mal).or_insert(id);
        }
    }
    // A legacy negative-ID row can stand for a single known AniList entry.
    // A shared MAL ID cannot decide which of two parts the user already owns.
    for item in &snapshot.entries {
        if let Some(mal) = item.entry.id_mal {
            parts.entry(mal).or_default().insert(item.entry.id);
        }
    }
    let today = Utc::now().date_naive();
    let mut dated = BTreeMap::<NaiveDate, Vec<Card>>::new();
    let mut announced = Vec::new();
    let mut formats = BTreeSet::new();
    for item in snapshot.entries {
        let entry = &item.entry;
        let exact = item.date.as_ref().and_then(|s| s.parse::<NaiveDate>().ok());
        if exact.is_some_and(|d| d < today) {
            continue;
        }
        if !entry.format.is_empty() {
            formats.insert(entry.format.clone());
        }
        let id = tracked.get(&entry.id).copied().or_else(|| {
            let mal = entry.id_mal?;
            if parts.get(&mal)?.len() == 1 {
                legacy.get(&mal).copied()
            } else {
                None
            }
        });
        if (filters.hide_tracked && id.is_some())
            || (!filters.format.is_empty() && filters.format != entry.format)
        {
            continue;
        }
        let card = Card {
            title: pick_title(
                &lang,
                &entry.title_english,
                &entry.title_romaji,
                &entry.title_native,
            )
            .to_string(),
            id: entry.id,
            format: entry.format.replace('_', " "),
            cover: entry.cover_url.clone(),
            date: if exact.is_none() {
                announced_date(&item)
            } else {
                String::new()
            },
            library_id: id,
            add_json: serde_json::to_string(entry).map_err(|e| e.to_string())?,
        };
        match exact {
            Some(date) => dated.entry(date).or_default().push(card),
            None => announced.push(card),
        }
    }
    let days = dated
        .into_iter()
        .map(|(date, mut cards)| {
            cards.sort_by_cached_key(|c| c.title.to_lowercase());
            Day { date, cards }
        })
        .collect();
    announced.sort_by_cached_key(|c| c.title.to_lowercase());
    Ok(DiscoveryPage {
        page: "calendar",
        title_language: lang,
        days,
        announced,
        formats,
        filters,
        refreshed: snapshot
            .refreshed_at
            .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()),
        failed: snapshot.failed,
    })
}

pub async fn page(
    State(db): State<SqlitePool>,
    Query(filters): Query<Filters>,
) -> Result<Html<String>, StatusCode> {
    let view = load(&db, filters).await.map_err(|err| {
        tracing::error!(%err, "Cannot load upcoming anime");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    view.render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

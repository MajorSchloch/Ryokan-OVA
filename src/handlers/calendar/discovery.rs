//! Calendar premiere list. All reads are local, including library membership.
use crate::{
    models::{config, series},
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
use std::collections::{BTreeMap, BTreeSet};

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
    let tracked = series::library_ids_by_anilist(db)
        .await
        .map_err(|e| e.to_string())?;
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
        let id = tracked.get(&entry.id).copied();
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

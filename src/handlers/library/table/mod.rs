//! One read-only row per Ryokan entry, plus follows outside the library.
use super::inventory;
use crate::{
    models::{config, release_follows, series},
    services::{anilist::plausible_episode_count, library_link::pick_title},
};
use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use sqlx::SqlitePool;
use std::collections::HashMap;

#[derive(Template)]
#[template(path = "library_table.html")]
struct Table {
    page: &'static str,
    title_language: String,
    rows: Vec<Entry>,
}

#[derive(Default)]
struct Entry {
    title: String,
    aliases: String,
    href: String,
    format: String,
    status: String,
    mode: &'static str,
    followed: bool,
    follow_only: bool,
    disk: Option<usize>,
    missing: Option<usize>,
    total: Option<i32>,
    next: Option<String>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct FollowDetail {
    format: String,
    status: String,
    episodes: Option<i64>,
    next_airing_at: Option<i64>,
    next_airing_episode: Option<i64>,
}

fn airing_label(at: i64, ep: i32) -> Option<String> {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|date| format!("{} UTC · Ep {ep}", date.format("%Y-%m-%d %H:%M")))
}

fn status_label(raw: &str) -> String {
    match raw {
        "RELEASING" | "CURRENTLY_AIRING" => "Airing".into(),
        "FINISHED" | "FINISHED_AIRING" => "Finished".into(),
        "NOT_YET_RELEASED" | "NOT_YET_AIRED" => "Upcoming".into(),
        "CANCELLED" => "Cancelled".into(),
        "HIATUS" => "On hiatus".into(),
        other => other.replace('_', " "),
    }
}

async fn load(db: &SqlitePool) -> Result<Table, sqlx::Error> {
    let (cfg, library, follows) = tokio::try_join!(
        config::get_config(db),
        series::get_all(db),
        release_follows::list(db)
    )?;
    let cfg = cfg.unwrap_or_default();
    let inventory = inventory::load(db, &cfg.media_root, &library).await?;
    let mut follows: HashMap<_, _> = follows.into_iter().map(|f| (f.anilist_id, f)).collect();
    let mapped: HashMap<_, _> = sqlx::query_as::<_, (i64, i64)>(sqlx::AssertSqlSafe(format!(
        "{} SELECT l.series_id, l.anilist_id FROM local_anilist_ids l
            JOIN release_follows f ON f.anilist_id = l.anilist_id",
        series::LOCAL_ANILIST_IDS
    )))
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    let mut rows = Vec::new();
    for s in library {
        let facts = &inventory[&s.id];
        let preferred = pick_title(
            &cfg.title_language,
            &s.title_english,
            &s.title_romaji,
            &s.title_native,
        );
        let followed = mapped
            .get(&s.id)
            .is_some_and(|id| follows.remove(id).is_some());
        rows.push(Entry {
            title: if preferred.is_empty() {
                s.title.clone()
            } else {
                preferred.into()
            },
            aliases: [
                &s.title,
                &s.title_english,
                &s.title_romaji,
                &s.title_native,
                &s.alternate_titles,
            ]
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" "),
            href: format!("/series/{}", s.id),
            mode: s.monitor_mode_enum().as_str(),
            format: s.format.replace('_', " "),
            status: status_label(&s.status),
            followed,
            follow_only: false,
            disk: facts.on_disk,
            missing: facts.missing.as_ref().map(Vec::len),
            total: s.episodes.and_then(|n| plausible_episode_count(n.into())),
            next: facts.next.and_then(|(at, ep)| airing_label(at, ep)),
        });
    }
    // Deleted titles can still have a provider detail in the local cache.
    let cached: HashMap<_, _> = sqlx::query_as::<_, (i64, String)>(
        "SELECT p.provider_id,p.detail_json FROM release_follows f
         JOIN provider_metadata_cache p ON p.provider_id=f.anilist_id",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    let now = chrono::Utc::now().timestamp();
    for f in follows.into_values() {
        let detail = cached
            .get(&f.anilist_id)
            .and_then(|raw| serde_json::from_str::<FollowDetail>(raw).ok())
            .unwrap_or_default();
        rows.push(Entry {
            href: format!(
                "/library/follows?anilist_id={}&title={}",
                f.anilist_id,
                urlencoding::encode(&f.title)
            ),
            title: f.title,
            format: detail.format.replace('_', " "),
            status: status_label(&detail.status),
            total: detail.episodes.and_then(plausible_episode_count),
            next: detail
                .next_airing_at
                .filter(|at| *at > now)
                .zip(detail.next_airing_episode.and_then(plausible_episode_count))
                .and_then(|(at, ep)| airing_label(at, ep)),
            followed: true,
            follow_only: true,
            disk: Some(0),
            ..Entry::default()
        });
    }
    rows.sort_by_cached_key(|r| r.title.to_lowercase());
    Ok(Table {
        page: "library",
        title_language: cfg.title_language,
        rows,
    })
}

pub async fn page(State(db): State<SqlitePool>) -> Result<Response, StatusCode> {
    let table = load(&db).await.map_err(|err| {
        tracing::error!(%err, "Could not read the library table");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let html = table
        .render()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((super::view_cookie("table"), Html(html)).into_response())
}

#[cfg(test)]
mod tests;

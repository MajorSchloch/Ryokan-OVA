//! A bounded catalogue of announced anime. Querying Media instead of
//! airingSchedules gives one premiere per entry, including undated titles.
use super::{
    AnimeEntry, HTTP_CLIENT, anilist_cooldown_active, anilist_post, classify_anilist_failure,
    cooldown_from_headers, extract_graphql_error, parse_search_entry, premiere_date,
    rate_limit::{ANILIST_COOLDOWN_DEFAULT, AniListFailureKind},
    record_rate_limit_headers, set_cooldown_until_now_plus, throttle_before_anilist_request,
};
use crate::services::http_body::CappedBody;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct Premiere {
    pub entry: AnimeEntry,
    pub date: Option<String>,
    pub season: String,
}

const PAGES: usize = 4;
const PAGE_SIZE: usize = 50;
const QUERY: &str = "query UpcomingAnime($page:Int!, $perPage:Int!) {
    Page(page:$page,perPage:$perPage) {
        pageInfo { hasNextPage }
        media(type:ANIME,status:NOT_YET_RELEASED,sort:[POPULARITY_DESC,ID]) {
            id idMal title { english romaji native } format status episodes coverImage { large }
            startDate { year month day } season seasonYear
        }
    }
}";

pub async fn fetch() -> Result<Vec<Premiere>, String> {
    let mut entries = BTreeMap::new();
    for page in 1..=PAGES {
        if anilist_cooldown_active() {
            return Err("AniList rate-limit cooldown active".into());
        }
        throttle_before_anilist_request().await;
        let response = anilist_post(&HTTP_CLIENT)
            .json(&json!({
                "query": QUERY, "variables":{"page":page,"perPage":PAGE_SIZE}
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let headers = response.headers().clone();
        record_rate_limit_headers(&headers);
        let text = response.text_capped().await.map_err(|e| e.to_string())?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() || extract_graphql_error(&body).is_some() {
            let (kind, message) = classify_anilist_failure(status, &text);
            if kind == AniListFailureKind::RateLimited || status.is_server_error() {
                set_cooldown_until_now_plus(cooldown_from_headers(
                    &headers,
                    ANILIST_COOLDOWN_DEFAULT,
                ));
            }
            return Err(message);
        }
        let page = &body["data"]["Page"];
        let more = page["pageInfo"]["hasNextPage"]
            .as_bool()
            .ok_or("Missing AniList page information")?;
        let media = page["media"]
            .as_array()
            .ok_or("Missing AniList media list")?;
        for node in media.iter().take(PAGE_SIZE) {
            let entry = parse_search_entry(node).ok_or("Missing AniList entry ID")?;
            entries.insert(
                entry.id,
                Premiere {
                    entry,
                    date: premiere_date(&node["startDate"]),
                    season: node["season"].as_str().unwrap_or_default().into(),
                },
            );
        }
        if !more {
            break;
        }
    }
    Ok(entries.into_values().collect())
}

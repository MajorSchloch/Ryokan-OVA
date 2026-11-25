//! The background job owns the cache. Page requests only read it.
use crate::{
    models::{config, metadata_cache, series::parse_alternate_titles},
    services::{
        anilist::{
            RelatedEntry, detail_rating,
            discovery::{self, Premiere},
            valid_rating,
        },
        library_link::pick_title,
    },
};
use sqlx::SqlitePool;
use std::{collections::HashMap, time::Duration};

const PERIOD: Duration = Duration::from_secs(12 * 3600);
const RETRY: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub(crate) struct Snapshot {
    pub entries: Vec<Premiere>,
    pub refreshed_at: Option<i64>,
    pub failed: bool,
}

pub(crate) async fn read(db: &SqlitePool) -> Result<Snapshot, String> {
    let row = sqlx::query_as::<_, (Option<String>, Option<i64>, bool)>(
        "SELECT entries,refreshed_at,failed FROM premiere_discovery WHERE id=1",
    )
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    let Some((entries, refreshed_at, failed)) = row else {
        return Ok(Snapshot::default());
    };
    Ok(Snapshot {
        entries: entries
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default(),
        refreshed_at,
        failed,
    })
}

#[derive(Default)]
pub(crate) struct PageContext {
    pub rating: Option<i32>,
    pub aliases: Vec<String>,
    pub franchise: Option<FranchiseLink>,
    has_exact_detail: bool,
}

#[derive(Clone)]
pub(crate) struct FranchiseLink {
    pub title: String,
    pub href: String,
    pub label: &'static str,
}

fn reference_link(id: i64, relations: &[RelatedEntry], lang: &str) -> Option<FranchiseLink> {
    let rank = |relation: &RelatedEntry| match relation.relation_type.as_str() {
        "PARENT" | "PARENT_STORY" => 0,
        "FULL_STORY" => 1,
        "PREQUEL" => 2,
        _ => 3,
    };
    let mut relations = relations
        .iter()
        .filter(|rel| rel.id > 0 && rel.id != id && rel.media_type == "ANIME" && rank(rel) < 3);
    let best_rank = relations.clone().map(rank).min()?;
    let best = relations.find(|relation| rank(relation) == best_rank)?;
    if relations.any(|relation| rank(relation) == best_rank && relation.id != best.id) {
        return None;
    }
    let title = pick_title(
        lang,
        &best.title_english,
        &best.title_romaji,
        &best.title_native,
    );
    let label = match best_rank {
        0 => "Part of",
        1 => "Summary of",
        _ => "Sequel to",
    };
    (!title.trim().is_empty()).then(|| FranchiseLink {
        title: title.to_owned(),
        href: format!("/discover?franchise_id={id}"),
        label,
    })
}

async fn contexts(
    db: &SqlitePool,
    ids: &[i64],
    lang: &str,
) -> Result<HashMap<i64, PageContext>, sqlx::Error> {
    let mut data = HashMap::new();
    if ids.is_empty() {
        return Ok(data);
    }
    for (id, detail) in metadata_cache::exact_cached_details(db, ids).await? {
        let rating = detail_rating(&detail);
        let franchise = reference_link(id, &detail.relations, lang);
        let aliases = [
            detail.title_english,
            detail.title_romaji,
            detail.title_native,
        ]
        .into_iter()
        .chain(detail.synonyms)
        .filter(|name| !name.is_empty())
        .collect();
        data.insert(
            id,
            PageContext {
                rating,
                aliases,
                franchise,
                has_exact_detail: true,
            },
        );
    }
    let requested = serde_json::json!(ids).to_string();
    let manual_aliases: Vec<(i64, String)> = sqlx::query_as(
        "SELECT s.anilist_id,s.alternate_titles FROM json_each(?) i
         JOIN series s ON s.anilist_id=i.value WHERE s.alternate_titles<>''",
    )
    .bind(&requested)
    .fetch_all(db)
    .await?;
    for (id, raw) in manual_aliases {
        data.entry(id)
            .or_default()
            .aliases
            .extend(parse_alternate_titles(&raw));
    }
    let franchises: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT m.provider_id, f.folder, f.page_id
         FROM json_each(?) AS ids JOIN library_franchise_members m ON m.provider_id=ids.value
         JOIN library_franchises f ON f.folder=m.folder WHERE f.page_id IS NOT NULL AND f.page_id<>''",
    )
    .bind(&requested)
    .fetch_all(db)
    .await?;
    for (id, title, page_id) in franchises {
        data.entry(id).or_default().franchise = Some(FranchiseLink {
            title,
            href: format!("/franchises/{page_id}"),
            label: "Franchise",
        });
    }
    Ok(data)
}

pub(crate) async fn page_context(
    db: &SqlitePool,
    ids: &[i64],
) -> Result<HashMap<i64, PageContext>, sqlx::Error> {
    let lang = config::get_naming_prefs(db).await.title_language;
    contexts(db, ids, &lang).await
}

pub(crate) async fn premiere_contexts(
    db: &SqlitePool,
    items: &[Premiere],
) -> Result<HashMap<i64, PageContext>, sqlx::Error> {
    let lang = config::get_naming_prefs(db).await.title_language;
    let ids: Vec<_> = items.iter().map(|item| item.entry.id).collect();
    let mut data = contexts(db, &ids, &lang).await?;
    for item in items {
        let id = item.entry.id;
        let context = data.entry(id).or_default();
        context.rating = context.rating.or(valid_rating(item.entry.average_score));
        if context.franchise.is_none() && !context.has_exact_detail {
            context.franchise = item
                .relations
                .as_deref()
                .and_then(|relations| reference_link(id, relations, &lang));
        }
    }
    Ok(data)
}

pub async fn refresh(db: &SqlitePool) -> Result<(), String> {
    match discovery::fetch().await {
        Ok(items) => {
            let json = serde_json::to_string(&items).map_err(|e| e.to_string())?;
            let mut tx = db
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(|e| e.to_string())?;
            // Save identities from the outgoing snapshot, including snapshots
            // published before this table existed. Older tabs can still post them.
            sqlx::query("INSERT INTO discovery_media_ids(anilist_id, mal_id)
                SELECT item.value ->> '$.entry.id', item.value ->> '$.entry.id_mal'
                FROM premiere_discovery, json_each(CASE WHEN json_valid(entries) THEN entries END) item
                WHERE item.type='object' AND item.value ->> '$.entry.id'>0 AND item.value ->> '$.entry.id_mal'>0
                ON CONFLICT(anilist_id) DO UPDATE SET mal_id=excluded.mal_id")
                .execute(&mut *tx).await.map_err(|e| e.to_string())?;
            sqlx::query("INSERT INTO premiere_discovery(id,entries,refreshed_at,failed) VALUES(1,?,unixepoch(),0)
                ON CONFLICT(id) DO UPDATE SET entries=excluded.entries,refreshed_at=excluded.refreshed_at,failed=0")
                .bind(json).execute(&mut *tx).await.map_err(|e| e.to_string())?;
            tx.commit().await.map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(err) => {
            sqlx::query(
                "INSERT INTO premiere_discovery(id,failed) VALUES(1,1)
                ON CONFLICT(id) DO UPDATE SET failed=1",
            )
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
            Err(err)
        }
    }
}

pub async fn run(db: &SqlitePool) {
    // Keep a fresh snapshot across restarts without spending more API calls.
    if let Ok(snapshot) = read(db).await
        && !snapshot.failed
        && snapshot.entries.iter().all(|item| item.relations.is_some())
        && let Some(at) = snapshot.refreshed_at
    {
        let age = chrono::Utc::now().timestamp().saturating_sub(at).max(0) as u64;
        tokio::time::sleep(PERIOD.saturating_sub(Duration::from_secs(age))).await;
    }
    loop {
        let delay = match refresh(db).await {
            Ok(()) => PERIOD,
            Err(err) => {
                tracing::warn!(%err, "Upcoming anime refresh failed");
                RETRY
            }
        };
        tokio::time::sleep(delay).await;
    }
}

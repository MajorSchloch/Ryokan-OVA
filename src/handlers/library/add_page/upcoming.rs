//! A bounded selection of premieres from Calendar's saved snapshot.
use std::collections::HashSet;

use chrono::{NaiveDate, Utc};

use crate::{
    AppState,
    models::series,
    services::{
        anilist::{AnimeEntry, AnimeSearchHit, discovery::Premiere},
        calendar::discovery,
    },
};

use super::{AddPage, ResultRow, title};

const POSTER_LIMIT: usize = 8;

pub(super) async fn saved_hit(db: &sqlx::SqlitePool, id: i64) -> Option<AnimeSearchHit> {
    let premiere = discovery::read(db)
        .await
        .ok()?
        .entries
        .into_iter()
        .find(|premiere| id > 0 && premiere.entry.id == id)?;
    Some(AnimeSearchHit {
        entry: premiere.entry,
        release_date: premiere.date,
        relations: Vec::new(),
    })
}

fn nearest_premieres(entries: Vec<Premiere>, today: NaiveDate) -> Vec<(NaiveDate, AnimeEntry)> {
    let mut dated: Vec<_> = entries
        .into_iter()
        .filter_map(|premiere| {
            let date = premiere.date?.parse::<NaiveDate>().ok()?;
            (premiere.entry.id > 0 && date >= today).then_some((date, premiere.entry))
        })
        .collect();
    dated.sort_by_key(|(date, entry)| (*date, entry.id));
    let mut seen = HashSet::new();
    dated
        .into_iter()
        .filter(|(_, entry)| seen.insert(entry.id))
        .take(POSTER_LIMIT)
        .collect()
}

pub(super) async fn load(state: &AppState, view: &AddPage) -> Vec<ResultRow> {
    let snapshot = match discovery::read(&state.db).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(%error, "Could not read upcoming anime for Discover");
            return Vec::new();
        }
    };
    let dated = nearest_premieres(snapshot.entries, Utc::now().date_naive());
    if dated.is_empty() {
        return Vec::new();
    }
    let tracked = match series::library_ids_by_anilist(&state.db).await {
        Ok(tracked) => tracked,
        Err(error) => {
            tracing::warn!(%error, "Could not check upcoming anime library status");
            return Vec::new();
        }
    };
    dated
        .into_iter()
        .map(|(date, entry)| {
            let title = title(
                &view.title_language,
                &entry.title_english,
                &entry.title_romaji,
                &entry.title_native,
            );
            let subtitle = [
                &entry.title_english,
                &entry.title_romaji,
                &entry.title_native,
            ]
            .into_iter()
            .find(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&title))
            .cloned()
            .unwrap_or_default();
            let date = date.format("%b %-d, %Y");
            let facts = if entry.format.is_empty() {
                date.to_string()
            } else {
                format!("{} · {date}", entry.format.replace('_', " "))
            };
            ResultRow {
                title,
                subtitle,
                cover: entry.cover_url,
                facts,
                href: view.franchise_path(entry.id),
                existing: tracked.contains_key(&entry.id),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Days;

    use super::*;
    use crate::{
        models::config,
        test_support::{build_test_app_state, in_memory_pool, seed_series},
    };

    fn premiere(id: i64, date: Option<String>) -> Premiere {
        Premiere {
            entry: AnimeEntry {
                id,
                id_mal: Some(800),
                title_romaji: format!("Romaji {id}"),
                title_english: format!("Anime {id}"),
                title_native: String::new(),
                cover_url: String::new(),
                format: "TV_SHORT".into(),
                status: "NOT_YET_RELEASED".into(),
                status_display: "Not yet released".into(),
                episodes: None,
                season_year: None,
                source: "anilist".into(),
                average_score: None,
            },
            date,
            season: String::new(),
        }
    }

    #[test]
    fn selects_eight_unique_dated_releases_in_order() {
        let today = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        let mut entries: Vec<_> = (1..=10)
            .rev()
            .map(|id| premiere(id, Some((today + Days::new(id as u64)).to_string())))
            .collect();
        entries.extend([
            premiere(99, Some(today.to_string())),
            premiere(0, Some(today.to_string())),
            premiere(101, Some((today - Days::new(1)).to_string())),
            premiere(102, Some("2030-01".into())),
            premiere(103, None),
            premiere(2, Some((today + Days::new(4)).to_string())),
        ]);

        let dated = nearest_premieres(entries, today);
        assert_eq!(
            dated.iter().map(|(_, entry)| entry.id).collect::<Vec<_>>(),
            [99, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(dated[2].0, today + Days::new(2));
    }

    #[tokio::test]
    async fn loads_saved_posters_with_exact_franchise_links_after_refresh_failure() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let cfg = config::Config {
            title_language: "english".into(),
            ..config::Config::default()
        };
        let view = AddPage::new(&cfg, String::new(), "al", None);
        assert!(load(&state, &view).await.is_empty());

        let date = Utc::now().date_naive() + Days::new(1);
        seed_series(&db, 101, "Anime 101").await;
        sqlx::query("INSERT INTO premiere_discovery(id,entries,refreshed_at,failed) VALUES(1,?,unixepoch(),1)")
            .bind(serde_json::to_string(&[
                premiere(101, Some(date.to_string())),
                premiere(102, Some(date.to_string())),
            ]).unwrap())
            .execute(&db)
            .await
            .unwrap();

        let posters = load(&state, &view).await;
        assert_eq!(posters.len(), 2);
        assert!(posters[0].existing && !posters[1].existing);
        assert_eq!(posters[0].href, view.franchise_path(101));
        assert_eq!(posters[1].href, view.franchise_path(102));
        assert_eq!(posters[0].title, "Anime 101");
        assert_eq!(posters[0].subtitle, "Romaji 101");
        assert_eq!(
            posters[0].facts,
            format!("TV SHORT · {}", date.format("%b %-d, %Y"))
        );
    }
}

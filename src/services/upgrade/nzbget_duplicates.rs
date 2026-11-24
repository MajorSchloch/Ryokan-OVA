use std::collections::{HashMap, HashSet};

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::grabbed_torrents::{NZBGET_DUPLICATE_REASON, source_replay};
use crate::models::log::LogCategory;
use crate::services::download_client::nzbget;
use crate::services::{logger, nyaa::SearchResult};

#[derive(sqlx::FromRow)]
struct Source {
    series_id: i64,
    episode_numbers: String,
    indexer_id: i64,
    client_id: i64,
    indexer_kind: String,
    indexer_url: String,
    source_identity: Option<String>,
    source_url: String,
}

#[derive(Default)]
pub(super) struct Duplicates {
    by_episode: HashMap<(i64, i32), Vec<Source>>,
}

impl Duplicates {
    pub(super) async fn load(state: &AppState) -> Result<Self, String> {
        if let Err(error) = recover_old_decisions(state).await {
            tracing::warn!(%error, "could not record past NZBGet duplicate decisions");
        }

        let sources = sqlx::query_as::<_, Source>(
            "SELECT g.series_id, g.episode_numbers, g.indexer_id, g.download_client_id AS client_id,
                    i.kind AS indexer_kind, i.url AS indexer_url, g.source_identity,
                    COALESCE(g.source_url,'') AS source_url
             FROM grabbed_torrents g
             JOIN indexers i ON i.id=g.indexer_id AND i.enabled=1
             JOIN download_clients dc ON dc.id=g.download_client_id AND dc.enabled=1
             WHERE g.state='removed' AND g.failure_reason=? AND g.imported_at IS NULL
               AND dc.kind='nzbget' AND unixepoch(g.grabbed_at)>=dc.updated_at
               AND g.is_batch=0 AND g.library_migration_bridge IS NULL
               AND NOT EXISTS (SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id)",
        )
        .bind(NZBGET_DUPLICATE_REASON)
        .fetch_all(&state.db)
        .await
        .map_err(|error| error.to_string())?;

        let mut indexer_routes = HashMap::new();
        let mut out = Self::default();
        for source in sources {
            let Ok([episode]) = serde_json::from_str::<[i32; 1]>(&source.episode_numbers) else {
                continue;
            };
            let route = if let Some(route) = indexer_routes.get(&source.indexer_id) {
                *route
            } else {
                let route = state
                    .client_for_indexer_with_id(Some(source.indexer_id))
                    .await
                    .filter(|(client, _)| client.sonarr_impl_name() == "Nzbget")
                    .map(|(_, id)| id);
                indexer_routes.insert(source.indexer_id, route);
                route
            };
            if route == Some(source.client_id) {
                out.by_episode
                    .entry((source.series_id, episode))
                    .or_default()
                    .push(source);
            }
        }
        Ok(out)
    }

    pub(super) fn contains(&self, series_id: i64, episode: i32, result: &SearchResult) -> bool {
        self.by_episode
            .get(&(series_id, episode))
            .is_some_and(|sources| sources.iter().any(|source| source.matches(result)))
    }
}

impl Source {
    fn matches(&self, result: &SearchResult) -> bool {
        if result.indexer_id != Some(self.indexer_id) {
            return false;
        }

        if let Some(identity) = &self.source_identity
            && !result.source_guid.trim().is_empty()
        {
            return source_replay::indexer_identity(
                self.indexer_id,
                &self.indexer_kind,
                &self.indexer_url,
                &result.source_guid,
            )
            .is_ok_and(|key| key.as_ref() == Some(identity));
        }

        let url = if result.magnet.is_empty() {
            &result.torrent
        } else {
            &result.magnet
        };
        !url.is_empty() && url == &self.source_url
    }
}

// Older versions saved COPY discards as generic removals. Recover their
// reason only when the original client still reports the exact job key.
// At most one scoped read per affected client in this upgrade pass.
async fn recover_old_decisions(state: &AppState) -> Result<(), sqlx::Error> {
    let removed: Vec<(i64, String)> = sqlx::query_as(
        "SELECT DISTINCT g.download_client_id, g.hash
         FROM grabbed_torrents g JOIN download_clients dc ON dc.id=g.download_client_id
         JOIN series s ON s.id=g.series_id AND s.allow_upgrades=1
         WHERE dc.enabled=1 AND dc.kind='nzbget' AND g.state='removed'
           AND g.failure_reason='' AND g.imported_at IS NULL AND g.indexer_id IS NOT NULL
           AND unixepoch(g.grabbed_at)>=dc.updated_at",
    )
    .fetch_all(&state.db)
    .await?;
    let mut owners: HashMap<i64, HashSet<String>> = HashMap::new();
    for (client_id, key) in removed {
        owners.entry(client_id).or_default().insert(key);
    }

    for (client_id, keys) in owners {
        let Some(client) = state.client_by_id(client_id).await else {
            continue;
        };
        let Ok(items) = client.list_scoped().await else {
            logger::warn(
                &state.db,
                LogCategory::DownloadClient,
                "NZBGet duplicate history unavailable",
                &format!("client_id={client_id}"),
            )
            .await;
            continue;
        };

        for item in items.iter().filter(|item| {
            keys.contains(&item.hash) && nzbget::is_duplicate_copy(client.as_ref(), client_id, item)
        }) {
            remember_old_duplicate(&state.db, client_id, &item.hash).await?;
        }
    }
    Ok(())
}

async fn remember_old_duplicate(
    db: &SqlitePool,
    client_id: i64,
    job_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE grabbed_torrents SET failure_reason=?
         WHERE download_client_id=? AND hash=? AND state='removed' AND failure_reason=''
           AND imported_at IS NULL AND indexer_id IS NOT NULL
           AND unixepoch(grabbed_at) >= (SELECT updated_at FROM download_clients WHERE id=?)",
    )
    .bind(NZBGET_DUPLICATE_REASON)
    .bind(client_id)
    .bind(job_key)
    .bind(client_id)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{download_client, indexers};
    use crate::test_support;

    #[tokio::test]
    async fn duplicate_decision_is_scoped_to_the_episode_and_original_client() {
        let db = test_support::in_memory_pool().await;
        let series = test_support::seed_series(&db, 123, "Example series").await;
        sqlx::query(
            "INSERT INTO download_clients (id,name,kind,url,label,is_default,updated_at)
                     VALUES (1,'First','nzbget','http://localhost:1','anime',1,0),
                            (2,'Second','nzbget','http://localhost:2','anime',0,0)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO indexers (id,name,kind,url,download_client_id)
                     VALUES (1,'Indexer','newznab','http://localhost:3',1)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO grabbed_torrents
                     (hash,torrent_name,series_id,episode_numbers,state,failure_reason,
                      indexer_id,download_client_id,source_url,grabbed_at)
                     VALUES ('attempt','Example',?,'[7]','removed',?,1,1,
                             'http://localhost:3/release','2026-01-01 00:00:00')",
        )
        .bind(series)
        .bind(NZBGET_DUPLICATE_REASON)
        .execute(&db)
        .await
        .unwrap();

        let mut state = test_support::build_test_app_state(db.clone(), None);
        download_client::rebuild_clients_cache(&state.download_clients, &db).await;
        state.indexers = indexers::rebuild_cache(&db).await;
        let initial = Duplicates::load(&state).await.unwrap();
        assert_eq!(
            initial.by_episode.keys().copied().collect::<Vec<_>>(),
            [(series, 7)]
        );

        sqlx::query("UPDATE indexers SET download_client_id=2 WHERE id=1")
            .execute(&db)
            .await
            .unwrap();
        state.indexers = indexers::rebuild_cache(&db).await;
        assert!(
            Duplicates::load(&state)
                .await
                .unwrap()
                .by_episode
                .is_empty()
        );

        sqlx::query("UPDATE indexers SET download_client_id=1 WHERE id=1")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE download_clients SET updated_at=unixepoch('2026-01-02') WHERE id=1")
            .execute(&db)
            .await
            .unwrap();
        state.indexers = indexers::rebuild_cache(&db).await;
        assert!(
            Duplicates::load(&state)
                .await
                .unwrap()
                .by_episode
                .is_empty()
        );
    }
}

//! The background job owns the cache. Page requests only read it.
use crate::services::anilist::discovery::{self, Premiere};
use sqlx::SqlitePool;
use std::time::Duration;

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

pub async fn refresh(db: &SqlitePool) -> Result<(), String> {
    match discovery::fetch().await {
        Ok(items) => {
            let json = serde_json::to_string(&items).map_err(|e| e.to_string())?;
            sqlx::query("INSERT INTO premiere_discovery(id,entries,refreshed_at,failed) VALUES(1,?,unixepoch(),0)
                ON CONFLICT(id) DO UPDATE SET entries=excluded.entries,refreshed_at=excluded.refreshed_at,failed=0")
                .bind(json).execute(db).await.map_err(|e| e.to_string())?;
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

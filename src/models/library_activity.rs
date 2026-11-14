//! Durable operation history. Entity snapshots survive library removal.
use sqlx::SqlitePool;

#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub struct Activity {
    pub provider_id: Option<i64>,
    pub occurred_at: String,
    pub action: String,
    pub title: String,
    pub reason: String,
    pub detail: String,
    pub note: String,
}
#[derive(Clone, Debug, Default)]
pub struct Event {
    pub provider_id: Option<i64>,
    pub key: String,
    pub action: String,
    pub series_id: Option<i64>,
    pub franchise_id: Option<String>,
    pub title: String,
    pub reason: String,
    pub detail: String,
    pub note: String,
    pub grab_id: Option<i64>,
    pub recycle_entry_id: Option<String>,
}
impl Event {
    pub fn new(
        key: impl Into<String>,
        action: &str,
        id: Option<i64>,
        title: &str,
        reason: &str,
    ) -> Self {
        Self {
            key: key.into(),
            action: action.into(),
            series_id: id,
            title: title.into(),
            reason: reason.into(),
            ..Default::default()
        }
    }
    pub async fn capture_parent(&mut self, db: &SqlitePool) {
        if let Some(id) = self.series_id
            && let Ok(Some(s)) = super::series::get_by_id(db, id).await
        {
            self.provider_id = Some(s.anilist_id);
            if self.title.is_empty() {
                self.title = s.title.clone();
            }
            self.franchise_id = super::library_layout::page_for_provider(db, s.anilist_id)
                .await
                .ok()
                .flatten()
                .map(|p| p.0);
        }
    }
}
pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS library_activity (
        id INTEGER PRIMARY KEY AUTOINCREMENT,operation_key TEXT NOT NULL UNIQUE,
        occurred_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,action TEXT NOT NULL,
        series_id INTEGER,provider_id INTEGER,franchise_id TEXT,title TEXT NOT NULL,reason TEXT NOT NULL,
        detail TEXT NOT NULL,note TEXT NOT NULL,grab_id INTEGER,recycle_entry_id TEXT);
        CREATE INDEX IF NOT EXISTS library_activity_series ON library_activity(series_id,id DESC);
        CREATE INDEX IF NOT EXISTS library_activity_franchise ON library_activity(franchise_id,id DESC);")
        .execute(db).await?;
    Ok(())
}
pub async fn append(db: &SqlitePool, mut event: Event) {
    if event.franchise_id.is_none() {
        event.capture_parent(db).await;
    }
    if let Err(error)=sqlx::query("INSERT OR IGNORE INTO library_activity(operation_key,action,series_id,provider_id,franchise_id,title,reason,detail,note,grab_id,recycle_entry_id) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
        .bind(&event.key).bind(&event.action).bind(event.series_id).bind(event.provider_id).bind(&event.franchise_id).bind(&event.title).bind(&event.reason).bind(&event.detail).bind(event.note.trim()).bind(event.grab_id).bind(&event.recycle_entry_id).execute(db).await {
        tracing::error!(%error,operation=%event.key,"Could not record library activity");
    }
}

pub async fn recent(
    db: &SqlitePool,
    ids: &[i64],
    franchise: Option<&str>,
) -> Result<Vec<Activity>, sqlx::Error> {
    let scope = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let mut rows:Vec<Activity>=sqlx::query_as("SELECT provider_id,occurred_at,action,title,reason,detail,note FROM library_activity a WHERE (series_id IN(SELECT value FROM json_each(?)) AND EXISTS(SELECT 1 FROM series s WHERE s.id=a.series_id AND (a.provider_id IS NULL OR s.anilist_id=a.provider_id))) OR franchise_id=? ORDER BY id DESC LIMIT 100")
        .bind(&scope).bind(franchise).fetch_all(db).await?;
    let mut legacy:Vec<Activity>=sqlx::query_as("SELECT s.anilist_id AS provider_id,h.grabbed_at AS occurred_at,h.state AS action,s.title AS title,'Reason not recorded' AS reason,h.release_title AS detail,'' AS note FROM episode_grab_history h JOIN series s ON s.id=h.series_id WHERE h.series_id IN(SELECT value FROM json_each(?)) AND NOT EXISTS(SELECT 1 FROM library_activity a WHERE a.grab_id IS NOT NULL AND a.series_id=h.series_id AND (a.detail=h.release_title OR (json_valid(a.detail) AND json_extract(a.detail,'$.release')=h.release_title))) GROUP BY h.series_id,h.grabbed_at,h.release_title,h.state ORDER BY h.grabbed_at DESC LIMIT 100")
        .bind(scope).fetch_all(db).await?;
    rows.append(&mut legacy);
    rows.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at));
    rows.truncate(100);
    Ok(rows)
}

pub async fn recent_all(db: &SqlitePool) -> Result<Vec<Activity>, sqlx::Error> {
    sqlx::query_as("SELECT provider_id,occurred_at,action,title,reason,detail,note FROM library_activity ORDER BY id DESC LIMIT 200").fetch_all(db).await
}

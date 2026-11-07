//! Create an EMPTY database with the real application schema for offline tests.
//! Refuses an existing path. Never reads environment-selected production paths.
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected new database path")?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    drop(file);
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await?;
    ryokan::models::migrate(&db).await?;
    db.close().await;
    Ok(())
}

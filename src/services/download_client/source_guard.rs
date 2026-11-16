//! The single native-action boundary for durable source exclusions.
//! Readers cover the native add/resume call. Retirement owns the writer while
//! it persists a block and removes the job. A database trigger also refuses a
//! late pending-owner write from an add that began before the block.
use super::*;
use sqlx::SqlitePool;
use std::sync::{Arc, LazyLock};
use tokio::sync::{RwLock, RwLockReadGuard};

pub static SOURCE_ACTION_LOCK: LazyLock<RwLock<()>> = LazyLock::new(|| RwLock::new(()));

pub(crate) struct SourceGuard {
    pub db: SqlitePool,
    pub inner: Arc<dyn DownloadClient>,
}
impl SourceGuard {
    async fn source(
        &self,
        url: &str,
        claimed: &str,
    ) -> Result<(RwLockReadGuard<'static, ()>, String, String), String> {
        check_release_url(url)?;
        let guard = SOURCE_ACTION_LOCK.read().await;
        let has_blocks:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM torrent_seed_reviews WHERE blocked_at IS NOT NULL AND unblocked_at IS NULL)")
            .fetch_one(&self.db).await.map_err(|e|format!("Source exclusion check unavailable: {e}"))?;
        // With no durable exclusions, preserve native HTTP compatibility. The
        // shared lock prevents a block from appearing during this native call.
        if !has_blocks && !url.to_ascii_lowercase().starts_with("magnet:") {
            return Ok((guard, url.to_owned(), claimed.to_owned()));
        }
        let (actual, url) = if url.to_ascii_lowercase().starts_with("magnet:") {
            let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid magnet identity")?;
            let mut identities = Vec::new();
            let mut parameters = Vec::new();
            for (key, value) in parsed.query_pairs() {
                if key.eq_ignore_ascii_case("xt") {
                    identities.push(
                        crate::services::torrent_file::magnet_info_hash(&format!(
                            "magnet:?xt={value}"
                        ))
                        .ok_or("Unsupported magnet identity; source needs review")?,
                    );
                } else {
                    parameters.push((key.into_owned(), value.into_owned()));
                }
            }
            let actual = identities
                .first()
                .cloned()
                .ok_or("Source identity unavailable; no download started")?;
            if identities.iter().any(|h| h != &actual) {
                return Err("Conflicting magnet identities; source needs review".into());
            }
            let mut canonical = reqwest::Url::parse(&format!("magnet:?xt=urn:btih:{actual}"))
                .map_err(|e| e.to_string())?;
            for (key, value) in parameters {
                canonical.query_pairs_mut().append_pair(&key, &value);
            }
            (actual, canonical.into())
        } else {
            crate::services::torrent_file::public_source_magnet(url).await?
        };
        if !claimed.is_empty() && normalize_info_hash(claimed).as_ref() != Some(&actual) {
            return Err("The supplied hash does not match the source identity".into());
        }
        if crate::models::seed_policy::is_blocked(&self.db, &actual)
            .await
            .map_err(|e| format!("Source exclusion check unavailable: {e}"))?
        {
            return Err("Source excluded by seeding policy; reconsider it explicitly before another grab or resume".into());
        }
        Ok((guard, url, actual))
    }
    async fn permit(&self, hash: &str) -> Result<RwLockReadGuard<'static, ()>, String> {
        check_info_hash(hash)?;
        let guard = SOURCE_ACTION_LOCK.read().await;
        if crate::models::seed_policy::is_blocked(&self.db, hash)
            .await
            .map_err(|e| format!("Source exclusion check unavailable: {e}"))?
        {
            return Err("Source excluded by seeding policy; reconsider it explicitly before another grab or resume".into());
        }
        Ok(guard)
    }
}

#[async_trait]
impl DownloadClient for SourceGuard {
    async fn test(&self) -> Result<String, String> {
        self.inner.test().await
    }
    async fn add_torrent(&self, url: &str, hash: &str) -> Result<AddOutcome, String> {
        let (_guard, url, hash) = self.source(url, hash).await?;
        self.inner.add_torrent(&url, &hash).await
    }
    async fn add_torrent_returning_id(
        &self,
        url: &str,
        hash: &str,
    ) -> Result<(AddOutcome, String), String> {
        let (_guard, url, hash) = self.source(url, hash).await?;
        self.inner.add_torrent_returning_id(&url, &hash).await
    }
    async fn add_torrent_paused(&self, url: &str, hash: &str) -> Result<AddOutcome, String> {
        let (_guard, url, hash) = self.source(url, hash).await?;
        self.inner.add_torrent_paused(&url, &hash).await
    }
    async fn add_torrent_metadata_only(&self, hash: &str) -> Result<AddOutcome, String> {
        let _guard = self.permit(hash).await?;
        self.inner.add_torrent_metadata_only(hash).await
    }
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        hash: &str,
        filter: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        let (_guard, url, hash) = self.source(url, hash).await?;
        self.inner
            .add_torrent_with_file_filter(&url, &hash, filter)
            .await
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        self.inner.list_scoped().await
    }
    async fn get_files(&self, hash: &str) -> Result<Vec<DownloadFile>, String> {
        self.inner.get_files(hash).await
    }
    async fn pause(&self, hash: &str) -> Result<(), String> {
        self.inner.pause(hash).await
    }
    async fn resume(&self, hash: &str) -> Result<(), String> {
        let _guard = self.permit(hash).await?;
        self.inner.resume(hash).await
    }
    async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), String> {
        self.inner.delete(hash, delete_files).await
    }
    async fn set_file_wanted(
        &self,
        hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        let _guard = self.permit(hash).await?;
        self.inner.set_file_wanted(hash, files, wanted).await
    }
    async fn set_seed_rules(&self, hash: &str, rules: SeedRules) -> Result<(), String> {
        self.inner.set_seed_rules(hash, rules).await
    }
    fn sonarr_impl_name(&self) -> &'static str {
        self.inner.sonarr_impl_name()
    }
    fn protocol(&self) -> &'static str {
        self.inner.protocol()
    }
}

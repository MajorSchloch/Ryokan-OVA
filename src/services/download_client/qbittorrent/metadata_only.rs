//! Strict source preparation; the legacy optional picker is deliberately separate.
use super::*;

const CAPABILITY_TIMEOUT: Duration = Duration::from_secs(5);
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct NativeJob {
    hash: String,
    state: String,
    progress: f64,
    downloaded: i64,
    downloaded_session: i64,
}

fn stable_version(value: &str) -> Option<(u32, u32, u32)> {
    let value = value.trim();
    let mut parts = value.strip_prefix('v').unwrap_or(value).split('.');
    let result = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(result)
}

fn supported_native_contract(app: &str, api: &str) -> bool {
    // qBit 5.0 includes the already-known-metadata fix (#20283) and the
    // synchronous canonical-magnet duplicate guard. The 4.6.4 backport is not
    // accepted here until that complete contract has its own fixture coverage.
    matches!(stable_version(app), Some((5, _, _)))
        && stable_version(api).is_some_and(|version| version.0 == 2 && version >= (2, 11, 0))
}

impl QbitClient {
    async fn reviewed_native_capability(&self) -> Result<(), String> {
        tokio::time::timeout(CAPABILITY_TIMEOUT, async {
            let app = self.do_get("/api/v2/app/version").await
                .map_err(|_| "Could not verify qBittorrent's metadata-only capability.")?;
            if !app.status().is_success() {
                return Err("Could not verify qBittorrent's metadata-only capability.".to_string());
            }
            let app = app.text().await.map_err(|_| "Could not read qBittorrent's version.")?;
            let api = self.do_get("/api/v2/app/webapiVersion").await
                .map_err(|_| "Could not verify qBittorrent's native API capability.")?;
            if !api.status().is_success() {
                return Err("Could not verify qBittorrent's native API capability.".to_string());
            }
            let api = api.text().await.map_err(|_| "Could not read qBittorrent's API version.")?;
            if !supported_native_contract(&app, &api) {
                return Err("Reviewed sources require a supported stable qBittorrent 5.x metadata-only API. No torrent was added.".into());
            }
            Ok(())
        }).await.map_err(|_| "qBittorrent capability check timed out. No torrent was added.".to_string())?
    }

    async fn reviewed_job(&self, hash: &str) -> Result<Option<NativeJob>, String> {
        let response = self
            .do_get(&format!("/api/v2/torrents/info?hashes={hash}"))
            .await
            .map_err(|_| {
                "Could not verify whether this torrent already exists. No ownership was assumed."
                    .to_string()
            })?;
        if !response.status().is_success() {
            return Err(
                "Could not verify whether this torrent already exists. No ownership was assumed."
                    .into(),
            );
        }
        let mut jobs: Vec<NativeJob> = response
            .json()
            .await
            .map_err(|_| "qBittorrent returned an unverifiable torrent state.".to_string())?;
        if jobs.len() > 1 || jobs.iter().any(|job| !job.hash.eq_ignore_ascii_case(hash)) {
            return Err("qBittorrent returned a different torrent identity.".into());
        }
        Ok(jobs.pop())
    }

    pub(super) async fn prepare_reviewed_metadata(
        &self,
        info_hash: &str,
    ) -> Result<AddOutcome, String> {
        self.prepare_reviewed_metadata_with_timeout(info_hash, PREPARATION_TIMEOUT)
            .await
    }

    async fn prepare_reviewed_metadata_with_timeout(
        &self,
        info_hash: &str,
        timeout: Duration,
    ) -> Result<AddOutcome, String> {
        if info_hash.len() != 40 || !info_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(
                "Reviewed metadata preparation requires one complete v1 torrent hash.".into(),
            );
        }
        let hash = info_hash.to_ascii_lowercase();
        // This uncached, unscoped Result must never turn a failed read into
        // permission to add or mutate an existing client job.
        if tokio::time::timeout(CAPABILITY_TIMEOUT, self.reviewed_job(&hash))
            .await
            .map_err(|_| "Torrent ownership check timed out. No torrent was added.".to_string())??
            .is_some()
        {
            return Ok(AddOutcome::AlreadyPresent);
        }
        self.reviewed_native_capability().await?;
        // One canonical magnet is processed synchronously by qBit's native
        // add manager. An HTTP URL's early acknowledgement cannot prove that
        // this caller created the hash, and trackers/webseeds could alter a
        // concurrently existing job, so none are supplied.
        let magnet = format!("magnet:?xt=urn:btih:{hash}");
        let form = [
            ("urls", magnet.as_str()),
            ("category", self.category.as_str()),
            ("stopCondition", "MetadataReceived"),
            ("stopped", "false"),
            ("paused", "false"),
        ];
        let response = tokio::time::timeout(
            CAPABILITY_TIMEOUT,
            self.do_post_form("/api/v2/torrents/add", &form),
        )
        .await
        .map_err(|_| {
            "The metadata-only add acknowledgement timed out. Existing jobs were not changed."
                .to_string()
        })?
        .map_err(|_| {
            "The metadata-only add was not acknowledged. Existing jobs were not changed."
                .to_string()
        })?;
        if !response.status().is_success() {
            return Err("qBittorrent did not accept the metadata-only add.".into());
        }
        let body = tokio::time::timeout(CAPABILITY_TIMEOUT, response.text())
            .await
            .map_err(|_| "The metadata-only add acknowledgement timed out.".to_string())?
            .map_err(|_| "Could not verify the metadata-only add acknowledgement.".to_string())?;
        if body.trim() != "Ok." {
            // A duplicate can win between the absence check and this add.
            // The native canonical-magnet path rejects it before applying
            // stop settings. Never stop, reselect or resume that other job.
            if body.trim() == "Fails." && self.reviewed_job(&hash).await?.is_some() {
                return Ok(AddOutcome::AlreadyPresent);
            }
            return Err(
                "The metadata-only add was not confirmed. Existing jobs were not changed.".into(),
            );
        }
        self.invalidate_torrents_cache().await;
        let result = tokio::time::timeout(timeout, async {
            loop {
                if let Some(job) = self.reviewed_job(&hash).await? {
                    if job.downloaded != 0 || job.downloaded_session != 0 || !job.progress.is_finite() || job.progress != 0.0 {
                        return Err("The reviewed source has payload data before preparation completed. It remains held for review.".to_string());
                    }
                    let files = self.get_torrent_files_raw(&hash).await?;
                    if !files.is_empty() && matches!(job.state.as_str(), "stoppedDL" | "pausedDL") {
                        return Ok(AddOutcome::Added);
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }).await.unwrap_or_else(|_| Err("Metadata-only preparation timed out. The reviewed source is held for review; no payload fallback was started.".into()));
        if result.is_err() {
            // Only an acknowledged new canonical-magnet job reaches here.
            // The native stop condition remains the payload barrier even
            // when the stop request cannot reach the client.
            let _ = tokio::time::timeout(Duration::from_secs(3), self.pause(&hash)).await;
        }
        result
    }
}

#[cfg(test)]
mod tests;

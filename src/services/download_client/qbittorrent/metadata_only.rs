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
    tags: String,
}

impl NativeJob {
    fn owns_marker(&self, marker: &str) -> bool {
        self.tags.split(',').any(|tag| tag.trim() == marker)
    }

    fn has_no_payload(&self) -> bool {
        self.downloaded == 0
            && self.downloaded_session == 0
            && self.progress.is_finite()
            && self.progress == 0.0
    }

    fn is_stopped(&self) -> bool {
        matches!(self.state.as_str(), "stoppedDL" | "pausedDL")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredAddAcknowledgement {
    success_count: u64,
    failure_count: u64,
    pending_count: u64,
    added_torrent_ids: Vec<String>,
}

fn acknowledged_add(body: &str, hash: &str) -> bool {
    if body.trim() == "Ok." {
        return true;
    }
    serde_json::from_str::<StructuredAddAcknowledgement>(body).is_ok_and(|value| {
        value.success_count == 1
            && value.failure_count == 0
            && value.pending_count == 0
            && value.added_torrent_ids.len() == 1
            && value.added_torrent_ids[0].eq_ignore_ascii_case(hash)
    })
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
    // qBit 5.0 includes the already-known-metadata fix (#20283). Newer 5.x
    // versions acknowledge scheduling before native duplicate resolution, so
    // an exact per-attempt marker must independently establish ownership.
    // The 4.6.4 backport needs separate complete-contract fixture coverage.
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

    async fn bounded_reviewed_job(&self, hash: &str) -> Result<Option<NativeJob>, String> {
        tokio::time::timeout(CAPABILITY_TIMEOUT, self.reviewed_job(hash))
            .await
            .map_err(|_| {
                "Torrent ownership check timed out. No ownership was assumed.".to_string()
            })?
    }

    async fn remove_reviewed_marker(&self, hash: &str, marker: &str) -> Result<(), String> {
        let response = tokio::time::timeout(
            CAPABILITY_TIMEOUT,
            self.do_post_form(
                "/api/v2/torrents/removeTags",
                &[("hashes", hash), ("tags", marker)],
            ),
        )
        .await
        .map_err(|_| {
            "Could not remove this attempt's metadata marker; the source remains held.".to_string()
        })?
        .map_err(|_| {
            "Could not remove this attempt's metadata marker; the source remains held.".to_string()
        })?;
        if !response.status().is_success() {
            return Err(
                "Could not remove this attempt's metadata marker; the source remains held.".into(),
            );
        }
        Ok(())
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
        if self.bounded_reviewed_job(&hash).await?.is_some() {
            return Ok(AddOutcome::AlreadyPresent);
        }
        self.reviewed_native_capability().await?;
        // A response can acknowledge scheduling before the native duplicate
        // check. Only the winning new object receives these add-time tags.
        // A canonical magnet also avoids merging trackers/webseeds into a
        // concurrently existing torrent.
        let marker = format!(
            "ryokan-metadata-{}",
            hex::encode(rand::random::<[u8; 16]>())
        );
        let magnet = format!("magnet:?xt=urn:btih:{hash}");
        let form = [
            ("urls", magnet.as_str()),
            ("category", self.category.as_str()),
            ("stopCondition", "MetadataReceived"),
            ("stopped", "false"),
            ("paused", "false"),
            ("tags", marker.as_str()),
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
        if response.status() == reqwest::StatusCode::CONFLICT {
            if self.bounded_reviewed_job(&hash).await?.is_some() {
                return Ok(AddOutcome::AlreadyPresent);
            }
            return Err("The metadata-only add conflicted with another request. Existing jobs were not changed.".into());
        }
        if !response.status().is_success() {
            return Err("qBittorrent did not accept the metadata-only add.".into());
        }
        let body = tokio::time::timeout(CAPABILITY_TIMEOUT, response.text())
            .await
            .map_err(|_| "The metadata-only add acknowledgement timed out.".to_string())?
            .map_err(|_| "Could not verify the metadata-only add acknowledgement.".to_string())?;
        if !acknowledged_add(&body, &hash) {
            if body.trim() == "Fails." && self.bounded_reviewed_job(&hash).await?.is_some() {
                return Ok(AddOutcome::AlreadyPresent);
            }
            return Err(
                "The metadata-only add was not confirmed. Existing jobs were not changed.".into(),
            );
        }
        self.invalidate_torrents_cache().await;
        let mut owned = false;
        let result = tokio::time::timeout(timeout, async {
            loop {
                let job = self.reviewed_job(&hash).await.inspect_err(|_| {
                    owned = false;
                })?;
                if let Some(job) = job {
                    if !job.owns_marker(&marker) {
                        owned = false;
                        return Err("This torrent belongs to another add attempt. Its state and selection were preserved; no ownership was assumed.".to_string());
                    }
                    owned = true;
                    if !job.has_no_payload() {
                        return Err("The reviewed source has payload data before preparation completed. It remains held for review.".to_string());
                    }
                    let files = self.get_torrent_files_raw(&hash).await?;
                    if !files.is_empty() && job.is_stopped() {
                        // File-list reads are asynchronous too. Recheck the
                        // winning marker and stopped state at the handoff.
                        let current = self.reviewed_job(&hash).await.inspect_err(|_| {
                            owned = false;
                        })?;
                        let Some(current) = current else {
                            owned = false;
                            return Err("The reviewed source disappeared during preparation.".to_string());
                        };
                        if !current.owns_marker(&marker) {
                            owned = false;
                            return Err("Reviewed source ownership changed during preparation; the other job was preserved.".into());
                        }
                        if !current.is_stopped() || !current.has_no_payload() {
                            return Err("Reviewed source ownership or stopped state changed during preparation; it remains held.".into());
                        }
                        // Scope removal to this exact job and marker. Never
                        // delete global tags, including a losing race's tag.
                        self.remove_reviewed_marker(&hash, &marker).await?;
                        return Ok(AddOutcome::Added);
                    }
                } else {
                    owned = false;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }).await.unwrap_or_else(|_| Err("Metadata-only preparation timed out. The reviewed source is held for review; no payload fallback was started.".into()));
        if result.is_err() {
            // An acknowledgement alone cannot authorize cleanup. A failed
            // read or a different/missing marker leaves the other job alone.
            if owned
                && self
                    .bounded_reviewed_job(&hash)
                    .await
                    .is_ok_and(|job| job.is_some_and(|job| job.owns_marker(&marker)))
            {
                let _ = tokio::time::timeout(Duration::from_secs(3), self.pause(&hash)).await;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests;

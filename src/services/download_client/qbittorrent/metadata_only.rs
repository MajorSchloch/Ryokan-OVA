//! Strict source preparation; the legacy optional picker is deliberately separate.
use super::*;
use crate::services::download_client::RETRYABLE_METADATA_PREFIX;
use std::sync::LazyLock;

const CAPABILITY_TIMEOUT: Duration = Duration::from_secs(5);
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const ATTEMPT_MARKER: &str = "ryokan-metadata-";
const RECOVERY_MARKER: &str = "ryokan-recovery-";
static RECOVERY_PERMIT: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));

#[derive(Debug, Deserialize)]
struct NativeJob {
    hash: String,
    state: String,
    progress: f64,
    downloaded: i64,
    downloaded_session: i64,
    tags: String,
    category: Option<String>,
    added_on: Option<i64>,
    uploaded: Option<i64>,
    uploaded_session: Option<i64>,
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

    fn recoverable_attempt(&self, category: &str) -> Option<(&str, bool)> {
        if !(self.category.as_deref() == Some(category)
            && self.added_on.is_some_and(|time| time > 0)
            && self.uploaded == Some(0)
            && self.uploaded_session == Some(0)
            && self.has_no_payload())
        {
            return None;
        }
        let mut tags = self.tags.split(',').map(str::trim);
        let first = tags.next()?;
        let second = tags.next();
        if tags.next().is_some() {
            return None;
        }
        let (marker, recovery) = if first.starts_with(RECOVERY_MARKER) {
            (second?, Some(first))
        } else {
            (first, second)
        };
        let nonce = marker.strip_prefix(ATTEMPT_MARKER)?;
        if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if recovery.is_some_and(|tag| tag.strip_prefix(RECOVERY_MARKER) != Some(nonce)) {
            return None;
        }
        Some((marker, recovery.is_some()))
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
    async fn reviewed_native_capability(&self, recovery: bool) -> Result<(), String> {
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
            if recovery && !stable_version(&app).is_some_and(|version| version >= (5, 3, 0)) {
                return Err("Automatic metadata recovery requires qBittorrent 5.3 or later in the 5.x series.".into());
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

    pub(super) async fn recover_reviewed_metadata(
        &self,
        info_hash: &str,
        publication_url: &str,
    ) -> Result<Option<MetadataRecovery>, String> {
        let hash = verified_v1_hash(info_hash)?;
        // Retain this permit through file priorities, the durable claim and
        // resume. Another acquisition path cannot also claim these tags.
        let permit = RECOVERY_PERMIT.clone().try_lock_owned().map_err(|_| {
            format!(
                "{RETRYABLE_METADATA_PREFIX}another qBittorrent metadata recovery is in progress."
            )
        })?;
        let Some(original) = self.bounded_reviewed_job(&hash).await? else {
            return Err(format!(
                "{RETRYABLE_METADATA_PREFIX}the metadata-only entry disappeared."
            ));
        };
        let Some((marker, started)) = original.recoverable_attempt(&self.category) else {
            return Ok(None);
        };
        if !original.is_stopped() {
            return Ok(None);
        }
        let mut files = self.bounded_reviewed_files(&hash).await?;
        if !files.is_empty() && (!started || !files.iter().all(is_unedited_file)) {
            return Ok(None);
        }
        self.reviewed_native_capability(true).await?;
        let metainfo =
            crate::services::nyaa::fetch_recovery_metainfo(publication_url, &hash).await?;
        let added_on = original
            .added_on
            .ok_or("The metadata attempt has no native creation time.")?;
        files = self.bounded_reviewed_files(&hash).await?;
        if !files.is_empty() && (!started || !files.iter().all(is_unedited_file)) {
            return Ok(None);
        }
        self.check_recovery_owner(&hash, marker, added_on, started)
            .await?;
        if !started {
            let recovery_marker = recovery_marker(marker)?;
            let response = tokio::time::timeout(
                CAPABILITY_TIMEOUT,
                self.do_post_form(
                    "/api/v2/torrents/addTags",
                    &[("hashes", &hash), ("tags", &recovery_marker)],
                ),
            )
            .await;
            if !response
                .is_ok_and(|result| result.is_ok_and(|response| response.status().is_success()))
            {
                return Err(format!(
                    "{RETRYABLE_METADATA_PREFIX}qBittorrent could not mark the recovery attempt."
                ));
            }
            self.check_recovery_owner(&hash, marker, added_on, true)
                .await?;
        }
        if files.is_empty() {
            // qBittorrent 5.3 adds a duplicate file's info dictionary to an
            // existing torrent without metadata. It leaves a stopped job
            // stopped. The upload carries only the exact info dictionary.
            let submitted = self
                .upload_recovery_metadata(&hash, marker, added_on, &metainfo)
                .await;
            let ready = tokio::time::timeout(RECOVERY_TIMEOUT, async {
                loop {
                    let job = self.read_recovery_owner(&hash, marker, added_on, true).await?;
                    if job.is_stopped() {
                        let files = self.bounded_reviewed_files(&hash).await?;
                        if !files.is_empty() {
                            if !files.iter().all(is_unedited_file) {
                                return Err("The client file selection changed during metadata recovery. The source remains stopped.".into());
                            }
                            return self.check_recovery_owner(&hash, marker, added_on, true).await;
                        }
                    } else if job.state != "checkingResumeData" {
                        return Err("The metadata attempt is no longer stopped. The client entry was preserved.".into());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }).await;
            match ready {
                Ok(result) => result?,
                Err(_) => {
                    submitted?;
                    return Err(format!(
                        "{RETRYABLE_METADATA_PREFIX}qBittorrent did not expose the recovered file list."
                    ));
                }
            }
        }
        Ok(Some(MetadataRecovery {
            marker: marker.to_string(),
            added_on,
            _permit: permit,
        }))
    }

    pub(super) async fn finish_reviewed_metadata_recovery(
        &self,
        info_hash: &str,
        recovery: &MetadataRecovery,
    ) -> Result<(), String> {
        let hash = verified_v1_hash(info_hash)?;
        self.check_recovery_owner(&hash, &recovery.marker, recovery.added_on, true)
            .await?;
        // Use the same public trackers as Nyaa's ordinary magnets. The
        // publication's outer .torrent fields are outside the verified hash.
        let urls = crate::services::nyaa::NYAA_TRACKER_SET.join("\n");
        let response = tokio::time::timeout(
            CAPABILITY_TIMEOUT,
            self.do_post_form(
                "/api/v2/torrents/addTrackers",
                &[("hash", &hash), ("urls", &urls)],
            ),
        )
        .await;
        if !response.is_ok_and(|result| result.is_ok_and(|response| response.status().is_success()))
        {
            return Err(
                "qBittorrent could not restore the public trackers. The source remains stopped."
                    .into(),
            );
        }
        self.check_recovery_owner(&hash, &recovery.marker, recovery.added_on, true)
            .await?;
        let tags = format!("{},{}", recovery.marker, recovery_marker(&recovery.marker)?);
        self.remove_reviewed_marker(&hash, &tags).await?;
        self.invalidate_torrents_cache().await;
        Ok(())
    }

    async fn bounded_reviewed_files(&self, hash: &str) -> Result<Vec<QbitRawFile>, String> {
        tokio::time::timeout(CAPABILITY_TIMEOUT, self.get_torrent_files_raw(hash))
            .await
            .map_err(|_| "Could not verify the existing client file list.")?
    }

    async fn check_recovery_owner(
        &self,
        hash: &str,
        marker: &str,
        added_on: i64,
        started: bool,
    ) -> Result<(), String> {
        if self
            .read_recovery_owner(hash, marker, added_on, started)
            .await?
            .is_stopped()
        {
            Ok(())
        } else {
            Err("The metadata attempt's ownership or stopped state changed. The client entry was preserved.".into())
        }
    }

    async fn read_recovery_owner(
        &self,
        hash: &str,
        marker: &str,
        added_on: i64,
        started: bool,
    ) -> Result<NativeJob, String> {
        self.bounded_reviewed_job(hash)
            .await?
            .filter(|job| {
                job.recoverable_attempt(&self.category) == Some((marker, started))
                    && job.added_on == Some(added_on)
            })
            .ok_or_else(|| {
                "The metadata attempt's ownership changed. The client entry was preserved.".into()
            })
    }

    async fn upload_recovery_metadata(
        &self,
        hash: &str,
        marker: &str,
        added_on: i64,
        bytes: &[u8],
    ) -> Result<(), String> {
        let submit = || {
            let file = reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(format!("{hash}.torrent"));
            let form = reqwest::multipart::Form::new()
                .part("torrents", file)
                .text("category", self.category.clone())
                .text("stopCondition", "MetadataReceived")
                .text("stopped", "true")
                .text(
                    "tags",
                    format!(
                        "{marker},{}",
                        recovery_marker(marker).expect("verified marker")
                    ),
                );
            self.http
                .post(format!("{}/api/v2/torrents/add", self.base_url))
                .multipart(form)
                .send()
        };
        // The duplicate submission can report a conflict after accepting
        // metadata. Do not infer ownership or readiness from its status.
        let unavailable = || {
            format!(
                "{RETRYABLE_METADATA_PREFIX}qBittorrent did not accept the publisher's metadata."
            )
        };
        let response = tokio::time::timeout(CAPABILITY_TIMEOUT, async {
            self.ensure_login().await.map_err(|_| unavailable())?;
            let response = submit().await.map_err(|_| unavailable())?;
            if response.status() != StatusCode::FORBIDDEN {
                return Ok(response);
            }
            *self.logged_in.lock().await = false;
            self.login().await.map_err(|_| unavailable())?;
            self.check_recovery_owner(hash, marker, added_on, true)
                .await?;
            submit().await.map_err(|_| unavailable())
        })
        .await
        .map_err(|_| unavailable())??;
        if response.status().is_success() || response.status() == StatusCode::CONFLICT {
            Ok(())
        } else {
            Err(unavailable())
        }
    }

    async fn prepare_reviewed_metadata_with_timeout(
        &self,
        info_hash: &str,
        timeout: Duration,
    ) -> Result<AddOutcome, String> {
        let hash = verified_v1_hash(info_hash)?;
        // This uncached, unscoped Result must never turn a failed read into
        // permission to add or mutate an existing client job.
        if self.bounded_reviewed_job(&hash).await?.is_some() {
            return Ok(AddOutcome::AlreadyPresent);
        }
        self.reviewed_native_capability(false).await?;
        // A response can acknowledge scheduling before the native duplicate
        // check. Only the winning new object receives these add-time tags.
        // A canonical magnet also avoids merging trackers/webseeds into a
        // concurrently existing torrent.
        let marker = format!(
            "{ATTEMPT_MARKER}{}",
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

fn verified_v1_hash(info_hash: &str) -> Result<String, String> {
    if info_hash.len() == 40 && info_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(info_hash.to_ascii_lowercase())
    } else {
        Err("Reviewed metadata preparation requires one complete v1 torrent hash.".into())
    }
}

fn recovery_marker(marker: &str) -> Result<String, String> {
    let nonce = marker
        .strip_prefix(ATTEMPT_MARKER)
        .ok_or("Invalid metadata attempt marker.")?;
    Ok(format!("{RECOVERY_MARKER}{nonce}"))
}

fn is_unedited_file(file: &QbitRawFile) -> bool {
    file.priority == 1 && file.progress == 0.0
}

#[cfg(test)]
mod tests;

//! NZBGet JSON-RPC. The DupeKey travels with a URL through fetch, unpack and
//! history. Keep it as the Ryokan job ID; resolve the current NZBID at edit time.
use super::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
    check_release_url, translate_client_path,
};
use async_trait::async_trait;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use std::time::Duration;

pub struct NzbgetClient {
    http: reqwest::Client,
    endpoint: String,
    username: String,
    password: String,
    category: String,
    local_path: String,
    owner: String,
    add_lock: tokio::sync::Mutex<()>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct Job {
    #[serde(rename = "NZBID")]
    id: i64,
    #[serde(rename = "NZBName")]
    name: String,
    kind: String,
    category: String,
    dupe_key: String,
    status: String,
    delete_status: String,
    par_status: String,
    unpack_status: String,
    dest_dir: String,
    final_dir: String,
    file_size_lo: u32,
    file_size_hi: u32,
    remaining_size_lo: u32,
    remaining_size_hi: u32,
}

fn valid_directory_path(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
}

impl Job {
    fn state(&self, history: bool) -> DownloadItemState {
        use DownloadItemState::*;
        if history {
            let status = self.status.split_once('/').unwrap_or((&self.status, ""));
            return match status {
                // COPY discarded this exact duplicate. Reconcile its attempt
                // without failed-download cleanup or adoption of the older job.
                ("DELETED", "MANUAL" | "COPY") => Removed,
                // Mark as good can finalize a discarded job without an
                // importable download. Close only its exact pending attempt.
                ("DELETED", "GOOD") if self.kind == "NZB" && self.delete_status == "GOOD" => {
                    Removed
                }
                ("WARNING", "SKIPPED") if self.kind == "URL" => Errored,
                ("SUCCESS", _) if self.kind == "NZB" => PausedComplete,
                ("FAILURE", _) => Errored,
                _ => Warning,
            };
        }
        match self.status.as_str() {
            "DOWNLOADING" | "FETCHING" => Downloading,
            "PAUSED" => Paused,
            "QUEUED" => DownloadingQueued,
            _ => CheckingDownload,
        }
    }

    // A script may set FinalDir. Only walk a job-specific output folder.
    // A shared category or download root is never a safe import source.
    fn job_dir<'a>(&self, dir: &'a str) -> Option<&'a Path> {
        let path = Path::new(dir);
        let leaf = path.file_name()?.to_str()?;
        // InterDir uses "<name>.#<number>" while downloading and on failure.
        let matches_name = leaf == self.name
            || leaf
                .strip_prefix(&self.name)
                .and_then(|rest| rest.strip_prefix(".#"))
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                });
        if !self.name.is_empty() && matches_name && valid_directory_path(path) {
            Some(path)
        } else {
            None
        }
    }

    fn reported_dirs(&self) -> impl Iterator<Item = &str> {
        [self.dest_dir.as_str(), self.final_dir.as_str()]
            .into_iter()
            .filter(|dir| !dir.is_empty())
    }

    // HistoryFinalDelete can erase a failed unpack even if the job was
    // later marked successful. NZBGet has no keep-data flag.
    fn can_keep_files(&self, history: bool) -> bool {
        self.state(history).is_complete()
            && self.delete_status == "NONE"
            && !matches!(self.par_status.as_str(), "" | "FAILURE")
            && !matches!(self.unpack_status.as_str(), "" | "FAILURE" | "PASSWORD")
    }

    fn item(&self, history: bool, local_path: &str) -> DownloadItem {
        let mut state = self.state(history);
        let mut status = self.status.as_str();
        let mut content_path = String::new();
        if state.is_complete() {
            let dir = if self.final_dir.is_empty() {
                &self.dest_dir
            } else {
                &self.final_dir
            };
            if let Some(path) = self.job_dir(dir)
                && let Some(parent) = path.parent().and_then(Path::to_str)
            {
                content_path = translate_client_path(dir, parent, local_path);
            } else {
                state = DownloadItemState::Warning;
                status = "Completed. Check the NZBGet job folder.";
            }
        }
        let bytes = |high: u32, low: u32| (u64::from(high) << 32) | u64::from(low);
        let total = bytes(self.file_size_hi, self.file_size_lo);
        let remaining = bytes(self.remaining_size_hi, self.remaining_size_lo);
        DownloadItem {
            hash: self.dupe_key.clone(),
            name: if self.kind == "URL" {
                "NZB fetch".into()
            } else {
                self.name.clone()
            },
            size: i64::try_from(total).unwrap_or(i64::MAX),
            progress: if state.is_complete() {
                1.0
            } else if total > 0 {
                total.saturating_sub(remaining) as f64 / total as f64
            } else {
                0.0
            },
            dlspeed: 0,
            eta: 0,
            category: self.category.clone(),
            state: status.to_owned(),
            state_kind: state,
            // The output path is already mapped to this client's local mount.
            save_path: String::new(),
            content_path,
            seeding_done: false,
        }
    }
}

impl NzbgetClient {
    pub fn new(
        id: i64,
        base: &str,
        username: &str,
        password: &str,
        category: &str,
        local_path: &str,
    ) -> Self {
        Self {
            endpoint: format!("{}/jsonrpc", super::sabnzbd::normalize_base_url(base)),
            http: reqwest::Client::new(),
            username: username.into(),
            password: password.into(),
            category: category.trim().into(),
            local_path: local_path.trim().into(),
            owner: format!("nzbget-{id}-"),
            add_lock: Default::default(),
        }
    }

    async fn rpc<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, String> {
        if self.category.is_empty() {
            return Err("Set a download category for NZBGet.".into());
        }
        let response = self
            .http
            .post(&self.endpoint)
            .timeout(Duration::from_secs(15))
            .basic_auth(&self.username, Some(&self.password))
            .json(&json!({"method":method, "params":params, "id":1}))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("NZBGet {method}: {}", e.without_url()))?;
        #[derive(Deserialize)]
        struct Reply<T> {
            result: Option<T>,
            error: Option<Value>,
        }
        let reply: Reply<T> = response
            .json()
            .await
            .map_err(|e| format!("NZBGet {method}: {}", e.without_url()))?;
        if reply.error.is_some() {
            return Err(format!(
                "NZBGet rejected {method}. Check the account permissions and NZBGet log."
            ));
        }
        reply
            .result
            .ok_or_else(|| format!("NZBGet {method} returned no result"))
    }

    async fn native_jobs(&self, include_hidden: bool) -> Result<Vec<(Job, bool)>, String> {
        let mut jobs = Vec::new();
        for (method, params, history) in [
            ("listgroups", json!([0]), false),
            ("history", json!([include_hidden]), true),
        ] {
            jobs.extend(
                self.rpc::<Vec<Job>>(method, params)
                    .await?
                    .into_iter()
                    .map(|job| (job, history)),
            );
        }
        Ok(jobs)
    }

    async fn jobs(&self) -> Result<Vec<(Job, bool)>, String> {
        Ok(self
            .native_jobs(false)
            .await?
            .into_iter()
            .filter(|(job, _)| {
                job.id > 0
                    && job.category == self.category
                    && self.owns_key(&job.dupe_key)
                    && matches!(job.kind.as_str(), "NZB" | "URL")
            })
            .collect())
    }

    fn owns_key(&self, key: &str) -> bool {
        key.strip_prefix(&self.owner).is_some_and(|suffix| {
            suffix.len() == 96 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
        })
    }

    async fn add(&self, url: &str) -> Result<(AddOutcome, String), String> {
        check_release_url(url)?;
        let url = url.trim();
        if !reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
            return Err("NZBGet needs an HTTP or HTTPS NZB URL.".into());
        }
        // Recover an accepted submission by URL digest, even if its source URL
        // is absent from history. Give a retry after failure a separate job ID.
        let release_key = format!(
            "{}{}",
            self.owner,
            hex::encode(Sha256::digest(url.as_bytes()))
        );
        let _guard = self.add_lock.lock().await;
        for (job, history) in self.jobs().await? {
            if job.dupe_key.starts_with(&release_key)
                && !(history
                    && (job.state(true).is_errored() || job.status.starts_with("DELETED/")))
            {
                return Ok((AddOutcome::AlreadyPresent, job.dupe_key));
            }
        }
        let key = format!("{}{}", release_key, hex::encode(rand::random::<[u8; 16]>()));
        let id: i64 = self
            .rpc(
                "append",
                json!([
                    "",
                    url,
                    self.category,
                    0,
                    false,
                    false,
                    key,
                    0,
                    "SCORE",
                    false,
                    []
                ]),
            )
            .await?;
        if id <= 0 {
            return Err("NZBGet did not accept the NZB.".into());
        }
        Ok((AddOutcome::Added, key))
    }

    async fn edit(&self, key: &str, action: &str, delete_files: bool) -> Result<(), String> {
        if !self.owns_key(key) {
            return Err("This job does not belong to this Ryokan client.".into());
        }
        let Some((job, history)) = self
            .jobs()
            .await?
            .into_iter()
            .find(|(job, _)| job.dupe_key == key)
        else {
            return Err("NZBGet job is absent or outside the configured category.".into());
        };
        let command = match (action, history) {
            ("delete", _) => {
                if !delete_files && !job.can_keep_files(history) {
                    return Err("NZBGet cannot remove this job and guarantee that its partial files are kept.".into());
                }
                if job.kind == "NZB"
                    && !job.dest_dir.is_empty()
                    && job.job_dir(&job.dest_dir).is_none()
                {
                    return Err("NZBGet did not report a separate job directory. Check the job before removal.".into());
                }
                if history {
                    "HistoryFinalDelete"
                } else {
                    "GroupFinalDelete"
                }
            }
            (_, true) => return Err("Only queued NZBGet jobs can be paused or resumed.".into()),
            _ => action,
        };
        self.edit_job(command, job.id).await
    }

    async fn edit_job(&self, command: &str, nzbid: i64) -> Result<(), String> {
        if self
            .rpc::<bool>("editqueue", json!([command, "", [nzbid]]))
            .await?
        {
            Ok(())
        } else {
            Err(format!("NZBGet could not apply {command}."))
        }
    }

    fn verified_cleanup_dirs<'a>(
        &self,
        job: &'a Job,
        content_path: &str,
    ) -> Result<Vec<&'a Path>, String> {
        if job.id <= 0
            || job.category != self.category
            || !job.can_keep_files(true)
            // GOOD and MARK can be assigned manually after a failed download.
            || !matches!(
                job.status.as_str(),
                "SUCCESS/ALL" | "SUCCESS/UNPACK" | "SUCCESS/PAR" | "SUCCESS/HEALTH"
            )
            || !matches!(job.par_status.as_str(), "NONE" | "SUCCESS")
            || !matches!(job.unpack_status.as_str(), "NONE" | "SUCCESS")
        {
            return Err("NZBGet did not verify a unique successful history item.".into());
        }

        let directories = job
            .reported_dirs()
            .map(|path| job.job_dir(path))
            .collect::<Option<Vec<_>>>()
            .ok_or("NZBGet did not report separate job directories.")?;
        if !valid_directory_path(Path::new(content_path))
            || job.item(true, &self.local_path).content_path != content_path
        {
            return Err("The completed NZBGet job directory changed.".into());
        }
        Ok(directories)
    }
}

#[async_trait]
impl DownloadClient for NzbgetClient {
    async fn test(&self) -> Result<String, String> {
        let version: String = self.rpc("version", json!([])).await?;
        self.jobs().await?;
        Ok(format!("NZBGet {version}"))
    }
    fn protocol(&self) -> &'static str {
        "usenet"
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "Nzbget"
    }
    async fn add_torrent(&self, url: &str, _: &str) -> Result<AddOutcome, String> {
        self.add(url).await.map(|v| v.0)
    }
    async fn add_torrent_returning_id(
        &self,
        url: &str,
        _: &str,
    ) -> Result<(AddOutcome, String), String> {
        self.add(url).await
    }
    async fn add_torrent_paused(&self, url: &str, _: &str) -> Result<AddOutcome, String> {
        check_release_url(url)?;
        Err(
            "Use a normal grab for NZBGet. Extracted files cannot be selected before download."
                .into(),
        )
    }
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        check_release_url(url)?;
        Err("NZBGet needs a full NZB grab.".into())
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(self
            .jobs()
            .await?
            .into_iter()
            .map(|(job, history)| job.item(history, &self.local_path))
            .collect())
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(Vec::new())
    }
    async fn set_file_wanted(&self, _: &str, _: &[usize], _: bool) -> Result<(), String> {
        Err("NZBGet does not support selecting unpacked files.".into())
    }
    async fn pause(&self, key: &str) -> Result<(), String> {
        self.edit(key, "GroupPause", false).await
    }
    async fn resume(&self, key: &str) -> Result<(), String> {
        self.edit(key, "GroupResume", false).await
    }
    async fn delete(&self, key: &str, delete_files: bool) -> Result<(), String> {
        self.edit(key, "delete", delete_files).await
    }

    async fn verify_completed_cleanup(&self, key: &str, content_path: &str) -> Result<i64, String> {
        if !self.owns_key(key) {
            return Err("This job does not belong to this Ryokan client.".into());
        }
        let jobs = self.native_jobs(true).await?;
        let mut matching = jobs.iter().filter(|(job, _)| job.dupe_key == key);
        let Some((job, true)) = matching.next() else {
            return Err("The completed NZBGet job is unavailable.".into());
        };
        if matching.next().is_some() {
            return Err("NZBGet did not verify a unique successful history item.".into());
        }
        let directories = self.verified_cleanup_dirs(job, content_path)?;

        // An unrelated app can use the same NZBGet instance. Inspect raw
        // paths across all categories before applying Ryokan's path mapping.
        for (other, history) in jobs.iter().filter(|(other, _)| other.dupe_key != key) {
            let mut paths = other.reported_dirs().peekable();
            if other.id == job.id
                || (!history
                    && paths.peek().is_none()
                    && (other.name.is_empty() || other.name.eq_ignore_ascii_case(&job.name)))
            {
                return Err("Another NZBGet job may share the completed job directory.".into());
            }
            for path in paths.map(Path::new) {
                if !valid_directory_path(path)
                    || directories
                        .iter()
                        .any(|own| path.starts_with(own) || own.starts_with(path))
                {
                    return Err("Another NZBGet job may share the completed job directory.".into());
                }
            }
        }
        Ok(job.id)
    }

    async fn finish_completed_cleanup(
        &self,
        key: &str,
        content_path: &str,
        nzbid: i64,
    ) -> Result<(), String> {
        if !self.owns_key(key) {
            return Err("This job does not belong to this Ryokan client.".into());
        }
        let history = self.rpc::<Vec<Job>>("history", json!([false])).await?;
        let mut matching = history.iter().filter(|job| job.dupe_key == key);
        let Some(job) = matching.next().filter(|job| job.id == nzbid) else {
            return Err("The completed NZBGet job ID changed.".into());
        };
        if matching.next().is_some()
            || history
                .iter()
                .any(|other| other.id == nzbid && other.dupe_key != key)
        {
            return Err("NZBGet did not verify a unique successful history item.".into());
        }
        self.verified_cleanup_dirs(job, content_path)?;
        self.edit_job("HistoryFinalDelete", nzbid).await
    }
}
#[cfg(test)]
mod wiremock_tests;

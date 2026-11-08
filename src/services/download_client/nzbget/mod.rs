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

impl Job {
    fn state(&self, history: bool) -> DownloadItemState {
        use DownloadItemState::*;
        if history {
            return match self.status.split('/').next() {
                Some("SUCCESS") if self.kind == "NZB" => PausedComplete,
                Some("FAILURE") => Errored,
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
        if !self.name.is_empty()
            && matches_name
            && path.is_absolute()
            && !path.components().any(|c| c == Component::ParentDir)
        {
            Some(path)
        } else {
            None
        }
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

    async fn jobs(&self) -> Result<Vec<(Job, bool)>, String> {
        let mut jobs = Vec::new();
        for (method, params, history) in [
            ("listgroups", json!([0]), false),
            ("history", json!([false]), true),
        ] {
            for job in self.rpc::<Vec<Job>>(method, params).await? {
                if job.id > 0
                    && job.category == self.category
                    && self.owns_key(&job.dupe_key)
                    && matches!(job.kind.as_str(), "NZB" | "URL")
                {
                    jobs.push((job, history));
                }
            }
        }
        Ok(jobs)
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
                // HistoryFinalDelete can erase a failed unpack even if the job
                // was later marked successful. NZBGet has no keep-data flag.
                let can_keep_files = job.state(history).is_complete()
                    && job.delete_status == "NONE"
                    && !matches!(job.par_status.as_str(), "" | "FAILURE")
                    && !matches!(job.unpack_status.as_str(), "" | "FAILURE" | "PASSWORD");
                if !delete_files && !can_keep_files {
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
        if self
            .rpc::<bool>("editqueue", json!([command, "", [job.id]]))
            .await?
        {
            Ok(())
        } else {
            Err(format!("NZBGet could not apply {command}."))
        }
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
}
#[cfg(test)]
mod wiremock_tests;

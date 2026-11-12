use super::*;
use crate::services::download_client::{AddOutcome, DownloadFile, DownloadItem, SelectiveOutcome};
use crate::services::source_numbering::tests::{lagoon_files, lagoon_proof};
use std::sync::Mutex;

struct StrictClient {
    files: Mutex<Vec<DownloadFile>>,
    events: Mutex<Vec<&'static str>>,
    added_urls: Mutex<Vec<String>>,
    fail_priority: bool,
    ignore_priority: bool,
    preexisting_on_add: bool,
}

impl StrictClient {
    fn fixture() -> Self {
        Self {
            files: Mutex::new(
                lagoon_files()
                    .into_iter()
                    .map(|name| DownloadFile {
                        name,
                        size: 100,
                        progress: 0.0,
                        wanted: true,
                    })
                    .collect(),
            ),
            events: Mutex::new(vec![]),
            added_urls: Mutex::new(vec![]),
            fail_priority: false,
            ignore_priority: false,
            preexisting_on_add: false,
        }
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl DownloadClient for StrictClient {
    async fn test(&self) -> Result<String, String> {
        Ok("ok".into())
    }
    async fn add_torrent(&self, _: &str, _: &str) -> Result<AddOutcome, String> {
        panic!("strict source must never use an unpaused whole add")
    }
    async fn add_torrent_with_file_filter(
        &self,
        _: &str,
        _: &str,
        _: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        panic!("strict source must never use legacy optional selection")
    }
    async fn add_torrent_paused(&self, url: &str, _: &str) -> Result<AddOutcome, String> {
        self.events.lock().unwrap().push("add paused");
        self.added_urls.lock().unwrap().push(url.to_string());
        Ok(if self.preexisting_on_add {
            AddOutcome::AlreadyPresent
        } else {
            AddOutcome::Added
        })
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(&self, _: &str) -> Result<Vec<DownloadFile>, String> {
        self.events.lock().unwrap().push("read files");
        Ok(self.files.lock().unwrap().clone())
    }
    async fn pause(&self, _: &str) -> Result<(), String> {
        self.events.lock().unwrap().push("pause");
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), String> {
        self.events.lock().unwrap().push("resume");
        Ok(())
    }
    async fn delete(&self, _: &str, _: bool) -> Result<(), String> {
        panic!("a held source must retain its data")
    }
    async fn set_file_wanted(
        &self,
        _: &str,
        indices: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        self.events
            .lock()
            .unwrap()
            .push(if wanted { "select" } else { "exclude" });
        if self.fail_priority {
            return Err("priority write failed".into());
        }
        if !self.ignore_priority {
            let mut files = self.files.lock().unwrap();
            for index in indices {
                files[*index].wanted = wanted;
            }
        }
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

fn mapping() -> ReleaseMapping {
    ReleaseMapping::from_bound_source(&lagoon_proof()).unwrap()
}

fn source_url() -> String {
    format!("magnet:?xt=urn:btih:{}", lagoon_proof().info_hash)
}

#[tokio::test]
async fn strict_source_grab_accepts_reordered_manifest_only_after_readback() {
    let client = StrictClient::fixture();
    client.files.lock().unwrap().reverse();
    let mapping = mapping();
    let prepared = add_bound(&client, &source_url(), &lagoon_proof().info_hash, &mapping)
        .await
        .unwrap();
    assert_eq!(prepared.info_hash, lagoon_proof().info_hash);
    assert_eq!(prepared.kept.as_ref().unwrap().len(), 12);
    assert!(prepared.newly_added);
    let events = client.events();
    assert_eq!(
        events,
        [
            "add paused",
            "pause",
            "read files",
            "exclude",
            "select",
            "read files"
        ]
    );
    verify_client_selection(&client, &prepared.info_hash, &mapping)
        .await
        .unwrap();
    assert!(
        !client.events().contains(&"resume"),
        "preparation alone cannot start a download"
    );
    prepared.resume_after_record(&client).await.unwrap();
    assert_eq!(client.events().last(), Some(&"resume"));
}

#[tokio::test]
async fn strict_source_grab_changed_unselected_manifest_cannot_resume_or_set_priorities() {
    let client = StrictClient::fixture();
    client.files.lock().unwrap()[0].name = "changed/unselected.mkv".into();
    assert!(
        add_bound(
            &client,
            &source_url(),
            &lagoon_proof().info_hash,
            &mapping()
        )
        .await
        .is_err()
    );
    let events = client.events();
    assert!(
        !events.contains(&"resume") && !events.contains(&"exclude") && !events.contains(&"select")
    );
    assert_eq!(events.last(), Some(&"pause"));
}

#[tokio::test]
async fn strict_source_grab_priority_error_and_failed_readback_never_resume() {
    for ignore in [false, true] {
        let mut client = StrictClient::fixture();
        client.fail_priority = !ignore;
        client.ignore_priority = ignore;
        assert!(
            add_bound(
                &client,
                &source_url(),
                &lagoon_proof().info_hash,
                &mapping()
            )
            .await
            .is_err()
        );
        assert!(!client.events().contains(&"resume"));
        assert_eq!(client.events().last(), Some(&"pause"));
    }
}

#[tokio::test]
async fn strict_source_grab_wrong_hash_is_rejected_before_any_client_action() {
    let client = StrictClient::fixture();
    assert!(
        add_bound(&client, &source_url(), &"f".repeat(40), &mapping())
            .await
            .is_err()
    );
    assert!(client.events().is_empty());
}

#[tokio::test]
async fn strict_source_grab_magnet_hash_must_match_the_proof_before_any_client_action() {
    let client = StrictClient::fixture();
    let wrong_url = format!("magnet:?xt=urn:btih:{}", "f".repeat(40));
    assert!(
        add_bound(&client, &wrong_url, &lagoon_proof().info_hash, &mapping())
            .await
            .is_err()
    );
    assert!(client.events().is_empty());
}

#[tokio::test]
async fn strict_source_grab_duplicate_add_cannot_change_a_job_missed_by_scoped_list() {
    let mut client = StrictClient::fixture();
    client.preexisting_on_add = true;
    assert!(
        add_bound(
            &client,
            &source_url(),
            &lagoon_proof().info_hash,
            &mapping()
        )
        .await
        .is_err()
    );
    let events = client.events();
    assert!(events.contains(&"add paused"));
    assert!(
        !events.contains(&"pause")
            && !events.contains(&"resume")
            && !events.contains(&"exclude")
            && !events.contains(&"select"),
        "{events:?}"
    );
}

#[tokio::test]
async fn strict_source_grab_http_input_dispatches_only_the_reviewed_hash_magnet() {
    let client = StrictClient::fixture();
    let _prepared = add_bound(
        &client,
        "https://example.test/changed-source.torrent",
        &lagoon_proof().info_hash,
        &mapping(),
    )
    .await
    .unwrap();
    assert_eq!(*client.added_urls.lock().unwrap(), vec![source_url()]);
    assert!(!client.events().contains(&"resume"));
}

#[tokio::test]
async fn strict_source_grab_preexisting_verified_selection_has_no_resume_after_record() {
    let mut client = StrictClient::fixture();
    client.preexisting_on_add = true;
    let proof = lagoon_proof();
    for file in client.files.lock().unwrap().iter_mut() {
        file.wanted = proof
            .selected_files
            .iter()
            .any(|selected| selected.name == file.name);
    }
    let prepared = add_bound(&client, &source_url(), &proof.info_hash, &mapping())
        .await
        .unwrap();
    assert!(!prepared.newly_added);
    prepared.resume_after_record(&client).await.unwrap();
    let events = client.events();
    assert!(
        !events.contains(&"pause")
            && !events.contains(&"resume")
            && !events.contains(&"exclude")
            && !events.contains(&"select"),
        "{events:?}"
    );
}

mod add;

use super::{Job, NzbgetClient};
use crate::services::download_client::{DownloadClient, DownloadItemState};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::body_partial_json};

#[test]
fn exact_copy_history_is_removed_without_an_import_path() {
    let job = Job {
        kind: "NZB".into(),
        name: "episode".into(),
        status: "DELETED/COPY".into(),
        dest_dir: "/completed/ryokan/episode".into(),
        ..Default::default()
    };
    let item = job.item(true, "/mount");
    assert_eq!(item.state_kind, DownloadItemState::Removed);
    assert_eq!(item.state, "DELETED/COPY");
    assert!(item.content_path.is_empty());
    assert!(!item.state_kind.is_complete());
    assert!(!item.state_kind.is_errored());
    assert!(!item.seeding_done);
    assert_ne!(job.state(false), DownloadItemState::Removed);
}

#[test]
fn other_history_warnings_still_require_review() {
    for status in [
        "WARNING/SPACE",
        "WARNING/PASSWORD",
        "WARNING/REPAIRABLE",
        "DELETED/DUPE",
        "DELETED/GOOD",
        "UNKNOWN",
    ] {
        let job = Job {
            kind: "NZB".into(),
            status: status.into(),
            ..Default::default()
        };
        assert_eq!(job.state(true), DownloadItemState::Warning, "{status}");
    }
}

#[test]
fn only_success_in_a_job_directory_is_ready_for_import() {
    let mut job = Job {
        kind: "NZB".into(),
        name: "episode".into(),
        status: "SUCCESS/UNPACK".into(),
        dest_dir: "/completed/ryokan/episode".into(),
        ..Default::default()
    };
    assert_eq!(job.item(true, "/mount").content_path, "/mount/episode");
    job.final_dir = "/completed/ryokan".into();
    let shared = job.item(true, "/mount");
    assert_eq!(shared.state_kind, DownloadItemState::Warning);
    assert!(shared.content_path.is_empty());

    job.final_dir.clear();
    job.status = "WARNING/SPACE".into();
    let warning = job.item(true, "/mount");
    assert_eq!(warning.state_kind, DownloadItemState::Warning);
    assert!(warning.content_path.is_empty());
}

fn cleanup_key() -> String {
    format!("nzbget-1-{}", "a".repeat(96))
}

fn successful_cleanup_job() -> Value {
    json!({
        "NZBID": 41, "NZBName": "Episode", "Kind": "NZB", "Category": "ryokan",
        "DupeKey": cleanup_key(), "Status": "SUCCESS/PAR", "DeleteStatus": "NONE",
        "ParStatus": "SUCCESS", "UnpackStatus": "NONE",
        "DestDir": "/completed/ryokan/Episode", "FinalDir": "/exports/Episode"
    })
}

async fn mock_rpc(server: &MockServer, method: &str, params: Value, result: Value) {
    Mock::given(body_partial_json(json!({"method":method,"params":params})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":result})))
        .expect(1)
        .mount(server)
        .await;
}

async fn verify_cleanup(queue: Vec<Value>, history: Vec<Value>) -> Result<i64, String> {
    let server = MockServer::start().await;
    mock_rpc(&server, "listgroups", json!([0]), json!(queue)).await;
    mock_rpc(&server, "history", json!([true]), json!(history)).await;
    NzbgetClient::new(1, &server.uri(), "", "", "ryokan", "/mount")
        .verify_completed_cleanup(&cleanup_key(), "/mount/Episode")
        .await
}

#[tokio::test]
async fn completed_cleanup_rejects_success_history_with_unsafe_native_flags() {
    for (field, value) in [
        ("DeleteStatus", "MANUAL"),
        ("ParStatus", "FAILURE"),
        ("ParStatus", "REPAIR_POSSIBLE"),
        ("UnpackStatus", "SPACE"),
        ("UnpackStatus", "PASSWORD"),
        ("UnpackStatus", ""),
        ("Status", "SUCCESS/MARK"),
    ] {
        let mut job = successful_cleanup_job();
        job[field] = json!(value);
        let error = verify_cleanup(Vec::new(), vec![job]).await.unwrap_err();
        assert!(
            error.contains("unique successful"),
            "{field}={value}: {error}"
        );
    }
}

#[tokio::test]
async fn completed_cleanup_rejects_foreign_native_jobs_with_overlapping_or_unknown_paths() {
    for (queued, field, path) in [
        (true, "DestDir", "/completed/ryokan/Episode"),
        (true, "DestDir", "/completed/ryokan/Episode/parts"),
        (false, "FinalDir", "/exports"),
        (true, "DestDir", ""),
    ] {
        let mut foreign = json!({
            "NZBID": 42, "NZBName": "Episode", "Kind": "NZB", "Category": "sonarr"
        });
        foreign[field] = json!(path);
        let mut history = vec![successful_cleanup_job()];
        let queue = if queued {
            vec![foreign]
        } else {
            history.push(foreign);
            Vec::new()
        };
        let error = verify_cleanup(queue, history).await.unwrap_err();
        assert!(error.contains("may share"), "{field}={path}: {error}");
    }
}

#[tokio::test]
async fn completed_cleanup_accepts_exclusively_owned_paths_with_unrelated_jobs() {
    let queued = json!({
        "NZBID": 42, "NZBName": "Episode", "Kind": "NZB", "Category": "sonarr",
        "DestDir": "/completed/sonarr/Episode"
    });
    let hidden = json!({
        "NZBID": 43, "NZBName": "Episode", "Kind": "DUP", "Status": "SUCCESS/HIDDEN"
    });
    assert_eq!(
        verify_cleanup(vec![queued], vec![successful_cleanup_job(), hidden])
            .await
            .unwrap(),
        41
    );
}

#[tokio::test]
async fn completed_cleanup_only_deletes_the_original_verified_native_id() {
    for current_id in [41, 42] {
        let server = MockServer::start().await;
        let original = successful_cleanup_job();
        let mut current = original.clone();
        current["NZBID"] = json!(current_id);
        mock_rpc(&server, "listgroups", json!([0]), json!([])).await;
        mock_rpc(&server, "history", json!([true]), json!([original])).await;
        mock_rpc(&server, "history", json!([false]), json!([current])).await;
        if current_id == 41 {
            mock_rpc(
                &server,
                "editqueue",
                json!(["HistoryFinalDelete", "", [41]]),
                json!(true),
            )
            .await;
        } else {
            Mock::given(body_partial_json(json!({"method":"editqueue"})))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
        }
        let client = NzbgetClient::new(1, &server.uri(), "", "", "ryokan", "/mount");
        let key = cleanup_key();
        let path = "/mount/Episode";
        let verified_id = client.verify_completed_cleanup(&key, path).await.unwrap();
        assert_eq!(verified_id, 41);
        let result = client
            .finish_completed_cleanup(&key, path, verified_id)
            .await;
        if current_id == 41 {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().contains("job ID changed"));
        }
    }
}

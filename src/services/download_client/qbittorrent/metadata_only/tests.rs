use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex, atomic::AtomicUsize};
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "91a564c6d0f605344b67cef81eeab3199d61872b";

type Marker = Arc<Mutex<Option<String>>>;

async fn fixture() -> (MockServer, QbitClient, Marker) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;
    let client = QbitClient::new(&server.uri(), "private", "fixture", "ryokan-test");
    (server, client, Arc::new(Mutex::new(None)))
}

async fn versions(server: &MockServer, app: &str, api: &str) {
    for (endpoint, body) in [("version", app), ("webapiVersion", api)] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v2/app/{endpoint}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }
}

fn job(state: &str, payload: i64) -> serde_json::Value {
    json!({"hash":HASH,"state":state,"progress":0.0,"downloaded":payload,"downloaded_session":payload,"tags":""})
}

async fn absent_then(server: &MockServer, later: serde_json::Value, marker: &Marker) {
    let marker = marker.clone();
    let calls = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(query_param("hashes", HASH))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                json!([])
            } else {
                let mut later = later.clone();
                later["tags"] = json!(marker.lock().unwrap().as_deref().unwrap_or(""));
                json!([later])
            })
        })
        .mount(server)
        .await;
}

async fn native_add(server: &MockServer, body: &str, marker: &Marker) {
    let saved_marker = marker.clone();
    let response_body = body.to_string();
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .and(body_string_contains(format!(
            "urls=magnet%3A%3Fxt%3Durn%3Abtih%3A{HASH}"
        )))
        .and(body_string_contains("stopCondition=MetadataReceived"))
        .and(body_string_contains("stopped=false"))
        .respond_with(move |request: &wiremock::Request| {
            let body = std::str::from_utf8(&request.body).unwrap();
            let value = body
                .split('&')
                .find_map(|field| field.strip_prefix("tags="))
                .unwrap();
            let nonce = value.strip_prefix("ryokan-metadata-").unwrap();
            assert_eq!(nonce.len(), 32);
            assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
            *saved_marker.lock().unwrap() = Some(value.to_string());
            ResponseTemplate::new(200).set_body_string(response_body.clone())
        })
        .expect(1)
        .mount(server)
        .await;
    let removal_marker = marker.clone();
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/removeTags"))
        .respond_with(move |request: &wiremock::Request| {
            let body = std::str::from_utf8(&request.body).unwrap();
            assert!(
                body.split('&')
                    .any(|field| field == format!("hashes={HASH}"))
            );
            assert!(body.split('&').any(|field| field
                == format!("tags={}", removal_marker.lock().unwrap().as_ref().unwrap())));
            ResponseTemplate::new(200)
        })
        .mount(server)
        .await;
}

async fn assert_no_control(server: &MockServer) {
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| {
        ![
            "/api/v2/torrents/start",
            "/api/v2/torrents/resume",
            "/api/v2/torrents/stop",
            "/api/v2/torrents/pause",
            "/api/v2/torrents/filePrio",
            "/api/v2/torrents/delete",
            "/api/v2/torrents/removeTags",
            "/api/v2/torrents/deleteTags",
        ]
        .contains(&request.url.path())
    }));
}

#[test]
fn reviewed_metadata_accepts_only_the_supported_native_contract() {
    assert!(supported_native_contract("v5.0.0", "2.11.0"));
    assert!(supported_native_contract("v5.1.4\n", "2.11.4"));
    for (app, api) in [
        ("4.6.3", "2.8.15"),
        ("4.6.4", "2.11.0"),
        ("5.1.4", "2.8.15"),
        ("5.1.4alpha1", "2.11.4"),
        ("vv5.1.4", "2.11.4"),
        ("6.0.0", "3.0.0"),
        ("", "2.11.4"),
        ("5.1.4", "not a version"),
    ] {
        assert!(!supported_native_contract(app, api), "{app}/{api}");
    }
}

#[tokio::test]
async fn reviewed_metadata_native_add_returns_stopped_without_payload_or_priority_writes() {
    let (server, client, marker) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("stoppedDL", 0), &marker).await;
    native_add(&server, "Ok.", &marker).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name":"source.mkv","size":1000,"progress":0.0,"priority":1}
        ])))
        .mount(&server)
        .await;
    assert_eq!(
        client.add_torrent_metadata_only(HASH).await.unwrap(),
        AddOutcome::Added
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/v2/torrents/removeTags")
            .count(),
        1
    );
    assert!(requests.iter().all(|request| {
        ![
            "/api/v2/torrents/start",
            "/api/v2/torrents/resume",
            "/api/v2/torrents/stop",
            "/api/v2/torrents/pause",
            "/api/v2/torrents/filePrio",
            "/api/v2/torrents/delete",
            "/api/v2/torrents/deleteTags",
        ]
        .contains(&request.url.path())
    }));
    let requests = server.received_requests().await.unwrap();
    let add = requests
        .iter()
        .find(|r| r.url.path() == "/api/v2/torrents/add")
        .unwrap();
    let form = String::from_utf8_lossy(&add.body);
    assert!(!form.contains("tr%3D") && !form.contains("ws%3D"));
}

#[tokio::test]
async fn reviewed_metadata_existing_empty_stopped_job_is_never_taken_over() {
    let (server, client, _marker) = fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(query_param("hashes", HASH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([job("stoppedDL", 0)])))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client.add_torrent_metadata_only(HASH).await.unwrap(),
        AddOutcome::AlreadyPresent
    );
    assert_no_control(&server).await;
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| matches!(r.url.path(), "/api/v2/auth/login" | "/api/v2/torrents/info"))
    );
}

#[tokio::test]
async fn reviewed_metadata_absence_lookup_errors_cannot_authorize_an_add() {
    for response in [
        ResponseTemplate::new(500),
        ResponseTemplate::new(200).set_body_string("invalid"),
    ] {
        let (server, client, _marker) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(response)
            .mount(&server)
            .await;
        assert!(client.add_torrent_metadata_only(HASH).await.is_err());
        assert_no_control(&server).await;
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == "/api/v2/torrents/add")
        );
    }
}

#[tokio::test]
async fn reviewed_metadata_unsupported_or_unknown_version_holds_before_add() {
    for (app, api) in [
        ("4.6.4", "2.8.15"),
        ("5.1.4", "2.8.15"),
        ("5.1.4alpha1", "2.11.4"),
    ] {
        let (server, client, marker) = fixture().await;
        versions(&server, app, api).await;
        absent_then(&server, job("stoppedDL", 0), &marker).await;
        assert!(client.add_torrent_metadata_only(HASH).await.is_err());
        assert_no_control(&server).await;
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == "/api/v2/torrents/add")
        );
    }
}

#[tokio::test]
async fn reviewed_metadata_concurrent_duplicate_is_preserved_without_controls() {
    let (server, client, marker) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("downloading", 0), &marker).await;
    native_add(&server, "Fails.", &marker).await;
    assert_eq!(
        client.add_torrent_metadata_only(HASH).await.unwrap(),
        AddOutcome::AlreadyPresent
    );
    assert_no_control(&server).await;
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/api/v2/torrents/files")
    );
}

#[tokio::test]
async fn reviewed_metadata_timeout_stops_only_the_acknowledged_new_job_without_resume() {
    let (server, client, marker) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("metaDL", 0), &marker).await;
    native_add(&server, "Ok.", &marker).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .and(body_string_contains(format!("hashes={HASH}")))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let result = client
        .prepare_reviewed_metadata_with_timeout(HASH, Duration::from_millis(50))
        .await;
    assert!(result.unwrap_err().contains("timed out"));
    assert!(server.received_requests().await.unwrap().iter().all(|r| {
        ![
            "/api/v2/torrents/start",
            "/api/v2/torrents/resume",
            "/api/v2/torrents/filePrio",
            "/api/v2/torrents/delete",
            "/api/v2/torrents/removeTags",
            "/api/v2/torrents/deleteTags",
        ]
        .contains(&r.url.path())
    }));
}

#[tokio::test]
async fn reviewed_metadata_unconfirmed_add_cannot_mutate_an_unowned_job() {
    let (server, client, marker) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("downloading", 0), &marker).await;
    native_add(&server, "unexpected acknowledgement", &marker).await;
    assert!(client.add_torrent_metadata_only(HASH).await.is_err());
    assert_no_control(&server).await;
}

#[tokio::test]
async fn reviewed_metadata_payload_or_readback_failure_stops_without_fallback() {
    for payload in [0, 1] {
        let (server, client, marker) = fixture().await;
        versions(&server, "v5.1.4", "2.11.4").await;
        absent_then(&server, job("stoppedDL", payload), &marker).await;
        native_add(&server, "Ok.", &marker).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/files"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/stop"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        assert!(client.add_torrent_metadata_only(HASH).await.is_err());
        assert!(server.received_requests().await.unwrap().iter().all(|r| {
            ![
                "/api/v2/torrents/start",
                "/api/v2/torrents/resume",
                "/api/v2/torrents/filePrio",
            ]
            .contains(&r.url.path())
        }));
    }
}

#[tokio::test]
async fn reviewed_metadata_invalid_identity_is_refused_before_any_request() {
    let (server, client, _marker) = fixture().await;
    for hash in ["", "all", &"a".repeat(64), &"z".repeat(40)] {
        assert!(client.add_torrent_metadata_only(hash).await.is_err());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn structured_ack() -> String {
    json!({"success_count":1,"failure_count":0,"pending_count":0,"added_torrent_ids":[HASH]})
        .to_string()
}

async fn ready_files(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name":"source.mkv","size":1000,"progress":0.0,"priority":1}
        ])))
        .mount(server)
        .await;
}

async fn marker_states(server: &MockServer, marker: &Marker, mode: &'static str) {
    let marker = marker.clone();
    let calls = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(query_param("hashes", HASH))
        .respond_with(move |_: &wiremock::Request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return ResponseTemplate::new(200).set_body_json(json!([]));
            }
            if mode == "owned_then_unreadable" && call >= 2 {
                return ResponseTemplate::new(500);
            }
            let mut result = job("stoppedDL", 0);
            let own = marker.lock().unwrap().clone().unwrap();
            match mode {
                "foreign" => result["tags"] = json!("someone-elses-tag"),
                "missing" => {
                    result.as_object_mut().unwrap().remove("tags");
                }
                "empty" => result["tags"] = json!(""),
                "substring" => result["tags"] = json!(format!("{own}-suffix")),
                "owned_then_foreign" | "owned_then_unreadable" => {
                    result["tags"] = json!(if call == 1 {
                        own.as_str()
                    } else {
                        "someone-elses-tag"
                    });
                }
                "owned_then_cleanup_foreign" => {
                    result["tags"] = json!(if call == 1 {
                        own.as_str()
                    } else {
                        "someone-elses-tag"
                    })
                }
                _ => panic!("unknown test marker state"),
            }
            ResponseTemplate::new(200).set_body_json(json!([result]))
        })
        .mount(server)
        .await;
}

#[test]
fn reviewed_metadata_acknowledgement_is_exact_and_not_an_ownership_witness() {
    assert!(acknowledged_add("Ok.", HASH));
    assert!(acknowledged_add(&structured_ack(), HASH));
    for body in [
        "",
        "OK",
        "Fails.",
        "{}",
        "not-json",
        "{\"success_count\":1}",
    ] {
        assert!(!acknowledged_add(body, HASH), "{body}");
    }
    let original: serde_json::Value = serde_json::from_str(&structured_ack()).unwrap();
    for (field, value) in [
        ("success_count", json!(0)),
        ("success_count", json!(2)),
        ("failure_count", json!(1)),
        ("pending_count", json!(1)),
        ("added_torrent_ids", json!([])),
        ("added_torrent_ids", json!(["f".repeat(40)])),
        ("added_torrent_ids", json!([HASH, HASH])),
        ("unknown_contract", json!(true)),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        assert!(!acknowledged_add(&changed.to_string(), HASH), "{changed}");
    }
}

#[tokio::test]
async fn reviewed_metadata_53_json_requires_a_fresh_marker_and_removes_only_its_own_tag() {
    let mut used = std::collections::HashSet::new();
    for _ in 0..2 {
        let (server, client, marker) = fixture().await;
        versions(&server, "v5.3.0", "2.16.2").await;
        absent_then(&server, job("stoppedDL", 0), &marker).await;
        native_add(&server, &structured_ack(), &marker).await;
        ready_files(&server).await;
        assert_eq!(
            client.add_torrent_metadata_only(HASH).await.unwrap(),
            AddOutcome::Added
        );
        assert!(
            used.insert(marker.lock().unwrap().clone().unwrap()),
            "each add needs a new random marker"
        );
        let requests = server.received_requests().await.unwrap();
        let controls: Vec<_> = requests
            .iter()
            .filter(|r| {
                r.method.as_str() == "POST"
                    && !matches!(r.url.path(), "/api/v2/auth/login" | "/api/v2/torrents/add")
            })
            .collect();
        assert_eq!(controls.len(), 1);
        assert_eq!(controls[0].url.path(), "/api/v2/torrents/removeTags");
    }
}

#[tokio::test]
async fn reviewed_metadata_success_ack_cannot_take_over_a_missing_or_foreign_marker() {
    for mode in [
        "foreign",
        "missing",
        "empty",
        "substring",
        "owned_then_foreign",
        "owned_then_unreadable",
    ] {
        let (server, client, marker) = fixture().await;
        versions(&server, "v5.3.0", "2.16.2").await;
        marker_states(&server, &marker, mode).await;
        native_add(&server, &structured_ack(), &marker).await;
        ready_files(&server).await;
        assert!(
            client.add_torrent_metadata_only(HASH).await.is_err(),
            "{mode}"
        );
        assert_no_control(&server).await;
    }
}

#[tokio::test]
async fn reviewed_metadata_readback_failure_cannot_stop_a_job_that_lost_the_marker() {
    let (server, client, marker) = fixture().await;
    versions(&server, "v5.3.0", "2.16.2").await;
    marker_states(&server, &marker, "owned_then_cleanup_foreign").await;
    native_add(&server, &structured_ack(), &marker).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(client.add_torrent_metadata_only(HASH).await.is_err());
    assert_no_control(&server).await;
}

#[tokio::test]
async fn reviewed_metadata_unacknowledged_json_cannot_authorize_marker_cleanup() {
    for body in [
        json!({"success_count":1,"failure_count":0,"pending_count":1,"added_torrent_ids":[HASH]}),
        json!({"success_count":1,"failure_count":0,"pending_count":0,"added_torrent_ids":["f".repeat(40)]}),
    ] {
        let (server, client, marker) = fixture().await;
        versions(&server, "v5.3.0", "2.16.2").await;
        absent_then(&server, job("stoppedDL", 0), &marker).await;
        native_add(&server, &body.to_string(), &marker).await;
        assert!(client.add_torrent_metadata_only(HASH).await.is_err());
        assert_no_control(&server).await;
    }
}

#[tokio::test]
async fn reviewed_metadata_53_conflict_preserves_the_existing_job() {
    let (server, client, marker) = fixture().await;
    versions(&server, "v5.3.0", "2.16.2").await;
    absent_then(&server, job("downloading", 0), &marker).await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client.add_torrent_metadata_only(HASH).await.unwrap(),
        AddOutcome::AlreadyPresent
    );
    assert_no_control(&server).await;
}

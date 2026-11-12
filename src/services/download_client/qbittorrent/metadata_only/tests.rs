use super::*;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "91a564c6d0f605344b67cef81eeab3199d61872b";

async fn fixture() -> (MockServer, QbitClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;
    let client = QbitClient::new(&server.uri(), "private", "fixture", "ryokan-test");
    (server, client)
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
    json!({"hash":HASH,"state":state,"progress":0.0,"downloaded":payload,"downloaded_session":payload})
}

async fn absent_then(server: &MockServer, later: serde_json::Value) {
    let calls = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(query_param("hashes", HASH))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                json!([])
            } else {
                json!([later.clone()])
            })
        })
        .mount(server)
        .await;
}

async fn native_add(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .and(body_string_contains(format!(
            "urls=magnet%3A%3Fxt%3Durn%3Abtih%3A{HASH}"
        )))
        .and(body_string_contains("stopCondition=MetadataReceived"))
        .and(body_string_contains("stopped=false"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
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
    let (server, client) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("stoppedDL", 0)).await;
    native_add(&server, "Ok.").await;
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
    assert_no_control(&server).await;
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
    let (server, client) = fixture().await;
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
        let (server, client) = fixture().await;
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
        let (server, client) = fixture().await;
        versions(&server, app, api).await;
        absent_then(&server, job("stoppedDL", 0)).await;
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
    let (server, client) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("downloading", 0)).await;
    native_add(&server, "Fails.").await;
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
    let (server, client) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("metaDL", 0)).await;
    native_add(&server, "Ok.").await;
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
        ]
        .contains(&r.url.path())
    }));
}

#[tokio::test]
async fn reviewed_metadata_unconfirmed_add_cannot_mutate_an_unowned_job() {
    let (server, client) = fixture().await;
    versions(&server, "v5.1.4", "2.11.4").await;
    absent_then(&server, job("downloading", 0)).await;
    native_add(&server, "unexpected acknowledgement").await;
    assert!(client.add_torrent_metadata_only(HASH).await.is_err());
    assert_no_control(&server).await;
}

#[tokio::test]
async fn reviewed_metadata_payload_or_readback_failure_stops_without_fallback() {
    for payload in [0, 1] {
        let (server, client) = fixture().await;
        versions(&server, "v5.1.4", "2.11.4").await;
        absent_then(&server, job("stoppedDL", payload)).await;
        native_add(&server, "Ok.").await;
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
    let (server, client) = fixture().await;
    for hash in ["", "all", &"a".repeat(64), &"z".repeat(40)] {
        assert!(client.add_torrent_metadata_only(hash).await.is_err());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

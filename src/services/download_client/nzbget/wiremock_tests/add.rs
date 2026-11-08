use crate::services::download_client::{
    nzbget::NzbgetClient, test_helpers::assert_refuses_non_url_releases,
};

#[tokio::test]
async fn rejects_non_url_releases_before_any_request() {
    let server = wiremock::MockServer::start().await;
    let client = NzbgetClient::new(1, &server.uri(), "", "", "ryokan", "");
    assert_refuses_non_url_releases(&client, &server).await;
}

/// Run only against a disposable localhost daemon with a "ryokan-test" category.
#[tokio::test]
#[ignore = "requires an isolated local NZBGet daemon"]
async fn live_smoke() {
    use crate::services::download_client::{AddOutcome, DownloadClient};
    use wiremock::{Mock, ResponseTemplate, matchers::path};
    let Ok(port) = std::env::var("RYOKAN_NZBGET_E2E_PORT") else {
        return;
    };
    let port: u16 = port.parse().unwrap();
    let client = NzbgetClient::new(
        1,
        &format!("http://127.0.0.1:{port}"),
        "test",
        "fixture",
        "ryokan-test",
        "",
    );
    assert!(client.test().await.unwrap().starts_with("NZBGet "));
    let indexer = wiremock::MockServer::start().await;
    let nzb = r#"<?xml version="1.0"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"><file poster="fixture" date="1" subject="&quot;fixture.mkv&quot; yEnc (1/1)"><groups><group>alt.test</group></groups><segments><segment bytes="42" number="1">fixture@invalid</segment></segments></file></nzb>"#;
    Mock::given(path("/fixture.nzb"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Disposition", "attachment; filename=Fixture.nzb")
                .set_body_raw(nzb, "application/x-nzb"),
        )
        .mount(&indexer)
        .await;
    let url = format!("{}/fixture.nzb", indexer.uri());
    let (outcome, key) = client.add_torrent_returning_id(&url, "").await.unwrap();
    assert_eq!(outcome, AddOutcome::Added);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if client
                .list_scoped()
                .await
                .unwrap()
                .iter()
                .any(|item| item.hash == key && item.name == "Fixture")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("daemon fetched the fixture NZB");
    let replay = client.add_torrent_returning_id(&url, "").await.unwrap();
    assert_eq!(replay, (AddOutcome::AlreadyPresent, key.clone()));
    client.pause(&key).await.unwrap();
    client.resume(&key).await.unwrap();
    client.delete(&key, true).await.unwrap();
    assert!(
        client
            .list_scoped()
            .await
            .unwrap()
            .iter()
            .all(|item| item.hash != key)
    );
}

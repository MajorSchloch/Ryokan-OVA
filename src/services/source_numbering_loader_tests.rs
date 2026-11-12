//! Mocked publication and private database evidence for the loader boundary.
use super::*;
use crate::models::{metadata_cache, source_numbering as decisions};
use crate::services::auto_search::SearchTarget;
use crate::test_support::{in_memory_pool, seed_series};
use std::sync::{Arc, LazyLock, RwLock};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TITLE: &str = "Dr. Stone: Science Future";
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct NyaaOrigin(Option<std::ffi::OsString>);
impl NyaaOrigin {
    fn set(origin: &str) -> Self {
        let old = std::env::var_os("RYOKAN_NYAA_API_BASE");
        // nextest isolates each test; this lock also serializes this module.
        unsafe {
            std::env::set_var("RYOKAN_NYAA_API_BASE", origin);
        }
        Self(old)
    }
}
impl Drop for NyaaOrigin {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(old) => std::env::set_var("RYOKAN_NYAA_API_BASE", old),
                None => std::env::remove_var("RYOKAN_NYAA_API_BASE"),
            }
        }
    }
}

fn detail(id: i64, title: &str, count: i32) -> AnimeDetail {
    AnimeDetail {
        release_date: None,
        id,
        id_mal: None,
        title_romaji: title.into(),
        title_english: title.into(),
        title_native: String::new(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(count),
        duration: Some(24),
        season: String::new(),
        season_year: Some(2025),
        end_year: Some(2025),
        is_adult: false,
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

async fn cache(db: &SqlitePool, detail: &AnimeDetail) {
    metadata_cache::upsert_provider(db, detail.id, None, detail)
        .await
        .unwrap();
}

async fn stone(db: &SqlitePool, complete: bool) {
    cache(db, &detail(172019, TITLE, 12)).await;
    cache(db, &detail(189117, "Dr. Stone: Science Future Part 2", 12)).await;
    if complete {
        cache(db, &detail(199221, "Dr. Stone: Science Future Part 3", 13)).await;
    }
}

async fn colliding_lagoon(db: &SqlitePool) {
    let mut root = detail(889, "Black Lagoon", 12);
    // A second canonical provider title, not a fuzzy/synonym alias.
    root.title_english = TITLE.into();
    cache(db, &root).await;
    cache(db, &detail(1519, "Black Lagoon: The Second Barrage", 12)).await;
}

fn filename(number: i32) -> String {
    format!("[Fixture] {TITLE} - {number:02} [1080p].mkv")
}

fn listing(files: &[String]) -> String {
    let leaves = files
        .iter()
        .map(|name| {
            format!(
                "<li><i class=\"fa fa-file\"></i>{}</li>",
                name.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"panel\"><div class=\"panel-heading\"><h3 class=\"panel-title\">{TITLE} published source</h3></div><div class=\"panel-footer\"><a class=\"card-footer-item\" href=\"magnet:?xt=urn:btih:{HASH}\">Magnet</a></div></div><div class=\"panel\"><div class=\"panel-heading\"><h3 class=\"panel-title\">File list</h3></div><div class=\"torrent-file-list panel-body\"><ul>{leaves}</ul></div></div>"
    )
}

async fn publication(files: Vec<String>) -> (MockServer, Arc<RwLock<Vec<String>>>) {
    let server = MockServer::start().await;
    let current = Arc::new(RwLock::new(files));
    let response = current.clone();
    Mock::given(method("GET"))
        .and(path("/view/123"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_string(listing(&response.read().unwrap()))
        })
        .mount(&server)
        .await;
    (server, current)
}

fn candidate(server: &MockServer, number: i32) -> nyaa::SearchResult {
    serde_json::from_value(serde_json::json!({
        "title":format!("[Fixture] {TITLE} - {number:02} [1080p]"),
        "link":format!("{}/view/123",server.uri()),"magnet":format!("magnet:?xt=urn:btih:{HASH}"),"torrent":"",
        "size":"1GiB","size_bytes":1073741824,"seeders":10,"leechers":0,"downloads":1,
        "group":"Fixture","resolution":"1080","quality_label":"WEB-1080p","source":"Web","web_kind":"WEB-DL",
        "is_remux":false,"is_bdmv":false,"is_batch":false,"is_trusted":true,"score":100,"info_hash":HASH,
    })).unwrap()
}

async fn no_admission_rows(db: &SqlitePool) {
    for sql in [
        "SELECT COUNT(*) FROM source_numbering_decisions",
        "SELECT COUNT(*) FROM grabbed_torrents",
        "SELECT COUNT(*) FROM pending_grabs",
    ] {
        let count = sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(db)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "unproven evidence cannot create admission rows: {sql}"
        );
    }
}

async fn saved_bytes(db: &SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT proof_json,fingerprint FROM source_numbering_decisions ORDER BY id")
        .fetch_all(db)
        .await
        .unwrap()
}

async fn save_automatic_fixture(
    db: &SqlitePool,
    server: &MockServer,
    source: i32,
    owner: i64,
    episode: i32,
) -> BoundSourceProof {
    let resolver = Resolver::default();
    let mut candidate = candidate(server, source);
    assert!(
        resolver
            .apply(db, owner, &SearchTarget::Episode(episode), &mut candidate)
            .await
            .unwrap()
    );
    let proof = decisions::lookup_exact(db, HASH, owner)
        .await
        .unwrap()
        .unwrap()
        .proof;
    assert_eq!(proof.version, 2);
    assert!(matches!(
        &proof.provenance,
        Some(ProofProvenance::AutomaticUnique { .. })
    ));
    assert_eq!(proof.selected_files.len(), 1);
    assert_eq!(proof.selected_files[0].source_first, source);
    assert_eq!(proof.selected_files[0].target_first, episode);
    for sql in [
        "SELECT COUNT(*) FROM grabbed_torrents",
        "SELECT COUNT(*) FROM pending_grabs",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(db)
                .await
                .unwrap(),
            0
        );
    }
    proof
}

#[tokio::test]
async fn missing_connected_canonical_metadata_holds_before_receipt_or_job_admission() {
    let _lock = ENV_LOCK.lock().await;
    let db = in_memory_pool().await;
    stone(&db, false).await;
    let (server, _) = publication(vec![filename(25)]).await;
    let _origin = NyaaOrigin::set(&server.uri());
    let mut candidate = candidate(&server, 25);
    let reason = Resolver::default()
        .apply(&db, 199221, &SearchTarget::Episode(1), &mut candidate)
        .await
        .unwrap_err();
    assert!(
        reason.contains("199221") && reason.contains("cached metadata"),
        "{reason}"
    );
    assert!(candidate.release_mapping.is_none());
    no_admission_rows(&db).await;
}

#[tokio::test]
async fn stale_independent_alias_competitor_is_not_removed_from_discovery() {
    let _lock = ENV_LOCK.lock().await;
    let db = in_memory_pool().await;
    stone(&db, true).await;
    colliding_lagoon(&db).await;
    sqlx::query("UPDATE provider_metadata_cache SET cached_at=datetime('now','-8 days') WHERE provider_id=889").execute(&db).await.unwrap();
    let (server, _) = publication(vec![filename(15)]).await;
    let _origin = NyaaOrigin::set(&server.uri());
    let mut candidate = candidate(&server, 15);
    let reason = Resolver::default()
        .apply(&db, 189117, &SearchTarget::Episode(3), &mut candidate)
        .await
        .unwrap_err();
    assert!(reason.contains("stale"), "{reason}");
    assert!(candidate.release_mapping.is_none());
    no_admission_rows(&db).await;
}

#[tokio::test]
async fn identical_canonical_aliases_find_disconnected_roots_without_wanted_owner_bias() {
    let _lock = ENV_LOCK.lock().await;
    let db = in_memory_pool().await;
    stone(&db, true).await;
    colliding_lagoon(&db).await;
    let (server, _) = publication(vec![filename(15)]).await;
    let _origin = NyaaOrigin::set(&server.uri());
    let resolver = Resolver::default();
    for owner in [189117, 1519] {
        let mut candidate = candidate(&server, 15);
        let reason = resolver
            .apply(&db, owner, &SearchTarget::Episode(3), &mut candidate)
            .await
            .unwrap_err();
        assert!(
            reason.contains("multiple valid source roots"),
            "owner{owner}: {reason}"
        );
        assert!(candidate.release_mapping.is_none());
    }
    no_admission_rows(&db).await;
}

#[tokio::test]
async fn persisted_automatic_source_refresh_holds_changed_semantics_or_manifest_without_rewrite() {
    let _lock = ENV_LOCK.lock().await;
    for change in [
        "canonical_alias",
        "identity_rule",
        "publication_manifest",
        "new_competing_alias",
    ] {
        let db = in_memory_pool().await;
        stone(&db, true).await;
        if change == "new_competing_alias" {
            cache(&db, &detail(889, "Black Lagoon", 12)).await;
            cache(&db, &detail(1519, "Black Lagoon: The Second Barrage", 12)).await;
        }
        let series_id = seed_series(&db, 189117, "Dr. Stone: Science Future Part 2").await;
        let (server, manifest) = publication(vec![filename(15)]).await;
        let _origin = NyaaOrigin::set(&server.uri());
        let proof = save_automatic_fixture(&db, &server, 15, 189117, 3).await;
        let before = saved_bytes(&db).await;
        match change {
            "canonical_alias" => {
                let mut root = detail(172019, TITLE, 12);
                root.title_native = "Additional Canonical Source".into();
                cache(&db, &root).await;
            }
            "identity_rule" => cache(&db, &detail(172019, TITLE, 11)).await,
            "publication_manifest" => manifest.write().unwrap().push("checksums.sha256".into()),
            "new_competing_alias" => colliding_lagoon(&db).await,
            _ => unreachable!(),
        }
        assert!(refresh(&db, &proof).await.is_err(), "{change}");
        assert!(
            crate::services::release_numbering::bound_mapping_for_source(&db, HASH, 189117)
                .await
                .is_err(),
            "{change}"
        );
        let selected: Vec<_> = proof
            .selected_files
            .iter()
            .map(|file| file.name.clone())
            .collect();
        assert!(
            crate::services::source_numbering_grab::mapping_for_selection(
                &db, series_id, HASH, &selected
            )
            .await
            .is_err(),
            "{change}"
        );
        assert_eq!(
            saved_bytes(&db).await,
            before,
            "{change}: old proof bytes changed"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_grabs")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn saved_automatic_receipt_does_not_replace_missing_fresh_competing_metadata() {
    let _lock = ENV_LOCK.lock().await;
    let db = in_memory_pool().await;
    stone(&db, true).await;
    let series_id = seed_series(&db, 199221, "Dr. Stone: Science Future Part 3").await;
    let (server, _) = publication(vec![filename(25)]).await;
    let _origin = NyaaOrigin::set(&server.uri());
    let proof = save_automatic_fixture(&db, &server, 25, 199221, 1).await;
    let before = saved_bytes(&db).await;
    sqlx::query("DELETE FROM provider_metadata_cache WHERE provider_id=189117")
        .execute(&db)
        .await
        .unwrap();
    let reason = crate::services::release_numbering::bound_mapping_for_source(&db, HASH, 199221)
        .await
        .unwrap_err();
    assert!(
        reason.contains("189117") && reason.contains("cached metadata"),
        "{reason}"
    );
    let selected: Vec<_> = proof
        .selected_files
        .iter()
        .map(|file| file.name.clone())
        .collect();
    assert!(
        crate::services::source_numbering_grab::mapping_for_selection(
            &db, series_id, HASH, &selected
        )
        .await
        .is_err()
    );
    assert_eq!(saved_bytes(&db).await, before);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

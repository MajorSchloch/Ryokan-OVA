use super::*;
use crate::{
    handlers::library::cleanup::cleanup_series_files,
    models::{monitoring::MonitorMode, release_follows, series},
    test_support::{build_test_app_state, in_memory_pool, seed_series},
};

#[test]
fn library_paths_keep_components_and_refuse_traversal() {
    for path in [
        "Show",
        "Monogatari/Bakemonogatari",
        "Legacy_Name/Release_Name",
    ] {
        assert!(usable_library_path(path), "{path}");
        assert!(valid_link_folder(path), "{path}");
    }
    for path in [
        "",
        ".",
        "..",
        "/Show",
        "Show/",
        "Show//Arc",
        "Show/../Arc",
        "Show/./Arc",
        "Show/Arc/Season 01",
        "Show\\Arc",
        "Show/..",
        " Show/Arc",
        "Show/Arc\0",
    ] {
        assert!(!usable_library_path(path), "{path:?}");
        assert!(!valid_link_folder(path), "{path:?}");
    }
    assert!(!usable_library_path(&format!("Show/{}", "a".repeat(256))));
    assert!(library_paths_overlap(
        "Monogatari",
        "monogatari/Bakemonogatari"
    ));
    assert!(!library_paths_overlap(
        "Monogatari/Bakemonogatari",
        "Monogatari/Nisemonogatari"
    ));
    assert!(!library_paths_overlap("Show", "Showcase/Arc"));
}

#[test]
fn a_nested_release_cannot_follow_a_linked_franchise_parent() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::create_dir(other.path().join("Release")).unwrap();
    std::fs::write(other.path().join("Release/S01E01.mkv"), b"outside bytes").unwrap();
    std::os::unix::fs::symlink(other.path(), root.path().join("Linked")).unwrap();
    assert!(!valid_library_location(root.path(), "Linked/Release"));
    assert!(
        scan_series_folder_split(root.path().to_str().unwrap(), "Linked/Release")
            .0
            .is_empty()
    );
    assert_eq!(
        std::fs::read(other.path().join("Release/S01E01.mkv")).unwrap(),
        b"outside bytes"
    );
}

#[tokio::test]
async fn nested_scans_keep_release_and_special_episode_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for (relative, file) in [
        ("Monogatari/Bakemonogatari/Season 01", "Bake - S01E01.mkv"),
        ("Monogatari/Bakemonogatari/Specials", "Bake - S00E01.mkv"),
        ("Monogatari/Nisemonogatari/Season 01", "Nise - S01E02.mkv"),
    ] {
        let dir = root.join(relative);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), b"untouched media").unwrap();
    }
    let root = root.to_str().unwrap();
    let (episodes, specials) = scan_series_folder_split(root, "Monogatari/Bakemonogatari");
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].episode_number, 1);
    assert_eq!(specials.len(), 1);
    assert_eq!(
        scan_series_folder_checked(root, "Monogatari/Bakemonogatari")
            .await
            .unwrap()
            .len(),
        1
    );
    let slots = library_slots(
        root.into(),
        HashSet::from([
            "Monogatari/Bakemonogatari".into(),
            "Monogatari/Nisemonogatari".into(),
        ]),
    )
    .await;
    assert_eq!(slots["Monogatari/Bakemonogatari"], HashSet::from([1]));
    assert_eq!(slots["Monogatari/Nisemonogatari"], HashSet::from([2]));
}

#[tokio::test]
async fn future_follow_releases_use_the_saved_franchise_folder() {
    let db = in_memory_pool().await;
    seed_series(&db, 1, "Existing release").await;
    release_follows::start(&db, 1, "Monogatari", true, false, false)
        .await
        .unwrap();
    sqlx::query("UPDATE release_follows SET library_folder='Monogatari',revision=1")
        .execute(&db)
        .await
        .unwrap();
    let follow = release_follows::list(&db).await.unwrap().pop().unwrap();
    let entry = serde_json::from_value(serde_json::json!({
        "id":2, "title_romaji":"Next Arc", "title_english":"Next Arc", "title_native":"", "cover_url":"",
        "format":"TV", "status":"NOT_YET_RELEASED", "status_display":"Not yet released",
        "relation_type":"SEQUEL", "media_type":"ANIME", "episodes":12,
    })).unwrap();
    assert!(release_follows::accept(&db, &follow, &entry).await.unwrap());
    let added = series::get_by_anilist_id(&db, 2).await.unwrap().unwrap();
    assert_eq!(added.folder_name, "Monogatari/Next Arc");
    assert!(!added.allow_upgrades);
    assert_eq!(added.monitor_mode, MonitorMode::All.as_str());
    assert!(!release_follows::accept(&db, &follow, &entry).await.unwrap());
    assert_eq!(
        series::get_by_anilist_id(&db, 2)
            .await
            .unwrap()
            .unwrap()
            .folder_name,
        added.folder_name
    );
}

#[tokio::test]
async fn folder_ownership_protects_franchise_parents_but_allows_siblings() {
    let db = in_memory_pool().await;
    let bake = seed_series(&db, 1, "Bake").await;
    let nise = seed_series(&db, 2, "Nise").await;
    series::update_folder(&db, bake, "Monogatari/Bakemonogatari")
        .await
        .unwrap();
    series::update_folder(&db, nise, "Monogatari/Nisemonogatari")
        .await
        .unwrap();
    assert!(
        series::other_series_in_folder(&db, bake, "Monogatari")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        series::other_series_in_folder(&db, bake, "Monogatari/Bakemonogatari")
            .await
            .unwrap()
            .is_none()
    );
    let unique = series::unique_series_folder(&db, "Monogatari".into(), Some(2026), None)
        .await
        .unwrap();
    assert_ne!(unique, "Monogatari");
    let parent_owner = seed_series(&db, 3, "Other").await;
    series::update_folder(&db, parent_owner, "Monogatari")
        .await
        .unwrap();
    assert!(
        series::unique_series_folder(&db, "Monogatari/New Arc".into(), None, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deleting_one_nested_release_keeps_its_sibling_and_parent() {
    let db = in_memory_pool().await;
    let bake = seed_series(&db, 1, "Bake").await;
    let nise = seed_series(&db, 2, "Nise").await;
    series::update_folder(&db, bake, "Monogatari/Bakemonogatari")
        .await
        .unwrap();
    series::update_folder(&db, nise, "Monogatari/Nisemonogatari")
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    for name in ["Bakemonogatari", "Nisemonogatari"] {
        let dir = temp.path().join("Monogatari").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("S01E01.mkv"), b"keep media").unwrap();
    }
    let state = build_test_app_state(db, None);
    let report = cleanup_series_files(
        &state,
        bake,
        "Monogatari/Bakemonogatari",
        "Bake",
        Some(temp.path().to_str().unwrap()),
        "",
    )
    .await
    .unwrap();
    assert_eq!(report.folder_status, "removed");
    assert!(!temp.path().join("Monogatari/Bakemonogatari").exists());
    assert_eq!(
        std::fs::read(temp.path().join("Monogatari/Nisemonogatari/S01E01.mkv")).unwrap(),
        b"keep media"
    );
    let report = cleanup_series_files(
        &state,
        bake,
        "Monogatari",
        "Bake",
        Some(temp.path().to_str().unwrap()),
        "",
    )
    .await
    .unwrap();
    assert_eq!(report.folder_status, "shared");
    assert!(
        temp.path()
            .join("Monogatari/Nisemonogatari/S01E01.mkv")
            .exists()
    );
}

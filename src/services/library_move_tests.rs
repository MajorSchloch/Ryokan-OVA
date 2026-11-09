use super::*;
use crate::{
    models::{config, release_follows, series},
    test_support::{in_memory_pool, seed_series},
};

async fn setup() -> (tempfile::TempDir, SqlitePool, i64) {
    let temp = tempfile::tempdir().unwrap();
    let db = in_memory_pool().await;
    config::save_config(&db, &config::Config::default())
        .await
        .unwrap();
    let id = seed_series(&db, 1, "Show").await;
    (temp, db, id)
}

fn contents(root: &Path) {
    fs::create_dir_all(root.join("Show/Season 01")).unwrap();
    fs::write(root.join("Show/Season 01/S01E01.mkv"), b"media bytes").unwrap();
    fs::write(root.join("Show/Season 01/S01E01.en.srt"), b"subtitles").unwrap();
    fs::write(
        root.join("Show/tvshow.nfo"),
        b"<tvshow><uniqueid type=\"AniList\">1</uniqueid></tvshow>",
    )
    .unwrap();
}

#[tokio::test]
async fn move_keeps_media_identity_and_bytes_and_can_repeat_and_reverse() {
    let (temp, db, id) = setup().await;
    let root = temp.path();
    let marker = root.join(".marker");
    contents(root);
    let identity = dir_id(&root.join("Show")).unwrap();
    let inode = root
        .join("Show/Season 01/S01E01.mkv")
        .metadata()
        .unwrap()
        .ino();
    for _ in 0..2 {
        apply(&db, root, &marker, id, "Franchise/Release")
            .await
            .unwrap();
    }
    assert_eq!(dir_id(&root.join("Franchise/Release")).unwrap(), identity);
    assert_eq!(
        root.join("Franchise/Release/Season 01/S01E01.mkv")
            .metadata()
            .unwrap()
            .ino(),
        inode
    );
    assert_eq!(
        fs::read(root.join("Franchise/Release/Season 01/S01E01.en.srt")).unwrap(),
        b"subtitles"
    );
    assert!(!marker.exists());
    apply(&db, root, &marker, id, "Show").await.unwrap();
    assert_eq!(dir_id(&root.join("Show")).unwrap(), identity);
    assert_eq!(
        fs::read(root.join("Show/Season 01/S01E01.mkv")).unwrap(),
        b"media bytes"
    );
}

#[tokio::test]
async fn a_one_off_can_move_below_its_own_name_and_return() {
    let (temp, db, id) = setup().await;
    let root = temp.path();
    let marker = root.join(".marker");
    contents(root);
    let identity = dir_id(&root.join("Show")).unwrap();
    apply(&db, root, &marker, id, "Show/Show").await.unwrap();
    assert_eq!(dir_id(&root.join("Show/Show")).unwrap(), identity);
    assert_ne!(dir_id(&root.join("Show")).unwrap(), identity);
    apply(&db, root, &marker, id, "Show").await.unwrap();
    assert_eq!(dir_id(&root.join("Show")).unwrap(), identity);
    assert!(!root.join("Show/Show").exists());
    assert!(root.read_dir().unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".ryokan-move-")
    }));
}

#[tokio::test]
async fn missing_release_updates_only_its_future_path() {
    let (temp, db, id) = setup().await;
    let root = temp.path();
    apply(&db, root, &root.join(".marker"), id, "Franchise/Release")
        .await
        .unwrap();
    assert!(!root.join("Franchise").exists());
    assert_eq!(
        series::get_by_id(&db, id)
            .await
            .unwrap()
            .unwrap()
            .folder_name,
        "Franchise/Release"
    );
}

#[tokio::test]
async fn recovery_covers_staging_filesystem_move_and_database_commit() {
    for state in 0..4 {
        let (temp, db, id) = setup().await;
        let root = temp.path();
        let marker = root.join(".marker");
        contents(root);
        let plan = Move {
            operation: "library-folder".into(),
            series_id: id,
            root: root.into(),
            from: "Show".into(),
            to: "Show/Show".into(),
            directory: dir_id(&root.join("Show")).unwrap().unwrap(),
            staging: Some(".ryokan-move-test".into()),
            container: None,
        };
        publish(&marker, &plan).unwrap();
        if state > 0 {
            move_directory(&root.join("Show"), &root.join(".ryokan-move-test")).unwrap();
        }
        if state > 1 {
            fs::create_dir(root.join("Show")).unwrap();
            move_directory(&root.join(".ryokan-move-test"), &root.join("Show/Show")).unwrap();
        }
        if state > 2 {
            series::update_folder(&db, id, "Show/Show").await.unwrap();
        }
        apply(&db, root, &marker, id, "Show/Show").await.unwrap();
        assert_eq!(
            dir_id(&root.join("Show/Show")).unwrap(),
            Some(plan.directory)
        );
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn reverse_recovery_preserves_foreign_parent_contents() {
    let (temp, db, id) = setup().await;
    let root = temp.path();
    let marker = root.join(".marker");
    contents(root);
    apply(&db, root, &marker, id, "Show/Show").await.unwrap();
    let plan = Move {
        operation: "library-folder".into(),
        series_id: id,
        root: root.into(),
        from: "Show/Show".into(),
        to: "Show".into(),
        directory: dir_id(&root.join("Show/Show")).unwrap().unwrap(),
        staging: Some(".ryokan-move-test".into()),
        container: dir_id(&root.join("Show")).unwrap(),
    };
    publish(&marker, &plan).unwrap();
    move_directory(&root.join("Show/Show"), &root.join(".ryokan-move-test")).unwrap();
    fs::write(root.join("Show/keep.txt"), b"foreign metadata").unwrap();
    assert!(apply(&db, root, &marker, id, "Show").await.is_err());
    assert_eq!(
        fs::read(root.join("Show/keep.txt")).unwrap(),
        b"foreign metadata"
    );
    assert!(marker.exists());
    assert_eq!(
        dir_id(&root.join(".ryokan-move-test")).unwrap(),
        Some(plan.directory)
    );
    fs::remove_file(root.join("Show/keep.txt")).unwrap();
    fs::remove_dir(root.join("Show")).unwrap();
    apply(&db, root, &marker, id, "Show").await.unwrap();
    assert_eq!(dir_id(&root.join("Show")).unwrap(), Some(plan.directory));
}

#[tokio::test]
async fn occupied_shared_linked_and_traversal_paths_are_refused() {
    let (temp, db, id) = setup().await;
    let root = temp.path();
    let marker = root.join(".marker");
    contents(root);
    fs::create_dir_all(root.join("Franchise/Release")).unwrap();
    assert!(
        apply(&db, root, &marker, id, "Franchise/Release")
            .await
            .is_err()
    );
    assert!(apply(&db, root, &marker, id, "../escape").await.is_err());
    assert!(
        apply(&db, root, &marker, id, "Franchise/Bad:Name")
            .await
            .is_err()
    );
    let other = seed_series(&db, 2, "Franchise").await;
    assert!(
        apply(&db, root, &marker, id, "Franchise/Another")
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM series WHERE id=?")
        .bind(other)
        .execute(&db)
        .await
        .unwrap();
    std::os::unix::fs::symlink(root.join("Franchise"), root.join("Linked")).unwrap();
    assert!(
        apply(&db, root, &marker, id, "Linked/Release")
            .await
            .is_err()
    );
    std::os::unix::fs::symlink(root.join("Franchise"), root.join("Show/Linked season")).unwrap();
    assert!(
        apply(&db, root, &marker, id, "Elsewhere/Release")
            .await
            .is_err()
    );
    assert!(!marker.exists());
    assert_eq!(
        fs::read(root.join("Show/Season 01/S01E01.mkv")).unwrap(),
        b"media bytes"
    );
}

#[tokio::test]
async fn franchise_path_is_saved_only_after_every_selected_release_moves() {
    let (temp, db, id) = setup().await;
    let root = temp.path();
    release_follows::start(&db, 1, "Franchise", false, false, false)
        .await
        .unwrap();
    sqlx::query("UPDATE release_follows SET revision=1")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO franchise_choices SELECT 1,id,'Show',1 FROM release_follows")
        .execute(&db)
        .await
        .unwrap();
    assert!(set_franchise(&db, 1, "Franchise").await.is_err());
    apply(&db, root, &root.join(".marker"), id, "Franchise/Release")
        .await
        .unwrap();
    set_franchise(&db, 1, "Franchise").await.unwrap();
    set_franchise(&db, 1, "Franchise").await.unwrap();
    let follow = release_follows::list(&db).await.unwrap().pop().unwrap();
    assert_eq!(follow.library_folder, "Franchise");
    assert_eq!(follow.revision, 2);
    assert!(set_franchise(&db, 1, "").await.is_err());
}

use super::*;
use crate::test_support::{in_memory_pool, seed_series};

async fn setup() -> (
    tempfile::TempDir,
    SqlitePool,
    PathBuf,
    PathBuf,
    PathBuf,
    i64,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("media");
    let view = temp.path().join("view");
    let source = temp.path().join("jellyfin-source");
    fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink(&root, &source).unwrap();
    let db = in_memory_pool().await;
    let id = seed_series(&db, 1, "Original release (2020)").await;
    let folder = root.join("Original release (2020)/Season 01");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("S01E01.mkv"), b"original media").unwrap();
    (temp, db, root, view, source, id)
}

#[tokio::test]
async fn view_keeps_jellyfin_paths_when_the_physical_release_moves() {
    let (_temp, db, root, view, source, id) = setup().await;
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    let alias = view.join("Original release (2020)");
    assert_eq!(
        fs::read(alias.join("Season 01/S01E01.mkv")).unwrap(),
        b"original media"
    );
    fs::create_dir(root.join("Franchise")).unwrap();
    fs::rename(
        root.join("Original release (2020)"),
        root.join("Franchise/Release"),
    )
    .unwrap();
    series::update_folder(&db, id, "Franchise/Release")
        .await
        .unwrap();
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    assert_eq!(
        fs::read_link(&alias).unwrap(),
        source.join("Franchise/Release")
    );
    assert_eq!(
        fs::read(alias.join("Season 01/S01E01.mkv")).unwrap(),
        b"original media"
    );
    assert!(!view.join("Franchise").exists());
    let before = fs::metadata(view.join(STATE)).unwrap().modified().unwrap();
    sync_with_paths(&db, root, view.clone(), source)
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(view.join(STATE)).unwrap().modified().unwrap(),
        before
    );
}

#[tokio::test]
async fn view_preserves_kept_files_and_allocates_distinct_future_aliases() {
    let (_temp, db, root, view, source, id) = setup().await;
    fs::create_dir(root.join("Franchise")).unwrap();
    fs::rename(
        root.join("Original release (2020)"),
        root.join("Franchise/Original release (2020)"),
    )
    .unwrap();
    series::update_folder(&db, id, "Franchise/Original release (2020)")
        .await
        .unwrap();
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    sqlx::query("DELETE FROM series WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    let new_id = seed_series(&db, 2, "Another").await;
    series::update_folder(&db, new_id, "Other/Original release (2020)")
        .await
        .unwrap();
    fs::create_dir_all(root.join("Other/Original release (2020)")).unwrap();
    fs::write(
        root.join("Other/Original release (2020)/S01E01.mkv"),
        b"new media",
    )
    .unwrap();
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    let alias: String =
        sqlx::query_scalar("SELECT folder_name FROM series_library_aliases WHERE series_id=?")
            .bind(new_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_ne!(alias, "Original release (2020)");
    assert!(!view.join("Franchise").exists());
    assert_eq!(
        fs::read(view.join("Original release (2020)/Season 01/S01E01.mkv")).unwrap(),
        b"original media"
    );
    assert_eq!(
        fs::read(view.join(&alias).join("S01E01.mkv")).unwrap(),
        b"new media"
    );
    fs::remove_dir_all(root.join("Other/Original release (2020)")).unwrap();
    sync_with_paths(&db, root, view.clone(), source)
        .await
        .unwrap();
    assert!(view.join(&alias).symlink_metadata().is_err());
    assert!(view.join("Original release (2020)").exists());
}

#[tokio::test]
async fn view_refuses_foreign_files_and_changed_links() {
    let (_temp, db, root, view, source, _id) = setup().await;
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    let alias = view.join("Original release (2020)");
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("/unrelated", &alias).unwrap();
    assert!(
        sync_with_paths(&db, root.clone(), view.clone(), source.clone())
            .await
            .is_err()
    );
    assert_eq!(fs::read_link(&alias).unwrap(), Path::new("/unrelated"));
    fs::remove_file(&alias).unwrap();
    fs::write(view.join("user-file.txt"), b"keep").unwrap();
    assert!(
        sync_with_paths(&db, root, view.clone(), source)
            .await
            .is_err()
    );
    assert_eq!(fs::read(view.join("user-file.txt")).unwrap(), b"keep");
}

#[tokio::test]
async fn view_does_not_hide_a_release_when_its_source_becomes_invalid() {
    let (_temp, db, root, view, source, _id) = setup().await;
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    let alias = view.join("Original release (2020)");
    let expected = fs::read_link(&alias).unwrap();
    fs::rename(
        root.join("Original release (2020)"),
        root.join("kept-original"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        root.join("kept-original"),
        root.join("Original release (2020)"),
    )
    .unwrap();
    assert!(
        sync_with_paths(&db, root.clone(), view.clone(), source)
            .await
            .is_err()
    );
    assert_eq!(fs::read_link(alias).unwrap(), expected);
    assert_eq!(
        fs::read(root.join("kept-original/Season 01/S01E01.mkv")).unwrap(),
        b"original media"
    );
}

#[tokio::test]
async fn view_recovers_an_interrupted_link_update() {
    let (_temp, db, root, view, source, id) = setup().await;
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    fs::create_dir(root.join("Franchise")).unwrap();
    fs::rename(
        root.join("Original release (2020)"),
        root.join("Franchise/Release"),
    )
    .unwrap();
    series::update_folder(&db, id, "Franchise/Release")
        .await
        .unwrap();
    let mut state: State = serde_json::from_slice(&fs::read(view.join(STATE)).unwrap()).unwrap();
    state.pending = Some(Links::from([(
        "Original release (2020)".into(),
        "Franchise/Release".into(),
    )]));
    save(&view, &state).unwrap();
    std::os::unix::fs::symlink(source.join("Franchise/Release"), view.join(TEMP_LINK)).unwrap();
    fs::rename(view.join(TEMP_LINK), view.join("Original release (2020)")).unwrap();
    sync_with_paths(&db, root, view.clone(), source)
        .await
        .unwrap();
    let final_state: State = serde_json::from_slice(&fs::read(view.join(STATE)).unwrap()).unwrap();
    assert!(final_state.pending.is_none());
    assert_eq!(
        final_state.links["Original release (2020)"],
        "Franchise/Release"
    );
}

#[tokio::test]
async fn view_retains_a_removed_release_inside_its_same_named_franchise_parent() {
    let (_temp, db, root, view, source, id) = setup().await;
    let title = "Original release (2020)";
    fs::rename(root.join(title), root.join("staged-release")).unwrap();
    fs::create_dir(root.join(title)).unwrap();
    fs::rename(root.join("staged-release"), root.join(title).join(title)).unwrap();
    let relative = format!("{title}/{title}");
    series::update_folder(&db, id, &relative).await.unwrap();
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    let alias = view.join(title);
    let target = fs::read_link(&alias).unwrap();
    let state_before = fs::read(view.join(STATE)).unwrap();
    let state_modified = fs::metadata(view.join(STATE)).unwrap().modified().unwrap();
    sqlx::query("DELETE FROM series WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();

    // A failed recycle or explicit keep-files removal leaves this tree.
    // The franchise parent must not displace its retained release alias.
    for _ in 0..2 {
        sync_with_paths(&db, root.clone(), view.clone(), source.clone())
            .await
            .unwrap();
        assert_eq!(fs::read_link(&alias).unwrap(), target);
        assert_eq!(
            fs::read(alias.join("Season 01/S01E01.mkv")).unwrap(),
            b"original media"
        );
        assert_eq!(fs::read(view.join(STATE)).unwrap(), state_before);
        assert_eq!(
            fs::metadata(view.join(STATE)).unwrap().modified().unwrap(),
            state_modified
        );
    }
}

#[tokio::test]
async fn view_rejects_an_unrelated_untracked_folder_with_a_retained_alias_name() {
    let (_temp, db, root, view, source, id) = setup().await;
    let title = "Original release (2020)";
    fs::create_dir(root.join("Different franchise")).unwrap();
    let relative = format!("Different franchise/{title}");
    fs::rename(root.join(title), root.join(&relative)).unwrap();
    series::update_folder(&db, id, &relative).await.unwrap();
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    sqlx::query("DELETE FROM series WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    fs::create_dir(root.join(title)).unwrap();
    fs::write(root.join(title).join("unrelated.mkv"), b"unrelated media").unwrap();
    let target = fs::read_link(view.join(title)).unwrap();
    let state_before = fs::read(view.join(STATE)).unwrap();

    let error = sync_with_paths(&db, root.clone(), view.clone(), source)
        .await
        .unwrap_err();
    assert!(error.contains("An untracked folder conflicts"), "{error}");
    assert_eq!(fs::read_link(view.join(title)).unwrap(), target);
    assert_eq!(fs::read(view.join(STATE)).unwrap(), state_before);
    assert_eq!(
        fs::read(root.join(relative).join("Season 01/S01E01.mkv")).unwrap(),
        b"original media"
    );
    assert_eq!(
        fs::read(root.join(title).join("unrelated.mkv")).unwrap(),
        b"unrelated media"
    );
}

#[tokio::test]
async fn view_does_not_treat_a_missing_retained_release_as_an_existing_franchise() {
    let (_temp, db, root, view, source, id) = setup().await;
    let title = "Original release (2020)";
    fs::rename(root.join(title), root.join("staged-release")).unwrap();
    fs::create_dir(root.join(title)).unwrap();
    fs::rename(root.join("staged-release"), root.join(title).join(title)).unwrap();
    let relative = format!("{title}/{title}");
    series::update_folder(&db, id, &relative).await.unwrap();
    sync_with_paths(&db, root.clone(), view.clone(), source.clone())
        .await
        .unwrap();
    sqlx::query("DELETE FROM series WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    fs::remove_dir_all(root.join(relative)).unwrap();
    fs::write(root.join(title).join("untracked.mkv"), b"remaining media").unwrap();

    sync_with_paths(&db, root, view.clone(), source.clone())
        .await
        .unwrap();
    assert_eq!(fs::read_link(view.join(title)).unwrap(), source.join(title));
    assert_eq!(
        fs::read(view.join(title).join("untracked.mkv")).unwrap(),
        b"remaining media"
    );
}

use std::fs;
use std::time::{Duration, Instant};

use locator::db::Database;
use locator::live_index::LiveIndex;
use locator::query::SearchOptions;

fn search_names(db: &Database, query: &str) -> Vec<String> {
    db.search_with_options(&SearchOptions::new(query).with_limit(100))
        .expect("search")
        .into_iter()
        .map(|result| result.name)
        .collect()
}

/// Poll until `predicate` holds or the deadline passes. Filesystem event
/// delivery (FSEvents/inotify) plus the watcher debounce is asynchronous, so we
/// give it a generous window.
fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    predicate()
}

#[test]
fn live_index_reflects_created_and_deleted_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    let db_path = root.join("index.sqlite");

    // Seed an index with one existing file.
    let existing = root.join("existing.txt");
    fs::write(&existing, b"hello").expect("write existing");
    {
        let db = Database::open(&db_path).expect("open db");
        let record = locator::db::FileRecord {
            path: existing.to_string_lossy().to_string(),
            name: "existing.txt".to_string(),
            parent: root.to_string_lossy().to_string(),
            extension: Some("txt".to_string()),
            root: root.to_string_lossy().to_string(),
            volume: "local".to_string(),
            kind: "text".to_string(),
            size_bytes: 5,
            created_at: None,
            modified_at: None,
        };
        db.upsert_file(&record).expect("seed file");
    }

    let _live = LiveIndex::spawn(root.clone(), db_path.clone()).expect("spawn watcher");

    // A brand-new file must appear in search results without a rescan.
    let fresh = root.join("fresh_report.txt");
    fs::write(&fresh, b"world").expect("write fresh");
    let appeared = wait_until(|| {
        let db = Database::open(&db_path).expect("reopen db");
        search_names(&db, "fresh_report")
            .iter()
            .any(|n| n == "fresh_report.txt")
    });
    assert!(appeared, "newly created file should be indexed live");

    // Removing the seeded file must drop it from results.
    fs::remove_file(&existing).expect("remove existing");
    let disappeared = wait_until(|| {
        let db = Database::open(&db_path).expect("reopen db");
        !search_names(&db, "existing")
            .iter()
            .any(|n| n == "existing.txt")
    });
    assert!(disappeared, "deleted file should be soft-deleted live");
}

fn paths_matching(db_path: &std::path::Path, query: &str) -> Vec<String> {
    Database::open(db_path)
        .expect("reopen db")
        .search_with_options(&SearchOptions::new(query).with_limit(100))
        .expect("search")
        .into_iter()
        .map(|result| result.path)
        .collect()
}

#[test]
fn live_index_indexes_files_under_renamed_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    let db_path = root.join(".locator").join("index.sqlite");
    let old_dir = root.join("olddir");
    fs::create_dir(&old_dir).expect("create dir");
    fs::write(old_dir.join("renamed_child.txt"), b"x").expect("write child");
    drop(Database::open(&db_path).expect("create index"));

    let _live = LiveIndex::spawn(root.clone(), db_path.clone()).expect("spawn watcher");
    let new_dir = root.join("newdir");
    fs::rename(&old_dir, &new_dir).expect("rename dir");

    let expected = new_dir
        .join("renamed_child.txt")
        .to_string_lossy()
        .to_string();
    let found = wait_until(|| paths_matching(&db_path, "renamed_child").contains(&expected));
    assert!(found, "file under renamed directory should be indexed");
}

#[test]
fn live_index_follows_replaced_index_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    let db_path = root.join(".locator").join("index.sqlite");
    drop(Database::open(&db_path).expect("create index"));

    let _live = LiveIndex::spawn(root.clone(), db_path.clone()).expect("spawn watcher");
    fs::write(root.join("before_swap.txt"), b"x").expect("write before");
    assert!(wait_until(
        || !paths_matching(&db_path, "before_swap").is_empty()
    ));

    // Replace the index by rename, as a staged scan does.
    let replacement = root.join(".locator").join("replacement.sqlite");
    drop(Database::open(&replacement).expect("create replacement"));
    locator::db::delete_index_files(&db_path).expect("remove old index files");
    fs::rename(&replacement, &db_path).expect("install replacement");

    fs::write(root.join("after_swap.txt"), b"x").expect("write after");
    let found = wait_until(|| !paths_matching(&db_path, "after_swap").is_empty());
    assert!(
        found,
        "change after index replacement should reach the new index"
    );
}

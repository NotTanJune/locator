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

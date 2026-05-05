use chrono::{TimeZone, Utc};
use locator::db::{
    db_path_for_working_dir, default_db_path, existing_db_path_for_working_dir,
    fallback_db_path_for_root, local_db_path_for_root, Database, FileRecord, ScanCompletion,
};
use locator::query::{QueryMode, SearchFilters, SearchOptions, SortField};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use tempfile::tempdir;

static LCTR_ENV_LOCK: Mutex<()> = Mutex::new(());

fn record(path: &str, name: &str, ext: Option<&str>, size: u64, modified_year: i32) -> FileRecord {
    FileRecord {
        path: path.into(),
        name: name.into(),
        parent: "/tmp".into(),
        extension: ext.map(str::to_string),
        root: "/tmp".into(),
        volume: "local".into(),
        kind: ext.unwrap_or("file").into(),
        size_bytes: size,
        created_at: Some(Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap()),
        modified_at: Some(Utc.with_ymd_and_hms(modified_year, 1, 1, 0, 0, 0).unwrap()),
    }
}

fn record_for_path(path: &Path, ext: Option<&str>, size: u64, modified_year: i32) -> FileRecord {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("utf-8 file name");
    let parent = path
        .parent()
        .and_then(|parent| parent.to_str())
        .expect("utf-8 parent");
    FileRecord {
        path: path.to_string_lossy().to_string(),
        name: name.to_string(),
        parent: parent.to_string(),
        extension: ext.map(str::to_string),
        root: parent.to_string(),
        volume: "local".into(),
        kind: ext.unwrap_or("file").into(),
        size_bytes: size,
        created_at: Some(Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap()),
        modified_at: Some(Utc.with_ymd_and_hms(modified_year, 1, 1, 0, 0, 0).unwrap()),
    }
}

#[test]
fn finds_records_by_keyword_and_filters() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/invoice.pdf",
        "invoice.pdf",
        Some("pdf"),
        150_000,
        2024,
    ))
    .expect("insert pdf");
    db.upsert_file(&record(
        "/tmp/invoice.txt",
        "invoice.txt",
        Some("txt"),
        50_000,
        2024,
    ))
    .expect("insert txt");
    db.upsert_file(&record(
        "/tmp/photo.jpg",
        "photo.jpg",
        Some("jpg"),
        900_000,
        2022,
    ))
    .expect("insert jpg");

    let filters = SearchFilters::new()
        .with_kind("pdf")
        .expect("kind parses")
        .with_min_size("100kb")
        .expect("size parses")
        .with_modified_after("2024-01-01")
        .expect("date parses");

    let results = db.search("invoice", &filters, 20).expect("search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/tmp/invoice.pdf");
}

#[test]
#[cfg(unix)]
fn readonly_existing_database_can_be_searched_without_migration() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    {
        let db = Database::open(&db_path).expect("db opens");
        db.upsert_file(&record(
            "/tmp/archive.zip",
            "archive.zip",
            Some("zip"),
            100,
            2024,
        ))
        .expect("insert archive");
    }
    let mut readonly = std::fs::metadata(&db_path).expect("metadata").permissions();
    readonly.set_mode(0o444);
    std::fs::set_permissions(&db_path, readonly).expect("make readonly");

    let db = Database::open_readonly(&db_path).expect("readonly db opens");
    let results = db
        .search_with_options(&SearchOptions::new("archive"))
        .expect("readonly search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "archive.zip");
}

#[test]
fn searches_with_query_modes_metadata_filters_and_sorting() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/alpha-report.pdf",
        "alpha-report.pdf",
        Some("pdf"),
        150_000,
        2024,
    ))
    .expect("insert alpha");
    db.upsert_file(&record(
        "/tmp/beta-report.md",
        "beta-report.md",
        Some("md"),
        25_000,
        2025,
    ))
    .expect("insert beta");
    db.upsert_file(&record(
        "/tmp/reporting.txt",
        "reporting.txt",
        Some("txt"),
        10_000,
        2023,
    ))
    .expect("insert text");

    let options = SearchOptions::new("report")
        .with_mode(QueryMode::Suffix)
        .with_sort(SortField::Modified)
        .with_reverse(true)
        .with_limit(10)
        .with_exts("pdf,md")
        .expect("extensions parse");

    let results = db.search_with_options(&options).expect("search works");

    assert_eq!(
        results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>(),
        vec!["beta-report.md", "alpha-report.pdf"]
    );
}

#[test]
fn option_search_uses_contains_mode_for_infix_filename_matches() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/my-archive-file.zip",
        "my-archive-file.zip",
        Some("zip"),
        150_000,
        2024,
    ))
    .expect("insert archive");

    let results = db
        .search_with_options(&SearchOptions::new("archive"))
        .expect("search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "my-archive-file.zip");
}

#[test]
fn marks_missing_root_records_deleted() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record("/tmp/old.pdf", "old.pdf", Some("pdf"), 10, 2024))
        .expect("insert");

    db.mark_missing_under_root("/tmp", &[])
        .expect("mark missing");
    let results = db
        .search("old", &SearchFilters::new(), 20)
        .expect("search works");

    assert!(results.is_empty());
}

#[test]
fn batch_upsert_indexes_multiple_records() {
    let db = Database::open_in_memory().expect("db opens");
    let records = vec![
        record("/tmp/a.pdf", "a.pdf", Some("pdf"), 10, 2024),
        record("/tmp/b.pdf", "b.pdf", Some("pdf"), 20, 2024),
    ];

    db.upsert_files(&records).expect("batch insert");
    let results = db
        .search("pdf", &SearchFilters::new(), 20)
        .expect("search works");

    assert_eq!(results.len(), 2);
}

#[test]
fn light_bulk_upsert_writes_searchable_name_path_rows() {
    let db = Database::open_in_memory().expect("db opens");
    let records = vec![
        record("/tmp/a.pdf", "a.pdf", Some("pdf"), 42, 2024),
        record("/tmp/b.txt", "b.txt", Some("txt"), 84, 2024),
    ];

    db.upsert_light_files_with_indexed_at(&records, 1234)
        .expect("light insert");
    db.finish_bulk_scan().expect("finish bulk scan");

    let results = db
        .search("a", &SearchFilters::new(), 20)
        .expect("search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/tmp/a.pdf");
    assert_eq!(results[0].extension.as_deref(), Some("pdf"));
    assert_eq!(results[0].size_bytes, 0);
    assert!(results[0].modified_at.is_none());
}

#[test]
fn light_bulk_insert_writes_searchable_name_path_rows() {
    let db = Database::open_in_memory().expect("db opens");
    let records = vec![
        record("/tmp/a.pdf", "a.pdf", Some("pdf"), 42, 2024),
        record("/tmp/b.txt", "b.txt", Some("txt"), 84, 2024),
    ];

    db.insert_light_files_with_indexed_at(&records, 1234)
        .expect("light insert");
    db.finish_bulk_scan().expect("finish bulk scan");

    let results = db
        .search_interactive("a", 20)
        .expect("interactive search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/tmp/a.pdf");
}

#[test]
fn fresh_staged_scan_defers_indexes_until_finish() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open_fresh_staged_scan(&db_path).expect("db opens");
    let records = vec![
        record("/tmp/a.pdf", "a.pdf", Some("pdf"), 42, 2024),
        record("/tmp/b.txt", "b.txt", Some("txt"), 84, 2024),
    ];

    assert!(!sqlite_index_exists(
        &db_path,
        "idx_files_deleted_name_lower"
    ));

    db.insert_light_files_with_indexed_at(&records, 1234)
        .expect("light insert");
    db.finish_fresh_staged_scan_profile()
        .expect("finish fresh staged scan");

    let results = db
        .search("a", &SearchFilters::new(), 20)
        .expect("fts search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/tmp/a.pdf");

    drop(db);

    assert!(sqlite_index_exists(
        &db_path,
        "idx_files_deleted_name_lower"
    ));
    assert!(sqlite_index_exists(&db_path, "idx_files_path_unique"));

    let db = Database::open(&db_path).expect("db reopens");
    db.upsert_light_files_with_indexed_at(
        &[record("/tmp/a.pdf", "a.pdf", Some("pdf"), 99, 2025)],
        5678,
    )
    .expect("future upsert uses staged unique path index");
}

#[test]
fn bulk_scan_keeps_secondary_indexes_by_default() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");

    assert!(sqlite_index_exists(
        &db_path,
        "idx_files_deleted_name_lower"
    ));
    db.begin_bulk_scan().expect("begin bulk scan");

    assert!(sqlite_index_exists(
        &db_path,
        "idx_files_deleted_name_lower"
    ));
}

#[test]
fn bulk_scan_defers_fts_until_rebuild() {
    let db = Database::open_in_memory().expect("db opens");

    db.begin_bulk_scan().expect("begin bulk scan");
    db.upsert_file(&record(
        "/tmp/deferred.pdf",
        "deferred.pdf",
        Some("pdf"),
        10,
        2024,
    ))
    .expect("insert while fts deferred");

    assert!(db
        .search("deferred", &SearchFilters::new(), 20)
        .expect("search works")
        .is_empty());

    db.finish_bulk_scan().expect("finish bulk scan");
    let results = db
        .search("deferred", &SearchFilters::new(), 20)
        .expect("search works");

    assert_eq!(results.len(), 1);
}

fn sqlite_index_exists(db_path: &std::path::Path, name: &str) -> bool {
    let conn = rusqlite::Connection::open(db_path).expect("open raw db");
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

#[test]
fn mark_missing_handles_large_seen_path_lists() {
    let db = Database::open_in_memory().expect("db opens");
    let seen_paths = (0..40_000)
        .map(|index| format!("/tmp/file-{index}.txt"))
        .collect::<Vec<_>>();

    db.mark_missing_under_root("/tmp", &seen_paths)
        .expect("large seen path list does not exceed SQLite variable limit");
}

#[test]
fn opening_legacy_contentless_fts_database_migrates_to_deletable_fts() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("legacy.sqlite");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open raw db");
        conn.execute_batch(
            "CREATE TABLE files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                parent TEXT NOT NULL,
                extension TEXT,
                root TEXT NOT NULL,
                volume TEXT NOT NULL,
                kind TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                created_at INTEGER,
                modified_at INTEGER,
                indexed_at INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0
             );
             CREATE VIRTUAL TABLE files_fts
             USING fts5(name, path, content='', tokenize='unicode61');",
        )
        .expect("create legacy schema");
    }

    let db = Database::open(&db_path).expect("migrates legacy db");
    let file = record("/tmp/legacy.pdf", "legacy.pdf", Some("pdf"), 10, 2024);
    db.upsert_file(&file).expect("first insert works");
    db.upsert_file(&file)
        .expect("second insert can replace FTS row");

    let results = db
        .search("legacy", &SearchFilters::new(), 10)
        .expect("search works");
    assert_eq!(results.len(), 1);
}

#[test]
fn punctuation_only_search_returns_empty_without_fts_error() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/report.pdf",
        "report.pdf",
        Some("pdf"),
        10,
        2024,
    ))
    .expect("insert");

    let results = db
        .search("-", &SearchFilters::new(), 10)
        .expect("search works");

    assert!(results.is_empty());
}

#[test]
fn interactive_search_prioritizes_filename_matches() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_files(&[
        record("/tmp/archive/report", "notes.txt", Some("txt"), 10, 2024),
        record("/tmp/report.pdf", "report.pdf", Some("pdf"), 10, 2024),
        record(
            "/tmp/report-final.pdf",
            "report-final.pdf",
            Some("pdf"),
            10,
            2024,
        ),
    ])
    .expect("insert");

    let results = db
        .search_interactive("report", 5)
        .expect("interactive search works");

    assert_eq!(results[0].name, "report.pdf");
    assert_eq!(results[1].name, "report-final.pdf");
    assert!(!results
        .iter()
        .take(2)
        .any(|result| result.name == "notes.txt"));
}

#[test]
fn interactive_search_finds_case_insensitive_filename_prefixes() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/QuarterlyBudget.xlsx",
        "QuarterlyBudget.xlsx",
        Some("xlsx"),
        10,
        2024,
    ))
    .expect("insert");

    let results = db
        .search_interactive("quarterly", 5)
        .expect("interactive search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "QuarterlyBudget.xlsx");
}

#[test]
fn interactive_search_handles_missing_filename_match_without_error() {
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/report-final.pdf",
        "report-final.pdf",
        Some("pdf"),
        10,
        2024,
    ))
    .expect("insert");

    let results = db
        .search_interactive("zzzzzz-unlikely", 5)
        .expect("interactive search works");

    assert!(results.is_empty());
}

#[test]
fn verified_search_drops_missing_persistent_paths() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let existing_path = dir.path().join("report-existing.pdf");
    let missing_path = dir.path().join("report-missing.pdf");
    std::fs::write(&existing_path, "real").expect("write existing file");

    {
        let db = Database::open(&db_path).expect("db opens");
        db.upsert_file(&record_for_path(&missing_path, Some("pdf"), 100, 2024))
            .expect("insert missing");
        db.upsert_file(&record_for_path(&existing_path, Some("pdf"), 100, 2024))
            .expect("insert existing");
    }

    let db = Database::open_readonly(&db_path)
        .expect("readonly db opens")
        .with_search_path_verification();
    let assert_only_existing = |results: Vec<locator::db::SearchResult>| {
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, existing_path.to_string_lossy());
        assert_eq!(results[0].size_bytes, 4);
    };

    assert_only_existing(
        db.search("report", &SearchFilters::new(), 10)
            .expect("fts search works"),
    );
    assert_only_existing(
        db.search_with_options(&SearchOptions::new("report"))
            .expect("option search works"),
    );
    assert_only_existing(
        db.search_interactive("report", 10)
            .expect("interactive search works"),
    );
}

#[test]
fn default_db_path_is_available_for_background_workers() {
    let _guard = LCTR_ENV_LOCK.lock().expect("env lock");
    let path = default_db_path().expect("path resolves");

    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("index.sqlite")
    );
}

#[test]
fn working_dir_db_path_uses_nearest_local_index() {
    let dir = tempdir().expect("temp dir");
    let parent = dir.path().join("project");
    let child = parent.join("nested");
    std::fs::create_dir_all(&child).expect("create dirs");
    let local_db = local_db_path_for_root(&parent).expect("local db path");
    Database::open(&local_db).expect("create local db");

    let resolved = db_path_for_working_dir(&child).expect("resolve local db");

    assert_eq!(resolved, local_db);
}

#[test]
fn existing_db_path_returns_none_when_directory_is_unindexed() {
    let dir = tempdir().expect("temp dir");

    let resolved = existing_db_path_for_working_dir(dir.path()).expect("resolve existing db");

    assert!(resolved.is_none());
}

#[test]
fn scan_completion_tracks_incomplete_and_complete_roots() {
    let db = Database::open_in_memory().expect("db opens");

    assert_eq!(
        db.scan_completion_for_root("/tmp").expect("status"),
        ScanCompletion::Unknown
    );

    db.mark_scan_started("/tmp", 10).expect("mark started");
    assert_eq!(
        db.scan_completion_for_root("/tmp").expect("status"),
        ScanCompletion::Incomplete
    );

    db.mark_scan_completed("/tmp", 10).expect("mark complete");
    assert_eq!(
        db.scan_completion_for_root("/tmp").expect("status"),
        ScanCompletion::Complete
    );
}

#[test]
fn working_dir_db_path_uses_root_fallback_index_when_local_index_is_absent() {
    let _guard = LCTR_ENV_LOCK.lock().expect("env lock");
    let dir = tempdir().expect("temp dir");
    std::env::set_var("LCTR_DATA_DIR", dir.path().join("data"));
    let parent = dir.path().join("readonly-drive");
    let child = parent.join("nested");
    std::fs::create_dir_all(&child).expect("create dirs");
    let fallback_db = fallback_db_path_for_root(&parent).expect("fallback db path");
    Database::open(&fallback_db).expect("create fallback db");

    let resolved = db_path_for_working_dir(&child).expect("resolve fallback db");

    assert_eq!(resolved, fallback_db);
    std::env::remove_var("LCTR_DATA_DIR");
}

#[test]
fn fallback_db_path_for_root_is_stable_and_root_specific() {
    let dir = tempdir().expect("temp dir");
    let one = dir.path().join("one");
    let two = dir.path().join("two");
    std::fs::create_dir_all(&one).expect("create first root");
    std::fs::create_dir_all(&two).expect("create second root");

    let first = fallback_db_path_for_root(&one).expect("first fallback");
    let repeated = fallback_db_path_for_root(&one).expect("repeated fallback");
    let second = fallback_db_path_for_root(&two).expect("second fallback");

    assert_eq!(first, repeated);
    assert_ne!(first, second);
    assert!(first.ends_with("index.sqlite"));
}

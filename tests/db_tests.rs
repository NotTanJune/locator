use chrono::{TimeZone, Utc};
use locator::db::{
    db_path_for_working_dir, default_db_path, delete_index_files, existing_db_path_for_working_dir,
    fallback_db_path_for_root, local_db_path_for_root, Database, FileRecord, ScanCompletion,
};
use locator::name_file::{name_file_path, NameFile, NameFileBuilder};
use locator::query::{QueryMode, SearchFilters, SearchOptions, SortField};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use tempfile::tempdir;

static LCTR_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Every test in this file holds this lock. Some tests point `LCTR_CONFIG_DIR` or `LCTR_DATA_DIR` at a temporary folder, and those
/// variables are global to the process. A test that does not hold the lock can see them change
/// while it runs. A poisoned lock (an earlier test panicked) is still taken, so one failure does
/// not cascade into the rest.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    LCTR_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

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

/// Installs `rows` (path and kind) as the rows of a finished scan of `root`, the way a scan
/// stores them.
fn install(db: &Database, root: &str, rows: &[(&str, &str)], token: i64) {
    let mut builder = NameFileBuilder::default();
    for (path, kind) in rows {
        builder.push(path, kind).expect("push row");
    }
    db.install_scan(root, builder, token).expect("install scan");
}

/// A row the watcher would record for `path` under `root`.
fn live_record(root: &str, path: &str, kind: &str, size: u64) -> FileRecord {
    let name = path.rsplit('/').next().expect("a name");
    FileRecord {
        path: path.into(),
        name: name.into(),
        parent: path[..path.len() - name.len()].trim_end_matches('/').into(),
        extension: name.rsplit_once('.').map(|(_, ext)| ext.to_string()),
        root: root.into(),
        volume: "local".into(),
        kind: kind.into(),
        size_bytes: size,
        created_at: None,
        modified_at: None,
    }
}

/// The path of every active row, sorted.
fn all_paths(db: &Database) -> Vec<String> {
    let mut paths: Vec<String> = db
        .search("", &SearchFilters::new(), 1_000_000)
        .expect("list every row")
        .into_iter()
        .map(|result| result.path)
        .collect();
    paths.sort();
    paths
}

/// The number of rows in the `changes` table of the database file at `db_path`.
fn change_count(db_path: &Path) -> i64 {
    rusqlite::Connection::open(db_path)
        .expect("raw connection")
        .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
        .expect("count changes")
}

#[test]
fn finds_records_by_keyword_and_filters() {
    let _guard = env_guard();
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
    let _guard = env_guard();
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
    let _guard = env_guard();
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
    let _guard = env_guard();
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
fn installing_a_scan_keeps_rows_the_watcher_wrote_during_the_scan() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    let scan_token = Utc::now().timestamp_millis() - 60_000;
    let gone = record("/tmp/gone.pdf", "gone.pdf", Some("pdf"), 10, 2024);
    db.upsert_files_with_indexed_at(std::slice::from_ref(&gone), scan_token - 1)
        .expect("row from an older scan");
    // The watcher stamps rows with the current time, which is after the scan started.
    db.upsert_file(&record("/tmp/new.pdf", "new.pdf", Some("pdf"), 10, 2024))
        .expect("watcher row");

    install(&db, "/tmp", &[("/tmp/scanned.pdf", "pdf")], scan_token);

    assert_eq!(
        all_paths(&db),
        ["/tmp/new.pdf", "/tmp/scanned.pdf"],
        "the scan covers the older row and the watcher row stays"
    );
}

#[test]
fn batch_upsert_indexes_multiple_records() {
    let _guard = env_guard();
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
fn stores_large_sizes_as_sqlite_safe_values() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/huge.bin",
        "huge.bin",
        Some("bin"),
        u64::MAX,
        2024,
    ))
    .expect("insert huge file");

    let results = db
        .search("huge", &SearchFilters::new(), 10)
        .expect("search huge file");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].size_bytes, i64::MAX as u64);
}

#[test]
fn installed_scan_rows_are_searchable_with_a_name_and_a_path_only() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    install(
        &db,
        "/tmp",
        &[("/tmp/a.pdf", "pdf"), ("/tmp/b.txt", "txt")],
        1234,
    );

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
fn installed_scan_rows_are_found_by_interactive_search() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    install(
        &db,
        "/tmp",
        &[("/tmp/a.pdf", "pdf"), ("/tmp/b.txt", "txt")],
        1234,
    );

    let results = db
        .search_interactive("a", 20)
        .expect("interactive search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/tmp/a.pdf");
}

#[test]
fn rows_written_while_a_scan_runs_are_found_after_it_is_installed() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    let scan_token = Utc::now().timestamp_millis() - 60_000;

    db.upsert_file(&record(
        "/tmp/deferred.pdf",
        "deferred.pdf",
        Some("pdf"),
        10,
        2024,
    ))
    .expect("insert while the scan runs");
    install(&db, "/tmp", &[("/tmp/other.pdf", "pdf")], scan_token);

    let results = db
        .search("deferred", &SearchFilters::new(), 20)
        .expect("search works");

    assert_eq!(results.len(), 1);
}

#[test]
fn opening_a_database_with_the_old_trigram_index_drops_it() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("legacy.sqlite");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open raw db");
        conn.execute_batch(
            "CREATE TABLE files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                name_lower TEXT,
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
             CREATE INDEX idx_files_extension ON files(extension);
             CREATE INDEX idx_files_deleted_name_lower ON files(deleted, name_lower);
             CREATE VIRTUAL TABLE files_fts
             USING fts5(name, content='files', content_rowid='id',
                        tokenize='trigram', detail=none, columnsize=0);
             CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
                INSERT INTO files_fts(rowid, name) VALUES (new.id, new.name);
             END;
             CREATE TRIGGER files_ad AFTER DELETE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, name) VALUES('delete', old.id, old.name);
             END;
             CREATE TRIGGER files_au AFTER UPDATE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, name) VALUES('delete', old.id, old.name);
                INSERT INTO files_fts(rowid, name) VALUES (new.id, new.name);
             END;
             INSERT INTO files (path, name, name_lower, parent, extension, root, volume, kind,
                                size_bytes, indexed_at)
             VALUES ('/tmp/legacy.pdf', 'legacy.pdf', 'legacy.pdf', '/tmp', 'pdf', '/tmp',
                     'local', 'pdf', 10, 1);",
        )
        .expect("create legacy schema");
    }

    let db = Database::open(&db_path).expect("migrates legacy db");

    let conn = rusqlite::Connection::open(&db_path).expect("raw conn for schema check");
    let leftovers: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN
             ('files','files_fts','files_ai','files_ad','files_au','idx_files_extension',
              'idx_files_deleted_name_lower','pending_changes')",
            [],
            |row| row.get(0),
        )
        .expect("count legacy objects");
    assert_eq!(leftovers, 0);

    let results = db
        .search("legacy", &SearchFilters::new(), 10)
        .expect("search works");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/tmp/legacy.pdf");
    assert_eq!(db.roots().expect("roots"), ["/tmp"]);
}

#[test]
fn punctuation_only_search_returns_empty_without_fts_error() {
    let _guard = env_guard();
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
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_files(&[
        record("/tmp/report/notes.txt", "notes.txt", Some("txt"), 10, 2024),
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
    let _guard = env_guard();
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
    let _guard = env_guard();
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
    let _guard = env_guard();
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
    let _guard = env_guard();
    let path = default_db_path().expect("path resolves");

    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("index.db")
    );
}

#[test]
fn working_dir_db_path_uses_nearest_local_index() {
    let _guard = env_guard();
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
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");

    let resolved = existing_db_path_for_working_dir(dir.path()).expect("resolve existing db");

    assert!(resolved.is_none());
}

#[test]
fn scan_completion_tracks_incomplete_and_complete_roots() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");

    assert_eq!(
        db.scan_completion_for_root("/tmp").expect("status"),
        ScanCompletion::Unknown
    );

    db.upsert_file(&record("/tmp/a.pdf", "a.pdf", Some("pdf"), 1, 2024))
        .expect("a watcher row registers the root");
    assert_eq!(
        db.scan_completion_for_root("/tmp").expect("status"),
        ScanCompletion::Incomplete
    );

    install(&db, "/tmp", &[("/tmp/a.pdf", "pdf")], 10);
    assert_eq!(
        db.scan_completion_for_root("/tmp").expect("status"),
        ScanCompletion::Complete
    );
}

#[test]
fn working_dir_db_path_uses_root_fallback_index_when_local_index_is_absent() {
    let _guard = env_guard();
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
    let _guard = env_guard();
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
    assert!(first.ends_with("index.db"));
}

#[test]
fn fts_detail_none_rejects_non_contiguous_trigram_false_positive() {
    let _guard = env_guard();
    // "abczzzbcdzzzcde.txt" contains all trigrams of "abcd" (abc, bcd) but
    // non-contiguously. "abcde.txt" contains "abcd" as a contiguous substring.
    // The Rust post-filter must eliminate the false positive.
    // Lock: a sibling test mutates LCTR_CONFIG_DIR (index_paths) globally; the
    // shared lock serializes against it so the FTS schema stays stable here.
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/abczzzbcdzzzcde.txt",
        "abczzzbcdzzzcde.txt",
        Some("txt"),
        100,
        2024,
    ))
    .expect("insert false positive");
    db.upsert_file(&record(
        "/tmp/abcde.txt",
        "abcde.txt",
        Some("txt"),
        100,
        2024,
    ))
    .expect("insert true positive");

    let results = db
        .search_with_options(&SearchOptions::new("abcd").with_mode(QueryMode::Contains))
        .expect("search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "abcde.txt");
}

#[test]
fn legacy_search_filters_fts_false_positives() {
    // Same fixture as above but through the legacy pub fn search path.
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    db.upsert_file(&record(
        "/tmp/abczzzbcdzzzcde.txt",
        "abczzzbcdzzzcde.txt",
        Some("txt"),
        100,
        2024,
    ))
    .expect("insert false positive");
    db.upsert_file(&record(
        "/tmp/abcde.txt",
        "abcde.txt",
        Some("txt"),
        100,
        2024,
    ))
    .expect("insert true positive");

    let results = db
        .search("abcd", &SearchFilters::new(), 10)
        .expect("search works");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "abcde.txt");
}

#[test]
fn import_access_from_copies_previous_history() {
    let _guard = env_guard();
    let tmp = tempdir().expect("tmp");
    let old_path = tmp.path().join("old.sqlite");
    let new_path = tmp.path().join("new.sqlite");

    let old_db = Database::open(&old_path).expect("open old");
    old_db.record_access("/tmp/a.txt").expect("record a");
    old_db.record_access("/tmp/a.txt").expect("record a again");
    old_db.record_access("/tmp/b.txt").expect("record b");
    drop(old_db);

    let new_db = Database::open(&new_path).expect("open new");
    assert_eq!(new_db.import_access_from(&old_path).unwrap(), 2);
    drop(new_db);

    let conn = rusqlite::Connection::open(&new_path).expect("open new raw");
    let count_a: i64 = conn
        .query_row(
            "SELECT open_count FROM access WHERE path = '/tmp/a.txt'",
            [],
            |row| row.get(0),
        )
        .expect("a count");
    assert_eq!(count_a, 2);
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM access", [], |row| row.get(0))
        .expect("total");
    assert_eq!(total, 2);
}

#[test]
fn import_access_from_missing_or_legacy_previous_is_noop() {
    let _guard = env_guard();
    let tmp = tempdir().expect("tmp");
    let new_db = Database::open(tmp.path().join("new.sqlite")).expect("open new");

    let missing = tmp.path().join("does-not-exist.sqlite");
    assert_eq!(new_db.import_access_from(&missing).unwrap(), 0);

    let legacy_path = tmp.path().join("legacy.sqlite");
    {
        let legacy = rusqlite::Connection::open(&legacy_path).expect("open legacy");
        legacy
            .execute_batch("CREATE TABLE files (id INTEGER PRIMARY KEY);")
            .expect("create legacy");
    }
    assert_eq!(new_db.import_access_from(&legacy_path).unwrap(), 0);
    // A second call proves DETACH ran after the first.
    assert_eq!(new_db.import_access_from(&legacy_path).unwrap(), 0);
}

#[test]
fn mark_path_deleted_only_removes_exact_subtree() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("open db");
    let sep = std::path::MAIN_SEPARATOR;
    let p = |parts: &[&str]| format!("{sep}{}", parts.join(&sep.to_string()));

    let dir = p(&["proj", "my_dir"]);
    let rows = [
        (dir.clone(), "my_dir", None),
        (
            p(&["proj", "my_dir", "alpha.txt"]),
            "alpha.txt",
            Some("txt"),
        ),
        (
            p(&["proj", "myXdir", "bravo.txt"]),
            "bravo.txt",
            Some("txt"),
        ),
        (
            p(&["proj", "MY_DIR", "charlie.txt"]),
            "charlie.txt",
            Some("txt"),
        ),
        (
            p(&["proj", "my_dir2", "delta.txt"]),
            "delta.txt",
            Some("txt"),
        ),
        (p(&["proj", "my%dir", "echo.txt"]), "echo.txt", Some("txt")),
    ];
    for (path, name, ext) in &rows {
        db.upsert_file(&record(path, name, *ext, 1, 2024))
            .expect("upsert");
    }

    assert_eq!(db.mark_path_deleted(&dir).unwrap(), 2);
    assert_eq!(db.count_active().unwrap(), 4);
    assert_eq!(db.mark_path_deleted(&dir).unwrap(), 0);

    let percent_dir = format!("{}{sep}", p(&["proj", "my%dir"]));
    assert_eq!(db.mark_path_deleted(&percent_dir).unwrap(), 1);
    assert_eq!(db.count_active().unwrap(), 3);
}

#[test]
fn mark_path_deleted_handles_a_single_file() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("open db");
    let sep = std::path::MAIN_SEPARATOR;
    let path = format!("{sep}proj{sep}only.txt");
    db.upsert_file(&record(&path, "only.txt", Some("txt"), 1, 2024))
        .expect("upsert");

    assert_eq!(db.mark_path_deleted(&path).unwrap(), 1);
    assert_eq!(db.count_active().unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn verified_search_keeps_unreadable_paths() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let locked_dir = dir.path().join("locked");
    std::fs::create_dir(&locked_dir).expect("create locked dir");
    let locked_file = locked_dir.join("report-locked.pdf");
    let open_file = dir.path().join("report-open.pdf");
    std::fs::write(&locked_file, "locked").expect("write locked file");
    std::fs::write(&open_file, "open").expect("write open file");

    {
        let db = Database::open(&db_path).expect("db opens");
        db.upsert_file(&record_for_path(&locked_file, Some("pdf"), 100, 2024))
            .expect("insert locked");
        db.upsert_file(&record_for_path(&open_file, Some("pdf"), 100, 2024))
            .expect("insert open");
    }

    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o000))
        .expect("lock dir");
    if std::fs::symlink_metadata(&locked_file).is_ok() {
        // Running as root: the permission-denied scenario cannot be produced.
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore dir");
        return;
    }

    let db = Database::open_readonly(&db_path)
        .expect("readonly db opens")
        .with_search_path_verification();
    let plain = db.search("report", &SearchFilters::new(), 10);
    let options = db.search_with_options(&SearchOptions::new("report"));
    let interactive = db.search_interactive("report", 10);

    // Restore permissions before any assert can panic so tempdir cleanup works.
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755))
        .expect("restore dir");

    for results in [
        plain.expect("fts search works"),
        options.expect("option search works"),
        interactive.expect("interactive search works"),
    ] {
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .any(|result| result.path == locked_file.to_string_lossy()));
        assert!(results
            .iter()
            .any(|result| result.path == open_file.to_string_lossy()));
    }
}

#[test]
fn verified_search_drops_paths_whose_parent_became_a_file() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let parent_file = dir.path().join("report-parent");
    std::fs::write(&parent_file, "i am a file").expect("write parent file");
    let child_path = parent_file.join("report-child.pdf");
    let real_path = dir.path().join("report-real.pdf");
    std::fs::write(&real_path, "real").expect("write real file");

    {
        let db = Database::open(&db_path).expect("db opens");
        db.upsert_file(&record_for_path(&child_path, Some("pdf"), 100, 2024))
            .expect("insert child");
        db.upsert_file(&record_for_path(&real_path, Some("pdf"), 100, 2024))
            .expect("insert real");
    }

    let db = Database::open_readonly(&db_path)
        .expect("readonly db opens")
        .with_search_path_verification();
    let results = db
        .search_with_options(&SearchOptions::new("report"))
        .expect("search succeeds despite ENOTDIR path");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, real_path.to_string_lossy());
}

#[test]
fn local_index_dir_gets_a_self_ignoring_gitignore() {
    let _env = env_guard();
    let dir = tempdir().expect("tempdir");
    let local = dir.path().join(".locator");
    let other = dir.path().join("elsewhere");

    Database::open(local.join("index.sqlite")).expect("open local index");
    Database::open(other.join("index.sqlite")).expect("open other index");

    assert_eq!(
        std::fs::read_to_string(local.join(".gitignore")).expect("gitignore written"),
        "*\n"
    );
    assert!(!other.join(".gitignore").exists());

    // An existing file is never overwritten.
    std::fs::write(local.join(".gitignore"), "custom\n").expect("write custom");
    Database::open(local.join("index.sqlite")).expect("reopen local index");
    assert_eq!(
        std::fs::read_to_string(local.join(".gitignore")).expect("read gitignore"),
        "custom\n"
    );
}

#[test]
fn embeddings_sidecar_in_a_local_index_dir_is_ignored_by_git() {
    let _env = env_guard();
    let dir = tempdir().expect("tempdir");
    let local = dir.path().join(".locator");
    let sidecar = locator::embed::store::sidecar_path_for_index(&local.join("index.sqlite"));

    locator::embed::store::EmbedStore::open(&sidecar).expect("open sidecar");

    assert!(sidecar.exists());
    assert_eq!(
        std::fs::read_to_string(local.join(".gitignore")).expect("gitignore written"),
        "*\n"
    );
}

#[test]
fn compacting_writes_a_name_file_that_matches_the_rows() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    db.upsert_files(&[
        record("/tmp/a.txt", "a.txt", Some("txt"), 1, 2024),
        record("/tmp/b.txt", "b.txt", Some("txt"), 2, 2024),
        record("/tmp/gone.txt", "gone.txt", Some("txt"), 3, 2024),
    ])
    .expect("upsert");
    db.mark_path_deleted("/tmp/gone.txt").expect("soft delete");

    db.compact_name_file().expect("compact");

    let file = NameFile::open(&name_file_path(&db_path)).expect("name file opens");
    let mut paths = Vec::new();
    file.reader().for_each(0..file.len(), &mut |row| {
        paths.push(row.path.to_string());
        true
    });
    paths.sort();
    assert_eq!(paths, ["/tmp/a.txt", "/tmp/b.txt"]);
}

#[test]
fn delete_index_files_removes_the_name_file() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    {
        let db = Database::open(&db_path).expect("db opens");
        db.upsert_file(&record("/tmp/a.txt", "a.txt", Some("txt"), 1, 2024))
            .expect("upsert");
        db.compact_name_file().expect("compact");
    }
    let names = name_file_path(&db_path);
    let stale_tmp = dir.path().join("index.names.tmp");
    std::fs::write(&stale_tmp, b"x").expect("write tmp");
    assert!(names.exists());

    delete_index_files(&db_path).expect("delete index files");

    assert!(!names.exists());
    assert!(!stale_tmp.exists());
    assert!(!db_path.exists());
}

#[test]
fn upserts_and_deletes_are_recorded_as_changes() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    db.upsert_file(&record("/tmp/a.txt", "a.txt", Some("txt"), 1, 2024))
        .expect("upsert");
    db.mark_path_deleted("/tmp/dir")
        .expect("delete a directory");

    let conn = rusqlite::Connection::open(&db_path).expect("raw connection");
    let mut stmt = conn
        .prepare("SELECT path, kind FROM changes ORDER BY path")
        .expect("prepare");
    let changes: Vec<(String, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        changes,
        [
            ("/tmp/a.txt".to_string(), Some("txt".to_string())),
            ("/tmp/dir".to_string(), None)
        ]
    );
}

#[test]
fn compacting_clears_the_changes_and_sets_a_snapshot_id() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    db.upsert_file(&record("/tmp/a.txt", "a.txt", Some("txt"), 1, 2024))
        .expect("upsert");
    db.mark_path_deleted("/tmp/dir")
        .expect("delete a directory");

    db.compact_name_file().expect("compact");

    let conn = rusqlite::Connection::open(&db_path).expect("raw connection");
    let pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(pending, 0);
    let snapshot: i64 = conn
        .query_row(
            "SELECT value FROM search_state WHERE key = 'snapshot_id'",
            [],
            |row| row.get(0),
        )
        .expect("snapshot id");
    assert_ne!(snapshot, 0);
}

#[test]
fn next_scan_token_never_repeats_for_a_root() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    let now = chrono::Utc::now().timestamp_millis();

    let first = db.next_scan_token("/tmp/a").expect("token for a new root");
    assert!(
        (first - now).abs() < 5_000,
        "token {first} is not near {now}"
    );

    let far_future = now + 24 * 60 * 60 * 1000;
    install(&db, "/tmp/a", &[], far_future);
    assert_eq!(db.next_scan_token("/tmp/a").unwrap(), far_future + 1);

    let other = db
        .next_scan_token("/tmp/b")
        .expect("token for another root");
    assert!(
        (other - now).abs() < 5_000,
        "token {other} is not near {now}"
    );
}

#[test]
fn install_scan_replaces_only_its_root() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    install(
        &db,
        "/a",
        &[
            ("/a/one.txt", "text"),
            ("/a/two.txt", "text"),
            ("/a/three.txt", "text"),
        ],
        1,
    );
    install(&db, "/b", &[("/b/x.txt", "text"), ("/b/y.txt", "text")], 1);

    install(&db, "/a", &[("/a/one.txt", "text")], 2);

    assert_eq!(all_paths(&db), ["/a/one.txt", "/b/x.txt", "/b/y.txt"]);
    assert_eq!(db.roots().expect("roots"), ["/a", "/b"]);
    for root in ["/a", "/b"] {
        assert_eq!(
            db.scan_completion_for_root(root).expect("status"),
            ScanCompletion::Complete
        );
    }
}

/// A scan installed with `token` over a root that already holds a row from before the scan,
/// a live row and a removal. Returns the database path and the database.
fn rescan_fixture(dir: &Path) -> (std::path::PathBuf, Database) {
    let db_path = dir.join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    install(
        &db,
        "/r",
        &[("/r/old.txt", "text"), ("/r/dir/inner.txt", "text")],
        1,
    );
    (db_path, db)
}

#[test]
fn a_rescan_keeps_changes_made_while_it_ran() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let (db_path, db) = rescan_fixture(dir.path());
    let token = 1_000;
    // Stamped at the token or later: the watcher wrote these while the scan walked.
    db.upsert_files_with_indexed_at(&[live_record("/r", "/r/live.txt", "text", 5)], token)
        .expect("live row at the token");
    // Stamped before the token: the scan already covers it.
    db.upsert_files_with_indexed_at(&[live_record("/r", "/r/early.txt", "text", 5)], token - 1)
        .expect("live row before the token");
    // A removal is stamped with the current time in milliseconds, far above the token.
    assert_eq!(db.mark_path_deleted("/r/old.txt").expect("remove"), 1);
    assert_eq!(change_count(&db_path), 3);

    // The walk saw `old.txt` before it was removed, and never saw `live.txt`.
    install(
        &db,
        "/r",
        &[("/r/old.txt", "text"), ("/r/dir/inner.txt", "text")],
        token,
    );

    assert_eq!(
        all_paths(&db),
        ["/r/dir/inner.txt", "/r/live.txt"],
        "the removal still hides old.txt and the live row still shows"
    );
    assert_eq!(change_count(&db_path), 2, "the older live row is gone");
}

#[test]
fn a_rescan_drops_changes_older_than_its_token() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let (db_path, db) = rescan_fixture(dir.path());
    db.upsert_files_with_indexed_at(&[live_record("/r", "/r/live.txt", "text", 5)], 100)
        .expect("live row");
    assert_eq!(db.mark_path_deleted("/r/old.txt").expect("remove"), 1);

    // The scan started after both changes, so what it saw is the truth.
    install(
        &db,
        "/r",
        &[("/r/old.txt", "text"), ("/r/dir/inner.txt", "text")],
        i64::MAX,
    );

    assert_eq!(all_paths(&db), ["/r/dir/inner.txt", "/r/old.txt"]);
    assert_eq!(change_count(&db_path), 0);
}

fn embeddable_paths(db: &Database, root: &str) -> Vec<String> {
    db.embeddable_files(root)
        .expect("embeddable files")
        .into_iter()
        .map(|file| file.path)
        .collect()
}

#[test]
fn an_outer_scan_takes_over_a_nested_root_it_walked() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    install(
        &db,
        "/r/sub",
        &[("/r/sub/a.txt", "text"), ("/r/sub/b.txt", "text")],
        1,
    );
    install(
        &db,
        "/r",
        &[("/r/top.txt", "text"), ("/r/sub/a.txt", "text")],
        2,
    );

    assert_eq!(db.roots().expect("roots"), ["/r"]);
    assert_eq!(all_paths(&db), ["/r/sub/a.txt", "/r/top.txt"]);
    assert_eq!(embeddable_paths(&db, "/r"), ["/r/sub/a.txt", "/r/top.txt"]);
}

#[test]
fn an_outer_scan_leaves_a_nested_root_it_did_not_walk() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    install(
        &db,
        "/r/build/app",
        &[
            ("/r/build/app/x.txt", "text"),
            ("/r/build/app/y.txt", "text"),
        ],
        1,
    );
    install(&db, "/r", &[("/r/a.txt", "text"), ("/r/b.txt", "text")], 2);

    assert_eq!(db.roots().expect("roots"), ["/r", "/r/build/app"]);
    assert_eq!(
        all_paths(&db),
        [
            "/r/a.txt",
            "/r/b.txt",
            "/r/build/app/x.txt",
            "/r/build/app/y.txt"
        ]
    );
    assert_eq!(embeddable_paths(&db, "/r"), ["/r/a.txt", "/r/b.txt"]);
    assert_eq!(
        embeddable_paths(&db, "/r/build/app"),
        ["/r/build/app/x.txt", "/r/build/app/y.txt"]
    );
}

/// Rows in the name file and changes of every kind, for the compaction tests. Returns the
/// database path and the database. The active rows are `/r/b.txt`, `/r/new.txt` and
/// `/r/d2.txt`.
fn changed_fixture(dir: &Path) -> (std::path::PathBuf, Database) {
    let db_path = dir.join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    install(
        &db,
        "/r",
        &[
            ("/r/a.txt", "text"),
            ("/r/b.txt", "text"),
            ("/r/d", "folder"),
            ("/r/d/c.txt", "text"),
            ("/r/d/e.txt", "text"),
        ],
        1,
    );
    db.upsert_file(&live_record("/r", "/r/new.txt", "text", 9))
        .expect("new file");
    db.upsert_file(&live_record("/r", "/r/d2.txt", "text", 9))
        .expect("sibling with the folder's prefix");
    db.mark_path_deleted("/r/d").expect("remove the folder");
    db.mark_path_deleted("/r/a.txt").expect("remove a file");
    (db_path, db)
}

#[test]
fn compacting_folds_changes_and_empties_the_table() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let (db_path, db) = changed_fixture(dir.path());
    let expected = ["/r/b.txt", "/r/d2.txt", "/r/new.txt"];
    assert_eq!(all_paths(&db), expected);
    assert_eq!(db.count_active().expect("count"), 3);
    assert!(change_count(&db_path) > 0);

    db.compact_name_file().expect("compact");

    assert_eq!(all_paths(&db), expected);
    assert_eq!(db.count_active().expect("count"), 3);
    assert_eq!(change_count(&db_path), 0);
    for (query, found) in [
        ("new", 1),
        ("c.txt", 0),
        ("a.txt", 0),
        ("d2", 1),
        ("b.txt", 1),
    ] {
        let results = db
            .search_with_options(&SearchOptions::new(query))
            .expect("search works");
        assert_eq!(results.len(), found, "query {query}");
    }
}

#[test]
fn folded_changes_applied_again_change_nothing() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let (db_path, db) = changed_fixture(dir.path());
    let queries = ["", "txt", "new", "c.txt", "d2", "/r/d"];
    let snapshot = |db: &Database| -> Vec<Vec<String>> {
        queries
            .iter()
            .map(|query| {
                let mut paths: Vec<String> = db
                    .search(query, &SearchFilters::new(), 100)
                    .expect("search works")
                    .into_iter()
                    .map(|result| result.path)
                    .collect();
                paths.sort();
                paths
            })
            .collect()
    };
    let before = snapshot(&db);
    let before_options = db
        .search_with_options(&SearchOptions::new("r").with_limit(100))
        .expect("option search")
        .len();

    let conn = rusqlite::Connection::open(&db_path).expect("raw connection");
    type ChangeRow = (String, Option<String>, i64, Option<i64>, Option<i64>, i64);
    let saved: Vec<ChangeRow> = conn
        .prepare("SELECT path, kind, size_bytes, created_at, modified_at, changed_at FROM changes")
        .expect("prepare")
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(!saved.is_empty());

    db.compact_name_file().expect("compact");
    assert_eq!(change_count(&db_path), 0);
    for row in &saved {
        conn.execute(
            "INSERT INTO changes (path, kind, size_bytes, created_at, modified_at, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![row.0, row.1, row.2, row.3, row.4, row.5],
        )
        .expect("put the change back");
    }
    assert_eq!(change_count(&db_path), saved.len() as i64);

    assert_eq!(snapshot(&db), before);
    assert_eq!(
        db.search_with_options(&SearchOptions::new("r").with_limit(100))
            .expect("option search")
            .len(),
        before_options
    );
    assert_eq!(db.count_active().expect("count"), 3);
}

#[test]
fn a_file_recreated_under_a_removed_folder_is_found() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    install(
        &db,
        "/r",
        &[
            ("/r/d/x.txt", "text"),
            ("/r/d/y.txt", "text"),
            ("/r/z.txt", "text"),
        ],
        1,
    );
    assert_eq!(db.mark_path_deleted("/r/d").expect("remove folder"), 2);
    assert_eq!(all_paths(&db), ["/r/z.txt"]);

    db.upsert_file(&live_record("/r", "/r/d/x.txt", "text", 3))
        .expect("recreate one file");

    let expected = ["/r/d/x.txt", "/r/z.txt"];
    assert_eq!(all_paths(&db), expected);
    assert_eq!(db.count_active().expect("count"), 2);

    db.compact_name_file().expect("compact");
    assert_eq!(all_paths(&db), expected);
    drop(db);
    let db = Database::open(&db_path).expect("reopen");
    assert_eq!(all_paths(&db), expected);
}

#[test]
fn remove_root_drops_its_rows_and_its_registration() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    install(
        &db,
        "/a",
        &[
            ("/a/x.txt", "text"),
            ("/a/y.txt", "text"),
            ("/a/z.txt", "text"),
        ],
        1,
    );
    install(&db, "/a/n", &[("/a/n/q.txt", "text")], 1);
    install(&db, "/b", &[("/b/w.txt", "text")], 1);
    db.upsert_file(&live_record("/a", "/a/live.txt", "text", 1))
        .expect("live row owned by the root");
    db.mark_path_deleted("/b/gone.txt").expect("a removal");

    let dropped = db.remove_root("/a").expect("remove root");

    assert_eq!(dropped, 4, "three scanned rows and one live row");
    assert_eq!(all_paths(&db), ["/a/n/q.txt", "/b/w.txt"]);
    assert_eq!(db.roots().expect("roots"), ["/a/n", "/b"]);
    assert_eq!(
        db.scan_completion_for_root("/a").expect("status"),
        ScanCompletion::Unknown
    );
    assert_eq!(change_count(&db_path), 0);
}

#[test]
fn an_abandoned_scan_changes_nothing() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    install(
        &db,
        "/r",
        &[("/r/old.txt", "text"), ("/r/older.txt", "text")],
        1,
    );
    let names = name_file_path(&db_path);
    let file_before = std::fs::read(&names).expect("read the name file");

    let mut abandoned = NameFileBuilder::default();
    abandoned.push("/r/fresh.txt", "text").expect("push");
    drop(abandoned);

    assert_eq!(std::fs::read(&names).expect("read again"), file_before);
    assert_eq!(all_paths(&db), ["/r/old.txt", "/r/older.txt"]);
    assert_eq!(
        db.scan_completion_for_root("/r").expect("status"),
        ScanCompletion::Complete
    );
    drop(db);
    let db = Database::open(&db_path).expect("reopen");
    assert_eq!(all_paths(&db), ["/r/old.txt", "/r/older.txt"]);
    assert_eq!(
        db.scan_completion_for_root("/r").expect("status"),
        ScanCompletion::Complete
    );
}

#[test]
fn a_missing_name_file_marks_roots_incomplete() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    {
        let db = Database::open(&db_path).expect("db opens");
        install(&db, "/r", &[("/r/a.txt", "text")], 1);
        assert_eq!(
            db.scan_completion_for_root("/r").expect("status"),
            ScanCompletion::Complete
        );
    }
    std::fs::remove_file(name_file_path(&db_path)).expect("delete the name file");

    let db = Database::open(&db_path).expect("reopen");

    assert_eq!(
        db.scan_completion_for_root("/r").expect("status"),
        ScanCompletion::Incomplete
    );
    let results = db
        .search("a", &SearchFilters::new(), 10)
        .expect("a search with no name file is not an error");
    assert!(results.is_empty());
    let results = db
        .search_with_options(&SearchOptions::new("a"))
        .expect("an option search with no name file is not an error");
    assert!(results.is_empty());
}

#[test]
fn the_index_holds_no_row_table() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    {
        let db = Database::open(&db_path).expect("db opens");
        let paths: Vec<String> = (0..20_000)
            .map(|index| format!("/data/folder-{:03}/report-{index:05}.txt", index % 200))
            .collect();
        let rows: Vec<(&str, &str)> = paths.iter().map(|path| (path.as_str(), "text")).collect();
        install(&db, "/data", &rows, 1);
        assert_eq!(db.count_active().expect("count"), 20_000);
    }

    let conn = rusqlite::Connection::open(&db_path).expect("raw connection");
    let files_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'files'",
            [],
            |row| row.get(0),
        )
        .expect("count tables");
    assert_eq!(files_tables, 0);
    drop(conn);

    let wal = std::path::PathBuf::from(format!("{}-wal", db_path.display()));
    let db_bytes = std::fs::metadata(&db_path).expect("db metadata").len()
        + std::fs::metadata(&wal).map_or(0, |meta| meta.len());
    let names_bytes = std::fs::metadata(name_file_path(&db_path))
        .expect("name file metadata")
        .len();
    assert!(db_bytes < 200 * 1024, "database is {db_bytes} bytes");
    assert!(names_bytes < 400 * 1024, "name file is {names_bytes} bytes");
}

#[test]
fn path_lookup_sees_the_file_and_the_changes() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let root = dir.path();
    let path = |name: &str| root.join(name).to_string_lossy().to_string();
    std::fs::create_dir(root.join("gone")).expect("create folder");
    for name in ["in-file.txt", "live.txt", "removed.txt", "gone/inside.txt"] {
        std::fs::write(root.join(name), "four").expect("write file");
    }
    let root_text = root.to_string_lossy().to_string();
    let db = Database::open_in_memory()
        .expect("db opens")
        .with_search_path_verification();
    install(
        &db,
        &root_text,
        &[
            (&path("in-file.txt"), "text"),
            (&path("removed.txt"), "text"),
            (&path("gone/inside.txt"), "text"),
        ],
        1,
    );
    db.upsert_file(&live_record(&root_text, &path("live.txt"), "text", 99))
        .expect("live change");
    db.upsert_file(&live_record(&root_text, &path("vanished.txt"), "text", 99))
        .expect("live change whose file is gone");
    db.mark_path_deleted(&path("removed.txt")).expect("remove");
    db.mark_path_deleted(&path("gone")).expect("remove folder");

    let mut lookup = db.path_lookup().expect("lookup");

    let in_file = lookup
        .result_for(&path("in-file.txt"))
        .expect("in the file");
    assert_eq!(in_file.kind, "text");
    assert_eq!(in_file.size_bytes, 4, "size comes from the disk");
    let live = lookup.result_for(&path("live.txt")).expect("live change");
    assert_eq!(live.size_bytes, 4, "a live row is hydrated like any other");
    assert!(lookup.result_for(&path("removed.txt")).is_none());
    assert!(lookup.result_for(&path("gone/inside.txt")).is_none());
    assert!(lookup.result_for(&path("absent.txt")).is_none());
    assert!(
        lookup.result_for(&path("vanished.txt")).is_none(),
        "verification drops a live row whose file is gone"
    );
    let one_off = db
        .result_for_path(&path("in-file.txt"))
        .expect("one-off lookup")
        .expect("found");
    assert_eq!(one_off.path, in_file.path);
}

#[test]
fn count_and_embeddable_files_follow_changes() {
    let _guard = env_guard();
    let db = Database::open_in_memory().expect("db opens");
    install(
        &db,
        "/r",
        &[
            ("/r/a.txt", "text"),
            ("/r/b.md", "markdown"),
            ("/r/sub", "folder"),
            ("/r/sub/c.txt", "text"),
        ],
        1,
    );
    install(&db, "/r/n", &[("/r/n/q.txt", "text")], 1);
    install(&db, "/o", &[("/o/z.txt", "text")], 1);
    let paths = |db: &Database| -> Vec<String> {
        db.embeddable_files("/r")
            .expect("embeddable files")
            .into_iter()
            .map(|file| file.path)
            .collect()
    };
    assert_eq!(db.count_active().expect("count"), 6);
    assert_eq!(paths(&db), ["/r/a.txt", "/r/b.md", "/r/sub/c.txt"]);

    db.upsert_file(&live_record("/r", "/r/d.txt", "text", 7))
        .expect("add a file");
    assert_eq!(db.count_active().expect("count"), 7);
    assert_eq!(
        paths(&db),
        ["/r/a.txt", "/r/b.md", "/r/d.txt", "/r/sub/c.txt"]
    );
    let added = db
        .embeddable_files("/r")
        .expect("embeddable files")
        .into_iter()
        .find(|file| file.path == "/r/d.txt")
        .expect("the new file");
    assert_eq!(added.size_bytes, 7);
    assert_eq!(added.extension.as_deref(), Some("txt"));

    db.mark_path_deleted("/r/a.txt").expect("remove a file");
    assert_eq!(db.count_active().expect("count"), 6);
    assert_eq!(paths(&db), ["/r/b.md", "/r/d.txt", "/r/sub/c.txt"]);

    assert_eq!(db.mark_path_deleted("/r/sub").expect("remove folder"), 2);
    assert_eq!(db.count_active().expect("count"), 4);
    assert_eq!(paths(&db), ["/r/b.md", "/r/d.txt"]);
}

/// One row of an old index (written by lctr 0.4.x): the path, whether it is deleted, and the
/// root that owns it.
struct OldRow<'a> {
    path: &'a str,
    deleted: bool,
    root: &'a str,
}

/// Writes an index in the old format at `path`: a `files` table, `scan_roots` (each root
/// listed here is complete with token 7) and `access` with one open of `/old/opened.txt`.
fn write_old_index(path: &Path, rows: &[OldRow], roots: &[&str]) {
    let conn = rusqlite::Connection::open(path).expect("open raw old db");
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            name_lower TEXT,
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
         CREATE TABLE scan_roots (
            root TEXT PRIMARY KEY,
            scan_token INTEGER NOT NULL,
            complete INTEGER NOT NULL,
            started_at INTEGER NOT NULL,
            completed_at INTEGER
         );
         CREATE TABLE access (
            path TEXT PRIMARY KEY,
            open_count INTEGER NOT NULL DEFAULT 0,
            last_opened_at INTEGER
         );
         INSERT INTO access (path, open_count, last_opened_at) VALUES ('/old/opened.txt', 3, 100);",
    )
    .expect("create old schema");
    for row in rows {
        let name = row.path.rsplit('/').next().expect("a name");
        conn.execute(
            "INSERT INTO files (path, name, name_lower, parent, extension, root, volume, kind,
                                size_bytes, indexed_at, deleted)
             VALUES (?1, ?2, ?3, '', NULL, ?4, 'local', 'txt', 0, 1, ?5)",
            rusqlite::params![
                row.path,
                name,
                name.to_lowercase(),
                row.root,
                row.deleted as i64
            ],
        )
        .expect("insert old row");
    }
    for root in roots {
        conn.execute(
            "INSERT INTO scan_roots (root, scan_token, complete, started_at, completed_at)
             VALUES (?1, 7, 1, 5, 6)",
            [root],
        )
        .expect("insert old root");
    }
}

/// Leaves empty `-wal` and `-shm` files beside the old index, as an unclean exit would.
fn leave_old_index_sidecars(old: &Path) {
    for suffix in ["-wal", "-shm"] {
        std::fs::write(format!("{}{suffix}", old.display()), b"").expect("write sidecar");
    }
}

fn old_index_files_exist(dir: &Path) -> bool {
    ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"]
        .iter()
        .any(|name| dir.join(name).exists())
}

fn old_row<'a>(path: &'a str, root: &'a str) -> OldRow<'a> {
    OldRow {
        path,
        deleted: false,
        root,
    }
}

#[test]
fn an_old_index_beside_the_new_one_is_converted_and_removed() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let old = dir.path().join("index.sqlite");
    write_old_index(
        &old,
        &[
            old_row("/old/opened.txt", "/old"),
            old_row("/old/notes.txt", "/old"),
            OldRow {
                path: "/old/gone.txt",
                deleted: true,
                root: "/old",
            },
        ],
        &["/old"],
    );
    leave_old_index_sidecars(&old);
    let new = dir.path().join("index.db");

    let db = Database::open(&new).expect("open converts the old index");

    assert_eq!(all_paths(&db), ["/old/notes.txt", "/old/opened.txt"]);
    assert_eq!(
        db.scan_completion_for_root("/old").expect("completion"),
        ScanCompletion::Complete
    );
    let raw = rusqlite::Connection::open(&new).expect("raw connection");
    let history: (i64, i64) = raw
        .query_row(
            "SELECT open_count, last_opened_at FROM access WHERE path = '/old/opened.txt'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("open history is kept");
    assert_eq!(history, (3, 100));
    assert!(!old_index_files_exist(dir.path()));
}

#[test]
fn discovery_finds_a_folder_that_only_has_an_old_index() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let project = dir.path().join("proj");
    let sub = project.join("sub");
    let local = project.join(".locator");
    std::fs::create_dir_all(&sub).expect("create dirs");
    std::fs::create_dir_all(&local).expect("create index dir");
    write_old_index(
        &local.join("index.sqlite"),
        &[old_row("/old/notes.txt", "/old")],
        &["/old"],
    );

    let found = existing_db_path_for_working_dir(&sub)
        .expect("discovery")
        .expect("an index is found");

    let expected = project
        .canonicalize()
        .expect("canonical project")
        .join(".locator")
        .join("index.db");
    assert_eq!(found, expected);
    let db = Database::open(&found).expect("open converts");
    assert_eq!(all_paths(&db), ["/old/notes.txt"]);
}

#[test]
fn an_old_index_that_reappears_replaces_its_roots_and_is_removed() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let old = dir.path().join("index.sqlite");
    let new = dir.path().join("index.db");
    write_old_index(&old, &[old_row("/old/first.txt", "/old")], &["/old"]);
    drop(Database::open(&new).expect("first conversion"));
    assert!(!old.exists());

    write_old_index(&old, &[old_row("/old/second.txt", "/old")], &["/old"]);
    let db = Database::open(&new).expect("second conversion");

    assert_eq!(all_paths(&db), ["/old/second.txt"]);
    assert!(!old_index_files_exist(dir.path()));
}

#[test]
fn a_sibling_that_is_not_an_index_is_left_alone() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let sqlite_only = dir.path().join("a");
    let not_sqlite = dir.path().join("b");
    std::fs::create_dir_all(&sqlite_only).expect("create dir a");
    std::fs::create_dir_all(&not_sqlite).expect("create dir b");
    {
        let conn = rusqlite::Connection::open(sqlite_only.join("index.sqlite")).expect("raw db");
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .expect("create other table");
    }
    std::fs::write(not_sqlite.join("index.sqlite"), b"this is not a database")
        .expect("write text file");
    let sqlite_before = std::fs::read(sqlite_only.join("index.sqlite")).expect("read a");

    Database::open(sqlite_only.join("index.db")).expect("open beside a plain sqlite file");
    Database::open(not_sqlite.join("index.db")).expect("open beside a text file");

    assert_eq!(
        std::fs::read(sqlite_only.join("index.sqlite")).expect("a stays"),
        sqlite_before
    );
    assert_eq!(
        std::fs::read(not_sqlite.join("index.sqlite")).expect("b stays"),
        b"this is not a database"
    );
}

#[test]
fn a_newer_database_is_refused_with_the_update_hint() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("index.db");
    drop(Database::open(&path).expect("create"));
    rusqlite::Connection::open(&path)
        .expect("raw connection")
        .pragma_update(None, "user_version", 99)
        .expect("set user_version");

    let writable = Database::open(&path).err().expect("open is refused");
    let readonly = Database::open_readonly(&path)
        .err()
        .expect("open_readonly is refused");

    assert!(format!("{writable:#}").contains("lctr update"));
    assert!(format!("{readonly:#}").contains("lctr update"));
}

#[test]
fn delete_index_files_removes_old_and_new_files() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.db");
    {
        let db = Database::open(&db_path).expect("db opens");
        db.upsert_file(&record("/tmp/a.txt", "a.txt", Some("txt"), 1, 2024))
            .expect("upsert");
        db.compact_name_file().expect("compact");
    }
    for name in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        std::fs::write(dir.path().join(name), b"x").expect("write old file");
    }

    let removed = delete_index_files(&db_path).expect("delete index files");

    assert!(!db_path.exists());
    assert!(!name_file_path(&db_path).exists());
    assert!(!old_index_files_exist(dir.path()));
    assert!(removed >= 5, "removed {removed}");
}

#[test]
fn an_old_index_in_another_root_keeps_this_index_rows() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    let new = dir.path().join("index.db");
    {
        let db = Database::open(&new).expect("db opens");
        install(&db, "/a", &[("/a/mine.txt", "txt")], 10);
    }
    write_old_index(
        &dir.path().join("index.sqlite"),
        &[old_row("/b/theirs.txt", "/b")],
        &["/b"],
    );

    let db = Database::open(&new).expect("open converts");

    assert_eq!(all_paths(&db), ["/a/mine.txt", "/b/theirs.txt"]);
    assert_eq!(
        db.scan_completion_for_root("/a").expect("completion"),
        ScanCompletion::Complete
    );
    assert!(!dir.path().join("index.sqlite").exists());
}

#[test]
fn remove_old_staging_files_clears_the_leftovers_of_an_interrupted_scan() {
    let _guard = env_guard();
    let dir = tempdir().expect("temp dir");
    std::env::set_var("LCTR_DATA_DIR", dir.path().join("data"));
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("create root");
    let folder = fallback_db_path_for_root(&root)
        .expect("fallback db path")
        .parent()
        .expect("index folder")
        .to_path_buf();
    let staging_names = [
        "staging.sqlite",
        "staging.sqlite-wal",
        "staging.sqlite-shm",
        "staging.names",
        "staging.names.tmp",
    ];
    let write_staging = || {
        std::fs::create_dir_all(&folder).expect("create index folder");
        for name in staging_names {
            std::fs::write(folder.join(name), "left behind").expect("write staging file");
        }
    };

    write_staging();
    locator::db::remove_old_staging_files(&root);
    for name in staging_names {
        assert!(!folder.join(name).exists(), "{name} should be gone");
    }
    assert!(!folder.exists(), "the empty folder should be gone");

    write_staging();
    std::fs::write(folder.join("index.db"), "a real index").expect("write index file");
    locator::db::remove_old_staging_files(&root);
    for name in staging_names {
        assert!(!folder.join(name).exists(), "{name} should be gone");
    }
    assert!(folder.join("index.db").exists(), "the real index stays");
    assert!(folder.exists(), "the folder with an index stays");
    std::env::remove_var("LCTR_DATA_DIR");
}

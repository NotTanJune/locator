use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::symlink;

use locator::db::Database;
use locator::query::SearchFilters;
use locator::scanner::{
    classify_scan_error, scan_root, scan_root_with_progress, ScanErrorKind, ScanOptions, ScanPhase,
};
use tempfile::tempdir;

#[test]
fn scanner_indexes_dotfiles_and_skips_noise_dirs() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join(".env"), "secret").expect("write dotfile");
    fs::write(dir.path().join(".DS_Store"), "ignored").expect("write finder metadata");
    fs::write(dir.path().join("._report.pdf"), "ignored").expect("write appledouble metadata");
    fs::create_dir(dir.path().join(".git")).expect("make git dir");
    fs::write(dir.path().join(".git").join("config"), "ignored").expect("write ignored");
    fs::create_dir(dir.path().join("node_modules")).expect("make node_modules");
    fs::write(dir.path().join("node_modules").join("pkg.js"), "ignored").expect("write ignored");
    fs::create_dir(dir.path().join("__MACOSX")).expect("make macosx dir");
    fs::write(
        dir.path().join("__MACOSX").join("archive-metadata"),
        "ignored",
    )
    .expect("write ignored");
    fs::create_dir(dir.path().join(".Spotlight-V100")).expect("make spotlight dir");
    fs::write(dir.path().join(".Spotlight-V100").join("store"), "ignored").expect("write ignored");
    fs::write(dir.path().join("report.pdf"), "fake").expect("write pdf");

    let db = Database::open_in_memory().expect("db opens");
    let stats = scan_root(&db, dir.path(), ScanOptions::default()).expect("scan succeeds");

    assert_eq!(stats.indexed_files, 2);
    assert_eq!(
        db.search("env", &SearchFilters::new(), 10)
            .expect("search")
            .len(),
        1
    );
    assert_eq!(
        db.search("report", &SearchFilters::new(), 10)
            .expect("search")
            .len(),
        1
    );
    assert!(db
        .search("config", &SearchFilters::new(), 10)
        .expect("search")
        .is_empty());
    assert!(db
        .search("pkg", &SearchFilters::new(), 10)
        .expect("search")
        .is_empty());
    assert!(db
        .search("store", &SearchFilters::new(), 10)
        .expect("search")
        .is_empty());
    assert!(db
        .search("DS_Store", &SearchFilters::new(), 10)
        .expect("search")
        .is_empty());
    assert!(db
        .search("archive-metadata", &SearchFilters::new(), 10)
        .expect("search")
        .is_empty());
}

#[test]
fn default_scan_options_prioritize_fast_first_scan() {
    let options = ScanOptions::default();

    assert!(!options.estimate_totals);
    assert_eq!(options.backend, locator::scanner::ScanBackend::Dirent);
    assert_eq!(options.batch_size, 500_000);
    assert_eq!(options.writer_queue_batches, 32);
    assert_eq!(options.native_buffer_bytes, 16 * 1024 * 1024);
    assert_eq!(options.native_workers, 8);
    assert_eq!(options.native_output_batch_size, 4096);
}

#[test]
fn scanner_reports_progress_during_scan() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");
    fs::write(dir.path().join("two.txt"), "two").expect("write file");

    let db = Database::open_in_memory().expect("db opens");
    let mut events = Vec::new();
    let canonical_root = dir
        .path()
        .canonicalize()
        .expect("canonical root")
        .to_string_lossy()
        .to_string();
    let stats = scan_root_with_progress(
        &db,
        dir.path(),
        ScanOptions {
            estimate_totals: true,
            ..Default::default()
        },
        |progress| {
            events.push((
                progress.indexed_files,
                progress.total_files,
                progress.phase,
                progress.current_path.clone(),
            ));
        },
    )
    .expect("scan succeeds");

    assert_eq!(stats.indexed_files, 2);
    assert!(!events.is_empty());
    assert!(events.iter().any(|(_, _, _, path)| path == &canonical_root));
    assert!(events
        .iter()
        .any(|(_, total, phase, _)| { *total == Some(2) && matches!(phase, ScanPhase::Indexing) }));
}

#[test]
fn scanner_reports_live_progress_while_discovering_totals() {
    let dir = tempdir().expect("temp dir");
    for index in 0..5 {
        fs::write(dir.path().join(format!("file-{index}.txt")), "data").expect("write file");
    }

    let db = Database::open_in_memory().expect("db opens");
    let mut discovery_counts = Vec::new();
    scan_root_with_progress(
        &db,
        dir.path(),
        ScanOptions {
            estimate_totals: true,
            ..Default::default()
        },
        |progress| {
            if matches!(progress.phase, ScanPhase::Discovering) {
                discovery_counts.push(progress.indexed_files);
            }
        },
    )
    .expect("scan succeeds");

    assert!(discovery_counts.iter().any(|count| *count > 0));
}

#[test]
fn scanner_skips_discovery_phase_when_eta_is_disabled() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");

    let db = Database::open_in_memory().expect("db opens");
    let mut phases = Vec::new();
    scan_root_with_progress(
        &db,
        dir.path(),
        ScanOptions {
            estimate_totals: false,
            ..Default::default()
        },
        |progress| phases.push(progress.phase),
    )
    .expect("scan succeeds");

    assert!(!phases
        .iter()
        .any(|phase| matches!(phase, ScanPhase::Discovering)));
}

#[test]
fn scanner_records_scan_profile_timings() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");

    let db = Database::open_in_memory().expect("db opens");
    let stats = scan_root(&db, dir.path(), ScanOptions::default()).expect("scan succeeds");

    assert!(stats.profile.total >= stats.profile.walk);
    assert_eq!(stats.profile.batches, 1);
    assert_eq!(stats.profile.indexed_files, 1);
}

#[test]
fn fresh_index_scan_skips_stale_mark() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");

    let db = Database::open_in_memory().expect("db opens");
    let stats = scan_root(
        &db,
        dir.path(),
        ScanOptions {
            fresh_index: true,
            ..Default::default()
        },
    )
    .expect("scan succeeds");

    assert_eq!(stats.indexed_files, 1);
    assert_eq!(stats.profile.stale_mark.as_secs(), 0);
}

#[test]
fn scan_defers_file_metadata_until_search_results() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");

    let db = Database::open_in_memory().expect("db opens");
    let stats = scan_root(&db, dir.path(), ScanOptions::default()).expect("scan succeeds");

    assert_eq!(stats.indexed_files, 1);
    assert_eq!(stats.indexed_bytes, 0);

    let results = db
        .search_interactive("one", 5)
        .expect("interactive search works");

    assert_eq!(results[0].size_bytes, 3);
    assert!(results[0].modified_at.is_some());
}

#[test]
fn scanner_classifies_common_metadata_errors() {
    assert_eq!(
        classify_scan_error(&io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
        ScanErrorKind::PermissionDenied
    );
    assert_eq!(
        classify_scan_error(&io::Error::new(io::ErrorKind::NotFound, "gone")),
        ScanErrorKind::NotFound
    );
    assert_eq!(
        classify_scan_error(&io::Error::new(io::ErrorKind::InvalidData, "bad")),
        ScanErrorKind::Other
    );
}

#[test]
#[cfg(unix)]
fn scanner_indexes_broken_symlink_without_metadata_error() {
    let dir = tempdir().expect("temp dir");
    symlink(
        dir.path().join("missing-target"),
        dir.path().join("broken-link"),
    )
    .expect("create symlink");

    let db = Database::open_in_memory().expect("db opens");
    let stats = scan_root(&db, dir.path(), ScanOptions::default()).expect("scan succeeds");

    assert_eq!(stats.indexed_files, 1);
    assert_eq!(stats.error_entries, 0);
    assert_eq!(
        db.search("broken", &SearchFilters::new(), 10)
            .expect("search")
            .len(),
        1
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_backend_indexes_files_and_reports_native_progress() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");

    let db = Database::open_in_memory().expect("db opens");
    let mut backends = Vec::new();
    let stats = scan_root_with_progress(
        &db,
        dir.path(),
        ScanOptions {
            backend: locator::scanner::ScanBackend::Native,
            ..Default::default()
        },
        |progress| backends.push(progress.backend),
    )
    .expect("native scan succeeds");

    assert_eq!(stats.indexed_files, 1);
    assert!(backends
        .iter()
        .all(|backend| matches!(backend, locator::scanner::ScanBackend::Native)));
    assert_eq!(
        db.search("one", &SearchFilters::new(), 10)
            .expect("search")
            .len(),
        1
    );
}

#[cfg(target_os = "macos")]
#[test]
fn dirent_backend_indexes_files_and_reports_dirent_progress() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("one.txt"), "one").expect("write file");
    fs::create_dir(dir.path().join("nested")).expect("create nested dir");
    fs::write(dir.path().join("nested").join("two.pdf"), "two").expect("write nested file");

    let db = Database::open_in_memory().expect("db opens");
    let mut backends = Vec::new();
    let stats = scan_root_with_progress(
        &db,
        dir.path(),
        ScanOptions {
            backend: locator::scanner::ScanBackend::Dirent,
            ..Default::default()
        },
        |progress| backends.push(progress.backend),
    )
    .expect("dirent scan succeeds");

    assert_eq!(stats.indexed_files, 2);
    assert!(backends
        .iter()
        .all(|backend| matches!(backend, locator::scanner::ScanBackend::Dirent)));
    assert_eq!(
        db.search("two", &SearchFilters::new(), 10)
            .expect("search")
            .len(),
        1
    );
}

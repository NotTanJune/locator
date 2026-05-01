use std::time::Duration;

use locator::scan_ui::{render_scan_frame, ScanAnimation};
use locator::scanner::{ScanBackend, ScanPhase, ScanProgress};

#[test]
fn scan_progress_estimates_eta_from_known_totals() {
    let progress = ScanProgress {
        phase: ScanPhase::Indexing,
        indexed_files: 250,
        skipped_entries: 3,
        error_entries: 1,
        indexed_bytes: 25_000,
        total_files: Some(1_000),
        total_bytes: Some(100_000),
        elapsed: Duration::from_secs(10),
        current_path: "/tmp/Documents".to_string(),
        backend: ScanBackend::ParallelWalk,
    };

    assert_eq!(progress.percent_complete(), Some(25));
    assert_eq!(progress.files_per_second(), 25.0);
    assert_eq!(progress.eta(), Some(Duration::from_secs(30)));
}

#[test]
fn scan_dashboard_frame_contains_ascii_art_eta_counts_and_backend() {
    let progress = ScanProgress {
        phase: ScanPhase::Indexing,
        indexed_files: 250,
        skipped_entries: 3,
        error_entries: 1,
        indexed_bytes: 25_000,
        total_files: Some(1_000),
        total_bytes: Some(100_000),
        elapsed: Duration::from_secs(10),
        current_path: "/tmp/Documents".to_string(),
        backend: ScanBackend::ParallelWalk,
    };

    let frame = render_scan_frame(&progress, ScanAnimation::frame(1));

    assert!(frame.contains("________"));
    assert!(frame.contains("(O)"));
    assert!(!frame.contains("[folder]"));
    assert!(frame.contains("ETA 00:30"));
    assert!(frame.contains("250 / 1000 files"));
    assert!(frame.contains("25.0 files/s"));
    assert!(frame.contains("parallel"));
}

#[test]
fn scan_dashboard_frame_labels_dirent_backend() {
    let progress = ScanProgress {
        phase: ScanPhase::Indexing,
        indexed_files: 10,
        skipped_entries: 0,
        error_entries: 0,
        indexed_bytes: 0,
        total_files: None,
        total_bytes: None,
        elapsed: Duration::from_secs(1),
        current_path: "/tmp/Documents".to_string(),
        backend: ScanBackend::Dirent,
    };

    let frame = render_scan_frame(&progress, ScanAnimation::frame(1));

    assert!(frame.contains("backend dirent"));
}

#[test]
fn scan_dashboard_frame_hides_eta_when_disabled() {
    let progress = ScanProgress {
        phase: ScanPhase::Indexing,
        indexed_files: 250,
        skipped_entries: 3,
        error_entries: 1,
        indexed_bytes: 25_000,
        total_files: None,
        total_bytes: None,
        elapsed: Duration::from_secs(10),
        current_path: "/tmp/Documents".to_string(),
        backend: ScanBackend::ParallelWalk,
    };

    let frame =
        locator::scan_ui::render_scan_frame_with_eta(&progress, ScanAnimation::frame(1), false);

    assert!(frame.contains("ETA off"));
    assert!(!frame.contains("ETA estimating"));
}

#[test]
fn auto_backend_reports_native_fallback_when_native_unavailable() {
    assert_eq!(ScanBackend::Auto.resolved_name(false), "parallel");
    assert_eq!(ScanBackend::Auto.resolved_name(true), "native");
    assert_eq!(ScanBackend::Dirent.resolved_name(true), "dirent");
}

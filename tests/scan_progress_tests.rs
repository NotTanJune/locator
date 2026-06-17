use std::time::Duration;

use std::collections::BTreeMap;
use std::path::Path;

use locator::scan_ui::{render_scan_frame, render_scan_summary, ScanAnimation, ScanSummary};
use locator::scanner::{
    ScanBackend, ScanErrorKind, ScanErrorSummary, ScanPhase, ScanProfile, ScanProgress, ScanStats,
};

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
    assert!(frame.contains("warnings 1"));
    assert!(!frame.contains("warnings1"));
}

#[test]
fn scan_dashboard_animation_changes_between_frames() {
    let progress = ScanProgress {
        phase: ScanPhase::Optimizing,
        indexed_files: 250,
        skipped_entries: 0,
        error_entries: 0,
        indexed_bytes: 0,
        total_files: None,
        total_bytes: None,
        elapsed: Duration::from_secs(10),
        current_path: "/tmp/Documents".to_string(),
        backend: ScanBackend::Dirent,
    };

    let first = render_scan_frame(&progress, ScanAnimation::frame(1));
    let second = render_scan_frame(&progress, ScanAnimation::frame(2));

    assert_ne!(first, second);
    assert!(first.contains("optimizing"));
    assert!(second.contains("optimizing"));
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

#[test]
fn scan_completion_dashboard_contains_summary_bars_and_commands() {
    let stats = ScanStats {
        indexed_files: 42,
        skipped_entries: 1,
        error_entries: 0,
        indexed_bytes: 4_200_000,
        error_summaries: BTreeMap::new(),
        profile: ScanProfile {
            total: Duration::from_secs(10),
            discovery: Duration::from_secs(1),
            walk: Duration::from_secs(4),
            sqlite_writes: Duration::from_secs(2),
            cleanup: Duration::from_secs(3),
            batches: 2,
            indexed_files: 42,
            indexed_bytes: 4_200_000,
            ..Default::default()
        },
    };
    let summary = ScanSummary {
        stats: &stats,
        root: Path::new("/tmp/docs"),
        index_path: Path::new("/tmp/docs/.locator/index.sqlite"),
        staged: true,
        detail: false,
    };

    let frame = render_scan_summary(&summary);

    assert!(frame.contains("lctr"));
    assert!(frame.contains("scan complete"));
    assert!(frame.contains("42 files indexed"));
    assert!(frame.contains("4.2 files/s"));
    assert!(frame.contains("search-ready index"));
    assert!(frame.contains("walk+metadata"));
    assert!(frame.contains("sqlite writes"));
    assert!(!frame.contains("staged index copied"));
    assert!(frame.contains("lctr search /tmp/docs"));
    assert!(frame.contains("lctr delete-index /tmp/docs"));
    assert!(frame.contains("timing breakdown"));
    assert!(frame.contains("warnings none"));
    assert!(!frame.contains("errors none"));
}

#[test]
fn scan_completion_dashboard_includes_error_samples_and_profile_detail() {
    let mut error_summaries = BTreeMap::new();
    error_summaries.insert(
        ScanErrorKind::PermissionDenied,
        ScanErrorSummary {
            count: 2,
            samples: vec![Path::new("/tmp/private/file").to_path_buf()],
        },
    );
    let stats = ScanStats {
        indexed_files: 7,
        skipped_entries: 0,
        error_entries: 2,
        indexed_bytes: 700,
        error_summaries,
        profile: ScanProfile {
            total: Duration::from_secs(2),
            record_handling: Duration::from_millis(10),
            writer_wait: Duration::from_millis(20),
            fts_rebuild: Duration::from_millis(30),
            index_rebuild: Duration::from_millis(40),
            native_getattr_calls: 5,
            native_parse: Duration::from_millis(50),
            native_queue_wait: Duration::from_millis(60),
            indexed_files: 7,
            indexed_bytes: 700,
            ..Default::default()
        },
    };
    let summary = ScanSummary {
        stats: &stats,
        root: Path::new("/tmp/docs"),
        index_path: Path::new("/tmp/index.sqlite"),
        staged: false,
        detail: true,
    };

    let frame = render_scan_summary(&summary);

    assert!(frame.contains("warning summary:"));
    assert!(!frame.contains("error summary:"));
    assert!(frame.contains("permission denied: 2"));
    assert!(frame.contains("/tmp/private/file"));
    assert!(frame.contains("post-scan index build"));
    assert!(frame.contains("filesystem scan counts"));
    assert!(frame.contains("filename index"));
    assert!(frame.contains("metadata reads"));
    assert!(!frame.contains("profile detail:"));
    assert!(!frame.contains("record handling"));
    assert!(!frame.contains("native detail:"));
    assert!(!frame.contains("getattr calls"));
}

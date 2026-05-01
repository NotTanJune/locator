use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use jwalk::WalkDir;

use crate::db::{Database, FileRecord};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub idle_timeout: Duration,
    pub batch_size: usize,
    pub writer_queue_batches: usize,
    pub native_buffer_bytes: usize,
    pub native_workers: usize,
    pub native_output_batch_size: usize,
    pub fresh_index: bool,
    pub estimate_totals: bool,
    pub backend: ScanBackend,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(300),
            batch_size: 500_000,
            writer_queue_batches: 32,
            native_buffer_bytes: 16 * 1024 * 1024,
            native_workers: 8,
            native_output_batch_size: 4096,
            fresh_index: false,
            estimate_totals: false,
            backend: ScanBackend::Dirent,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub indexed_files: u64,
    pub skipped_entries: u64,
    pub error_entries: u64,
    pub indexed_bytes: u64,
    pub error_summaries: BTreeMap<ScanErrorKind, ScanErrorSummary>,
    pub profile: ScanProfile,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanProfile {
    pub total: Duration,
    pub discovery: Duration,
    pub walk: Duration,
    pub sqlite_writes: Duration,
    pub cleanup: Duration,
    pub record_handling: Duration,
    pub writer_wait: Duration,
    pub stale_mark: Duration,
    pub fts_rebuild: Duration,
    pub index_rebuild: Duration,
    pub trigger_recreate: Duration,
    pub native_dirs_opened: u64,
    pub native_entries_seen: u64,
    pub native_files_seen: u64,
    pub native_dirs_seen: u64,
    pub native_getattr_calls: u64,
    pub native_unknown_type: u64,
    pub native_open_dir: Duration,
    pub native_getattr: Duration,
    pub native_parse: Duration,
    pub native_emit: Duration,
    pub native_queue_wait: Duration,
    pub batches: u64,
    pub indexed_files: u64,
    pub indexed_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScanErrorKind {
    PermissionDenied,
    NotFound,
    UnsupportedFileType,
    Other,
}

impl ScanErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission denied",
            Self::NotFound => "not found during scan",
            Self::UnsupportedFileType => "unsupported file type",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanErrorSummary {
    pub count: u64,
    pub samples: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhase {
    Discovering,
    Indexing,
    Optimizing,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScanBackend {
    Auto,
    Native,
    Dirent,
    #[value(name = "parallel")]
    ParallelWalk,
}

impl ScanBackend {
    pub fn resolved_name(self, native_available: bool) -> &'static str {
        match self {
            Self::Auto if native_available => "native",
            Self::Native if native_available => "native",
            Self::Dirent => "dirent",
            _ => "parallel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub indexed_files: u64,
    pub skipped_entries: u64,
    pub error_entries: u64,
    pub indexed_bytes: u64,
    pub total_files: Option<u64>,
    pub total_bytes: Option<u64>,
    pub elapsed: Duration,
    pub current_path: String,
    pub backend: ScanBackend,
}

impl ScanProgress {
    pub fn percent_complete(&self) -> Option<u64> {
        let total = self.total_files?;
        if total == 0 {
            return Some(100);
        }
        Some(((self.indexed_files.saturating_mul(100)) / total).min(100))
    }

    pub fn files_per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        self.indexed_files as f64 / seconds
    }

    pub fn megabytes_per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        (self.indexed_bytes as f64 / 1_000_000.0) / seconds
    }

    pub fn eta(&self) -> Option<Duration> {
        let total = self.total_files?;
        if self.indexed_files == 0 || self.indexed_files >= total {
            return Some(Duration::from_secs(0));
        }

        let rate = self.files_per_second();
        if rate <= 0.0 {
            return None;
        }

        let remaining = total.saturating_sub(self.indexed_files) as f64;
        Some(Duration::from_secs_f64(remaining / rate))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ScanTotals {
    files: u64,
    bytes: u64,
    skipped: u64,
}

pub fn scan_root(db: &Database, root: impl AsRef<Path>, options: ScanOptions) -> Result<ScanStats> {
    scan_root_with_progress(db, root, options, |_| {})
}

pub fn scan_root_with_progress(
    db: &Database,
    root: impl AsRef<Path>,
    options: ScanOptions,
    mut on_progress: impl FnMut(&ScanProgress),
) -> Result<ScanStats> {
    lower_process_priority();
    let root = root.as_ref().canonicalize().with_context(|| {
        format!(
            "scan root {} is not readable or no longer exists",
            root.as_ref().display()
        )
    })?;
    let root_string = root.to_string_lossy().to_string();
    let start = Instant::now();
    let native_available = native_catalog_available(&root);
    let backend = resolve_backend(options.backend, native_available);
    let mut stats = ScanStats::default();
    let mut profile = ScanProfile::default();

    emit_progress(
        &mut on_progress,
        ScanProgressInput {
            phase: if options.estimate_totals {
                ScanPhase::Discovering
            } else {
                ScanPhase::Indexing
            },
            stats: stats.clone(),
            totals: None,
            elapsed: start.elapsed(),
            current_path: root_string.as_str(),
            backend,
        },
    );

    let totals = if options.estimate_totals {
        let discovery_start = Instant::now();
        let totals = discover_totals(
            &root,
            start,
            backend,
            options.native_buffer_bytes,
            options.native_workers,
            &mut on_progress,
        )?;
        profile.discovery = discovery_start.elapsed();
        stats.skipped_entries += totals.skipped;
        Some(totals)
    } else {
        None
    };

    let scan_token = Utc::now().timestamp_millis();
    db.mark_scan_started(&root_string, scan_token)?;
    db.begin_bulk_scan()?;
    let mut batch = Vec::with_capacity(options.batch_size);
    let mut writer = ScanWriter::new(
        db,
        scan_token,
        options.writer_queue_batches,
        options.fresh_index,
    )?;
    let mut error_summaries = BTreeMap::<ScanErrorKind, ScanErrorSummary>::new();
    let mut last_progress_emit = Instant::now() - Duration::from_secs(1);
    let walk_start = Instant::now();

    let mut handle_record = |record: FileRecord,
                             stats: &mut ScanStats,
                             profile: &mut ScanProfile,
                             batch: &mut Vec<FileRecord>|
     -> Result<()> {
        let record_start = Instant::now();
        stats.indexed_files += 1;
        stats.indexed_bytes = stats.indexed_bytes.saturating_add(record.size_bytes);
        if stats.indexed_files <= 10 || last_progress_emit.elapsed() >= Duration::from_millis(250) {
            emit_progress(
                &mut on_progress,
                ScanProgressInput {
                    phase: ScanPhase::Indexing,
                    stats: stats.clone(),
                    totals,
                    elapsed: start.elapsed(),
                    current_path: record.parent.as_str(),
                    backend,
                },
            );
            last_progress_emit = Instant::now();
        }
        batch.push(record);

        if batch.len() >= options.batch_size {
            let full_batch = std::mem::replace(batch, Vec::with_capacity(options.batch_size));
            writer.write_batch(full_batch, profile)?;
        }
        profile.record_handling += record_start.elapsed();
        Ok(())
    };

    match backend {
        ScanBackend::Native | ScanBackend::Dirent => {
            native_scan_records(
                &root,
                &root_string,
                options.native_buffer_bytes,
                options.native_workers,
                options.native_output_batch_size,
                backend,
                |entry| match entry {
                    NativeScanEntry::File(record) => {
                        handle_record(record, &mut stats, &mut profile, &mut batch)
                    }
                    NativeScanEntry::Files(records) => {
                        for record in records {
                            handle_record(record, &mut stats, &mut profile, &mut batch)?;
                        }
                        Ok(())
                    }
                    NativeScanEntry::Profile(native_profile) => {
                        native_profile.add_to(&mut profile);
                        Ok(())
                    }
                    NativeScanEntry::Error { path, error } => {
                        stats.error_entries += 1;
                        record_scan_error(&mut error_summaries, &error, path);
                        Ok(())
                    }
                },
            )?;
        }
        _ => {
            for entry in pruned_walk(&root) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        stats.skipped_entries += 1;
                        continue;
                    }
                };
                let path = entry.path();
                if path == root {
                    continue;
                }
                if entry.file_type().is_dir() {
                    continue;
                }

                let record = match record_from_path(&root_string, &path) {
                    Ok(record) => record,
                    Err(error) => {
                        stats.error_entries += 1;
                        record_scan_error(&mut error_summaries, &error, path);
                        continue;
                    }
                };

                handle_record(record, &mut stats, &mut profile, &mut batch)?;
            }
        }
    }
    profile.walk = walk_start.elapsed();

    if !batch.is_empty() {
        writer.write_batch(batch, &mut profile)?;
    }
    writer.finish(&mut profile)?;
    profile.walk = profile.walk.saturating_sub(profile.sqlite_writes);
    emit_progress(
        &mut on_progress,
        ScanProgressInput {
            phase: ScanPhase::Optimizing,
            stats: stats.clone(),
            totals,
            elapsed: start.elapsed(),
            current_path: root_string.as_str(),
            backend,
        },
    );
    let cleanup_start = Instant::now();
    if !options.fresh_index {
        let stale_start = Instant::now();
        db.mark_stale_under_root(&root_string, scan_token)?;
        profile.stale_mark = stale_start.elapsed();
    }
    let bulk_profile = if options.fresh_index {
        db.finish_fresh_staged_scan_profile()?
    } else {
        db.finish_bulk_scan_profile()?
    };
    profile.fts_rebuild = bulk_profile.fts_rebuild;
    profile.index_rebuild = bulk_profile.index_rebuild;
    profile.trigger_recreate = bulk_profile.trigger_recreate;
    db.mark_scan_completed(&root_string, scan_token)?;
    profile.cleanup = cleanup_start.elapsed();
    emit_progress(
        &mut on_progress,
        ScanProgressInput {
            phase: ScanPhase::Done,
            stats: stats.clone(),
            totals,
            elapsed: start.elapsed(),
            current_path: root_string.as_str(),
            backend,
        },
    );
    stats.error_summaries = error_summaries;
    profile.total = start.elapsed();
    profile.indexed_files = stats.indexed_files;
    profile.indexed_bytes = stats.indexed_bytes;
    stats.profile = profile;
    Ok(stats)
}

pub fn classify_scan_error(error: &io::Error) -> ScanErrorKind {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ScanErrorKind::PermissionDenied,
        io::ErrorKind::NotFound => ScanErrorKind::NotFound,
        io::ErrorKind::Unsupported => ScanErrorKind::UnsupportedFileType,
        _ => ScanErrorKind::Other,
    }
}

fn record_scan_error(
    summaries: &mut BTreeMap<ScanErrorKind, ScanErrorSummary>,
    error: &io::Error,
    path: PathBuf,
) {
    let summary = summaries.entry(classify_scan_error(error)).or_default();
    summary.count += 1;
    if summary.samples.len() < 5 {
        summary.samples.push(path);
    }
}

struct ScanProgressInput<'a> {
    phase: ScanPhase,
    stats: ScanStats,
    totals: Option<ScanTotals>,
    elapsed: Duration,
    current_path: &'a str,
    backend: ScanBackend,
}

fn emit_progress(on_progress: &mut impl FnMut(&ScanProgress), input: ScanProgressInput<'_>) {
    on_progress(&ScanProgress {
        phase: input.phase,
        indexed_files: input.stats.indexed_files,
        skipped_entries: input.stats.skipped_entries,
        error_entries: input.stats.error_entries,
        indexed_bytes: input.stats.indexed_bytes,
        total_files: input.totals.map(|totals| totals.files),
        total_bytes: input.totals.map(|totals| totals.bytes),
        elapsed: input.elapsed,
        current_path: input.current_path.to_string(),
        backend: input.backend,
    });
}

fn discover_totals(
    root: &Path,
    start: Instant,
    backend: ScanBackend,
    native_buffer_bytes: usize,
    native_workers: usize,
    on_progress: &mut impl FnMut(&ScanProgress),
) -> Result<ScanTotals> {
    if matches!(backend, ScanBackend::Native | ScanBackend::Dirent) {
        return native_discover_totals(
            root,
            start,
            backend,
            native_buffer_bytes,
            native_workers,
            on_progress,
        );
    }

    let mut totals = ScanTotals::default();
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    for entry in pruned_walk(root) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                totals.skipped += 1;
                continue;
            }
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        if path.is_dir() {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                totals.files += 1;
                totals.bytes = totals.bytes.saturating_add(metadata.len());
                if totals.files <= 10 || last_emit.elapsed() >= Duration::from_millis(90) {
                    emit_progress(
                        on_progress,
                        ScanProgressInput {
                            phase: ScanPhase::Discovering,
                            stats: ScanStats {
                                indexed_files: totals.files,
                                skipped_entries: totals.skipped,
                                error_entries: 0,
                                indexed_bytes: totals.bytes,
                                error_summaries: BTreeMap::new(),
                                profile: ScanProfile::default(),
                            },
                            totals: None,
                            elapsed: start.elapsed(),
                            current_path: path.parent().and_then(Path::to_str).unwrap_or(""),
                            backend,
                        },
                    );
                    last_emit = Instant::now();
                }
            }
            Err(_) => totals.skipped += 1,
        }
    }
    Ok(totals)
}

fn pruned_walk(root: &Path) -> impl Iterator<Item = jwalk::Result<jwalk::DirEntry<((), ())>>> {
    WalkDir::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(|_depth, _path, _state, children| {
            children.iter_mut().for_each(|child| {
                if let Ok(entry) = child {
                    if is_skip_name(&entry.file_name.to_string_lossy()) {
                        entry.read_children_path = None;
                    }
                }
            });
            children.retain(|child| {
                child
                    .as_ref()
                    .map(|entry| !is_skip_name(&entry.file_name.to_string_lossy()))
                    .unwrap_or(true)
            });
        })
        .into_iter()
}

fn resolve_backend(requested: ScanBackend, native_available: bool) -> ScanBackend {
    match requested {
        ScanBackend::Native if native_available => ScanBackend::Native,
        ScanBackend::Dirent if native_available => ScanBackend::Dirent,
        ScanBackend::Auto if native_available => ScanBackend::Native,
        _ => ScanBackend::ParallelWalk,
    }
}

fn native_catalog_available(root: &Path) -> bool {
    native_backend_available(root)
}

fn lower_process_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        // Best effort only: scanning should stay responsive without stealing CPU.
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

pub(crate) fn is_skip_name(name: &str) -> bool {
    is_skip_name_bytes(name.as_bytes())
}

pub(crate) fn is_skip_name_bytes(name: &[u8]) -> bool {
    if name.starts_with(b"._") {
        return true;
    }

    matches!(
        name,
        b".git"
            | b".DS_Store"
            | b".DocumentRevisions-V100"
            | b".TemporaryItems"
            | b".VolumeIcon.icns"
            | b".apdisk"
            | b".metadata_never_index"
            | b".locator"
            | b".Spotlight-V100"
            | b".fseventsd"
            | b".Trashes"
            | b"__MACOSX"
            | b"node_modules"
            | b".cache"
            | b"target"
            | b"dist"
            | b"build"
            | b".next"
            | b".turbo"
            | b"DerivedData"
            | b".Trash"
            | b".npm"
            | b".yarn"
            | b".pnpm-store"
    )
}

fn record_from_path(root: &str, path: &Path) -> io::Result<FileRecord> {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?;
    let parent = path
        .parent()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase());
    let kind = kind_for(path, false);

    Ok(FileRecord {
        path: path.to_string_lossy().to_string(),
        name,
        parent,
        extension,
        root: root.to_string(),
        volume: volume_for(path),
        kind,
        size_bytes: 0,
        created_at: None,
        modified_at: None,
    })
}

pub(crate) fn kind_for(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return "folder".to_string();
    }

    match path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => "pdf",
        Some("jpg" | "jpeg" | "png" | "gif" | "heic" | "webp" | "tiff") => "image",
        Some("mp4" | "mov" | "mkv" | "avi" | "webm") => "video",
        Some("mp3" | "wav" | "m4a" | "flac" | "aac") => "audio",
        Some(
            "txt" | "md" | "csv" | "json" | "yaml" | "yml" | "toml" | "rs" | "py" | "js" | "ts",
        ) => "text",
        Some("zip" | "tar" | "gz" | "rar" | "7z") => "archive",
        _ => "file",
    }
    .to_string()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn kind_for_extension(extension: Option<&str>) -> String {
    match extension {
        Some("pdf") => "pdf",
        Some("jpg" | "jpeg" | "png" | "gif" | "heic" | "webp" | "tiff") => "image",
        Some("mp4" | "mov" | "mkv" | "avi" | "webm") => "video",
        Some("mp3" | "wav" | "m4a" | "flac" | "aac") => "audio",
        Some(
            "txt" | "md" | "csv" | "json" | "yaml" | "yml" | "toml" | "rs" | "py" | "js" | "ts",
        ) => "text",
        Some("zip" | "tar" | "gz" | "rar" | "7z") => "archive",
        _ => "file",
    }
    .to_string()
}

fn volume_for(path: &Path) -> String {
    let mut parts = path.components();
    let first = parts.next();
    let second = parts.next();
    let third = parts.next();

    match (
        first.map(|value| value.as_os_str().to_string_lossy().to_string()),
        second.map(|value| value.as_os_str().to_string_lossy().to_string()),
        third.map(|value| value.as_os_str().to_string_lossy().to_string()),
    ) {
        (Some(root), Some(volumes), Some(name)) if root == "/" && volumes == "Volumes" => name,
        _ => "local".to_string(),
    }
}

pub(crate) fn system_time_to_utc(time: SystemTime) -> Option<DateTime<Utc>> {
    Some(DateTime::<Utc>::from(time))
}

#[cfg(test)]
mod tests {
    use super::{is_skip_name, is_skip_name_bytes, kind_for_extension};

    #[test]
    fn byte_skip_name_matches_string_skip_name_for_known_noise() {
        for name in [
            ".DS_Store",
            "._report.pdf",
            "__MACOSX",
            ".Spotlight-V100",
            ".Trashes",
            "node_modules",
            "DerivedData",
            "report.pdf",
            ".env",
        ] {
            assert_eq!(is_skip_name_bytes(name.as_bytes()), is_skip_name(name));
        }
    }

    #[test]
    fn extension_kind_classification_avoids_path_parsing() {
        assert_eq!(kind_for_extension(Some("pdf")), "pdf");
        assert_eq!(kind_for_extension(Some("jpg")), "image");
        assert_eq!(kind_for_extension(Some("mp4")), "video");
        assert_eq!(kind_for_extension(Some("txt")), "text");
        assert_eq!(kind_for_extension(Some("zip")), "archive");
        assert_eq!(kind_for_extension(None), "file");
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct WriteProfile {
    sqlite_writes: Duration,
    batches: u64,
}

enum ScanWriter<'a> {
    Direct {
        db: &'a Database,
        scan_token: i64,
        insert_only: bool,
    },
    Pipelined {
        tx: SyncSender<Vec<FileRecord>>,
        handle: thread::JoinHandle<Result<WriteProfile>>,
    },
}

impl<'a> ScanWriter<'a> {
    fn new(
        db: &'a Database,
        scan_token: i64,
        queue_batches: usize,
        insert_only: bool,
    ) -> Result<Self> {
        let Some(db_path) = db.path().map(Path::to_path_buf) else {
            return Ok(Self::Direct {
                db,
                scan_token,
                insert_only,
            });
        };

        let (tx, rx) = mpsc::sync_channel::<Vec<FileRecord>>(queue_batches.max(1));
        let handle = thread::spawn(move || -> Result<WriteProfile> {
            let writer_db = if insert_only {
                Database::open_existing_for_staged_writer(&db_path)?
            } else {
                Database::open_existing_without_migrate(&db_path)?
            };
            let mut profile = WriteProfile::default();
            for batch in rx {
                if batch.is_empty() {
                    continue;
                }
                let write_start = Instant::now();
                if insert_only {
                    writer_db.insert_light_files_with_indexed_at(&batch, scan_token)?;
                } else {
                    writer_db.upsert_light_files_with_indexed_at(&batch, scan_token)?;
                }
                profile.sqlite_writes += write_start.elapsed();
                profile.batches += 1;
            }
            Ok(profile)
        });

        Ok(Self::Pipelined { tx, handle })
    }

    fn write_batch(&mut self, batch: Vec<FileRecord>, profile: &mut ScanProfile) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        match self {
            Self::Direct {
                db,
                scan_token,
                insert_only,
            } => {
                let write_start = Instant::now();
                if *insert_only {
                    db.insert_light_files_with_indexed_at(&batch, *scan_token)?;
                } else {
                    db.upsert_light_files_with_indexed_at(&batch, *scan_token)?;
                }
                profile.sqlite_writes += write_start.elapsed();
                profile.batches += 1;
                Ok(())
            }
            Self::Pipelined { tx, .. } => {
                let wait_start = Instant::now();
                tx.send(batch).context("send scan batch to writer")?;
                profile.writer_wait += wait_start.elapsed();
                Ok(())
            }
        }
    }

    fn finish(self, profile: &mut ScanProfile) -> Result<()> {
        match self {
            Self::Direct { .. } => Ok(()),
            Self::Pipelined { tx, handle } => {
                drop(tx);
                let write_profile = handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("scan writer thread panicked"))??;
                profile.sqlite_writes += write_profile.sqlite_writes;
                profile.batches += write_profile.batches;
                Ok(())
            }
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum NativeScanEntry {
    File(FileRecord),
    Files(Vec<FileRecord>),
    Profile(NativeProfile),
    Error { path: PathBuf, error: io::Error },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NativeProfile {
    dirs_opened: u64,
    entries_seen: u64,
    files_seen: u64,
    dirs_seen: u64,
    getattr_calls: u64,
    unknown_type: u64,
    open_dir: Duration,
    getattr: Duration,
    parse: Duration,
    emit: Duration,
    queue_wait: Duration,
}

impl NativeProfile {
    fn add_to(self, profile: &mut ScanProfile) {
        profile.native_dirs_opened += self.dirs_opened;
        profile.native_entries_seen += self.entries_seen;
        profile.native_files_seen += self.files_seen;
        profile.native_dirs_seen += self.dirs_seen;
        profile.native_getattr_calls += self.getattr_calls;
        profile.native_unknown_type += self.unknown_type;
        profile.native_open_dir += self.open_dir;
        profile.native_getattr += self.getattr;
        profile.native_parse += self.parse;
        profile.native_emit += self.emit;
        profile.native_queue_wait += self.queue_wait;
    }
}

fn native_backend_available(root: &Path) -> bool {
    native::available(root)
}

fn native_scan_records(
    root: &Path,
    root_string: &str,
    buffer_bytes: usize,
    workers: usize,
    output_batch_size: usize,
    backend: ScanBackend,
    on_entry: impl FnMut(NativeScanEntry) -> Result<()>,
) -> Result<()> {
    native::scan_records(
        root,
        root_string,
        buffer_bytes,
        workers,
        output_batch_size,
        backend,
        on_entry,
    )
}

fn native_discover_totals(
    root: &Path,
    start: Instant,
    backend: ScanBackend,
    native_buffer_bytes: usize,
    native_workers: usize,
    on_progress: &mut impl FnMut(&ScanProgress),
) -> Result<ScanTotals> {
    let mut totals = ScanTotals::default();
    let mut last_emit = Instant::now() - Duration::from_secs(1);

    native_scan_records(
        root,
        "",
        native_buffer_bytes,
        native_workers,
        4096,
        backend,
        |entry| {
            match entry {
                NativeScanEntry::File(record) => {
                    totals.files += 1;
                    totals.bytes = totals.bytes.saturating_add(record.size_bytes);
                    if totals.files <= 10 || last_emit.elapsed() >= Duration::from_millis(90) {
                        emit_progress(
                            on_progress,
                            ScanProgressInput {
                                phase: ScanPhase::Discovering,
                                stats: ScanStats {
                                    indexed_files: totals.files,
                                    skipped_entries: totals.skipped,
                                    error_entries: 0,
                                    indexed_bytes: totals.bytes,
                                    error_summaries: BTreeMap::new(),
                                    profile: ScanProfile::default(),
                                },
                                totals: None,
                                elapsed: start.elapsed(),
                                current_path: record.parent.as_str(),
                                backend,
                            },
                        );
                        last_emit = Instant::now();
                    }
                }
                NativeScanEntry::Files(records) => {
                    for record in records {
                        totals.files += 1;
                        totals.bytes = totals.bytes.saturating_add(record.size_bytes);
                        if totals.files <= 10 || last_emit.elapsed() >= Duration::from_millis(90) {
                            emit_progress(
                                on_progress,
                                ScanProgressInput {
                                    phase: ScanPhase::Discovering,
                                    stats: ScanStats {
                                        indexed_files: totals.files,
                                        skipped_entries: totals.skipped,
                                        error_entries: 0,
                                        indexed_bytes: totals.bytes,
                                        error_summaries: BTreeMap::new(),
                                        profile: ScanProfile::default(),
                                    },
                                    totals: None,
                                    elapsed: start.elapsed(),
                                    current_path: record.parent.as_str(),
                                    backend,
                                },
                            );
                            last_emit = Instant::now();
                        }
                    }
                }
                NativeScanEntry::Profile(_) => {}
                NativeScanEntry::Error { .. } => {
                    totals.skipped += 1;
                }
            }
            Ok(())
        },
    )?;

    Ok(totals)
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::*;

    pub fn available(_root: &Path) -> bool {
        false
    }

    pub fn scan_records(
        _root: &Path,
        _root_string: &str,
        _buffer_bytes: usize,
        _workers: usize,
        _output_batch_size: usize,
        _backend: ScanBackend,
        _on_entry: impl FnMut(NativeScanEntry) -> Result<()>,
    ) -> Result<()> {
        anyhow::bail!("native scan backend is only available on macOS")
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::collections::VecDeque;
    use std::ffi::CString;
    use std::mem;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::Result;

    use super::{
        is_skip_name, is_skip_name_bytes, kind_for_extension, volume_for, NativeProfile,
        NativeScanEntry, ScanBackend,
    };
    use crate::db::FileRecord;

    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;
    const DT_UNKNOWN: u8 = 0;
    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;
    const DT_LNK: u8 = 10;
    const MIN_ATTR_BUF_SIZE: usize = 1024 * 1024;

    pub fn available(root: &Path) -> bool {
        open_dir(root).is_ok()
    }

    pub fn scan_records(
        root: &Path,
        root_string: &str,
        buffer_bytes: usize,
        workers: usize,
        output_batch_size: usize,
        backend: ScanBackend,
        mut on_entry: impl FnMut(NativeScanEntry) -> Result<()>,
    ) -> Result<()> {
        if matches!(backend, ScanBackend::Dirent) {
            return scan_records_dirent(root, root_string, workers, output_batch_size, on_entry);
        }

        if workers <= 1 {
            return scan_records_serial(root, root_string, buffer_bytes, on_entry);
        }

        let worker_count = workers.max(1);
        let queue = Arc::new(WorkQueue::new(root.to_path_buf()));
        let root_string = Arc::new(root_string.to_string());
        let volume = Arc::new(volume_for(root));
        let buffer_len = buffer_bytes.max(MIN_ATTR_BUF_SIZE);
        let output_batch_size = output_batch_size.max(1);
        let (tx, rx) = mpsc::channel::<NativeScanEntry>();
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let root_string = Arc::clone(&root_string);
            let volume = Arc::clone(&volume);
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let mut buffer = vec![0u8; buffer_len];
                let mut output = Vec::<FileRecord>::with_capacity(output_batch_size);
                let mut profile = NativeProfile::default();
                let mut queue_wait_start = Instant::now();

                while let Some(dir) = queue.pop() {
                    profile.queue_wait += queue_wait_start.elapsed();
                    let mut emit_time = Duration::default();
                    let result = scan_one_dir(
                        &dir,
                        &root_string,
                        &volume,
                        &mut buffer,
                        &mut profile,
                        |entry| match entry {
                            ParsedEntry::Directory(path) => {
                                queue.push(path);
                                Ok(())
                            }
                            ParsedEntry::File(record) => {
                                let emit_start = Instant::now();
                                output.push(record);
                                if output.len() >= output_batch_size {
                                    let batch = std::mem::take(&mut output);
                                    let _ = tx.send(NativeScanEntry::Files(batch));
                                    output = Vec::with_capacity(output_batch_size);
                                }
                                emit_time += emit_start.elapsed();
                                Ok(())
                            }
                        },
                    );
                    profile.emit += emit_time;

                    if let Err(error) = result {
                        match error {
                            ScanDirError::Io(error) => {
                                let _ = tx.send(NativeScanEntry::Error { path: dir, error });
                            }
                            ScanDirError::Callback(_) => {
                                unreachable!("parallel native scan callbacks are infallible")
                            }
                        }
                    }

                    queue.finish_dir();
                    queue_wait_start = Instant::now();
                }

                if !output.is_empty() {
                    let emit_start = Instant::now();
                    let _ = tx.send(NativeScanEntry::Files(output));
                    profile.emit += emit_start.elapsed();
                }
                let _ = tx.send(NativeScanEntry::Profile(profile));
            }));
        }

        drop(tx);
        let mut callback_error = None;
        while let Ok(entry) = rx.recv() {
            if let Err(error) = on_entry(entry) {
                callback_error = Some(error);
                break;
            }
        }
        drop(rx);

        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("native scan worker thread panicked"))?;
        }

        if let Some(error) = callback_error {
            return Err(error);
        }

        Ok(())
    }

    fn scan_records_dirent(
        root: &Path,
        root_string: &str,
        workers: usize,
        output_batch_size: usize,
        mut on_entry: impl FnMut(NativeScanEntry) -> Result<()>,
    ) -> Result<()> {
        let worker_count = workers.max(1);
        let output_batch_size = output_batch_size.max(1);
        let queue = Arc::new(WorkQueue::new(root.to_path_buf()));
        let root_string = Arc::new(root_string.to_string());
        let volume = Arc::new(volume_for(root));
        let (tx, rx) = mpsc::channel::<NativeScanEntry>();
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let root_string = Arc::clone(&root_string);
            let volume = Arc::clone(&volume);
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let mut output = Vec::<FileRecord>::with_capacity(output_batch_size);
                let mut profile = NativeProfile::default();
                let mut queue_wait_start = Instant::now();

                while let Some(dir) = queue.pop() {
                    profile.queue_wait += queue_wait_start.elapsed();
                    let mut emit_time = Duration::default();
                    let result =
                        scan_one_dir_dirent(&dir, &root_string, &volume, &mut profile, |entry| {
                            match entry {
                                ParsedEntry::Directory(path) => {
                                    queue.push(path);
                                    Ok(())
                                }
                                ParsedEntry::File(record) => {
                                    let emit_start = Instant::now();
                                    output.push(record);
                                    if output.len() >= output_batch_size {
                                        let batch = std::mem::take(&mut output);
                                        let _ = tx.send(NativeScanEntry::Files(batch));
                                        output = Vec::with_capacity(output_batch_size);
                                    }
                                    emit_time += emit_start.elapsed();
                                    Ok(())
                                }
                            }
                        });
                    profile.emit += emit_time;

                    if let Err(error) = result {
                        match error {
                            ScanDirError::Io(error) => {
                                let _ = tx.send(NativeScanEntry::Error { path: dir, error });
                            }
                            ScanDirError::Callback(_) => {
                                unreachable!("dirent scan callbacks are infallible")
                            }
                        }
                    }

                    queue.finish_dir();
                    queue_wait_start = Instant::now();
                }

                if !output.is_empty() {
                    let emit_start = Instant::now();
                    let _ = tx.send(NativeScanEntry::Files(output));
                    profile.emit += emit_start.elapsed();
                }
                let _ = tx.send(NativeScanEntry::Profile(profile));
            }));
        }

        drop(tx);
        let mut callback_error = None;
        while let Ok(entry) = rx.recv() {
            if let Err(error) = on_entry(entry) {
                callback_error = Some(error);
                break;
            }
        }
        drop(rx);

        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("dirent scan worker thread panicked"))?;
        }

        if let Some(error) = callback_error {
            return Err(error);
        }

        Ok(())
    }

    fn scan_records_serial(
        root: &Path,
        root_string: &str,
        buffer_bytes: usize,
        mut on_entry: impl FnMut(NativeScanEntry) -> Result<()>,
    ) -> Result<()> {
        let mut stack = vec![root.to_path_buf()];
        let mut buffer = vec![0u8; buffer_bytes.max(MIN_ATTR_BUF_SIZE)];
        let volume = volume_for(root);
        let mut profile = NativeProfile::default();

        while let Some(dir) = stack.pop() {
            let result = scan_one_dir(
                &dir,
                root_string,
                &volume,
                &mut buffer,
                &mut profile,
                |entry| match entry {
                    ParsedEntry::Directory(path) => {
                        stack.push(path);
                        Ok(())
                    }
                    ParsedEntry::File(record) => {
                        on_entry(NativeScanEntry::File(record))?;
                        Ok(())
                    }
                },
            );

            if let Err(error) = result {
                match error {
                    ScanDirError::Io(error) => {
                        on_entry(NativeScanEntry::Error { path: dir, error })?;
                    }
                    ScanDirError::Callback(error) => return Err(error),
                }
            }
        }

        on_entry(NativeScanEntry::Profile(profile))?;
        Ok(())
    }

    fn scan_one_dir(
        dir: &Path,
        root_string: &str,
        volume: &str,
        buffer: &mut [u8],
        profile: &mut NativeProfile,
        mut on_entry: impl FnMut(ParsedEntry) -> Result<()>,
    ) -> std::result::Result<(), ScanDirError> {
        let open_start = Instant::now();
        let fd = open_dir(dir)?;
        profile.open_dir += open_start.elapsed();
        profile.dirs_opened += 1;
        let parent_string = dir.to_string_lossy().to_string();

        loop {
            let getattr_start = Instant::now();
            let count = unsafe {
                libc::getattrlistbulk(
                    fd.raw(),
                    &mut attr_list() as *mut libc::attrlist as *mut libc::c_void,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                    0,
                )
            };
            profile.getattr += getattr_start.elapsed();
            profile.getattr_calls += 1;

            if count == 0 {
                break;
            }
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }

            let mut offset = 0usize;
            for _ in 0..count {
                let Some(length) = read_value::<u32>(buffer, &mut offset) else {
                    break;
                };
                if length < mem::size_of::<u32>() as u32 {
                    break;
                }
                let entry_start = offset - mem::size_of::<u32>();
                let entry_end = entry_start.saturating_add(length as usize);
                if entry_end > buffer.len() {
                    break;
                }
                let entry = &buffer[entry_start..entry_end];
                offset = entry_end;

                profile.entries_seen += 1;
                let parse_start = Instant::now();
                let Some(native_entry) =
                    parse_entry(dir, &parent_string, root_string, volume, entry)
                else {
                    profile.parse += parse_start.elapsed();
                    continue;
                };
                match &native_entry {
                    ParsedEntry::Directory(_) => profile.dirs_seen += 1,
                    ParsedEntry::File(_) => profile.files_seen += 1,
                }
                profile.parse += parse_start.elapsed();

                on_entry(native_entry).map_err(ScanDirError::Callback)?;
            }
        }

        Ok(())
    }

    fn scan_one_dir_dirent(
        dir: &Path,
        root_string: &str,
        volume: &str,
        profile: &mut NativeProfile,
        mut on_entry: impl FnMut(ParsedEntry) -> Result<()>,
    ) -> std::result::Result<(), ScanDirError> {
        let open_start = Instant::now();
        let dirp = open_dir_stream(dir)?;
        profile.open_dir += open_start.elapsed();
        profile.dirs_opened += 1;
        let parent_string = dir.to_string_lossy().to_string();

        loop {
            let read_start = Instant::now();
            let entry = unsafe { libc::readdir(dirp.raw()) };
            profile.getattr += read_start.elapsed();
            profile.getattr_calls += 1;
            if entry.is_null() {
                break;
            }

            profile.entries_seen += 1;
            let parse_start = Instant::now();
            let Some(parsed) = parse_dirent_entry(
                dir,
                dirp.fd(),
                &parent_string,
                root_string,
                volume,
                entry,
                profile,
            )?
            else {
                profile.parse += parse_start.elapsed();
                continue;
            };
            match &parsed {
                ParsedEntry::Directory(_) => profile.dirs_seen += 1,
                ParsedEntry::File(_) => profile.files_seen += 1,
            }
            profile.parse += parse_start.elapsed();

            on_entry(parsed).map_err(ScanDirError::Callback)?;
        }

        Ok(())
    }

    fn parse_dirent_entry(
        parent: &Path,
        parent_fd: libc::c_int,
        parent_string: &str,
        root_string: &str,
        volume: &str,
        entry: *mut libc::dirent,
        profile: &mut NativeProfile,
    ) -> std::io::Result<Option<ParsedEntry>> {
        let entry = unsafe { &*entry };
        let name = unsafe { std::ffi::CStr::from_ptr(entry.d_name.as_ptr()) };
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            return Ok(None);
        }
        if is_skip_name_bytes(name_bytes) {
            return Ok(None);
        }
        let name = std::ffi::OsStr::from_bytes(name_bytes);

        let dtype = entry.d_type;
        if dtype == DT_DIR {
            return Ok(Some(ParsedEntry::Directory(parent.join(name))));
        }
        if dtype == DT_UNKNOWN {
            profile.unknown_type += 1;
            let kind = dirent_unknown_kind(parent_fd, name)?;
            if kind == VDIR {
                return Ok(Some(ParsedEntry::Directory(parent.join(name))));
            }
            if !matches!(kind, VREG | VLNK) {
                return Ok(None);
            }
        } else if !matches!(dtype, DT_REG | DT_LNK) {
            return Ok(None);
        }

        Ok(Some(file_record_from_name(
            parent_string,
            root_string,
            volume,
            name_bytes,
        )))
    }

    fn dirent_unknown_kind(parent_fd: libc::c_int, name: &std::ffi::OsStr) -> std::io::Result<u32> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte")
        })?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                parent_fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        let mode = stat.st_mode & libc::S_IFMT;
        if mode == libc::S_IFDIR {
            Ok(VDIR)
        } else if mode == libc::S_IFREG {
            Ok(VREG)
        } else if mode == libc::S_IFLNK {
            Ok(VLNK)
        } else {
            Ok(0)
        }
    }

    enum ScanDirError {
        Io(std::io::Error),
        Callback(anyhow::Error),
    }

    impl From<std::io::Error> for ScanDirError {
        fn from(error: std::io::Error) -> Self {
            Self::Io(error)
        }
    }

    struct WorkQueue {
        state: Mutex<WorkState>,
        ready: Condvar,
    }

    struct WorkState {
        dirs: VecDeque<PathBuf>,
        pending: usize,
    }

    impl WorkQueue {
        fn new(root: PathBuf) -> Self {
            let mut dirs = VecDeque::new();
            dirs.push_back(root);
            Self {
                state: Mutex::new(WorkState { dirs, pending: 1 }),
                ready: Condvar::new(),
            }
        }

        fn pop(&self) -> Option<PathBuf> {
            let mut state = self.state.lock().expect("native scan queue lock");
            loop {
                if let Some(dir) = state.dirs.pop_front() {
                    return Some(dir);
                }
                if state.pending == 0 {
                    return None;
                }
                state = self.ready.wait(state).expect("native scan queue wait");
            }
        }

        fn push(&self, path: PathBuf) {
            let mut state = self.state.lock().expect("native scan queue lock");
            state.pending += 1;
            state.dirs.push_back(path);
            self.ready.notify_one();
        }

        fn finish_dir(&self) {
            let mut state = self.state.lock().expect("native scan queue lock");
            state.pending = state.pending.saturating_sub(1);
            if state.pending == 0 {
                self.ready.notify_all();
            } else {
                self.ready.notify_one();
            }
        }
    }

    fn attr_list() -> libc::attrlist {
        libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_NAME
                | libc::ATTR_CMN_OBJTYPE,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        }
    }

    enum ParsedEntry {
        Directory(PathBuf),
        File(FileRecord),
    }

    fn parse_entry(
        parent: &Path,
        parent_string: &str,
        root_string: &str,
        volume: &str,
        entry: &[u8],
    ) -> Option<ParsedEntry> {
        let mut offset = mem::size_of::<u32>();
        let returned = read_value::<libc::attribute_set_t>(entry, &mut offset)?;

        let name = if returned.commonattr & libc::ATTR_CMN_NAME != 0 {
            let reference_offset = offset;
            let name_ref = read_value::<libc::attrreference_t>(entry, &mut offset)?;
            let data_start = reference_offset.checked_add(name_ref.attr_dataoffset as usize)?;
            let data_end = data_start.checked_add(name_ref.attr_length as usize)?;
            if data_end > entry.len() || data_start >= data_end {
                return None;
            }
            let mut bytes = entry[data_start..data_end].to_vec();
            if bytes.last() == Some(&0) {
                bytes.pop();
            }
            std::ffi::OsString::from_vec(bytes)
        } else {
            return None;
        };

        if name == "." || name == ".." {
            return None;
        }
        if is_skip_name(&name.to_string_lossy()) {
            return None;
        }

        let obj_type = if returned.commonattr & libc::ATTR_CMN_OBJTYPE != 0 {
            read_value::<u32>(entry, &mut offset)?
        } else {
            return None;
        };

        if obj_type == VDIR {
            let path = parent.join(&name);
            return Some(ParsedEntry::Directory(path));
        }
        if !matches!(obj_type, VREG | VLNK) {
            return None;
        }

        let name = name.to_string_lossy().to_string();
        Some(file_record_from_owned_name(
            parent_string,
            root_string,
            volume,
            name,
        ))
    }

    fn file_record_from_name(
        parent_string: &str,
        root_string: &str,
        volume: &str,
        name_bytes: &[u8],
    ) -> ParsedEntry {
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        let extension = extension_lower_from_bytes(name_bytes);
        let kind = kind_for_extension(extension.as_deref());
        file_record_from_parts(parent_string, root_string, volume, name, extension, kind)
    }

    fn file_record_from_owned_name(
        parent_string: &str,
        root_string: &str,
        volume: &str,
        name: String,
    ) -> ParsedEntry {
        let extension = Path::new(&name)
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase());
        let kind = kind_for_extension(extension.as_deref());
        file_record_from_parts(parent_string, root_string, volume, name, extension, kind)
    }

    fn extension_lower_from_bytes(name: &[u8]) -> Option<String> {
        let dot = name.iter().rposition(|byte| *byte == b'.')?;
        if dot == 0 || dot + 1 >= name.len() {
            return None;
        }
        Some(
            name[dot + 1..]
                .iter()
                .map(|byte| byte.to_ascii_lowercase() as char)
                .collect(),
        )
    }

    fn file_record_from_parts(
        parent_string: &str,
        root_string: &str,
        volume: &str,
        name: String,
        extension: Option<String>,
        kind: String,
    ) -> ParsedEntry {
        let path = if parent_string == "/" {
            format!("/{name}")
        } else {
            format!("{parent_string}/{name}")
        };

        ParsedEntry::File(FileRecord {
            path,
            name,
            parent: parent_string.to_string(),
            extension,
            root: root_string.to_string(),
            volume: volume.to_string(),
            kind,
            size_bytes: 0,
            created_at: None,
            modified_at: None,
        })
    }

    fn read_value<T: Copy>(bytes: &[u8], offset: &mut usize) -> Option<T> {
        let size = mem::size_of::<T>();
        let end = offset.checked_add(size)?;
        if end > bytes.len() {
            return None;
        }
        let value = unsafe { ptr::read_unaligned(bytes.as_ptr().add(*offset) as *const T) };
        *offset = end;
        Some(value)
    }

    struct DirFd(libc::c_int);

    impl DirFd {
        fn raw(&self) -> libc::c_int {
            self.0
        }
    }

    impl Drop for DirFd {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.0);
            }
        }
    }

    fn open_dir(path: &Path) -> std::io::Result<DirFd> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte")
        })?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(DirFd(fd))
    }

    struct DirStream(*mut libc::DIR);

    impl DirStream {
        fn raw(&self) -> *mut libc::DIR {
            self.0
        }

        fn fd(&self) -> libc::c_int {
            unsafe { libc::dirfd(self.0) }
        }
    }

    impl Drop for DirStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    fn open_dir_stream(path: &Path) -> std::io::Result<DirStream> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte")
        })?;
        let dir = unsafe { libc::opendir(path.as_ptr()) };
        if dir.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(DirStream(dir))
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::symlink;

        use super::{dirent_unknown_kind, open_dir_stream, VDIR, VLNK, VREG};

        #[test]
        fn unknown_dirent_kind_uses_existing_directory_fd() {
            let dir = tempfile::tempdir().expect("temp dir");
            std::fs::write(dir.path().join("file.txt"), "data").expect("write file");
            std::fs::create_dir(dir.path().join("folder")).expect("create folder");
            symlink(dir.path().join("file.txt"), dir.path().join("link")).expect("create symlink");

            let stream = open_dir_stream(dir.path()).expect("open dir stream");

            assert_eq!(
                dirent_unknown_kind(stream.fd(), std::ffi::OsStr::new("file.txt"))
                    .expect("file kind"),
                VREG
            );
            assert_eq!(
                dirent_unknown_kind(stream.fd(), std::ffi::OsStr::new("folder"))
                    .expect("folder kind"),
                VDIR
            );
            assert_eq!(
                dirent_unknown_kind(stream.fd(), std::ffi::OsStr::new("link")).expect("link kind"),
                VLNK
            );
        }
    }
}

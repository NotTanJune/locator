//! Live filesystem watcher that keeps a persistent index current while the
//! search TUI is open. A background thread receives `notify` events, debounces
//! them, and applies per-path upserts / soft-deletes to the same SQLite file
//! the search reads (WAL makes the concurrent reader + writer safe). A
//! generation counter is bumped on every applied batch so the TUI knows when to
//! refresh its current query.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};

use crate::db::{Database, FileRecord};
use crate::scanner::{is_skip_name, kind_for, system_time_to_utc, volume_for};

/// How long to wait for a quiet period before applying a batch of events. Bursts
/// (e.g. a build writing many files) coalesce into a single apply.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// Handle to a running watcher. Dropping it stops the watcher thread.
pub struct LiveIndex {
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LiveIndex {
    /// Start watching `root`, writing changes into the index at `db_path`.
    /// `root` must match the `root` string stored by the scan so upserts land on
    /// the same rows.
    pub fn spawn(root: PathBuf, db_path: PathBuf) -> Result<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_generation = Arc::clone(&generation);
        let thread_stop = Arc::clone(&stop);

        // Probe that we can open the index for writing before spawning, so
        // callers can fall back cleanly when the index is read-only.
        let db = Database::open(&db_path).context("open index for live watching")?;

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            // A send failure just means the consumer is gone; ignore it.
            let _ = tx.send(event);
        })
        .context("create filesystem watcher")?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("watch {}", root.display()))?;

        let root_string = root.to_string_lossy().to_string();
        let handle = std::thread::Builder::new()
            .name("lctr-live-index".into())
            .spawn(move || {
                // Keep the watcher alive for the duration of the thread.
                let _watcher = watcher;
                watch_loop(&rx, &db, &root_string, &thread_stop, &thread_generation);
            })
            .context("spawn live-index thread")?;

        Ok(Self {
            generation,
            stop,
            handle: Some(handle),
        })
    }

    /// Monotonic counter bumped each time a batch of changes is applied. The TUI
    /// compares it against the last value it saw to decide whether to refresh.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

impl Drop for LiveIndex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn watch_loop(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    db: &Database,
    root: &str,
    stop: &AtomicBool,
    generation: &AtomicU64,
) {
    while !stop.load(Ordering::Relaxed) {
        let first = match rx.recv_timeout(DEBOUNCE) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let mut paths: HashSet<PathBuf> = HashSet::new();
        collect_paths(first, &mut paths);
        // Drain anything already queued so a burst applies as one batch.
        while let Ok(event) = rx.recv_timeout(DEBOUNCE) {
            collect_paths(event, &mut paths);
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }

        let mut changed = false;
        for path in paths {
            if apply_path(db, root, &path) {
                changed = true;
            }
        }
        if changed {
            generation.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn collect_paths(event: notify::Result<notify::Event>, out: &mut HashSet<PathBuf>) {
    if let Ok(event) = event {
        for path in event.paths {
            out.insert(path);
        }
    }
}

/// Returns true if the index was modified for this path.
fn apply_path(db: &Database, root: &str, path: &Path) -> bool {
    if path_is_pruned(path) {
        return false;
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => match record_for(root, path, &metadata) {
            Some(record) => db.upsert_file(&record).is_ok(),
            None => false,
        },
        // Missing on disk: a delete or rename-away. Soft-delete it (and anything
        // beneath, if it was a directory).
        Err(_) => db
            .mark_path_deleted(&path.to_string_lossy())
            .map(|count| count > 0)
            .unwrap_or(false),
    }
}

fn path_is_pruned(path: &Path) -> bool {
    path.components()
        .any(|component| is_skip_name(&component.as_os_str().to_string_lossy()))
}

fn record_for(root: &str, path: &Path, metadata: &std::fs::Metadata) -> Option<FileRecord> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let parent = path
        .parent()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase());
    let kind = kind_for(path, metadata.is_dir());

    Some(FileRecord {
        path: path.to_string_lossy().to_string(),
        name,
        parent,
        extension,
        root: root.to_string(),
        volume: volume_for(path),
        kind,
        size_bytes: metadata.len(),
        created_at: metadata.created().ok().and_then(system_time_to_utc),
        modified_at: metadata.modified().ok().and_then(system_time_to_utc),
    })
}

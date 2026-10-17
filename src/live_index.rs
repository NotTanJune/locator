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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};

use crate::db::{Database, FileRecord};
use crate::scanner::{is_skip_name, kind_for, system_time_to_utc, volume_for};

/// How long to wait for a quiet period before applying a batch of events. Bursts
/// (e.g. a build writing many files) coalesce into a single apply.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// Upper bound on how long one batch may keep absorbing events. Without it, a
/// steady stream of events (a build, or any process touching files under the
/// root) would keep resetting the quiet period and the batch would never apply.
const MAX_BATCH: Duration = Duration::from_secs(2);

/// Handle to a running watcher. Dropping it stops the watcher thread.
pub struct LiveIndex {
    generation: Arc<AtomicU64>,
    write_errors: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LiveIndex {
    /// Start watching `root`, writing changes into the index at `db_path`.
    /// `root` must match the `root` string stored by the scan so upserts land on
    /// the same rows.
    pub fn spawn(root: PathBuf, db_path: PathBuf) -> Result<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let write_errors = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_generation = Arc::clone(&generation);
        let thread_write_errors = Arc::clone(&write_errors);
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
                watch_loop(
                    &rx,
                    &db,
                    &root_string,
                    &thread_stop,
                    &thread_generation,
                    &thread_write_errors,
                );
            })
            .context("spawn live-index thread")?;

        Ok(Self {
            generation,
            write_errors,
            stop,
            handle: Some(handle),
        })
    }

    /// Monotonic counter bumped each time a batch of changes is applied. The TUI
    /// compares it against the last value it saw to decide whether to refresh.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Number of filesystem changes that failed to apply to the index (e.g.
    /// disk full, lock contention). Non-zero means search results may be stale.
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
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
    write_errors: &AtomicU64,
) {
    while !stop.load(Ordering::Relaxed) {
        let first = match rx.recv_timeout(DEBOUNCE) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let mut paths: HashSet<PathBuf> = HashSet::new();
        collect_paths(first, &mut paths);
        // Drain anything already queued so a burst applies as one batch, but stop
        // after MAX_BATCH so a continuous stream cannot starve the apply step.
        let batch_deadline = Instant::now() + MAX_BATCH;
        while let Ok(event) = rx.recv_timeout(DEBOUNCE) {
            collect_paths(event, &mut paths);
            if stop.load(Ordering::Relaxed) || Instant::now() >= batch_deadline {
                break;
            }
        }

        let mut changed = false;
        for path in paths {
            match apply_path(db, root, &path) {
                Applied::Changed => changed = true,
                Applied::Unchanged => {}
                Applied::Failed => {
                    write_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if changed {
            generation.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn collect_paths(event: notify::Result<notify::Event>, out: &mut HashSet<PathBuf>) {
    if let Ok(event) = event {
        // Opens, reads and closes without a write (inotify reports them) never
        // change the index, and readers polling the index file would otherwise
        // generate them constantly.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        for path in event.paths {
            out.insert(path);
        }
    }
}

/// Outcome of applying one filesystem change to the index. `Failed` means the
/// database write itself errored (disk full, lock contention) — distinct from
/// "nothing to do", so the watcher can surface persistent write trouble.
enum Applied {
    Changed,
    Unchanged,
    Failed,
}

fn apply_path(db: &Database, root: &str, path: &Path) -> Applied {
    if path_is_pruned(Path::new(root), path) {
        return Applied::Unchanged;
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => match record_for(root, path, &metadata) {
            Some(record) => match db.upsert_file(&record) {
                Ok(()) => Applied::Changed,
                Err(_) => Applied::Failed,
            },
            None => Applied::Unchanged,
        },
        // Missing on disk: a delete or rename-away. Soft-delete it (and anything
        // beneath, if it was a directory).
        Err(_) => match db.mark_path_deleted(&path.to_string_lossy()) {
            Ok(count) if count > 0 => Applied::Changed,
            Ok(_) => Applied::Unchanged,
            Err(_) => Applied::Failed,
        },
    }
}

/// Skip events under pruned directories (node_modules, target, .git, ...),
/// matching the scanner, which prunes only entries *below* the scanned root.
/// Components of the root itself are never considered, so a project living
/// at e.g. `~/build/app` still gets live updates.
fn path_is_pruned(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::tempdir;

    /// Verify that `apply_path` returns `Applied::Failed` when the database
    /// write fails. Strategy: open a valid `Database`, then concurrently drop
    /// the `files` table via a second raw connection. SQLite will return
    /// SQLITE_SCHEMA on the first statement the original connection tries to
    /// prepare after the schema change, causing `upsert_file` to return `Err`.
    #[test]
    fn apply_path_returns_failed_on_bad_db() {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("index.sqlite");
        let root = dir.path().to_string_lossy().to_string();

        // Open the DB through the normal path (creates schema).
        let db = Database::open(&db_path).expect("open db");

        // Drop the files table via a parallel raw connection so the existing
        // `db` handle sees SQLITE_SCHEMA on its next write.
        {
            let conn = Connection::open(&db_path).expect("raw connection");
            conn.execute_batch("DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS files_fts;")
                .expect("drop tables");
        }

        let target = dir.path().join("new_file.txt");
        std::fs::write(&target, b"hello").expect("create target");

        let result = apply_path(&db, &root, &target);
        assert!(
            matches!(result, Applied::Failed),
            "expected Failed after files table dropped"
        );
    }

    #[test]
    fn access_events_are_ignored() {
        let path = PathBuf::from("/tmp/x/index.sqlite");
        let mut out = HashSet::new();

        let access = notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Any))
            .add_path(path.clone());
        collect_paths(Ok(access), &mut out);
        assert!(out.is_empty());

        let modify = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(path.clone());
        collect_paths(Ok(modify), &mut out);
        assert!(out.contains(&path));
    }

    #[cfg(unix)]
    #[test]
    fn pruning_ignores_root_components() {
        let root = Path::new("/home/u/build/app");
        assert!(!path_is_pruned(
            root,
            Path::new("/home/u/build/app/src/main.rs")
        ));
        assert!(path_is_pruned(
            root,
            Path::new("/home/u/build/app/node_modules/x/index.js")
        ));
        assert!(path_is_pruned(
            root,
            Path::new("/home/u/build/app/.git/HEAD")
        ));

        let cache_root = Path::new("/home/u/.cache/proj");
        assert!(!path_is_pruned(
            cache_root,
            Path::new("/home/u/.cache/proj/notes.txt")
        ));

        // Outside the root: fallback keeps the old whole-path behavior.
        assert!(path_is_pruned(
            Path::new("/elsewhere"),
            Path::new("/x/target/y")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn apply_path_indexes_files_when_root_has_skip_named_ancestor() {
        let dir = tempdir().expect("temp dir");
        let app = dir.path().join("build").join("app");
        std::fs::create_dir_all(&app).expect("create root");
        let root = app.canonicalize().expect("canonical root");
        let db = Database::open(root.join("index.sqlite")).expect("open db");

        let file = root.join("hello.txt");
        std::fs::write(&file, b"hello").expect("write file");

        let result = apply_path(&db, &root.to_string_lossy(), &file);
        assert!(matches!(result, Applied::Changed));
        assert_eq!(db.count_active().unwrap(), 1);
    }
}

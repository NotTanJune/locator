use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use std::time::Duration;

use crate::name_file::{
    self, name_file_path, LookupCache, NameFile, NameFileBuilder, Reader, Row, BLOCK_ROWS,
    INDEX_FORMAT,
};
use crate::query::{
    CompiledQuery, QueryMode, QueryScorer, SearchFilters, SearchOptions, SortField,
};

pub const LOCAL_INDEX_DIR: &str = ".locator";
/// The file name of the index database. The name file sits beside it as `index.names`.
pub const INDEX_FILE_NAME: &str = "index.db";
/// The file name that lctr 0.4.x and earlier gave the index; a file by this name beside
/// `index.db` is converted and removed.
const OLD_INDEX_FILE_NAME: &str = "index.sqlite";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub name: String,
    pub parent: String,
    pub extension: Option<String>,
    pub root: String,
    pub volume: String,
    pub kind: String,
    pub size_bytes: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub extension: Option<String>,
    pub kind: String,
    pub size_bytes: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
}

pub(crate) const SEARCH_BATCH_SIZE: usize = 500;

/// A completed chunk of search results. The count is the number of results
/// emitted by the search so far, not the number of rows examined.
#[derive(Debug)]
pub(crate) struct SearchBatch {
    pub results: Vec<SearchResult>,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchStreamStatus {
    Complete { count: usize },
    Cancelled,
}

struct IndexedSearchCandidate {
    result: SearchResult,
    frecency: u32,
}

/// `lctr find` ranks at least this many rows, however small `--limit` is.
const MIN_RANKED_POOL: usize = 1024;

/// The best-ranked matches of a search, cut to a pool size.
struct TopMatches {
    results: Vec<SearchResult>,
    /// Rows that matched, including the ones ranked below the pool.
    matched_count: usize,
    /// Rows SQLite returned, before the Rust-side match.
    candidate_count: usize,
}

/// The rows a search matched, before sorting and before any disk read.
struct MatchedRows {
    results: Vec<SearchResult>,
    /// Recency-weighted open count per path, for the paths that have one.
    frecency: HashMap<String, u32>,
    /// Rows SQLite returned, before the Rust-side match.
    candidate_count: usize,
}

/// A file row eligible for the embedding pass (see `Database::embeddable_files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddableFile {
    pub path: String,
    pub kind: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_at: i64,
}

/// The changes table may hold this many rows before a search folds them into the name file.
const MAX_PENDING_CHANGES: usize = 2_000;

/// Where a handle keeps the name file it last opened. A handle normally has its own. The
/// search screen gives one to every query's handle and asks it to keep names, so the open
/// file and the names it has decoded stay in memory between queries.
#[derive(Clone, Default)]
pub struct NameFileCache {
    file: Arc<Mutex<Option<Arc<NameFile>>>>,
    keep_names: bool,
}

impl NameFileCache {
    /// A cache whose name files keep their decoded names in memory.
    pub fn keeping_names() -> Self {
        Self {
            file: Arc::default(),
            keep_names: true,
        }
    }

    fn get(&self) -> Option<Arc<NameFile>> {
        self.file.lock().ok()?.clone()
    }

    fn set(&self, file: Option<Arc<NameFile>>) {
        if let Ok(mut slot) = self.file.lock() {
            *slot = file;
        }
    }
}

pub struct Database {
    conn: Connection,
    path: Option<PathBuf>,
    verify_paths_on_search: bool,
    /// The name file last written or opened by this handle, or by every handle that shares
    /// its cache. For an in-memory database it is the only copy.
    name_file: NameFileCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCompletion {
    Complete,
    Incomplete,
    Unknown,
}

impl Database {
    fn from_connection(conn: Connection, path: Option<PathBuf>) -> Self {
        Self {
            conn,
            path,
            verify_paths_on_search: false,
            name_file: NameFileCache::default(),
        }
    }

    pub fn open_default() -> Result<Self> {
        let path = default_db_path()?;
        Self::open(path)
    }

    pub fn open_default_for_search() -> Result<Self> {
        let path = default_db_path()?;
        Self::open(&path)
            .or_else(|_| Self::open_readonly(&path))
            .map(Self::with_search_path_verification)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            create_index_dir(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        let db = Self::from_connection(conn, Some(path.to_path_buf()));
        db.migrate()?;
        db.adopt_old_sibling(path);
        Ok(db)
    }

    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if old_sibling_of(path).is_some_and(|old| old.exists()) {
            // An old index beside this one: a short writable open converts it. A failure
            // leaves the read-only open below to report what is wrong.
            let _ = Self::open(path);
        }
        let mut conn = open_read_only_connection(path)?;
        if table_exists(&conn, "files").unwrap_or(false) {
            // An index from before the name file: a short writable open converts it. When that
            // fails the index reads as one with no rows.
            drop(conn);
            let _ = Self::open(path);
            conn = open_read_only_connection(path)?;
        }
        check_index_format(&conn)?;
        Ok(Self::from_connection(conn, Some(path.to_path_buf())))
    }

    pub fn open_for_scan_root(root: impl AsRef<Path>) -> Result<(Self, PathBuf)> {
        if let Ok(path) = std::env::var("LCTR_DB") {
            let path = PathBuf::from(path);
            return Ok((Self::open(&path)?, path));
        }

        let local_path = local_db_path_for_root(&root)?;
        match Self::open(&local_path) {
            Ok(db) => Ok((db, local_path)),
            Err(error) if should_fallback_to_app_support(&error) => {
                let fallback_path = fallback_db_path_for_root(root)?;
                Ok((Self::open(&fallback_path)?, fallback_path))
            }
            Err(error) => Err(error),
        }
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory database")?;
        let db = Self::from_connection(conn, None);
        db.migrate()?;
        Ok(db)
    }

    pub fn with_search_path_verification(mut self) -> Self {
        self.verify_paths_on_search = true;
        self
    }

    /// Makes this handle use `cache` for its name file (see `NameFileCache`).
    pub fn with_name_file_cache(mut self, cache: NameFileCache) -> Self {
        self.name_file = cache;
        self
    }

    pub fn upsert_file(&self, record: &FileRecord) -> Result<()> {
        self.upsert_files(std::slice::from_ref(record))
    }

    pub fn upsert_files(&self, records: &[FileRecord]) -> Result<()> {
        self.upsert_files_with_indexed_at(records, Utc::now().timestamp_millis())
    }

    /// Records one live row per record in `changes` and registers each record's root. A root
    /// that is new here starts out incomplete: only a scan makes it complete.
    pub fn upsert_files_with_indexed_at(
        &self,
        records: &[FileRecord],
        indexed_at: i64,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let tx = self.begin_write()?;
        {
            let mut upsert = tx.prepare(
                "INSERT OR REPLACE INTO changes
                    (path, kind, size_bytes, created_at, modified_at, changed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut register = tx.prepare(
                "INSERT OR IGNORE INTO scan_roots
                    (root, scan_token, complete, started_at, completed_at)
                 VALUES (?1, 0, 0, 0, NULL)",
            )?;
            let mut registered = HashSet::new();
            for record in records {
                if registered.insert(record.root.as_str()) {
                    register.execute([&record.root])?;
                }
                upsert.execute(params![
                    record.path,
                    record.kind,
                    sqlite_size_bytes(record.size_bytes),
                    record.created_at.map(|dt| dt.timestamp()),
                    record.modified_at.map(|dt| dt.timestamp()),
                    indexed_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// A token for a scan of `root` that starts now: the current time in milliseconds, or
    /// one more than the root's last token when the clock has not moved past it. A scan
    /// tells its rows from older ones by token, so two scans must never share one.
    pub fn next_scan_token(&self, root: &str) -> Result<i64> {
        let previous: Option<i64> = self
            .conn
            .query_row(
                "SELECT scan_token FROM scan_roots WHERE root = ?1",
                params![root],
                |row| row.get(0),
            )
            .optional()
            .context("read the previous scan token")?;
        let now = Utc::now().timestamp_millis();
        Ok(previous.map_or(now, |last| now.max(last.saturating_add(1))))
    }

    pub fn scan_completion_for_root(&self, root: &str) -> Result<ScanCompletion> {
        let complete = self
            .conn
            .query_row(
                "SELECT complete FROM scan_roots WHERE root = ?1",
                [root],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        Ok(match complete {
            Some(1) => ScanCompletion::Complete,
            Some(_) => ScanCompletion::Incomplete,
            None => ScanCompletion::Unknown,
        })
    }

    /// The older search API. A query's runs of three or more letters and digits must each
    /// appear in the file name; a query with no such run must appear in the name as a whole.
    /// Results are ordered by stored modification time, newest first.
    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let needle = query.trim().to_lowercase();
        let runs: Vec<&str> = needle
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|run| run.chars().count() >= 3)
            .collect();

        let mut options = SearchOptions::new("");
        options.filters = filters.clone();
        let mut scorer = QueryScorer::new();
        let mut rows = Vec::new();
        self.for_each_match(
            &options,
            &CompiledQuery::Empty,
            &mut scorer,
            &mut || false,
            &mut |candidate, _| {
                let result = candidate.result;
                let name = result.name.to_lowercase();
                let found = if runs.is_empty() {
                    name.contains(&needle)
                } else {
                    runs.iter().all(|run| name.contains(run))
                };
                if found
                    && filters.metadata_bounds_match(
                        result.size_bytes,
                        result.created_at,
                        result.modified_at,
                    )
                {
                    rows.push(result);
                }
                Ok(())
            },
        )?;

        rows.sort_by(|a, b| {
            a.modified_at
                .is_none()
                .cmp(&b.modified_at.is_none())
                .then_with(|| b.modified_at.cmp(&a.modified_at))
                .then_with(|| a.name.cmp(&b.name))
        });
        rows.truncate(search_candidate_limit(self.verify_paths_on_search, limit));

        let mut results = self.hydrate_search_results(rows)?;
        results.truncate(limit);
        Ok(results)
    }

    pub fn search_with_options(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        if options.mode == QueryMode::Semantic {
            return Err(anyhow::anyhow!(
                "semantic search is served by locator::embed::search"
            ));
        }
        options.validate()?;
        if options.limit == 0 {
            return Ok(Vec::new());
        }

        let timing = SearchTiming::start();
        let compiled = CompiledQuery::compile(options.mode, &options.query)?;
        let mut scorer = QueryScorer::new();
        let on_disk = needs_disk_metadata(options);
        // Hydration drops rows whose file is gone, so rank more rows than `limit` asks for.
        let mut pool = options.limit.saturating_mul(4).max(MIN_RANKED_POOL);
        loop {
            let top = self.top_matches(options, &compiled, &mut scorer, pool, on_disk)?;
            timing.lap("match+rank");
            let unranked_matches_remain = top.matched_count > top.results.len();

            let mut results = Vec::with_capacity(options.limit.min(top.results.len()));
            for result in top.results {
                // With `on_disk` the rows were already read from disk before ranking.
                let result = if on_disk {
                    Some(result)
                } else {
                    self.hydrate_search_result(result)?
                };
                if let Some(result) = result {
                    results.push(result);
                    if results.len() == options.limit {
                        break;
                    }
                }
            }
            timing.lap("hydrate");

            if results.len() == options.limit || !unranked_matches_remain {
                timing.finish(top.candidate_count, top.matched_count);
                return Ok(results);
            }
            // Too many of the ranked rows pointed at files that no longer exist. Rank a
            // larger pool so the missing rows do not shorten the result.
            pool = pool.saturating_mul(8);
        }
    }

    /// Search the complete indexed result set for the interactive UI.
    ///
    /// Candidates are filtered and globally ordered while they are still
    /// lightweight index metadata, then hydrated in bounded batches so the UI
    /// can render progress without changing the final indexed order. A size or
    /// date filter or sort needs the metadata first, so in that case every
    /// match is hydrated before the first batch.
    pub(crate) fn search_streaming_with_options(
        &self,
        options: &SearchOptions,
        mut should_cancel: impl FnMut() -> bool,
        mut on_batch: impl FnMut(SearchBatch) -> Result<()>,
    ) -> Result<SearchStreamStatus> {
        if options.mode == QueryMode::Semantic {
            return Err(anyhow::anyhow!(
                "semantic search is served by locator::embed::search"
            ));
        }
        options.validate()?;
        if options.limit == 0 {
            return Ok(SearchStreamStatus::Complete { count: 0 });
        }

        let timing = SearchTiming::start();
        let compiled = CompiledQuery::compile(options.mode, &options.query)?;
        let mut scorer = QueryScorer::new();
        let Some(matched) =
            self.matching_rows(options, &compiled, &mut scorer, &mut should_cancel)?
        else {
            return Ok(SearchStreamStatus::Cancelled);
        };
        let MatchedRows {
            mut results,
            frecency,
            candidate_count,
        } = matched;
        let matched_count = results.len();
        timing.lap("match");

        let hydrated_up_front = needs_disk_metadata(options);
        if hydrated_up_front {
            let Some(mut hydrated) = self.hydrate_all(results, &mut should_cancel) else {
                return Ok(SearchStreamStatus::Cancelled);
            };
            hydrated.retain(|result| metadata_bounds_match(result, &options.filters));
            results = hydrated;
            timing.lap("hydrate+filter");
        }

        sort_results_compiled(&mut results, options, &compiled, &mut scorer, &frecency);
        timing.lap("sort");

        // Rows go to disk a wave at a time so the first batch is not held back by the rest.
        const HYDRATE_WAVE: usize = 4 * SEARCH_BATCH_SIZE;
        let mut pending = Vec::with_capacity(SEARCH_BATCH_SIZE);
        let mut emitted = 0;
        let mut rows = results.into_iter().peekable();
        while rows.peek().is_some() {
            let wave: Vec<SearchResult> = rows.by_ref().take(HYDRATE_WAVE).collect();
            let wave = if hydrated_up_front {
                wave
            } else {
                match self.hydrate_all(wave, &mut should_cancel) {
                    Some(wave) => wave,
                    None => return Ok(SearchStreamStatus::Cancelled),
                }
            };
            for result in wave {
                pending.push(result);
                if pending.len() == SEARCH_BATCH_SIZE {
                    if should_cancel() {
                        return Ok(SearchStreamStatus::Cancelled);
                    }
                    emitted += pending.len();
                    on_batch(SearchBatch {
                        results: std::mem::replace(
                            &mut pending,
                            Vec::with_capacity(SEARCH_BATCH_SIZE),
                        ),
                        count: emitted,
                    })?;
                }
            }
        }
        if !pending.is_empty() {
            emitted += pending.len();
            on_batch(SearchBatch {
                results: pending,
                count: emitted,
            })?;
        }

        timing.finish(candidate_count, matched_count);
        Ok(SearchStreamStatus::Complete { count: emitted })
    }

    /// Every active row that matches `options`, unsorted and not yet read from disk.
    /// Returns `None` when `should_cancel` fires.
    fn matching_rows(
        &self,
        options: &SearchOptions,
        compiled: &CompiledQuery,
        scorer: &mut QueryScorer,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> Result<Option<MatchedRows>> {
        let mut results = Vec::new();
        let mut frecency = HashMap::new();
        let candidate_count = self.for_each_match(
            options,
            compiled,
            scorer,
            should_cancel,
            &mut |candidate, _| {
                if candidate.frecency > 0 {
                    frecency.insert(candidate.result.path.clone(), candidate.frecency);
                }
                results.push(candidate.result);
                Ok(())
            },
        )?;
        Ok(candidate_count.map(|candidate_count| MatchedRows {
            results,
            frecency,
            candidate_count,
        }))
    }

    /// The best `pool` matches for `options`, in final order. Memory stays proportional to
    /// `pool`, not to the number of matches: rows are ranked and cut back to `pool` as they
    /// arrive. With `on_disk`, each row is read from disk and checked against the size and
    /// date bounds before it is ranked.
    fn top_matches(
        &self,
        options: &SearchOptions,
        compiled: &CompiledQuery,
        scorer: &mut QueryScorer,
        pool: usize,
        on_disk: bool,
    ) -> Result<TopMatches> {
        const HYDRATE_WAVE: usize = 16_384;
        let compact_at = pool.saturating_mul(8);
        let mut kept: Vec<SearchResult> = Vec::new();
        let mut pending: Vec<SearchResult> = Vec::new();
        let mut frecency = HashMap::new();
        let mut matched_count = 0usize;

        let candidate_count = self
            .for_each_match(
                options,
                compiled,
                scorer,
                &mut || false,
                &mut |candidate, scorer| {
                    if candidate.frecency > 0 {
                        frecency.insert(candidate.result.path.clone(), candidate.frecency);
                    }
                    if on_disk {
                        pending.push(candidate.result);
                        if pending.len() < HYDRATE_WAVE {
                            return Ok(());
                        }
                        let wave = std::mem::take(&mut pending);
                        let mut wave = self.hydrate_all(wave, &mut || false).unwrap_or_default();
                        wave.retain(|result| metadata_bounds_match(result, &options.filters));
                        matched_count += wave.len();
                        kept.append(&mut wave);
                    } else {
                        matched_count += 1;
                        kept.push(candidate.result);
                    }
                    if kept.len() >= compact_at {
                        sort_results_compiled(&mut kept, options, compiled, scorer, &frecency);
                        kept.truncate(pool);
                    }
                    Ok(())
                },
            )?
            .unwrap_or(0);

        if !pending.is_empty() {
            let mut wave = self.hydrate_all(pending, &mut || false).unwrap_or_default();
            wave.retain(|result| metadata_bounds_match(result, &options.filters));
            matched_count += wave.len();
            kept.append(&mut wave);
        }
        sort_results_compiled(&mut kept, options, compiled, scorer, &frecency);
        kept.truncate(pool);

        Ok(TopMatches {
            results: kept,
            matched_count,
            candidate_count,
        })
    }

    /// Runs the search for `options` over the name file and calls `on_match` for every active
    /// row that `compiled` accepts. `lctr find` and the TUI both start here, so they agree on
    /// what matches. Rows changed since the name file was written come from `changes`. Returns
    /// the number of rows the match test ran on, or `None` when `should_cancel` fired.
    fn for_each_match(
        &self,
        options: &SearchOptions,
        compiled: &CompiledQuery,
        scorer: &mut QueryScorer,
        should_cancel: &mut dyn FnMut() -> bool,
        on_match: &mut dyn FnMut(IndexedSearchCandidate, &mut QueryScorer) -> Result<()>,
    ) -> Result<Option<usize>> {
        let (file, pending) = self.name_file_for_search()?;
        let frecency = self.access_weights();
        // Below this size a second thread costs more than it saves.
        const MIN_ROWS_FOR_THREADS: usize = 50_000;
        let threads = if file.len() < MIN_ROWS_FOR_THREADS {
            1
        } else {
            worker_threads()
        };
        let scan = NameScan {
            file: &file,
            options,
            pending: &pending,
            frecency: &frecency,
            threads,
        };
        let Some(mut examined) = scan.run(compiled, scorer, should_cancel, on_match)? else {
            return Ok(None);
        };
        for result in &pending.live {
            if should_cancel() {
                return Ok(None);
            }
            let extension = result.extension.as_deref();
            if !row_passes_filters(&options.filters, &result.name, &result.kind, extension) {
                continue;
            }
            examined += 1;
            if !row_matches(options, compiled, scorer, &result.name, &result.path) {
                continue;
            }
            let frecency = frecency.get(&result.path).copied().unwrap_or(0);
            on_match(
                IndexedSearchCandidate {
                    result: result.clone(),
                    frecency,
                },
                scorer,
            )?;
        }
        Ok(Some(examined))
    }

    /// The `snapshot_id` and the changes, read in one transaction so they describe the same
    /// moment. A database without the state tables (an old index opened read-only) reads as
    /// "no name file, no changes".
    fn read_search_state(&self) -> Result<(u64, Pending)> {
        let tx = self.conn.unchecked_transaction()?;
        let state = match load_state(&tx) {
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("no such table") =>
            {
                (0, Pending::default())
            }
            other => other?,
        };
        tx.commit()?;
        Ok(state)
    }

    /// The name file a reader should use for `snapshot_id`. The id only tells whether the
    /// cached file is current: when it is not, the file on disk is used whatever id it
    /// carries. Id 0 and a missing file both read as a file with no rows; a file that exists
    /// but cannot be opened is an error.
    fn name_file_with_id(&self, snapshot_id: u64) -> Result<Arc<NameFile>> {
        if snapshot_id == 0 {
            return empty_name_file();
        }
        if let Some(cached) = self.name_file.get() {
            if cached.snapshot_id() == snapshot_id || self.path.is_none() {
                return Ok(cached);
            }
        }
        let Some(db_path) = &self.path else {
            return empty_name_file();
        };
        match NameFile::open(&name_file_path(db_path)) {
            Ok(file) => {
                let file = if self.name_file.keep_names {
                    file.keeping_names()
                } else {
                    file
                };
                let file = Arc::new(file);
                self.name_file.set(Some(Arc::clone(&file)));
                Ok(file)
            }
            Err(error) if is_not_found(&error) => empty_name_file(),
            Err(error) => Err(error),
        }
    }

    /// The name file a search should read, with the changes it does not yet contain. Past
    /// `MAX_PENDING_CHANGES` the changes are folded into a new file first.
    fn name_file_for_search(&self) -> Result<(Arc<NameFile>, Pending)> {
        let (mut snapshot_id, mut pending) = self.read_search_state()?;
        if pending.len() > MAX_PENDING_CHANGES && self.compact_name_file().is_ok() {
            (snapshot_id, pending) = self.read_search_state()?;
        }
        Ok((self.name_file_with_id(snapshot_id)?, pending))
    }

    /// Recency-weighted open count of every path that has one. A database without the
    /// `access` table yields an empty map.
    fn access_weights(&self) -> HashMap<String, u32> {
        let now = Utc::now().timestamp();
        let mut weights = HashMap::new();
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT path, open_count, last_opened_at FROM access")
        else {
            return weights;
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        });
        for (path, open_count, last_opened_at) in rows.into_iter().flatten().flatten() {
            let weight = frecency_weight(open_count.max(0) as u32, last_opened_at, now);
            if weight > 0 {
                weights.insert(path, weight);
            }
        }
        weights
    }

    /// Reads every row's metadata from disk and drops rows whose file is gone (when this
    /// database verifies paths). The work is one `symlink_metadata` call per row, so it runs
    /// on several threads. Row order is kept. Returns `None` when `should_cancel` fires.
    fn hydrate_all(
        &self,
        mut results: Vec<SearchResult>,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> Option<Vec<SearchResult>> {
        const ROWS_PER_THREAD: usize = 256;
        let verify_paths = self.verify_paths_on_search;
        let wave = ROWS_PER_THREAD * worker_threads();
        let mut keep = vec![true; results.len()];

        for (rows, kept) in results.chunks_mut(wave).zip(keep.chunks_mut(wave)) {
            if should_cancel() {
                return None;
            }
            if rows.len() <= ROWS_PER_THREAD {
                for (row, keep) in rows.iter_mut().zip(kept.iter_mut()) {
                    *keep = hydrate_in_place(row, verify_paths);
                }
                continue;
            }
            std::thread::scope(|scope| {
                for (rows, kept) in rows
                    .chunks_mut(ROWS_PER_THREAD)
                    .zip(kept.chunks_mut(ROWS_PER_THREAD))
                {
                    scope.spawn(move || {
                        for (row, keep) in rows.iter_mut().zip(kept.iter_mut()) {
                            *keep = hydrate_in_place(row, verify_paths);
                        }
                    });
                }
            });
        }

        let mut keep = keep.into_iter();
        results.retain(|_| keep.next().unwrap_or(true));
        Some(results)
    }

    pub fn search_interactive(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let Some(needle) = normalize_filename_query(query) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }

        let options = SearchOptions::new(needle.as_str()).with_mode(QueryMode::Prefix);
        let compiled = CompiledQuery::compile(QueryMode::Prefix, &needle)?;
        let mut scorer = QueryScorer::new();
        let stem_prefix = format!("{needle}.");
        // Tier 0: the name is the needle. Tier 1: the name starts with `needle.`. Tier 2: any
        // other name that starts with the needle. Prefix mode also matches on the path, so the
        // name is checked here.
        let mut rows: Vec<(u8, String, SearchResult)> = Vec::new();
        self.for_each_match(
            &options,
            &compiled,
            &mut scorer,
            &mut || false,
            &mut |candidate, _| {
                let name = candidate.result.name.to_ascii_lowercase();
                let tier = if name == needle {
                    0
                } else if name.starts_with(&stem_prefix) {
                    1
                } else if name.starts_with(&needle) {
                    2
                } else {
                    return Ok(());
                };
                rows.push((tier, name, candidate.result));
                Ok(())
            },
        )?;
        rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        rows.truncate(search_candidate_limit(self.verify_paths_on_search, limit));

        let mut results =
            self.hydrate_search_results(rows.into_iter().map(|row| row.2).collect())?;
        results.truncate(limit);
        Ok(results)
    }

    /// Record that a path was opened/revealed/copied, for frecency ranking.
    pub fn record_access(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO access(path, open_count, last_opened_at) VALUES (?1, 1, ?2)
             ON CONFLICT(path) DO UPDATE SET
                open_count = open_count + 1,
                last_opened_at = ?2",
            params![path, Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Copy frecency rows (`access`) from a previous index file into this
    /// database. Converting an old index calls this so the open/reveal/copy
    /// ranking carries over. Returns the number of rows copied. A missing previous file, or one
    /// without an `access` table, copies nothing.
    pub fn import_access_from(&self, previous: &Path) -> Result<u64> {
        if !previous.exists() {
            return Ok(0);
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS access (
                path TEXT PRIMARY KEY,
                open_count INTEGER NOT NULL DEFAULT 0,
                last_opened_at INTEGER
             );",
        )?;
        self.conn.execute(
            "ATTACH DATABASE ?1 AS previous",
            params![previous.to_string_lossy()],
        )?;
        let copied = (|| -> Result<u64> {
            let has_access: bool = self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM previous.sqlite_master
                    WHERE type = 'table' AND name = 'access'
                 )",
                [],
                |row| row.get(0),
            )?;
            if !has_access {
                return Ok(0);
            }
            let rows = self.conn.execute(
                "INSERT OR REPLACE INTO main.access(path, open_count, last_opened_at)
                 SELECT path, open_count, last_opened_at FROM previous.access",
                [],
            )?;
            Ok(rows as u64)
        })();
        let detached = self.conn.execute_batch("DETACH DATABASE previous");
        let copied = copied?;
        detached?;
        Ok(copied)
    }

    /// Recency-weighted open count for each of `paths` that has one. Best effort: a database
    /// without the `access` table yields an empty map.
    pub(crate) fn frecency_for_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, u32> {
        let now = Utc::now().timestamp();
        let mut scores = HashMap::new();
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT open_count, last_opened_at FROM access WHERE path = ?1")
        else {
            return scores;
        };
        for path in paths {
            let row = stmt.query_row(params![path], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
            });
            if let Ok((count, last_opened_at)) = row {
                let weight = frecency_weight(count.max(0) as u32, last_opened_at, now);
                if weight > 0 {
                    scores.insert(path.to_string(), weight);
                }
            }
        }
        scores
    }

    /// Records that `path` and everything beneath it is gone. Returns how many active rows
    /// that removed. The removal is recorded even when the count is 0: a scan that is walking
    /// the tree right now may have seen the path, and the newer removal hides it.
    pub fn mark_path_deleted(&self, path: &str) -> Result<u64> {
        // The rows beneath `path` are exactly the paths in a byte range of the BINARY-collated
        // key. LIKE would treat `_` and `%` as wildcards and ignore ASCII case.
        let next = char::from_u32(MAIN_SEPARATOR as u32 + 1).context("separator successor")?;
        let base = path.trim_end_matches(MAIN_SEPARATOR);
        let lower = format!("{base}{MAIN_SEPARATOR}");
        let upper = format!("{base}{next}");

        let tx = self.begin_write()?;
        let (snapshot_id, pending) = load_state(&tx)?;
        let live: i64 = tx.query_row(
            "SELECT COUNT(*) FROM changes
             WHERE kind IS NOT NULL AND (path = ?1 OR (path >= ?2 AND path < ?3))",
            params![base, lower, upper],
            |row| row.get(0),
        )?;
        let mut count = u64::try_from(live).unwrap_or(0);
        let file = self.name_file_with_id(snapshot_id)?;
        file.reader().for_each_under(base, &mut |row| {
            count += u64::from(!pending.hides(row.path));
            true
        });
        // A live row beneath the path was written before this removal, so it goes too.
        tx.execute(
            "DELETE FROM changes WHERE path >= ?1 AND path < ?2",
            params![lower, upper],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO changes
                (path, kind, size_bytes, created_at, modified_at, changed_at)
             VALUES (?1, NULL, 0, NULL, NULL, ?2)",
            params![base, Utc::now().timestamp_millis()],
        )?;
        tx.commit()?;
        Ok(count)
    }

    /// Replaces the rows of `root` with the rows of a finished scan. The rows of every other
    /// registered root stay. Changes the watcher recorded while the scan ran (`changed_at` at
    /// or after `scan_token`) stay too and still apply; older ones are covered by the scan.
    /// Nothing is written until this call, so an abandoned scan changes nothing. A registered
    /// root nested inside `root` that the scan walked (it has a row at or beneath it) is
    /// unregistered, and `root` takes over its rows. A nested root the walk did not reach keeps
    /// its registration and its rows.
    pub fn install_scan(
        &self,
        root: &str,
        mut rows: NameFileBuilder,
        scan_token: i64,
    ) -> Result<()> {
        let tx = self.begin_write()?;
        let mut registered = registered_roots(&tx)?;
        let mut taken = Vec::new();
        registered.retain(|known| {
            let walked =
                known != root && is_at_or_beneath(known, root) && rows.has_rows_beneath(known);
            if walked {
                taken.push(known.clone());
            }
            !walked
        });
        for nested in &taken {
            tx.execute("DELETE FROM scan_roots WHERE root = ?1", [nested])?;
        }
        let has_other_roots = registered.iter().any(|known| known != root);
        let mut roots = registered;
        if !roots.iter().any(|known| known == root) {
            roots.push(root.to_string());
        }
        if has_other_roots {
            let (snapshot_id, pending) = load_state(&tx)?;
            let old = self.name_file_with_id(snapshot_id)?;
            push_active_rows(&old, &pending, &mut rows, &mut |path| {
                owner(path, &roots) != Some(root)
            })?;
        }
        self.replace_name_file(&tx, rows)?;

        let candidates = {
            let mut stmt = tx.prepare("SELECT path FROM changes WHERE changed_at < ?1")?;
            let paths = stmt.query_map([scan_token], |row| row.get::<_, String>(0))?;
            paths.collect::<rusqlite::Result<Vec<_>>>()?
        };
        {
            let mut delete = tx.prepare("DELETE FROM changes WHERE path = ?1")?;
            for path in &candidates {
                if owner(path, &roots) == Some(root) {
                    delete.execute([path])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO scan_roots (root, scan_token, complete, started_at, completed_at)
             VALUES (?1, ?2, 1, ?2, ?3)
             ON CONFLICT(root) DO UPDATE SET
                scan_token = excluded.scan_token,
                complete = 1,
                started_at = excluded.started_at,
                completed_at = excluded.completed_at",
            params![root, scan_token, Utc::now().timestamp_millis()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Drops the active rows that `root` owns and its registration. Returns how many rows it
    /// dropped. A nested root keeps its rows.
    pub fn remove_root(&self, root: &str) -> Result<u64> {
        let tx = self.begin_write()?;
        let roots = registered_roots(&tx)?;
        let mut dropped = 0;
        self.fold_changes(&tx, &mut |path| {
            let owned = owner(path, &roots) == Some(root);
            dropped += u64::from(owned);
            !owned
        })?;
        tx.execute("DELETE FROM scan_roots WHERE root = ?1", [root])?;
        tx.commit()?;
        Ok(dropped)
    }

    /// Folds the changes into a new name file and empties `changes`.
    pub fn compact_name_file(&self) -> Result<()> {
        let tx = self.begin_write()?;
        self.fold_changes(&tx, &mut |_| true)?;
        tx.commit()?;
        Ok(())
    }

    /// Folds the changes when there are more than `MAX_PENDING_CHANGES` of them.
    pub fn compact_name_file_if_needed(&self) -> Result<()> {
        let changes: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))?;
        if usize::try_from(changes).is_ok_and(|count| count > MAX_PENDING_CHANGES) {
            self.compact_name_file()?;
        }
        Ok(())
    }

    /// Folds the changes into a new name file, keeping the active rows `keep` accepts, and
    /// empties `changes`. The caller commits `tx`.
    fn fold_changes(&self, tx: &Transaction<'_>, keep: &mut dyn FnMut(&str) -> bool) -> Result<()> {
        let (snapshot_id, pending) = load_state(tx)?;
        let file = self.name_file_with_id(snapshot_id)?;
        let mut rows = NameFileBuilder::default();
        push_active_rows(&file, &pending, &mut rows, keep)?;
        self.replace_name_file(tx, rows)?;
        tx.execute("DELETE FROM changes", [])?;
        Ok(())
    }

    /// Writes a new name file from `rows` and records its id. The caller commits `tx`. The
    /// file is renamed into place before SQLite learns of it, so a crash in between leaves a
    /// file that is newer than `changes` says, which readers handle (see `load_state`).
    fn replace_name_file(&self, tx: &Transaction<'_>, rows: NameFileBuilder) -> Result<()> {
        let snapshot_id = new_snapshot_id();
        let image = rows.finish(snapshot_id)?;
        if let Some(db_path) = &self.path {
            let file_path = name_file_path(db_path);
            name_file::install(&image, &file_path)
                .with_context(|| format!("write name file {}", file_path.display()))?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO search_state(key, value) VALUES ('snapshot_id', ?1)",
            [i64::try_from(snapshot_id).unwrap_or(i64::MAX)],
        )?;
        self.name_file.set(if self.path.is_some() {
            None
        } else {
            Some(Arc::new(
                NameFile::from_bytes(image).context("load the new name file")?,
            ))
        });
        Ok(())
    }

    fn begin_write(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context("start a write transaction")
    }

    /// The number of active rows: the name file's rows that no change hides, plus the live
    /// changes.
    pub fn count_active(&self) -> Result<u64> {
        let (snapshot_id, pending) = self.read_search_state()?;
        let file = self.name_file_with_id(snapshot_id)?;
        if pending.is_empty() {
            return Ok(file.len() as u64);
        }
        let mut count = pending.live.len() as u64;
        file.reader().for_each(0..file.len(), &mut |row| {
            count += u64::from(!pending.hides(row.path));
            true
        });
        Ok(count)
    }

    /// Active non-folder files owned by `root`, for the embedding pass.
    pub fn embeddable_files(&self, root: &str) -> Result<Vec<EmbeddableFile>> {
        let (snapshot_id, pending) = self.read_search_state()?;
        let file = self.name_file_with_id(snapshot_id)?;
        let mut roots = self.roots()?;
        if !roots.iter().any(|known| known == root) {
            roots.push(root.to_string());
        }
        let wanted = |path: &str, kind: &str| kind != "folder" && owner(path, &roots) == Some(root);

        let mut found = Vec::new();
        file.reader().for_each(0..file.len(), &mut |row| {
            if wanted(row.path, row.kind) && !pending.hides(row.path) {
                found.push(EmbeddableFile {
                    path: row.path.to_string(),
                    kind: row.kind.to_string(),
                    extension: extension_of(row.name),
                    size_bytes: 0,
                    modified_at: 0,
                });
            }
            true
        });
        for live in &pending.live {
            if wanted(&live.path, &live.kind) {
                found.push(EmbeddableFile {
                    path: live.path.clone(),
                    kind: live.kind.clone(),
                    extension: live.extension.clone(),
                    size_bytes: live.size_bytes,
                    modified_at: live.modified_at.map_or(0, |at| at.timestamp()),
                });
            }
        }
        found.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(found)
    }

    pub fn roots(&self) -> Result<Vec<String>> {
        registered_roots(&self.conn).context("collect roots")
    }

    pub fn vacuum(&self) -> Result<()> {
        self.compact_name_file()?;
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Imports the rows of an index written before the name file from this database's own
    /// `files` table, then drops the old tables.
    fn convert_old_tables(&self) -> Result<()> {
        self.import_old_rows(&self.conn)?;
        // Dropping `files` also drops its indexes and triggers; the full-text table and the
        // triggers that feed it are named so no trigger is left pointing at a dropped table.
        self.conn.execute_batch(
            "DROP TRIGGER IF EXISTS files_ai;
             DROP TRIGGER IF EXISTS files_ad;
             DROP TRIGGER IF EXISTS files_au;
             DROP TABLE IF EXISTS files_fts;
             DROP TABLE IF EXISTS pending_changes;
             DROP TABLE IF EXISTS files;",
        )?;
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Copies the active rows, the roots and the scan state of an index written before the
    /// compact format out of `old` (a database with a `files` table) into this one. The old
    /// rows replace whatever this index holds for the roots they list; the rows of other
    /// roots stay. `old` is either this database's own connection or another one.
    fn import_old_rows(&self, old: &Connection) -> Result<()> {
        let location = old
            .path()
            .filter(|path| !path.is_empty())
            .unwrap_or("an in-memory database");
        eprintln!("lctr: converting the index at {location} to the new format (one time)");
        let own_tables = std::ptr::eq(old, &self.conn);
        let old_roots = old_scan_roots(old)?;

        let tx = self.begin_write()?;
        let mut roots = registered_roots(&tx)?;
        for old_root in &old_roots {
            if !roots.contains(&old_root.root) {
                roots.push(old_root.root.clone());
            }
        }
        let mut rows = NameFileBuilder::default();
        if !own_tables {
            let (snapshot_id, pending) = load_state(&tx)?;
            let file = self.name_file_with_id(snapshot_id)?;
            push_active_rows(&file, &pending, &mut rows, &mut |path| {
                !owner(path, &roots)
                    .is_some_and(|root| old_roots.iter().any(|old_root| old_root.root == root))
            })?;
        }
        push_old_rows(old, &mut rows)?;
        self.replace_name_file(&tx, rows)?;
        // The changes that are not covered by the old rows were folded in above.
        tx.execute("DELETE FROM changes", [])?;
        for old_root in &old_roots {
            tx.execute(
                "INSERT OR REPLACE INTO scan_roots
                    (root, scan_token, complete, started_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    old_root.root,
                    old_root.scan_token,
                    old_root.complete,
                    old_root.started_at,
                    old_root.completed_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A name file that is gone while SQLite says one was written means the rows are lost.
    /// Every root is then marked incomplete, so the search falls back to the disk.
    fn mark_lost_name_file(&self) -> Result<()> {
        let Some(db_path) = &self.path else {
            return Ok(());
        };
        let file_path = name_file_path(db_path);
        let missing = matches!(
            fs::metadata(&file_path),
            Err(error) if error.kind() == ErrorKind::NotFound
        );
        if !missing || snapshot_id_of(&self.conn)? == 0 {
            return Ok(());
        }
        let tx = self.begin_write()?;
        tx.execute("UPDATE scan_roots SET complete = 0", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO search_state(key, value) VALUES ('snapshot_id', 0)",
            [],
        )?;
        tx.commit()?;
        eprintln!(
            "lctr: the name file {} is missing, so the index at {} has lost its rows; run `lctr scan` to rebuild it",
            file_path.display(),
            db_path.display()
        );
        Ok(())
    }

    /// Converts the old index that sits beside `db_path`, when there is one: its rows and open
    /// history move into this index and its files are deleted. A failure is reported and the
    /// open goes on.
    fn adopt_old_sibling(&self, db_path: &Path) {
        let Some(old_path) = old_sibling_of(db_path).filter(|old| old.exists()) else {
            return;
        };
        if let Err(error) = self.convert_sibling(&old_path) {
            eprintln!(
                "lctr: could not convert the old index at {}: {error:#}",
                old_path.display()
            );
        }
    }

    fn convert_sibling(&self, old_path: &Path) -> Result<()> {
        let old = open_read_only_connection(old_path)?;
        if !table_exists(&old, "files")? {
            // Not an index of ours, so it is not ours to delete.
            return Ok(());
        }
        self.import_old_rows(&old)?;
        drop(old);
        self.import_access_from(old_path)?;
        for suffix in ["", "-wal", "-shm"] {
            let file = PathBuf::from(format!("{}{suffix}", old_path.display()));
            match fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("remove old index file {}", file.display()));
                }
            }
        }
        Ok(())
    }

    /// Creates the state tables, converts an index written before the name file, and notices
    /// a lost name file. An index written by a newer lctr is refused before anything changes.
    fn migrate(&self) -> Result<()> {
        check_index_format(&self.conn)?;
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             CREATE TABLE IF NOT EXISTS scan_roots (
                root TEXT PRIMARY KEY,
                scan_token INTEGER NOT NULL,
                complete INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS access (
                path TEXT PRIMARY KEY,
                open_count INTEGER NOT NULL DEFAULT 0,
                last_opened_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS search_state (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS changes (
                path TEXT PRIMARY KEY,
                kind TEXT,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER,
                modified_at INTEGER,
                changed_at INTEGER NOT NULL
             ) WITHOUT ROWID;",
        )?;
        if table_exists(&self.conn, "files")? {
            self.convert_old_tables()?;
        }
        self.mark_lost_name_file()?;
        if index_format_of(&self.conn)? != i64::from(INDEX_FORMAT) {
            self.conn
                .pragma_update(None, "user_version", INDEX_FORMAT)?;
        }
        Ok(())
    }
}

/// The `user_version` of a database: the format of the index that lctr wrote into it.
fn index_format_of(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// Refuses a database that a newer lctr wrote.
fn check_index_format(conn: &Connection) -> Result<()> {
    if index_format_of(conn)? > i64::from(INDEX_FORMAT) {
        anyhow::bail!("this index was written by a newer lctr; run `lctr update`");
    }
    Ok(())
}

/// The old-format index that would sit beside `db_path`, when `db_path` has the default name.
fn old_sibling_of(db_path: &Path) -> Option<PathBuf> {
    (db_path.file_name()? == INDEX_FILE_NAME).then(|| db_path.with_file_name(OLD_INDEX_FILE_NAME))
}

/// Whether an index is present at `db_path`: the file exists, or an old one sits beside it
/// and the next open will convert it.
fn index_is_present(db_path: &Path) -> bool {
    db_path.exists() || db_path.with_file_name(OLD_INDEX_FILE_NAME).exists()
}

/// A `scan_roots` row of an index written before the name file.
struct OldRoot {
    root: String,
    scan_token: i64,
    complete: i64,
    started_at: i64,
    completed_at: Option<i64>,
}

/// The roots of an old index: its `scan_roots` rows, plus every root that only appears in
/// `files`, which counts as incomplete.
fn old_scan_roots(old: &Connection) -> Result<Vec<OldRoot>> {
    let mut roots = Vec::new();
    if table_exists(old, "scan_roots")? {
        let mut stmt = old.prepare(
            "SELECT root, scan_token, complete, started_at, completed_at FROM scan_roots",
        )?;
        let found = stmt.query_map([], |row| {
            Ok(OldRoot {
                root: row.get(0)?,
                scan_token: row.get(1)?,
                complete: row.get(2)?,
                started_at: row.get(3)?,
                completed_at: row.get(4)?,
            })
        })?;
        for root in found {
            roots.push(root?);
        }
    }
    let mut stmt = old.prepare("SELECT DISTINCT root FROM files WHERE deleted = 0")?;
    for root in stmt.query_map([], |row| row.get::<_, String>(0))? {
        let root = root?;
        if !roots.iter().any(|known| known.root == root) {
            roots.push(OldRoot {
                root,
                scan_token: 0,
                complete: 0,
                started_at: 0,
                completed_at: None,
            });
        }
    }
    Ok(roots)
}

/// Streams the active rows of an old index into `rows`, never holding them all at once.
fn push_old_rows(old: &Connection, rows: &mut NameFileBuilder) -> Result<()> {
    let mut stmt = old.prepare("SELECT path, kind FROM files WHERE deleted = 0")?;
    let mut cursor = stmt.query([])?;
    while let Some(row) = cursor.next()? {
        rows.push(row.get_ref(0)?.as_str()?, row.get_ref(1)?.as_str()?)?;
    }
    Ok(())
}

/// Pushes the active rows that `keep` accepts into `rows`: the name file's rows that no
/// change hides, then the live changes. `keep` sees each active row once.
fn push_active_rows(
    file: &NameFile,
    pending: &Pending,
    rows: &mut NameFileBuilder,
    keep: &mut dyn FnMut(&str) -> bool,
) -> Result<()> {
    let mut failure = None;
    file.reader().for_each(0..file.len(), &mut |row| {
        if pending.hides(row.path) || !keep(row.path) {
            return true;
        }
        match rows.push(row.path, row.kind) {
            Ok(()) => true,
            Err(error) => {
                failure = Some(error);
                false
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    for live in &pending.live {
        if keep(&live.path) {
            rows.push(&live.path, &live.kind)?;
        }
    }
    Ok(())
}

fn open_read_only_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open read-only database {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
}

/// The registered roots, sorted.
fn registered_roots(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT root FROM scan_roots ORDER BY root")?;
    let roots = stmt.query_map([], |row| row.get::<_, String>(0))?;
    roots.collect()
}

fn snapshot_id_of(conn: &Connection) -> rusqlite::Result<u64> {
    let id = conn
        .query_row(
            "SELECT value FROM search_state WHERE key = 'snapshot_id'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(id.and_then(|id| u64::try_from(id).ok()).unwrap_or(0))
}

/// The id for a name file written now: the nanosecond clock, which is never 0.
fn new_snapshot_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .and_then(|nanos| u64::try_from(nanos).ok())
        .filter(|nanos| *nanos > 0)
        .unwrap_or(1)
}

fn empty_name_file() -> Result<Arc<NameFile>> {
    let image = NameFileBuilder::default().finish(1)?;
    Ok(Arc::new(
        NameFile::from_bytes(image).context("load an empty name file")?,
    ))
}

/// Whether `error` came from a file that does not exist.
fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| io_error.kind() == ErrorKind::NotFound)
    })
}

/// Creates the directory that holds an index. A `.locator` directory also gets a `.gitignore`
/// containing `*`, so a scanned git project never picks up the index or the embeddings sidecar
/// (which stores short plain-text snippets of the scanned files).
pub fn create_index_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    if dir.file_name().is_some_and(|name| name == LOCAL_INDEX_DIR) {
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            // Best effort: a read-only or racing directory must not fail the scan.
            let _ = fs::write(&ignore, "*\n");
        }
    }
    Ok(())
}

pub fn delete_index_files(path: impl AsRef<Path>) -> Result<u64> {
    let path = path.as_ref();
    let mut removed = 0;
    let mut files = vec![
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
        name_file_path(path),
        PathBuf::from(format!("{}.tmp", name_file_path(path).display())),
    ];
    if let Some(old) = old_sibling_of(path) {
        files.push(PathBuf::from(format!("{}-wal", old.display())));
        files.push(PathBuf::from(format!("{}-shm", old.display())));
        files.push(old);
    }
    for file in files {
        match fs::remove_file(&file) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove index file {}", file.display()));
            }
        }
    }
    Ok(removed)
}

/// Removes the staging files that older versions left in the data folder of `root` after an
/// interrupted scan. Nothing is reported, and the folder goes too when it is then empty.
pub fn remove_old_staging_files(root: &Path) {
    let Some(folder) = fallback_db_path_for_root(root)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };
    for name in [
        "staging.sqlite",
        "staging.sqlite-wal",
        "staging.sqlite-shm",
        "staging.names",
        "staging.names.tmp",
    ] {
        let _ = fs::remove_file(folder.join(name));
    }
    let _ = fs::remove_dir(&folder);
}

pub fn default_db_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LCTR_DB") {
        return Ok(PathBuf::from(path));
    }

    db_path_for_working_dir(std::env::current_dir().context("locate current directory")?)
}

pub fn existing_db_path_for_working_dir(start: impl AsRef<Path>) -> Result<Option<PathBuf>> {
    if let Ok(path) = std::env::var("LCTR_DB") {
        let path = PathBuf::from(path);
        return Ok(path.exists().then_some(path));
    }

    let mut current = start.as_ref().canonicalize().with_context(|| {
        format!(
            "resolve working directory for existing local index lookup {}",
            start.as_ref().display()
        )
    })?;

    loop {
        let candidate = current.join(LOCAL_INDEX_DIR).join(INDEX_FILE_NAME);
        if index_is_present(&candidate) {
            return Ok(Some(candidate));
        }
        let fallback_candidate = fallback_db_path_for_root(&current)?;
        if index_is_present(&fallback_candidate) {
            return Ok(Some(fallback_candidate));
        }
        if !current.pop() {
            break;
        }
    }

    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedIndex {
    pub db_path: PathBuf,
    pub root: PathBuf,
}

pub fn existing_index_for_working_dir(start: impl AsRef<Path>) -> Result<Option<LocatedIndex>> {
    if let Ok(path) = std::env::var("LCTR_DB") {
        let db_path = PathBuf::from(path);
        if db_path.exists() {
            let root = start.as_ref().canonicalize().with_context(|| {
                format!(
                    "resolve working directory for env index lookup {}",
                    start.as_ref().display()
                )
            })?;
            return Ok(Some(LocatedIndex { db_path, root }));
        }
        return Ok(None);
    }

    let mut current = start.as_ref().canonicalize().with_context(|| {
        format!(
            "resolve working directory for existing index lookup {}",
            start.as_ref().display()
        )
    })?;

    loop {
        let candidate = current.join(LOCAL_INDEX_DIR).join(INDEX_FILE_NAME);
        if index_is_present(&candidate) {
            return Ok(Some(LocatedIndex {
                db_path: candidate,
                root: current,
            }));
        }
        let fallback_candidate = fallback_db_path_for_root(&current)?;
        if index_is_present(&fallback_candidate) {
            return Ok(Some(LocatedIndex {
                db_path: fallback_candidate,
                root: current,
            }));
        }
        if !current.pop() {
            break;
        }
    }

    Ok(None)
}

pub fn db_path_for_working_dir(start: impl AsRef<Path>) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LCTR_DB") {
        return Ok(PathBuf::from(path));
    }

    let mut current = start.as_ref().canonicalize().with_context(|| {
        format!(
            "resolve working directory for local index lookup {}",
            start.as_ref().display()
        )
    })?;

    loop {
        let candidate = current.join(LOCAL_INDEX_DIR).join(INDEX_FILE_NAME);
        if index_is_present(&candidate) {
            return Ok(candidate);
        }
        let fallback_candidate = fallback_db_path_for_root(&current)?;
        if index_is_present(&fallback_candidate) {
            return Ok(fallback_candidate);
        }
        if !current.pop() {
            break;
        }
    }

    app_support_db_path()
}

pub fn local_db_path_for_root(root: impl AsRef<Path>) -> Result<PathBuf> {
    let root = root.as_ref().canonicalize().with_context(|| {
        format!(
            "resolve scan root for local index {}",
            root.as_ref().display()
        )
    })?;
    Ok(root.join(LOCAL_INDEX_DIR).join(INDEX_FILE_NAME))
}

pub fn db_path_for_scan_root(root: impl AsRef<Path>) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LCTR_DB") {
        return Ok(PathBuf::from(path));
    }

    local_db_path_for_root(root)
}

pub fn fallback_db_path_for_root(root: impl AsRef<Path>) -> Result<PathBuf> {
    let root = root.as_ref().canonicalize().with_context(|| {
        format!(
            "resolve scan root for fallback index {}",
            root.as_ref().display()
        )
    })?;
    let root_key = stable_path_key(&root);
    Ok(app_support_indices_dir()?
        .join(root_key)
        .join(INDEX_FILE_NAME))
}

pub fn app_support_db_path() -> Result<PathBuf> {
    Ok(locator_data_dir()?.join(INDEX_FILE_NAME))
}

fn app_support_indices_dir() -> Result<PathBuf> {
    Ok(locator_data_dir()?.join("indices"))
}

pub(crate) fn locator_data_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LCTR_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }

    let base = dirs::data_dir().context("locate user data directory")?;
    Ok(base.join("locator"))
}

fn stable_path_key(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn should_fallback_to_app_support(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io_error| {
                matches!(
                    io_error.kind(),
                    ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
                )
            })
            .unwrap_or(false)
    })
}

fn normalize_filename_query(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if !trimmed.chars().any(|ch| ch.is_alphanumeric()) {
        return None;
    }

    Some(trimmed.to_lowercase())
}

impl Database {
    /// A lookup that many paths can share: it reads the changes once and keeps the blocks of
    /// the name file it decodes.
    pub fn path_lookup(&self) -> Result<PathLookup> {
        let (snapshot_id, mut pending) = self.read_search_state()?;
        let file = self.name_file_with_id(snapshot_id)?;
        let live = std::mem::take(&mut pending.live)
            .into_iter()
            .map(|result| (result.path.clone(), result))
            .collect();
        Ok(PathLookup {
            file,
            cache: LookupCache::default(),
            pending,
            live,
            verify_paths: self.verify_paths_on_search,
        })
    }

    /// The active row for `path`, hydrated from disk like search results. `None` when the path
    /// is not indexed or the file no longer exists. One-off form of `PathLookup::result_for`.
    pub fn result_for_path(&self, path: &str) -> Result<Option<SearchResult>> {
        Ok(self.path_lookup()?.result_for(path))
    }

    fn hydrate_search_results(&self, results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
        let mut hydrated = Vec::with_capacity(results.len());
        for result in results {
            if let Some(result) = self.hydrate_search_result(result)? {
                hydrated.push(result);
            }
        }
        Ok(hydrated)
    }

    fn hydrate_search_result(&self, mut result: SearchResult) -> Result<Option<SearchResult>> {
        let keep = hydrate_in_place(&mut result, self.verify_paths_on_search);
        Ok(keep.then_some(result))
    }
}

/// Threads to use for work that scales with the machine: one per available core.
fn worker_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |count| count.get())
}

/// Answers "is this path indexed" for many paths against one view of the index. Make one
/// with `Database::path_lookup`.
pub struct PathLookup {
    file: Arc<NameFile>,
    cache: LookupCache,
    pending: Pending,
    /// The live changes by path.
    live: HashMap<String, SearchResult>,
    verify_paths: bool,
}

impl PathLookup {
    /// The active row for `path`, hydrated from disk like a search result. `None` when the
    /// path is not indexed, or when its file is gone and the database verifies paths.
    pub fn result_for(&mut self, path: &str) -> Option<SearchResult> {
        let mut result = match self.live.get(path) {
            Some(live) => live.clone(),
            None if self.pending.hides(path) => return None,
            None => {
                let kind = self.file.kind_of(&mut self.cache, path)?.to_string();
                let name = file_name_of(path);
                SearchResult {
                    path: path.to_string(),
                    name: name.to_string(),
                    extension: extension_of(name),
                    kind,
                    size_bytes: 0,
                    created_at: None,
                    modified_at: None,
                }
            }
        };
        hydrate_in_place(&mut result, self.verify_paths).then_some(result)
    }
}

/// The changes recorded since the name file was written (see `changes`).
#[derive(Default)]
struct Pending {
    /// Every changed path. The name file's row for one of these is out of date.
    changed: HashSet<String>,
    /// Removed paths, without a trailing separator. Everything beneath one is gone too.
    removed: HashSet<String>,
    /// The rows that exist now, as last written.
    live: Vec<SearchResult>,
}

impl Pending {
    fn add_live(&mut self, result: SearchResult) {
        self.changed.insert(result.path.clone());
        self.live.push(result);
    }

    fn add_removed(&mut self, path: String) {
        self.changed.insert(path.clone());
        self.removed
            .insert(path.trim_end_matches(MAIN_SEPARATOR).to_string());
    }

    fn len(&self) -> usize {
        self.changed.len()
    }

    fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    /// Whether the name file's row for `path` is out of date: the path itself changed, or a
    /// folder above it was removed.
    fn hides(&self, path: &str) -> bool {
        if self.changed.contains(path) {
            return true;
        }
        !self.removed.is_empty()
            && path
                .match_indices(MAIN_SEPARATOR)
                .any(|(at, _)| self.removed.contains(&path[..at]))
    }
}

/// The `snapshot_id` and the changes. It starts no transaction, so a write operation can call
/// it inside its own; `Database::read_search_state` wraps it in a read transaction. A reader
/// uses the name file on disk whatever id it carries, so a crash between replacing the file
/// and committing leaves a state readers handle: applying changes to a file that already
/// holds them gives the same active rows.
fn load_state(conn: &Connection) -> rusqlite::Result<(u64, Pending)> {
    let snapshot_id = snapshot_id_of(conn)?;
    let mut pending = Pending::default();
    let mut stmt =
        conn.prepare("SELECT path, kind, size_bytes, created_at, modified_at FROM changes")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let Some(kind) = row.get::<_, Option<String>>(1)? else {
            pending.add_removed(path);
            continue;
        };
        let name = file_name_of(&path).to_string();
        pending.add_live(SearchResult {
            extension: extension_of(&name),
            name,
            kind,
            size_bytes: row_size_bytes(row.get(2)?),
            created_at: timestamp_to_datetime(row.get(3)?),
            modified_at: timestamp_to_datetime(row.get(4)?),
            path,
        });
    }
    Ok((snapshot_id, pending))
}

/// The last component of `path`.
fn file_name_of(path: &str) -> &str {
    path.rsplit(MAIN_SEPARATOR).next().unwrap_or(path)
}

/// The registered root that owns `path`: the longest one the path equals or lies beneath.
fn owner<'a>(path: &str, roots: &'a [String]) -> Option<&'a str> {
    roots
        .iter()
        .map(String::as_str)
        .filter(|root| is_at_or_beneath(path, root))
        .max_by_key(|root| root.len())
}

/// Whether `path` is `root` or lies beneath it: it starts with `root` and the next character
/// is a separator, or `root` itself ends with one (a scan of `/`).
fn is_at_or_beneath(path: &str, root: &str) -> bool {
    match path.strip_prefix(root) {
        Some("") => true,
        Some(rest) => root.ends_with(MAIN_SEPARATOR) || rest.starts_with(MAIN_SEPARATOR),
        None => false,
    }
}

/// Whether `compiled` accepts a row. Contains looks at the name only, unless the query has a
/// path separator in it; every other mode also looks at the whole path.
fn row_matches(
    options: &SearchOptions,
    compiled: &CompiledQuery,
    scorer: &mut QueryScorer,
    name: &str,
    path: &str,
) -> bool {
    let name_only =
        options.mode == QueryMode::Contains && !options.query.trim().contains(['/', '\\']);
    if name_only {
        compiled.is_match(scorer, name)
    } else {
        compiled.matches_any(scorer, [name, path])
    }
}

/// The kind, extension and name filters. Size and date filters need the file on disk and
/// are applied later (see `needs_disk_metadata`).
fn row_passes_filters(
    filters: &SearchFilters,
    name: &str,
    kind: &str,
    extension: Option<&str>,
) -> bool {
    if filters
        .kind
        .as_ref()
        .is_some_and(|wanted| wanted.as_str() != kind)
    {
        return false;
    }
    if !filters.exts.is_empty()
        && !extension.is_some_and(|ext| filters.exts.iter().any(|wanted| wanted == ext))
    {
        return false;
    }
    filters.name.as_ref().is_none_or(|wanted| {
        name.to_ascii_lowercase()
            .contains(&wanted.to_ascii_lowercase())
    })
}

/// The extension the scanner stores for a file name.
fn extension_of(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
}

/// Whether a match can only lie inside the name, so a byte search over the names finds
/// every candidate row. `query` is trimmed and ASCII-lowercased.
fn can_search_names(options: &SearchOptions, query: &str) -> bool {
    if query.is_empty() || query.contains(['/', '\\']) {
        return false;
    }
    match options.mode {
        QueryMode::Contains | QueryMode::Exact | QueryMode::Suffix => true,
        QueryMode::Prefix => !may_start_a_path(query),
        _ => false,
    }
}

/// The byte strings to search the names for, or none to visit every row. Every
/// lowercased name ends with a 0 byte, so a needle that ends in one can only hit the end
/// of a name. The rule, in order: the query when it is 3 bytes or longer, because it is
/// usually the most selective; else the wanted extensions as `.ext` plus the 0 byte; else
/// a 1 or 2 byte query. Exact mode adds the 0 byte to the query, since a name equal to
/// the query always ends with it. A hit is only a candidate, the scan still decides.
fn needles(options: &SearchOptions, query: &str) -> Vec<Vec<u8>> {
    let query_needle = || {
        let mut needle = query.as_bytes().to_vec();
        if options.mode == QueryMode::Exact {
            needle.push(0);
        }
        vec![needle]
    };
    let searchable = can_search_names(options, query);
    if searchable && query.len() >= 3 {
        return query_needle();
    }
    if !options.filters.exts.is_empty() {
        return options
            .filters
            .exts
            .iter()
            .map(|ext| format!(".{ext}\0").into_bytes())
            .collect();
    }
    if searchable {
        return query_needle();
    }
    Vec::new()
}

/// A search over the name file, split across threads. Each worker claims one block of
/// rows at a time, so a fast core scans more than a slow one. Workers only read the mapped
/// file; they open no SQLite connection.
struct NameScan<'a> {
    file: &'a NameFile,
    options: &'a SearchOptions,
    pending: &'a Pending,
    frecency: &'a HashMap<String, u32>,
    threads: usize,
}

/// What one worker keeps while it scans: the query, its scratch space and its matches.
struct ScanSink<'s, 'a> {
    scan: &'s NameScan<'a>,
    compiled: CompiledQuery,
    scorer: QueryScorer,
    tested: usize,
    matches: Vec<IndexedSearchCandidate>,
    tx: &'s std::sync::mpsc::SyncSender<Vec<IndexedSearchCandidate>>,
}

impl NameScan<'_> {
    /// Matches sent to the consumer at a time.
    const CHUNK: usize = 1024;

    /// Runs the search and passes every match to `on_match`, in no particular order. Returns
    /// the number of rows the match test ran on, or `None` when `should_cancel` fired.
    fn run(
        &self,
        compiled: &CompiledQuery,
        scorer: &mut QueryScorer,
        should_cancel: &mut dyn FnMut() -> bool,
        on_match: &mut dyn FnMut(IndexedSearchCandidate, &mut QueryScorer) -> Result<()>,
    ) -> Result<Option<usize>> {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let next_block = std::sync::atomic::AtomicUsize::new(0);
        let query = self.options.query.trim().to_ascii_lowercase();
        let needles = needles(self.options, &query);
        let finders: Vec<memchr::memmem::Finder<'_>> =
            needles.iter().map(memchr::memmem::Finder::new).collect();
        // Bounded, so a slow consumer holds the workers back instead of buffering every match.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<IndexedSearchCandidate>>(64);

        std::thread::scope(|scope| {
            let (stop, next_block, finders) = (&stop, &next_block, finders.as_slice());
            let workers: Vec<_> = (0..self.threads.max(1))
                .map(|_| {
                    let tx = tx.clone();
                    scope.spawn(move || self.scan_blocks(next_block, compiled, finders, stop, &tx))
                })
                .collect();
            drop(tx);

            let mut outcome = Ok(());
            let mut cancelled = false;
            loop {
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(chunk) => {
                        outcome = chunk
                            .into_iter()
                            .try_for_each(|candidate| on_match(candidate, scorer));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                cancelled = should_cancel();
                if cancelled || outcome.is_err() {
                    break;
                }
            }
            // Workers blocked on a full channel wake up when the receiver goes away.
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(rx);

            let mut tested = 0;
            for worker in workers {
                let scanned = worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("search worker thread panicked"))?;
                tested += scanned;
            }
            outcome?;
            Ok((!cancelled).then_some(tested))
        })
    }

    /// Claims blocks until none are left and sends the rows that match. Returns how many rows
    /// it ran the match test on.
    fn scan_blocks(
        &self,
        next_block: &std::sync::atomic::AtomicUsize,
        compiled: &CompiledQuery,
        finders: &[memchr::memmem::Finder<'_>],
        stop: &std::sync::atomic::AtomicBool,
        tx: &std::sync::mpsc::SyncSender<Vec<IndexedSearchCandidate>>,
    ) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let mut sink = ScanSink {
            scan: self,
            compiled: compiled.clone(),
            scorer: QueryScorer::new(),
            tested: 0,
            matches: Vec::with_capacity(Self::CHUNK),
            tx,
        };
        let mut reader = self.file.reader();
        let mut names = Vec::new();
        let blocks = self.file.block_count();
        loop {
            let block = next_block.fetch_add(1, Relaxed);
            if block >= blocks || stop.load(Relaxed) {
                break;
            }
            let go_on = if finders.is_empty() {
                self.scan_all_rows(block, &mut reader, stop, &mut sink)
            } else {
                self.scan_hits(block, &mut reader, &mut names, finders, stop, &mut sink)
            };
            if !go_on {
                break;
            }
        }
        // The receiver may already be gone after a cancel; there is nobody to tell.
        let _ = tx.send(sink.matches);
        sink.tested
    }

    /// Visits every row of `block`. `false` means the scan should end.
    fn scan_all_rows(
        &self,
        block: usize,
        reader: &mut Reader<'_>,
        stop: &std::sync::atomic::AtomicBool,
        sink: &mut ScanSink<'_, '_>,
    ) -> bool {
        let first = block.saturating_mul(BLOCK_ROWS);
        let last = first.saturating_add(BLOCK_ROWS).min(self.file.len());
        let mut go_on = true;
        reader.for_each(first..last, &mut |row| {
            go_on = !stop.load(std::sync::atomic::Ordering::Relaxed) && sink.visit(&row);
            go_on
        });
        go_on
    }

    /// Visits the rows of `block` where a finder hits the names. `false` means the scan
    /// should end.
    fn scan_hits(
        &self,
        block: usize,
        reader: &mut Reader<'_>,
        scratch: &mut Vec<u8>,
        finders: &[memchr::memmem::Finder<'_>],
        stop: &std::sync::atomic::AtomicBool,
        sink: &mut ScanSink<'_, '_>,
    ) -> bool {
        let Some(names) = reader.names(block, scratch) else {
            return true;
        };
        for finder in finders {
            // `index` is the number of 0 bytes before `position`, so it is the row of the
            // name that starts there.
            let (mut position, mut index) = (0, 0);
            while let Some(hit) = names
                .get(position..)
                .and_then(|rest| finder.find(rest))
                .map(|found| position + found)
            {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return false;
                }
                let Some(skipped) = names.get(position..hit) else {
                    break;
                };
                index += memchr::memchr_iter(0, skipped).count();
                let Some(head) = names.get(..hit) else {
                    break;
                };
                let start = memchr::memrchr(0, head).map_or(0, |zero| zero + 1);
                let Some(end) = names
                    .get(hit..)
                    .and_then(|tail| memchr::memchr(0, tail))
                    .map(|zero| hit + zero)
                else {
                    break;
                };
                let Some(lower) = names.get(start..end) else {
                    break;
                };
                if let Some(row) = reader.row(block, index, lower, start) {
                    if !sink.visit(&row) {
                        return false;
                    }
                }
                // The rest of this row cannot match again; go on at the next row.
                position = end + 1;
                index += 1;
            }
        }
        true
    }
}

impl ScanSink<'_, '_> {
    /// Tests one row and queues it when it matches. `false` means the receiver is gone.
    fn visit(&mut self, row: &Row<'_>) -> bool {
        let scan = self.scan;
        let filters = &scan.options.filters;
        let extension = if filters.exts.is_empty() {
            None
        } else {
            extension_of(row.name)
        };
        if !row_passes_filters(filters, row.name, row.kind, extension.as_deref()) {
            return true;
        }
        self.tested += 1;
        if !row_matches(
            scan.options,
            &self.compiled,
            &mut self.scorer,
            row.name,
            row.path,
        ) || scan.pending.hides(row.path)
        {
            return true;
        }
        self.matches.push(IndexedSearchCandidate {
            frecency: scan.frecency.get(row.path).copied().unwrap_or(0),
            result: SearchResult {
                path: row.path.to_string(),
                name: row.name.to_string(),
                extension: extension_of(row.name),
                kind: row.kind.to_string(),
                size_bytes: 0,
                created_at: None,
                modified_at: None,
            },
        });
        if self.matches.len() < NameScan::CHUNK {
            return true;
        }
        self.tx
            .send(std::mem::replace(
                &mut self.matches,
                Vec::with_capacity(NameScan::CHUNK),
            ))
            .is_ok()
    }
}

/// Replaces a row's indexed size, dates and kind with what the file has on disk now. Returns
/// false when the row should be dropped: the file is gone and `verify_paths` is set.
fn hydrate_in_place(result: &mut SearchResult, verify_paths: bool) -> bool {
    let metadata = match fs::symlink_metadata(&result.path) {
        Ok(metadata) => metadata,
        // Gone from disk: a removed file, or a parent folder that is now a file.
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return !verify_paths;
        }
        // Present but not inspectable (permission denied, I/O error, stale network
        // handle). Keep the indexed row unhydrated rather than failing the whole
        // search over one path.
        Err(_) => return true,
    };

    result.size_bytes = metadata.len();
    result.created_at = metadata.created().ok().map(DateTime::<Utc>::from);
    result.modified_at = metadata.modified().ok().map(DateTime::<Utc>::from);
    if metadata.is_dir() {
        result.kind = "folder".to_string();
    }
    true
}

/// A size or date filter, or a sort by size or date, needs each file's real metadata. A scan
/// stores names only (size 0 and no timestamps), so the index cannot answer these and the
/// rows have to be read from disk first.
fn needs_disk_metadata(options: &SearchOptions) -> bool {
    options.filters.has_metadata_bounds()
        || matches!(
            options.sort,
            SortField::Size | SortField::Created | SortField::Modified
        )
}

fn metadata_bounds_match(result: &SearchResult, filters: &SearchFilters) -> bool {
    filters.metadata_bounds_match(result.size_bytes, result.created_at, result.modified_at)
}

fn search_candidate_limit(verify_paths_on_search: bool, limit: usize) -> usize {
    if verify_paths_on_search {
        limit.saturating_mul(20).max(200)
    } else {
        limit
    }
}

fn timestamp_to_datetime(value: Option<i64>) -> Option<DateTime<Utc>> {
    value.and_then(|ts| DateTime::from_timestamp(ts, 0))
}

fn sqlite_size_bytes(size_bytes: u64) -> i64 {
    i64::try_from(size_bytes).unwrap_or(i64::MAX)
}

fn row_size_bytes(size_bytes: i64) -> u64 {
    u64::try_from(size_bytes).unwrap_or(0)
}

/// Whether a lowercased query could be the start of a stored absolute path: a leading
/// separator anywhere, or a drive letter (`c`, `c:\...`) on Windows.
fn may_start_a_path(query: &str) -> bool {
    let bytes = query.as_bytes();
    match bytes.first() {
        Some(b'/' | b'\\') => true,
        Some(first) => {
            cfg!(windows)
                && first.is_ascii_alphabetic()
                && bytes.get(1).is_none_or(|second| *second == b':')
        }
        None => false,
    }
}

/// Sort with an already-compiled query so Relevance can rank by nucleo score
/// (Fuzzy) or tier (other modes) and blend in frecency. `frecency` maps a path
/// to its recency-weighted open count; pass an empty map to disable the boost.
/// Non-Relevance sort fields are honoured verbatim via [`sort_results`].
pub(crate) fn sort_results_compiled(
    results: &mut Vec<SearchResult>,
    options: &SearchOptions,
    compiled: &CompiledQuery,
    scorer: &mut QueryScorer,
    frecency: &HashMap<String, u32>,
) {
    if options.sort != SortField::Relevance {
        sort_results(results, options);
        return;
    }

    let query = options.query.trim().to_ascii_lowercase();
    let mut scored = std::mem::take(results)
        .into_iter()
        .map(|result| {
            let boost = frecency.get(&result.path).copied().unwrap_or(0);
            // Fuzzy: blend nucleo score + frecency. Other modes: tier (lower is
            // better) with frecency lifting ties.
            let key = if options.mode == QueryMode::Fuzzy {
                let base = compiled
                    .fuzzy_rank(scorer, [result.name.as_str(), result.path.as_str()])
                    .unwrap_or(0);
                RelevanceKey::Fuzzy(base.saturating_add(boost))
            } else {
                RelevanceKey::Tier(relevance_score(&result, &query), boost)
            };
            (key, result)
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.modified_at.cmp(&left.1.modified_at))
            .then_with(|| cmp_ascii_lowercase(&left.1.name, &right.1.name))
            .then_with(|| left.1.path.cmp(&right.1.path))
    });
    *results = scored.into_iter().map(|(_, result)| result).collect();
    if options.reverse {
        results.reverse();
    }
}

/// Ordering key for relevance sort. `Ord` is defined so that "better" sorts
/// first (ascending): higher fuzzy score first, lower tier first, then higher
/// frecency first within a tier.
#[derive(PartialEq, Eq)]
enum RelevanceKey {
    Fuzzy(u32),
    Tier(u8, u32),
}

impl Ord for RelevanceKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            // Higher blended score is better, so reverse the comparison.
            (Self::Fuzzy(a), Self::Fuzzy(b)) => b.cmp(a),
            // Lower tier is better; within a tier, higher frecency is better.
            (Self::Tier(ta, fa), Self::Tier(tb, fb)) => ta.cmp(tb).then_with(|| fb.cmp(fa)),
            // Mixed modes never compare in practice (a single search is one
            // mode); define a stable fallback.
            (Self::Fuzzy(_), Self::Tier(..)) => Ordering::Less,
            (Self::Tier(..), Self::Fuzzy(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for RelevanceKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Sorts by `options.sort`. Every field ends in the same tie-breaks (name, then path), so the
/// order is total and does not depend on the order the rows arrived in. `reverse` reverses
/// the whole order, tie-breaks included.
pub(crate) fn sort_results(results: &mut [SearchResult], options: &SearchOptions) {
    match options.sort {
        SortField::Relevance => {
            let query = options.query.trim().to_ascii_lowercase();
            results.sort_by(|left, right| {
                relevance_score(left, &query)
                    .cmp(&relevance_score(right, &query))
                    .then_with(|| right.modified_at.cmp(&left.modified_at))
                    .then_with(|| cmp_ascii_lowercase(&left.name, &right.name))
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
        field => results.sort_by(|left, right| {
            let by_field = match field {
                SortField::Path => cmp_ascii_lowercase(&left.path, &right.path),
                SortField::Kind => cmp_ascii_lowercase(&left.kind, &right.kind),
                SortField::Size => left.size_bytes.cmp(&right.size_bytes),
                SortField::Created => left.created_at.cmp(&right.created_at),
                SortField::Modified => left.modified_at.cmp(&right.modified_at),
                SortField::Name | SortField::Relevance => std::cmp::Ordering::Equal,
            };
            by_field
                .then_with(|| cmp_ascii_lowercase(&left.name, &right.name))
                .then_with(|| left.path.cmp(&right.path))
        }),
    }
    if options.reverse {
        results.reverse();
    }
}

/// Orders two strings as their ASCII-lowercased forms would compare, without allocating.
fn cmp_ascii_lowercase(left: &str, right: &str) -> std::cmp::Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

/// Per-stage search timing, emitted to stderr only when `LCTR_TIMING` is set.
/// Zero overhead (no `Instant::now`) when disabled.
struct SearchTiming {
    start: Option<std::time::Instant>,
    last: std::cell::Cell<Option<std::time::Instant>>,
}

impl SearchTiming {
    fn start() -> Self {
        if std::env::var_os("LCTR_TIMING").is_some() {
            let now = std::time::Instant::now();
            Self {
                start: Some(now),
                last: std::cell::Cell::new(Some(now)),
            }
        } else {
            Self {
                start: None,
                last: std::cell::Cell::new(None),
            }
        }
    }

    fn lap(&self, label: &str) {
        if let Some(last) = self.last.get() {
            let now = std::time::Instant::now();
            eprintln!("[timing] {label:<16} {:>8.2} ms", elapsed_ms(last, now));
            self.last.set(Some(now));
        }
    }

    fn finish(&self, candidates: usize, matched: usize) {
        if let Some(start) = self.start {
            let now = std::time::Instant::now();
            eprintln!(
                "[timing] {:<16} {:>8.2} ms  (candidates={candidates}, matched={matched})",
                "TOTAL",
                elapsed_ms(start, now)
            );
        }
    }
}

fn elapsed_ms(from: std::time::Instant, to: std::time::Instant) -> f64 {
    to.duration_since(from).as_secs_f64() * 1000.0
}

fn frecency_weight(open_count: u32, last_opened_at: Option<i64>, now: i64) -> u32 {
    if open_count == 0 {
        return 0;
    }
    const DAY: i64 = 86_400;
    let multiplier = match last_opened_at {
        Some(ts) => {
            let age = now - ts;
            if age < DAY {
                8
            } else if age < 7 * DAY {
                4
            } else if age < 30 * DAY {
                2
            } else {
                1
            }
        }
        None => 1,
    };
    open_count.saturating_mul(multiplier)
}

/// Rank tier for a lowercased query: exact name, name prefix, name substring, path
/// substring, anything else. Runs once per row per sort, so it does not allocate.
fn relevance_score(result: &SearchResult, query: &str) -> u8 {
    let name = result.name.as_bytes();
    let query = query.as_bytes();
    if name.eq_ignore_ascii_case(query) {
        0
    } else if name.len() >= query.len() && name[..query.len()].eq_ignore_ascii_case(query) {
        1
    } else if contains_ignoring_ascii_case(name, query) {
        2
    } else if contains_ignoring_ascii_case(result.path.as_bytes(), query) {
        3
    } else {
        4
    }
}

fn contains_ignoring_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tempfile::tempdir;

    use super::*;
    use crate::name_file::NameFileBuilder;
    use crate::query::SortField;

    #[test]
    fn streaming_search_emits_every_match_in_500_item_batches() {
        let dir = tempdir().expect("create temp directory");
        let db = Database::open_in_memory().expect("open database");
        let mut records = Vec::with_capacity(5_501);

        for index in 0..5_501 {
            let name = format!("needle-{index:05}.txt");
            let path = dir.path().join(&name);
            std::fs::write(&path, b"match").expect("write match");
            records.push(FileRecord {
                path: path.to_string_lossy().to_string(),
                name,
                parent: dir.path().to_string_lossy().to_string(),
                extension: Some("txt".to_string()),
                root: dir.path().to_string_lossy().to_string(),
                volume: "local".to_string(),
                kind: "text".to_string(),
                size_bytes: 5,
                created_at: None,
                modified_at: None,
            });
        }
        db.upsert_files(&records).expect("insert matches");

        let options = SearchOptions::new("needle").with_sort(SortField::Name);
        let mut batch_sizes = Vec::new();
        let mut paths = Vec::new();
        let status = db
            .search_streaming_with_options(
                &options,
                || false,
                |batch| {
                    batch_sizes.push(batch.results.len());
                    paths.extend(batch.results.into_iter().map(|result| result.path));
                    Ok(())
                },
            )
            .expect("stream indexed matches");

        let mut expected_sizes = vec![SEARCH_BATCH_SIZE; 11];
        expected_sizes.push(1);
        assert_eq!(batch_sizes, expected_sizes);
        assert_eq!(status, SearchStreamStatus::Complete { count: 5_501 });
        assert_eq!(paths.len(), 5_501);
        assert_eq!(paths.iter().collect::<HashSet<_>>().len(), 5_501);
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// Installs `paths` as the rows of a finished scan of `root`, the way a scan stores them:
    /// the path and the kind, no size and no dates.
    fn install_rows(db: &Database, root: &std::path::Path, paths: &[std::path::PathBuf]) {
        let mut rows = NameFileBuilder::default();
        for path in paths {
            rows.push(&path.to_string_lossy(), "text")
                .expect("push scanned row");
        }
        db.install_scan(&root.to_string_lossy(), rows, 1)
            .expect("install scan");
    }

    fn stream_all(db: &Database, options: &SearchOptions) -> Vec<SearchResult> {
        let mut results = Vec::new();
        db.search_streaming_with_options(
            options,
            || false,
            |batch| {
                results.extend(batch.results);
                Ok(())
            },
        )
        .expect("stream search");
        results
    }

    /// The TUI path, on rows stored the way a scan stores them (size 0, no timestamps).
    #[test]
    fn streaming_search_sorts_and_filters_by_real_size_for_scanned_rows() {
        let dir = tempdir().expect("create temp directory");
        let db = Database::open_in_memory().expect("open database");
        let mut paths = Vec::new();
        // Name order (a, b, c) differs from size order.
        for (name, size) in [("a-note.txt", 30), ("b-note.txt", 10), ("c-note.txt", 20)] {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![b'x'; size]).expect("write file");
            paths.push(path);
        }
        install_rows(&db, dir.path(), &paths);

        let by_size = stream_all(
            &db,
            &SearchOptions::new("note")
                .with_sort(SortField::Size)
                .with_limit(usize::MAX),
        );
        let sizes: Vec<u64> = by_size.iter().map(|result| result.size_bytes).collect();
        assert_eq!(sizes, [10, 20, 30]);

        let at_least_20 = stream_all(
            &db,
            &SearchOptions::new("note")
                .with_min_size("20b")
                .expect("size parses")
                .with_limit(usize::MAX),
        );
        let mut names: Vec<&str> = at_least_20
            .iter()
            .map(|result| result.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["a-note.txt", "c-note.txt"]);
    }

    /// `lctr find` and the TUI share one matching and ranking path, so the same options
    /// give the same rows in the same order.
    #[test]
    fn find_and_streaming_search_return_the_same_order() {
        let db = Database::open_in_memory().expect("open database");
        let root = std::path::Path::new("/same-order");
        let paths: Vec<std::path::PathBuf> = [
            "report",
            "Report.txt",
            "annual-report.txt",
            "report-2024.txt",
            "notes/report.txt",
            "main.rs",
            "main.rs.bak",
            "lib.rs",
        ]
        .iter()
        .map(|name| root.join(name))
        .collect();
        install_rows(&db, root, &paths);

        for (mode, query) in [
            (QueryMode::Contains, "report"),
            (QueryMode::Exact, "report"),
            (QueryMode::Prefix, "rep"),
            (QueryMode::Suffix, ".rs"),
            (QueryMode::Fuzzy, "rprt"),
            (QueryMode::Regex, r"\.rs$"),
            (QueryMode::Glob, "*.txt"),
        ] {
            for sort in [SortField::Relevance, SortField::Name, SortField::Path] {
                let options = SearchOptions::new(query)
                    .with_mode(mode)
                    .with_sort(sort)
                    .with_limit(usize::MAX);
                let found = db.search_with_options(&options).expect("find");
                let streamed = stream_all(&db, &options);
                assert!(!found.is_empty(), "{mode:?} {query:?} matches nothing");
                assert_eq!(found, streamed, "{mode:?} {query:?} sorted by {sort:?}");
            }
        }
    }

    fn push_all(rows: &mut NameFileBuilder, paths: &[&str]) {
        for path in paths {
            rows.push(path, "text").expect("push row");
        }
    }

    fn record_in(root: &str, path: &str) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            name: file_name_of(path).to_string(),
            parent: root.to_string(),
            extension: None,
            root: root.to_string(),
            volume: "local".to_string(),
            kind: "text".to_string(),
            size_bytes: 3,
            created_at: None,
            modified_at: None,
        }
    }

    fn found_paths(db: &Database) -> Vec<String> {
        let options = SearchOptions::new("")
            .with_mode(QueryMode::Contains)
            .with_sort(SortField::Path)
            .with_limit(usize::MAX);
        let mut paths: Vec<String> = db
            .search_with_options(&options)
            .expect("search")
            .into_iter()
            .map(|result| result.path)
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn changes_hide_and_add_rows_and_folding_keeps_the_active_set() {
        let sep = MAIN_SEPARATOR;
        let dir = tempdir().expect("temp dir");
        let db = Database::open(dir.path().join("index.db")).expect("open database");
        let root = format!("{sep}r");
        let path = |name: &str| format!("{root}{sep}{name}");
        let mut rows = NameFileBuilder::default();
        push_all(
            &mut rows,
            &[
                &path("a"),
                &path(&format!("dir{sep}x")),
                &path(&format!("dir{sep}y")),
            ],
        );
        db.install_scan(&root, rows, 1).expect("install scan");
        assert_eq!(db.count_active().expect("count"), 3);

        db.upsert_file(&record_in(&root, &path("new")))
            .expect("upsert");
        assert_eq!(db.count_active().expect("count"), 4);
        let dir_path = path("dir");
        assert_eq!(db.mark_path_deleted(&dir_path).expect("delete dir"), 2);
        assert_eq!(
            db.mark_path_deleted(&path("absent")).expect("delete none"),
            0
        );
        let before = found_paths(&db);
        assert_eq!(before, [path("a"), path("new")]);
        assert_eq!(db.count_active().expect("count"), 2);

        db.compact_name_file().expect("compact");
        let changes: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
            .expect("count changes");
        assert_eq!(changes, 0);
        assert_eq!(found_paths(&db), before);
        assert_eq!(db.count_active().expect("count"), 2);

        let reopened = Database::open(dir.path().join("index.db")).expect("reopen");
        assert_eq!(found_paths(&reopened), before);
    }

    #[test]
    fn install_scan_keeps_other_roots_and_newer_changes() {
        let sep = MAIN_SEPARATOR;
        let dir = tempdir().expect("temp dir");
        let db = Database::open(dir.path().join("index.db")).expect("open database");
        let (one, two) = (format!("{sep}one"), format!("{sep}two"));
        let mut rows = NameFileBuilder::default();
        push_all(
            &mut rows,
            &[&format!("{one}{sep}a"), &format!("{one}{sep}b")],
        );
        db.install_scan(&one, rows, 1).expect("install one");
        let mut rows = NameFileBuilder::default();
        push_all(&mut rows, &[&format!("{two}{sep}c")]);
        db.install_scan(&two, rows, 1).expect("install two");

        // A change written after the scan token survives; one before it is covered.
        db.upsert_files_with_indexed_at(&[record_in(&one, &format!("{one}{sep}old"))], 5)
            .expect("old change");
        db.upsert_files_with_indexed_at(&[record_in(&one, &format!("{one}{sep}late"))], 50)
            .expect("late change");
        let mut rows = NameFileBuilder::default();
        push_all(&mut rows, &[&format!("{one}{sep}a")]);
        db.install_scan(&one, rows, 10).expect("reinstall one");

        assert_eq!(
            found_paths(&db),
            [
                format!("{one}{sep}a"),
                format!("{one}{sep}late"),
                format!("{two}{sep}c"),
            ]
        );
        assert_eq!(
            db.scan_completion_for_root(&one).expect("completion"),
            ScanCompletion::Complete
        );
        assert_eq!(db.roots().expect("roots"), [one.clone(), two.clone()]);
        assert_eq!(db.remove_root(&two).expect("remove"), 1);
        assert_eq!(db.roots().expect("roots"), [one]);
    }

    #[test]
    fn an_old_index_in_the_opened_file_is_converted() {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("legacy.sqlite");
        {
            let conn = Connection::open(&db_path).expect("open raw db");
            conn.execute_batch(
                "CREATE TABLE files (
                    id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
                    name_lower TEXT, parent TEXT NOT NULL, extension TEXT, root TEXT NOT NULL,
                    volume TEXT NOT NULL, kind TEXT NOT NULL, size_bytes INTEGER NOT NULL,
                    created_at INTEGER, modified_at INTEGER, indexed_at INTEGER NOT NULL,
                    deleted INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE VIRTUAL TABLE files_fts
                 USING fts5(name, content='files', content_rowid='id', tokenize='trigram');
                 CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
                    INSERT INTO files_fts(rowid, name) VALUES (new.id, new.name);
                 END;
                 CREATE TABLE pending_changes (path TEXT PRIMARY KEY, subtree INTEGER NOT NULL);
                 CREATE TABLE scan_roots (
                    root TEXT PRIMARY KEY, scan_token INTEGER NOT NULL, complete INTEGER NOT NULL,
                    started_at INTEGER NOT NULL, completed_at INTEGER
                 );
                 INSERT INTO scan_roots VALUES ('/legacy', 7, 1, 7, 8);
                 INSERT INTO files (path, name, parent, root, volume, kind, size_bytes, indexed_at)
                 VALUES ('/legacy/live.pdf', 'live.pdf', '/legacy', '/legacy', 'local', 'pdf', 1, 1);
                 INSERT INTO files (path, name, parent, root, volume, kind, size_bytes, indexed_at, deleted)
                 VALUES ('/legacy/gone.pdf', 'gone.pdf', '/legacy', '/legacy', 'local', 'pdf', 1, 1, 1);
                 INSERT INTO files (path, name, parent, root, volume, kind, size_bytes, indexed_at)
                 VALUES ('/other/seen.txt', 'seen.txt', '/other', '/other', 'local', 'text', 1, 1);",
            )
            .expect("create legacy schema");
        }

        let db = Database::open(&db_path).expect("convert");
        assert_eq!(
            found_paths(&db),
            [
                "/legacy/live.pdf".to_string(),
                "/other/seen.txt".to_string()
            ]
        );
        assert_eq!(db.roots().expect("roots"), ["/legacy", "/other"]);
        assert_eq!(
            db.scan_completion_for_root("/legacy").expect("completion"),
            ScanCompletion::Complete
        );
        assert_eq!(
            db.scan_completion_for_root("/other").expect("completion"),
            ScanCompletion::Incomplete
        );
        assert!(!table_exists(&db.conn, "files").expect("table check"));
        assert!(!table_exists(&db.conn, "pending_changes").expect("table check"));
    }

    #[test]
    fn owner_is_the_longest_root_the_path_equals_or_lies_beneath() {
        let sep = MAIN_SEPARATOR;
        let roots = vec![
            format!("{sep}a"),
            format!("{sep}a{sep}b"),
            format!("{sep}ab"),
        ];
        let owner_of = |path: String| owner(&path, &roots).map(str::to_string);

        assert_eq!(owner_of(format!("{sep}a")), Some(format!("{sep}a")));
        assert_eq!(owner_of(format!("{sep}a{sep}x")), Some(format!("{sep}a")));
        assert_eq!(
            owner_of(format!("{sep}a{sep}b{sep}c")),
            Some(format!("{sep}a{sep}b"))
        );
        assert_eq!(owner_of(format!("{sep}ab{sep}c")), Some(format!("{sep}ab")));
        assert_eq!(owner_of(format!("{sep}abc")), None);
        assert_eq!(owner_of(format!("{sep}elsewhere")), None);
        let everything = vec![sep.to_string()];
        assert_eq!(
            owner(&format!("{sep}x{sep}y"), &everything),
            Some(sep.to_string().as_str())
        );
    }
}

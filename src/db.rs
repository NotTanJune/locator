use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::time::{Duration, Instant};

use crate::query::{
    CompiledQuery, QueryMode, QueryScorer, SearchFilters, SearchOptions, SortField,
};

pub const LOCAL_INDEX_DIR: &str = ".locator";
pub const INDEX_FILE_NAME: &str = "index.sqlite";

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

pub struct Database {
    conn: Connection,
    path: Option<PathBuf>,
    verify_paths_on_search: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BulkFinishProfile {
    pub fts_rebuild: Duration,
    pub index_rebuild: Duration,
    pub trigger_recreate: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCompletion {
    Complete,
    Incomplete,
    Unknown,
}

impl Database {
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
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        let db = Self {
            conn,
            path: Some(path.to_path_buf()),
            verify_paths_on_search: false,
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open read-only database {}", path.display()))?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            verify_paths_on_search: false,
        })
    }

    pub fn open_fresh_staged_scan(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        let db = Self {
            conn,
            path: Some(path.to_path_buf()),
            verify_paths_on_search: false,
        };
        db.configure_fast_staging_connection()?;
        db.create_fresh_scan_schema()?;
        Ok(db)
    }

    pub(crate) fn open_existing_without_migrate(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -200000;",
        )?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            verify_paths_on_search: false,
        })
    }

    pub(crate) fn open_existing_for_staged_writer(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        let db = Self {
            conn,
            path: Some(path.to_path_buf()),
            verify_paths_on_search: false,
        };
        db.configure_fast_staging_connection()?;
        Ok(db)
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
        let db = Self {
            conn: Connection::open_in_memory().context("open in-memory database")?,
            path: None,
            verify_paths_on_search: false,
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn with_search_path_verification(mut self) -> Self {
        self.verify_paths_on_search = true;
        self
    }

    pub fn upsert_file(&self, record: &FileRecord) -> Result<()> {
        self.upsert_files(std::slice::from_ref(record))
    }

    pub fn upsert_files(&self, records: &[FileRecord]) -> Result<()> {
        self.upsert_files_with_indexed_at(records, Utc::now().timestamp_millis())
    }

    pub fn upsert_files_with_indexed_at(
        &self,
        records: &[FileRecord],
        indexed_at: i64,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        {
            let mut upsert_file = tx.prepare(
                "INSERT INTO files (
                    path, name, name_lower, parent, extension, root, volume, kind, size_bytes,
                    created_at, modified_at, indexed_at, deleted
                ) VALUES (?1, ?2, lower(?2), ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)
                ON CONFLICT(path) DO UPDATE SET
                    name = excluded.name,
                    name_lower = excluded.name_lower,
                    parent = excluded.parent,
                    extension = excluded.extension,
                    root = excluded.root,
                    volume = excluded.volume,
                    kind = excluded.kind,
                    size_bytes = excluded.size_bytes,
                    created_at = excluded.created_at,
                    modified_at = excluded.modified_at,
                    indexed_at = excluded.indexed_at,
                    deleted = 0",
            )?;

            for record in records {
                let path = record.path.as_str();
                upsert_file.execute(params![
                    path,
                    record.name,
                    record.parent,
                    record.extension,
                    record.root,
                    record.volume,
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

    pub fn upsert_light_files_with_indexed_at(
        &self,
        records: &[FileRecord],
        indexed_at: i64,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        {
            let mut upsert_file = tx.prepare(
                "INSERT INTO files (
                    path, name, name_lower, parent, extension, root, volume, kind, size_bytes,
                    created_at, modified_at, indexed_at, deleted
                ) VALUES (?1, ?2, lower(?2), ?3, ?4, ?5, ?6, ?7, 0, NULL, NULL, ?8, 0)
                ON CONFLICT(path) DO UPDATE SET
                    name = excluded.name,
                    name_lower = excluded.name_lower,
                    parent = excluded.parent,
                    extension = excluded.extension,
                    root = excluded.root,
                    volume = excluded.volume,
                    kind = excluded.kind,
                    size_bytes = 0,
                    created_at = NULL,
                    modified_at = NULL,
                    indexed_at = excluded.indexed_at,
                    deleted = 0",
            )?;

            for record in records {
                upsert_file.execute(params![
                    record.path,
                    record.name,
                    record.parent,
                    record.extension,
                    record.root,
                    record.volume,
                    record.kind,
                    indexed_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_light_files_with_indexed_at(
        &self,
        records: &[FileRecord],
        indexed_at: i64,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert_file = tx.prepare(
                "INSERT INTO files (
                    path, name, name_lower, parent, extension, root, volume, kind, size_bytes,
                    created_at, modified_at, indexed_at, deleted
                ) VALUES (?1, ?2, lower(?2), ?3, ?4, ?5, ?6, ?7, 0, NULL, NULL, ?8, 0)",
            )?;

            for record in records {
                insert_file.execute(params![
                    record.path,
                    record.name,
                    record.parent,
                    record.extension,
                    record.root,
                    record.volume,
                    record.kind,
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

    pub fn mark_scan_started(&self, root: &str, scan_token: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO scan_roots (root, scan_token, complete, started_at, completed_at)
             VALUES (?1, ?2, 0, ?2, NULL)
             ON CONFLICT(root) DO UPDATE SET
                scan_token = excluded.scan_token,
                complete = 0,
                started_at = excluded.started_at,
                completed_at = NULL",
            params![root, scan_token],
        )?;
        Ok(())
    }

    pub fn begin_bulk_scan(&self) -> Result<()> {
        self.drop_fts_triggers()
    }

    pub fn finish_bulk_scan(&self) -> Result<()> {
        self.finish_bulk_scan_profile().map(|_| ())
    }

    pub fn finish_bulk_scan_profile(&self) -> Result<BulkFinishProfile> {
        let fts_start = Instant::now();
        self.rebuild_fts()?;
        let fts_rebuild = fts_start.elapsed();

        let trigger_start = Instant::now();
        self.create_fts_triggers()?;
        let trigger_recreate = trigger_start.elapsed();

        Ok(BulkFinishProfile {
            fts_rebuild,
            index_rebuild: Duration::ZERO,
            trigger_recreate,
        })
    }

    pub fn finish_fresh_staged_scan_profile(&self) -> Result<BulkFinishProfile> {
        let index_start = Instant::now();
        self.create_fresh_scan_indexes()?;
        let index_rebuild = index_start.elapsed();

        let fts_start = Instant::now();
        self.recreate_fts_if_needed()?;
        let fts_rebuild = fts_start.elapsed();

        Ok(BulkFinishProfile {
            fts_rebuild,
            index_rebuild,
            trigger_recreate: Duration::ZERO,
        })
    }

    pub fn mark_scan_completed(&self, root: &str, scan_token: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE scan_roots
             SET complete = 1, completed_at = ?2
             WHERE root = ?1 AND scan_token = ?2",
            params![root, scan_token],
        )?;
        Ok(())
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

    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let mut sql = String::from(
            "SELECT f.path, f.name, f.extension, f.kind, f.size_bytes, f.created_at, f.modified_at
             FROM files f",
        );
        let trimmed = query.trim();
        let fts_query = sanitize_fts_query(query);
        // Trigram needs >=3-char tokens. For shorter non-empty queries fall back
        // to a name substring LIKE so single/double-char searches still work.
        let short_like = fts_query.is_none() && !trimmed.is_empty();
        let used_fts = fts_query.is_some();
        if fts_query.is_some() {
            sql.push_str(" JOIN files_fts ON files_fts.rowid = f.id");
        }
        sql.push_str(" WHERE f.deleted = 0");

        let mut values = Vec::<rusqlite::types::Value>::new();
        if let Some(fts) = fts_query {
            sql.push_str(" AND files_fts MATCH ?");
            values.push(fts.into());
        } else if short_like {
            sql.push_str(" AND f.name_lower LIKE ?");
            values.push(format!("%{}%", trimmed.to_lowercase()).into());
        }
        if let Some(kind) = &filters.kind {
            sql.push_str(" AND f.kind = ?");
            values.push(kind.as_str().to_string().into());
        }
        if !filters.exts.is_empty() {
            sql.push_str(" AND f.extension IN (");
            sql.push_str(
                &std::iter::repeat_n("?", filters.exts.len())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            sql.push(')');
            for ext in &filters.exts {
                values.push(ext.clone().into());
            }
        }
        if let Some(min_size) = filters.min_size {
            sql.push_str(" AND f.size_bytes >= ?");
            values.push(sqlite_size_bytes(min_size).into());
        }
        if let Some(max_size) = filters.max_size {
            sql.push_str(" AND f.size_bytes <= ?");
            values.push(sqlite_size_bytes(max_size).into());
        }
        if let Some(created_after) = filters.created_after {
            sql.push_str(" AND f.created_at >= ?");
            values.push(created_after.timestamp().into());
        }
        if let Some(created_before) = filters.created_before {
            sql.push_str(" AND f.created_at <= ?");
            values.push(created_before.timestamp().into());
        }
        if let Some(modified_after) = filters.modified_after {
            sql.push_str(" AND f.modified_at >= ?");
            values.push(modified_after.timestamp().into());
        }
        if let Some(modified_before) = filters.modified_before {
            sql.push_str(" AND f.modified_at <= ?");
            values.push(modified_before.timestamp().into());
        }
        if let Some(name) = &filters.name {
            sql.push_str(" AND f.name LIKE ?");
            values.push(format!("%{name}%").into());
        }

        sql.push_str(" ORDER BY f.modified_at DESC NULLS LAST, f.name ASC LIMIT ?");
        values.push((search_candidate_limit(self.verify_paths_on_search, limit) as i64).into());

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), search_result_from_row)?;

        let mut rows: Vec<_> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect search results")?;

        // Under detail=none the trigram AND query can produce false positives
        // (all trigrams present but non-contiguous). Post-filter to retain only
        // rows where every alphanumeric run from the query appears as a substring.
        if used_fts {
            let needle = trimmed.to_lowercase();
            let runs: Vec<String> = needle
                .split(|ch: char| !ch.is_alphanumeric())
                .filter(|t| t.chars().count() >= 3)
                .map(str::to_string)
                .collect();
            rows.retain(|r| {
                let name = r.name.to_lowercase();
                let path = r.path.to_lowercase();
                runs.iter()
                    .all(|run| name.contains(run) || path.contains(run))
            });
        }

        let mut results = self.hydrate_search_results(rows)?;
        results.truncate(limit);
        Ok(results)
    }

    pub fn search_with_options(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        options.validate()?;
        if options.limit == 0 {
            return Ok(Vec::new());
        }

        let mut sql = String::from(
            "SELECT f.path, f.name, f.extension, f.kind, f.size_bytes, f.created_at, f.modified_at
             FROM files f
             WHERE f.deleted = 0",
        );
        let mut values = Vec::<rusqlite::types::Value>::new();
        apply_query_mode_sql(&mut sql, &mut values, options);
        apply_filter_sql(&mut sql, &mut values, &options.filters);
        apply_sort_sql(&mut sql, options);
        values.push((candidate_limit(options) as i64).into());

        let timing = SearchTiming::start();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), search_result_from_row)?;
        let raw = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect search results")?;
        let candidate_count = raw.len();
        timing.lap("sql+collect");

        // Filter/rank/sort on the metadata already stored in the index, then
        // hydrate ONLY the rows we will return. Hydration does one
        // `symlink_metadata` syscall per row, so hydrating the full candidate set
        // (limit*20) before truncating was the real cost on large indexes.
        let compiled = CompiledQuery::compile(options.mode, &options.query)?;
        let mut scorer = QueryScorer::new();
        let mut results = raw
            .into_iter()
            .filter(|result| {
                compiled.matches_any(&mut scorer, [result.name.as_str(), result.path.as_str()])
            })
            .collect::<Vec<_>>();
        let matched_count = results.len();
        timing.lap("compile+filter");

        let frecency = if options.sort == SortField::Relevance {
            let paths = results.iter().map(|r| r.path.as_str()).collect::<Vec<_>>();
            self.frecency_scores(&paths)
        } else {
            HashMap::new()
        };
        timing.lap("frecency");

        sort_results_compiled(&mut results, options, &compiled, &mut scorer, &frecency);
        results.truncate(options.limit);
        timing.lap("sort");

        let results = self.hydrate_search_results(results)?;
        timing.finish(candidate_count, matched_count);
        Ok(results)
    }

    pub fn search_interactive(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let Some(needle) = normalize_filename_query(query) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }

        let candidate_limit = search_candidate_limit(self.verify_paths_on_search, limit);
        let mut results = Vec::with_capacity(candidate_limit.min(50));
        let mut seen_paths = HashSet::new();

        self.collect_filename_matches(
            "f.name_lower = ?1",
            &[&needle as &dyn rusqlite::ToSql],
            candidate_limit,
            &mut seen_paths,
            &mut results,
        )?;

        if results.len() < candidate_limit {
            let stem_prefix = format!("{needle}.");
            if let Some(stem_end) = next_prefix_bound(&stem_prefix) {
                self.collect_filename_matches(
                    "f.name_lower >= ?1 AND f.name_lower < ?2",
                    &[&stem_prefix as &dyn rusqlite::ToSql, &stem_end],
                    candidate_limit,
                    &mut seen_paths,
                    &mut results,
                )?;
            }
        }

        if results.len() < candidate_limit {
            if let Some(end) = next_prefix_bound(&needle) {
                self.collect_filename_matches(
                    "f.name_lower >= ?1 AND f.name_lower < ?2 AND f.name_lower != ?1",
                    &[&needle as &dyn rusqlite::ToSql, &end],
                    candidate_limit,
                    &mut seen_paths,
                    &mut results,
                )?;
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    pub fn mark_missing_under_root(&self, root: &str, seen_paths: &[String]) -> Result<()> {
        if seen_paths.is_empty() {
            self.conn
                .execute("UPDATE files SET deleted = 1 WHERE root = ?1", [root])?;
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS locator_seen_paths(path TEXT PRIMARY KEY);
             DELETE FROM locator_seen_paths;",
        )?;
        {
            let mut insert =
                tx.prepare("INSERT OR IGNORE INTO locator_seen_paths(path) VALUES (?1)")?;
            for path in seen_paths {
                insert.execute([path])?;
            }
        }
        tx.execute(
            "UPDATE files
             SET deleted = 1
             WHERE root = ?1
             AND NOT EXISTS (
                SELECT 1 FROM locator_seen_paths s WHERE s.path = files.path
             )",
            [root],
        )?;
        tx.execute_batch("DELETE FROM locator_seen_paths;")?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_stale_under_root(&self, root: &str, indexed_at: i64) -> Result<u64> {
        let changed = self.conn.execute(
            "UPDATE files SET deleted = 1 WHERE root = ?1 AND indexed_at != ?2",
            params![root, indexed_at],
        )?;
        Ok(changed as u64)
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

    /// Frecency weight per path (open count scaled by recency). Best-effort: any
    /// failure (e.g. a connection without the `access` table) yields an empty
    /// map so search never breaks.
    fn frecency_scores(&self, paths: &[&str]) -> HashMap<String, u32> {
        let now = Utc::now().timestamp();
        let mut scores = HashMap::new();
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT open_count, last_opened_at FROM access WHERE path = ?1")
        else {
            return scores;
        };
        for &path in paths {
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

    /// Soft-delete a single path (and, if it is a directory, everything beneath
    /// it). Used by the live filesystem watcher when a path is removed.
    pub fn mark_path_deleted(&self, path: &str) -> Result<u64> {
        let prefix = format!("{}/%", path.trim_end_matches('/'));
        let changed = self.conn.execute(
            "UPDATE files SET deleted = 1 WHERE path = ?1 OR path LIKE ?2",
            params![path, prefix],
        )?;
        Ok(changed as u64)
    }

    pub fn count_active(&self) -> Result<u64> {
        let count =
            self.conn
                .query_row("SELECT COUNT(*) FROM files WHERE deleted = 0", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        Ok(count as u64)
    }

    pub fn roots(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT root FROM files WHERE deleted = 0 ORDER BY root")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect roots")
    }

    pub fn remove_root(&self, root: &str) -> Result<u64> {
        let changed = self
            .conn
            .execute("UPDATE files SET deleted = 1 WHERE root = ?1", [root])?;
        Ok(changed as u64)
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -200000;
             CREATE TABLE IF NOT EXISTS files (
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
             CREATE INDEX IF NOT EXISTS idx_files_kind ON files(kind);
             CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
             CREATE INDEX IF NOT EXISTS idx_files_size ON files(size_bytes);
             CREATE INDEX IF NOT EXISTS idx_files_created ON files(created_at);
             CREATE INDEX IF NOT EXISTS idx_files_modified ON files(modified_at);
             CREATE INDEX IF NOT EXISTS idx_files_root ON files(root);
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
             DROP INDEX IF EXISTS idx_files_deleted;",
        )?;
        self.ensure_name_lower_column()?;
        self.create_bulk_indexes()?;
        self.recreate_fts_if_needed()?;
        Ok(())
    }

    fn configure_fast_staging_connection(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA page_size = 8192;
             PRAGMA journal_mode = OFF;
             PRAGMA synchronous = OFF;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -400000;",
        )?;
        Ok(())
    }

    fn create_fresh_scan_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL,
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
             CREATE TABLE IF NOT EXISTS scan_roots (
                root TEXT PRIMARY KEY,
                scan_token INTEGER NOT NULL,
                complete INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER
             );",
        )?;
        Ok(())
    }

    fn create_fresh_scan_indexes(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_files_path_unique ON files(path);
             CREATE INDEX IF NOT EXISTS idx_files_kind ON files(kind);
             CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
             CREATE INDEX IF NOT EXISTS idx_files_root ON files(root);
             CREATE INDEX IF NOT EXISTS idx_files_deleted_name_lower ON files(deleted, name_lower);",
        )?;
        Ok(())
    }

    fn ensure_name_lower_column(&self) -> Result<()> {
        let has_name_lower = self
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('files') WHERE name = 'name_lower'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !has_name_lower {
            self.conn
                .execute_batch("ALTER TABLE files ADD COLUMN name_lower TEXT;")?;
        }

        self.conn.execute_batch(
            "UPDATE files SET name_lower = lower(name) WHERE name_lower IS NULL;
             CREATE INDEX IF NOT EXISTS idx_files_name_lower ON files(name_lower);
             CREATE INDEX IF NOT EXISTS idx_files_deleted_name_lower ON files(deleted, name_lower);",
        )?;
        Ok(())
    }

    fn create_bulk_indexes(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_files_kind ON files(kind);
             CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
             CREATE INDEX IF NOT EXISTS idx_files_size ON files(size_bytes);
             CREATE INDEX IF NOT EXISTS idx_files_created ON files(created_at);
             CREATE INDEX IF NOT EXISTS idx_files_modified ON files(modified_at);
             CREATE INDEX IF NOT EXISTS idx_files_root ON files(root);
             CREATE INDEX IF NOT EXISTS idx_files_name_lower ON files(name_lower);
             CREATE INDEX IF NOT EXISTS idx_files_deleted_name_lower ON files(deleted, name_lower);",
        )?;
        Ok(())
    }

    fn recreate_fts_if_needed(&self) -> Result<()> {
        let create_sql = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'files_fts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        // Recreate when the table is missing, lacks the content-table wiring,
        // still uses the old word tokenizer, or its indexed columns no longer
        // match the `index_paths` setting. The trigram tokenizer makes substring
        // matching index-accelerated; indexing `path` as well is far more
        // expensive to build, so it is opt-in.
        let want_paths = fts_index_paths();
        let needs_recreate = create_sql
            .as_deref()
            .map(|sql| {
                !sql.contains("content='files'")
                    || !sql.contains("tokenize='trigram'")
                    || !sql.contains("detail=none")
                    || fts_sql_has_path(sql) != want_paths
            })
            .unwrap_or(true);

        if needs_recreate {
            let columns = fts_columns(want_paths);
            self.conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS files_fts;
                 CREATE VIRTUAL TABLE files_fts
                 USING fts5({columns}, content='files', content_rowid='id',
                            tokenize='trigram', detail=none, columnsize=0);
                 INSERT INTO files_fts(files_fts, rank) VALUES('pgsz', 8050);
                 INSERT INTO files_fts(rowid, {columns})
                 SELECT id, {columns} FROM files WHERE deleted = 0;",
            ))?;
        }

        self.create_fts_triggers()?;
        Ok(())
    }

    fn drop_fts_triggers(&self) -> Result<()> {
        self.conn.execute_batch(
            "DROP TRIGGER IF EXISTS files_ai;
             DROP TRIGGER IF EXISTS files_ad;
             DROP TRIGGER IF EXISTS files_au;",
        )?;
        Ok(())
    }

    fn rebuild_fts(&self) -> Result<()> {
        self.conn
            .execute_batch("INSERT INTO files_fts(files_fts) VALUES('rebuild');")?;
        Ok(())
    }

    fn create_fts_triggers(&self) -> Result<()> {
        // Triggers must reference exactly the columns the FTS table indexes. Only
        // rebuild them when missing or when the column set changed (with
        // `index_paths`) -- dropping them on every open would open a window where
        // a concurrent writer's row never reaches the FTS index.
        let want_paths = fts_index_paths();
        let existing = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'files_ai'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(sql) = &existing {
            if sql.contains("new.path") == want_paths {
                return Ok(()); // triggers already match the desired columns
            }
        }
        self.drop_fts_triggers()?;
        let columns = fts_columns(want_paths);
        let new_values = if want_paths {
            "new.id, new.name, new.path"
        } else {
            "new.id, new.name"
        };
        let old_values = if want_paths {
            "old.id, old.name, old.path"
        } else {
            "old.id, old.name"
        };
        self.conn.execute_batch(&format!(
            "CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
                INSERT INTO files_fts(rowid, {columns}) VALUES ({new_values});
             END;
             CREATE TRIGGER files_ad AFTER DELETE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, {columns})
                VALUES('delete', {old_values});
             END;
             CREATE TRIGGER files_au AFTER UPDATE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, {columns})
                VALUES('delete', {old_values});
                INSERT INTO files_fts(rowid, {columns}) VALUES ({new_values});
             END;",
        ))?;
        Ok(())
    }

    fn collect_filename_matches(
        &self,
        predicate: &str,
        predicate_params: &[&dyn rusqlite::ToSql],
        limit: usize,
        seen_paths: &mut HashSet<String>,
        results: &mut Vec<SearchResult>,
    ) -> Result<()> {
        if results.len() >= limit {
            return Ok(());
        }

        let sql = format!(
            "SELECT f.path, f.name, f.extension, f.kind, f.size_bytes, f.created_at, f.modified_at
             FROM files f
             WHERE f.deleted = 0 AND {predicate}
             ORDER BY f.name_lower ASC
             LIMIT ?"
        );
        let remaining = (limit - results.len()) as i64;
        let mut values = predicate_params.to_vec();
        values.push(&remaining);

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), search_result_from_row)?;
        for row in rows {
            if let Some(result) = self.hydrate_search_result(row?)? {
                if seen_paths.insert(result.path.clone()) {
                    results.push(result);
                }
            }
        }
        Ok(())
    }
}

pub fn delete_index_files(path: impl AsRef<Path>) -> Result<u64> {
    let path = path.as_ref();
    let mut removed = 0;
    for file in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
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
        if candidate.exists() {
            return Ok(Some(candidate));
        }
        let fallback_candidate = fallback_db_path_for_root(&current)?;
        if fallback_candidate.exists() {
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
        if candidate.exists() {
            return Ok(Some(LocatedIndex {
                db_path: candidate,
                root: current,
            }));
        }
        let fallback_candidate = fallback_db_path_for_root(&current)?;
        if fallback_candidate.exists() {
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
        if candidate.exists() {
            return Ok(candidate);
        }
        let fallback_candidate = fallback_db_path_for_root(&current)?;
        if fallback_candidate.exists() {
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

pub fn staging_db_path_for_root(root: impl AsRef<Path>) -> Result<PathBuf> {
    let root = root.as_ref().canonicalize().with_context(|| {
        format!(
            "resolve scan root for staging index {}",
            root.as_ref().display()
        )
    })?;
    let root_key = stable_path_key(&root);
    Ok(app_support_indices_dir()?
        .join(root_key)
        .join("staging.sqlite"))
}

fn locator_data_dir() -> Result<PathBuf> {
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

/// Build an FTS5 MATCH expression for a substring query against a
/// detail=none trigram table. Phrase queries are not allowed under
/// detail=none, so emit every 3-char window as its own single-token term,
/// AND-ed. False positives (all trigrams present but not contiguous) are
/// removed by the Rust-side post-filter.
fn trigram_match(query: &str) -> String {
    let folded = query.trim().to_lowercase();
    let chars: Vec<char> = folded.chars().collect();
    let mut terms = Vec::new();
    for window in chars.windows(3) {
        let tri: String = window.iter().collect();
        terms.push(format!("\"{}\"", tri.replace('"', "\"\"")));
    }
    debug_assert!(!terms.is_empty());
    terms.join(" AND ")
}

/// Whether the FTS index should cover full paths (opt-in via config) or just
/// filenames (default, far cheaper to build).
fn fts_index_paths() -> bool {
    crate::config::Config::load().index_paths
}

/// FTS column list: `"name, path"` when paths are indexed, else `"name"`.
fn fts_columns(index_paths: bool) -> &'static str {
    if index_paths {
        "name, path"
    } else {
        "name"
    }
}

/// Whether an existing `files_fts` CREATE statement indexes the `path` column.
fn fts_sql_has_path(create_sql: &str) -> bool {
    create_sql.contains("(name, path")
}

fn sanitize_fts_query(query: &str) -> Option<String> {
    // Trigram matching requires tokens of at least 3 chars; shorter fragments
    // are dropped. Each alphanumeric run becomes a quoted substring, ANDed.
    let terms = query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| term.chars().count() >= 3)
        .map(trigram_match)
        .collect::<Vec<_>>();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

fn normalize_filename_query(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if !trimmed.chars().any(|ch| ch.is_alphanumeric()) {
        return None;
    }

    Some(trimmed.to_lowercase())
}

fn next_prefix_bound(prefix: &str) -> Option<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    for index in (0..bytes.len()).rev() {
        if bytes[index] < 0x7f {
            bytes[index] += 1;
            bytes.truncate(index + 1);
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

fn search_result_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchResult> {
    Ok(SearchResult {
        path: row.get(0)?,
        name: row.get(1)?,
        extension: row.get(2)?,
        kind: row.get(3)?,
        size_bytes: row_size_bytes(row.get(4)?),
        created_at: timestamp_to_datetime(row.get(5)?),
        modified_at: timestamp_to_datetime(row.get(6)?),
    })
}

impl Database {
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
        let metadata = match fs::symlink_metadata(&result.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok((!self.verify_paths_on_search).then_some(result));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read metadata for {}", result.path));
            }
        };

        result.size_bytes = metadata.len();
        result.created_at = metadata.created().ok().map(DateTime::<Utc>::from);
        result.modified_at = metadata.modified().ok().map(DateTime::<Utc>::from);
        if metadata.is_dir() {
            result.kind = "folder".to_string();
        }
        Ok(Some(result))
    }
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

fn apply_filter_sql(
    sql: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    filters: &SearchFilters,
) {
    if let Some(kind) = &filters.kind {
        sql.push_str(" AND f.kind = ?");
        values.push(kind.as_str().to_string().into());
    }
    if !filters.exts.is_empty() {
        sql.push_str(" AND f.extension IN (");
        sql.push_str(
            &std::iter::repeat_n("?", filters.exts.len())
                .collect::<Vec<_>>()
                .join(","),
        );
        sql.push(')');
        for ext in &filters.exts {
            values.push(ext.clone().into());
        }
    }
    if let Some(min_size) = filters.min_size {
        sql.push_str(" AND f.size_bytes >= ?");
        values.push(sqlite_size_bytes(min_size).into());
    }
    if let Some(max_size) = filters.max_size {
        sql.push_str(" AND f.size_bytes <= ?");
        values.push(sqlite_size_bytes(max_size).into());
    }
    if let Some(created_after) = filters.created_after {
        sql.push_str(" AND f.created_at >= ?");
        values.push(created_after.timestamp().into());
    }
    if let Some(created_before) = filters.created_before {
        sql.push_str(" AND f.created_at <= ?");
        values.push(created_before.timestamp().into());
    }
    if let Some(modified_after) = filters.modified_after {
        sql.push_str(" AND f.modified_at >= ?");
        values.push(modified_after.timestamp().into());
    }
    if let Some(modified_before) = filters.modified_before {
        sql.push_str(" AND f.modified_at <= ?");
        values.push(modified_before.timestamp().into());
    }
    if let Some(name) = &filters.name {
        sql.push_str(" AND f.name LIKE ?");
        values.push(format!("%{name}%").into());
    }
}

fn apply_query_mode_sql(
    sql: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    options: &SearchOptions,
) {
    let query = options.query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return;
    }

    match options.mode {
        QueryMode::Contains => {
            // Trigram FTS needs >=3 chars; below that fall back to an indexed
            // name-prefix scan (path substring matching is not worth a full scan
            // for 1-2 char queries).
            if query.chars().count() >= 3 {
                sql.push_str(" AND f.id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH ?)");
                values.push(trigram_match(&query).into());
            } else {
                sql.push_str(" AND f.name_lower LIKE ?");
                values.push(format!("%{query}%").into());
            }
        }
        QueryMode::Exact => {
            sql.push_str(" AND (f.name_lower = ? OR LOWER(f.path) = ?)");
            values.push(query.clone().into());
            values.push(query.into());
        }
        QueryMode::Prefix => {
            sql.push_str(" AND (f.name_lower LIKE ? OR LOWER(f.path) LIKE ?)");
            let pattern = format!("{query}%");
            values.push(pattern.clone().into());
            values.push(pattern.into());
        }
        QueryMode::Suffix => {
            sql.push_str(
                " AND (f.name_lower LIKE ? OR f.name_lower LIKE ? OR LOWER(f.path) LIKE ? OR LOWER(f.path) LIKE ?)",
            );
            let pattern = format!("%{query}");
            let stem_pattern = format!("%{query}.%");
            values.push(pattern.clone().into());
            values.push(stem_pattern.clone().into());
            values.push(pattern.into());
            values.push(stem_pattern.into());
        }
        QueryMode::Fuzzy | QueryMode::Regex | QueryMode::Glob => {}
    }
}

fn apply_sort_sql(sql: &mut String, options: &SearchOptions) {
    let direction = if options.reverse { "DESC" } else { "ASC" };
    match options.sort {
        SortField::Relevance => {
            sql.push_str(" ORDER BY f.modified_at DESC NULLS LAST, f.name_lower ASC LIMIT ?");
        }
        SortField::Name => sql.push_str(&format!(" ORDER BY f.name_lower {direction} LIMIT ?")),
        SortField::Path => sql.push_str(&format!(" ORDER BY f.path {direction} LIMIT ?")),
        SortField::Kind => sql.push_str(&format!(
            " ORDER BY f.kind {direction}, f.name_lower ASC LIMIT ?"
        )),
        SortField::Size => {
            sql.push_str(&format!(
                " ORDER BY f.size_bytes {direction}, f.name_lower ASC LIMIT ?"
            ));
        }
        SortField::Created => {
            sql.push_str(&format!(
                " ORDER BY f.created_at {direction} NULLS LAST, f.name_lower ASC LIMIT ?"
            ));
        }
        SortField::Modified => {
            sql.push_str(&format!(
                " ORDER BY f.modified_at {direction} NULLS LAST, f.name_lower ASC LIMIT ?"
            ));
        }
    }
}

fn candidate_limit(options: &SearchOptions) -> usize {
    // Clamped (not just floored): these bound how many rows are pulled and
    // ranked per search. Hydration now runs after truncation, so the cap mainly
    // bounds in-memory filter/sort work; keep it generous but finite.
    match options.mode {
        // Fuzzy/Regex/Glob filter in Rust, so they need a wider candidate pool.
        QueryMode::Fuzzy | QueryMode::Regex | QueryMode::Glob => {
            options.limit.saturating_mul(100).clamp(5000, 20000)
        }
        _ => options.limit.saturating_mul(20).clamp(200, 5000),
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
                RelevanceKey::Tier(
                    relevance_score(&result, &options.query.to_ascii_lowercase()),
                    boost,
                )
            };
            (key, result)
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.modified_at.cmp(&left.1.modified_at))
            .then_with(|| {
                left.1
                    .name
                    .to_ascii_lowercase()
                    .cmp(&right.1.name.to_ascii_lowercase())
            })
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

pub(crate) fn sort_results(results: &mut [SearchResult], options: &SearchOptions) {
    match options.sort {
        SortField::Relevance => {
            let query = options.query.to_ascii_lowercase();
            results.sort_by(|left, right| {
                relevance_score(left, &query)
                    .cmp(&relevance_score(right, &query))
                    .then_with(|| right.modified_at.cmp(&left.modified_at))
                    .then_with(|| {
                        left.name
                            .to_ascii_lowercase()
                            .cmp(&right.name.to_ascii_lowercase())
                    })
                    .then_with(|| left.path.cmp(&right.path))
            });
            if options.reverse {
                results.reverse();
            }
        }
        SortField::Name => sort_by_key(results, options.reverse, |result| {
            result.name.to_ascii_lowercase()
        }),
        SortField::Path => sort_by_key(results, options.reverse, |result| {
            result.path.to_ascii_lowercase()
        }),
        SortField::Kind => sort_by_key(results, options.reverse, |result| {
            result.kind.to_ascii_lowercase()
        }),
        SortField::Size => {
            results.sort_by_key(|result| result.size_bytes);
            if options.reverse {
                results.reverse();
            }
        }
        SortField::Created => {
            results.sort_by_key(|result| result.created_at);
            if options.reverse {
                results.reverse();
            }
        }
        SortField::Modified => {
            results.sort_by_key(|result| result.modified_at);
            if options.reverse {
                results.reverse();
            }
        }
    }
}

fn sort_by_key<T: Ord>(
    results: &mut [SearchResult],
    reverse: bool,
    key: impl Fn(&SearchResult) -> T,
) {
    results.sort_by_key(key);
    if reverse {
        results.reverse();
    }
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

fn relevance_score(result: &SearchResult, query: &str) -> u8 {
    let name = result.name.to_ascii_lowercase();
    let path = result.path.to_ascii_lowercase();
    if name == query {
        0
    } else if name.starts_with(query) {
        1
    } else if name.contains(query) {
        2
    } else if path.contains(query) {
        3
    } else {
        4
    }
}

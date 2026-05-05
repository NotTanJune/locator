use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::time::{Duration, Instant};

use crate::query::{QueryMode, SearchFilters, SearchOptions, SortField};

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

#[derive(Debug, Clone, PartialEq, Eq)]
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
                    record.size_bytes,
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
        let fts_query = sanitize_fts_query(query);
        if !query.trim().is_empty() && fts_query.is_none() {
            return Ok(Vec::new());
        }
        if fts_query.is_some() {
            sql.push_str(" JOIN files_fts ON files_fts.rowid = f.id");
        }
        sql.push_str(" WHERE f.deleted = 0");

        let mut values = Vec::<rusqlite::types::Value>::new();
        if let Some(fts) = fts_query {
            sql.push_str(" AND files_fts MATCH ?");
            values.push(fts.into());
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
            values.push((min_size as i64).into());
        }
        if let Some(max_size) = filters.max_size {
            sql.push_str(" AND f.size_bytes <= ?");
            values.push((max_size as i64).into());
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

        let mut results = self.hydrate_search_results(
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("collect search results")?,
        )?;
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

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), search_result_from_row)?;
        let mut results = self
            .hydrate_search_results(
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .context("collect search results")?,
            )?
            .into_iter()
            .filter(|result| {
                candidate_matches(
                    options.mode,
                    &options.query,
                    [result.name.as_str(), result.path.as_str()],
                )
                .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        sort_results(&mut results, options);
        results.truncate(options.limit);
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
             CREATE INDEX IF NOT EXISTS idx_files_deleted ON files(deleted);
             CREATE TABLE IF NOT EXISTS scan_roots (
                root TEXT PRIMARY KEY,
                scan_token INTEGER NOT NULL,
                complete INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER
             );",
        )?;
        self.ensure_name_lower_column()?;
        self.create_bulk_indexes()?;
        self.recreate_fts_if_needed()?;
        Ok(())
    }

    fn configure_fast_staging_connection(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode = OFF;
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
             CREATE INDEX IF NOT EXISTS idx_files_deleted ON files(deleted);
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
             CREATE INDEX IF NOT EXISTS idx_files_deleted ON files(deleted);
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

        let needs_recreate = create_sql
            .as_deref()
            .map(|sql| !sql.contains("content='files'"))
            .unwrap_or(true);

        if needs_recreate {
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS files_fts;
                 CREATE VIRTUAL TABLE files_fts
                 USING fts5(name, path, content='files', content_rowid='id', tokenize='unicode61');
                 INSERT INTO files_fts(rowid, name, path)
                 SELECT id, name, path FROM files WHERE deleted = 0;",
            )?;
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
        self.conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
                INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
             END;
             CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, name, path)
                VALUES('delete', old.id, old.name, old.path);
             END;
             CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, name, path)
                VALUES('delete', old.id, old.name, old.path);
                INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
             END;",
        )?;
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

fn sanitize_fts_query(query: &str) -> Option<String> {
    let terms = query
        .split(|ch: char| !ch.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|term| !term.is_empty())
        .map(|term| format!("{term}*"))
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
        size_bytes: row.get::<_, i64>(4)? as u64,
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
        values.push((min_size as i64).into());
    }
    if let Some(max_size) = filters.max_size {
        sql.push_str(" AND f.size_bytes <= ?");
        values.push((max_size as i64).into());
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
            sql.push_str(" AND (f.name_lower LIKE ? OR LOWER(f.path) LIKE ?)");
            let pattern = format!("%{query}%");
            values.push(pattern.clone().into());
            values.push(pattern.into());
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
    match options.mode {
        QueryMode::Fuzzy | QueryMode::Regex | QueryMode::Glob => {
            options.limit.saturating_mul(100).max(5000)
        }
        _ => options.limit.saturating_mul(20).max(200),
    }
}

pub(crate) fn candidate_matches<'a>(
    mode: QueryMode,
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Result<bool> {
    for candidate in candidates {
        if mode.matches(query, candidate)? {
            return Ok(true);
        }
    }
    Ok(false)
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

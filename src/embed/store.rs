//! Sidecar vector store (`<index>.embeddings.sqlite`, keyed by path) and the
//! in-memory vector index used for search.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::vector::{dot_f16, encode_f16};

const SCHEMA_VERSION: &str = "1";

/// One file's embedding progress, as stored in `embed_files`.
struct Progress {
    status: i64,
    mtime: i64,
    size: i64,
    depth_done: i64,
    chunks_total: i64,
}

/// Adds the progress columns to a sidecar made before staged passes and sets them from the
/// stored chunk count (old files were embedded in full). Vectors are not touched.
fn migrate_progress_columns(conn: &Connection) -> Result<()> {
    let names: Vec<String> = {
        let mut statement = conn
            .prepare("PRAGMA table_info(embed_files)")
            .context("read embed_files columns")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if !names.iter().any(|name| name == "chunks_total") {
        conn.execute_batch("ALTER TABLE embed_files ADD COLUMN chunks_total INTEGER")?;
    }
    if !names.iter().any(|name| name == "depth_done") {
        conn.execute_batch("ALTER TABLE embed_files ADD COLUMN depth_done INTEGER")?;
    }
    conn.execute(
        "UPDATE embed_files SET chunks_total = chunk_count, depth_done = chunk_count
         WHERE chunks_total IS NULL OR depth_done IS NULL",
        [],
    )?;
    Ok(())
}
const MAX_SEARCH_WORKERS: usize = 8;

/// Sidecar path for a main index: `index.sqlite` becomes `index.embeddings.sqlite`.
pub fn sidecar_path_for_index(index_path: &Path) -> PathBuf {
    let stem = index_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "index".to_string());
    index_path.with_file_name(format!("{stem}.embeddings.sqlite"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Done = 1,
    Skipped = 2,
    Failed = 3,
}

/// Identity of a file for change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
}

/// One chunk ready to store. `vector` is already truncated to the store's dims.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredChunk {
    pub ordinal: u32,
    pub start_byte: u64,
    pub snippet: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    pub files: u64,
    pub chunks: u64,
    pub skipped: u64,
    pub failed: u64,
    pub sidecar_bytes: u64,
}

pub struct EmbedStore {
    conn: Connection,
    path: PathBuf,
}

/// Creates the sidecar owner-only (0600) if it does not exist, and tightens an existing one.
/// SQLite gives its -wal and -shm files the database file's mode. Vectors and snippets of private
/// documents must not be readable by other accounts on the machine.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create embeddings sidecar {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict embeddings sidecar {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

impl EmbedStore {
    /// Creates the file and schema when missing.
    pub fn open(path: &Path) -> Result<EmbedStore> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        restrict_to_owner(path)?;
        let conn = Connection::open(path)
            .with_context(|| format!("open embeddings sidecar {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enable WAL on embeddings sidecar")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("set synchronous on embeddings sidecar")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS embed_files (
                 path TEXT PRIMARY KEY,
                 root TEXT NOT NULL,
                 mtime INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 status INTEGER NOT NULL,
                 chunk_count INTEGER NOT NULL,
                 detail TEXT
             );
             CREATE TABLE IF NOT EXISTS chunks (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL,
                 ordinal INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL,
                 snippet TEXT NOT NULL,
                 vec BLOB NOT NULL,
                 UNIQUE(path, ordinal)
             );
             CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
             CREATE INDEX IF NOT EXISTS idx_embed_files_root ON embed_files(root);",
        )
        .context("create embeddings sidecar schema")?;
        migrate_progress_columns(&conn).context("migrate embeddings sidecar")?;
        let store = EmbedStore {
            conn,
            path: path.to_path_buf(),
        };
        store.set_meta("schema_version", SCHEMA_VERSION)?;
        Ok(store)
    }

    /// Opens an existing sidecar read-only. Returns `None` when the file is missing.
    pub fn open_readonly(path: &Path) -> Result<Option<EmbedStore>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open embeddings sidecar {}", path.display()))?;
        Ok(Some(EmbedStore {
            conn,
            path: path.to_path_buf(),
        }))
    }

    fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("read embeddings meta")
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .context("write embeddings meta")?;
        Ok(())
    }

    /// Wipes all rows and rewrites meta when `model` or `dims` differ from what is stored.
    /// Returns true when it wiped.
    pub fn ensure_compatible(&self, model: &str, dims: usize) -> Result<bool> {
        let stored_model = self.get_meta("model")?;
        let stored_dims = self.get_meta("dims")?;
        let wanted_dims = dims.to_string();
        let matches = stored_model.as_deref() == Some(model)
            && stored_dims.as_deref() == Some(wanted_dims.as_str());
        if matches {
            return Ok(false);
        }
        let wiped = stored_model.is_some() || stored_dims.is_some();
        if wiped {
            self.clear()?;
        }
        self.set_meta("model", model)?;
        self.set_meta("dims", &wanted_dims)?;
        Ok(wiped)
    }

    pub fn dims(&self) -> Result<Option<usize>> {
        match self.get_meta("dims")? {
            Some(value) => Ok(Some(value.parse().context("stored dims is not a number")?)),
            None => Ok(None),
        }
    }

    /// Deletes rows for files under `root` that are no longer active in the main index.
    pub fn prune_to_active(&self, root: &str, active_paths: &[String]) -> Result<u64> {
        let tx = self.conn.unchecked_transaction().context("begin prune")?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS active (path TEXT PRIMARY KEY);
             DELETE FROM temp.active;",
        )
        .context("create temp active table")?;
        {
            let mut insert = tx
                .prepare("INSERT OR IGNORE INTO temp.active (path) VALUES (?1)")
                .context("prepare active insert")?;
            for path in active_paths {
                insert
                    .execute(params![path])
                    .context("insert active path")?;
            }
        }
        let removed = tx
            .execute(
                "DELETE FROM embed_files WHERE root = ?1
                 AND NOT EXISTS (SELECT 1 FROM temp.active a WHERE a.path = embed_files.path)",
                params![root],
            )
            .context("prune embed_files")? as u64;
        tx.execute(
            "DELETE FROM chunks WHERE NOT EXISTS
             (SELECT 1 FROM embed_files f WHERE f.path = chunks.path)",
            [],
        )
        .context("prune orphan chunks")?;
        tx.execute_batch("DELETE FROM temp.active;")
            .context("clear temp active table")?;
        tx.commit().context("commit prune")?;
        Ok(removed)
    }

    /// True when the stamp differs from the stored one, or the stored row failed.
    /// True when the first stage (ordinals below `limit`) of this file still needs embedding: no
    /// row, a changed stamp, a failed row, or fewer chunks embedded than `min(chunks_total, limit)`.
    pub fn needs_embedding(&self, file: &FileStamp, limit: usize) -> Result<bool> {
        let Some(row) = self.progress(&file.path)? else {
            return Ok(true);
        };
        let current = row.mtime == file.mtime && row.size == file.size;
        let finished =
            row.status == FileStatus::Done as i64 || row.status == FileStatus::Skipped as i64;
        Ok(!(current && finished && row.depth_done >= row.chunks_total.min(limit as i64)))
    }

    /// True when a deeper stage (ordinals below `limit`) of this file is still missing. Only for a
    /// file that is embedded and unchanged: failed and changed files are handled by the first stage.
    pub fn needs_deeper(&self, file: &FileStamp, limit: usize) -> Result<bool> {
        let Some(row) = self.progress(&file.path)? else {
            return Ok(false);
        };
        let current = row.mtime == file.mtime && row.size == file.size;
        Ok(current
            && row.status == FileStatus::Done as i64
            && row.depth_done < row.chunks_total.min(limit as i64))
    }

    fn progress(&self, path: &str) -> Result<Option<Progress>> {
        self.conn
            .query_row(
                // NULL means the row was written by a build that predates these columns. Such a row
                // was embedded in full, so both values fall back to its chunk count.
                "SELECT status, mtime, size, COALESCE(depth_done, chunk_count),
                        COALESCE(chunks_total, chunk_count)
                 FROM embed_files WHERE path = ?1",
                params![path],
                |row| {
                    Ok(Progress {
                        status: row.get(0)?,
                        mtime: row.get(1)?,
                        size: row.get(2)?,
                        depth_done: row.get(3)?,
                        chunks_total: row.get(4)?,
                    })
                },
            )
            .optional()
            .context("read embed_files row")
    }

    /// Replaces a file's row and chunks in one transaction; the file is complete at `chunks`.
    pub fn write_file(
        &self,
        root: &str,
        stamp: &FileStamp,
        status: FileStatus,
        chunks: &[StoredChunk],
        detail: Option<&str>,
    ) -> Result<()> {
        self.write_stage(root, stamp, status, chunks, detail, 0, chunks.len())
    }

    /// Writes one stage of a file in one transaction. `chunks` are ordinals `start..` and
    /// `chunks_total` is how many chunks the file has. Stage 0 replaces the file's chunks. A later
    /// stage appends, and requires the stored depth to be exactly `start`.
    #[allow(clippy::too_many_arguments)]
    pub fn write_stage(
        &self,
        root: &str,
        stamp: &FileStamp,
        status: FileStatus,
        chunks: &[StoredChunk],
        detail: Option<&str>,
        start: usize,
        chunks_total: usize,
    ) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("begin file write")?;
        let depth_done = start + chunks.len();
        if start == 0 {
            tx.execute("DELETE FROM chunks WHERE path = ?1", params![stamp.path])
                .context("delete old chunks")?;
            tx.execute(
                "INSERT OR REPLACE INTO embed_files
                 (path, root, mtime, size, status, chunk_count, detail, chunks_total, depth_done)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?6)",
                params![
                    stamp.path,
                    root,
                    stamp.mtime,
                    stamp.size,
                    status as i64,
                    depth_done as i64,
                    detail,
                    chunks_total as i64,
                ],
            )
            .context("write embed_files row")?;
        } else {
            let stored: Option<i64> = tx
                .query_row(
                    "SELECT COALESCE(depth_done, chunk_count) FROM embed_files WHERE path = ?1",
                    params![stamp.path],
                    |row| row.get(0),
                )
                .optional()
                .context("read stored depth")?;
            ensure!(
                stored == Some(start as i64) && status == FileStatus::Done,
                "stage starts at ordinal {start} but the file is embedded to {stored:?}"
            );
            tx.execute(
                "UPDATE embed_files SET chunk_count = ?2, depth_done = ?2, chunks_total = ?3,
                 status = ?4, detail = ?5 WHERE path = ?1",
                params![
                    stamp.path,
                    depth_done as i64,
                    chunks_total as i64,
                    status as i64,
                    detail
                ],
            )
            .context("update embed_files row")?;
        }
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO chunks (path, ordinal, start_byte, snippet, vec)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .context("prepare chunk insert")?;
            for chunk in chunks {
                insert
                    .execute(params![
                        stamp.path,
                        chunk.ordinal as i64,
                        chunk.start_byte as i64,
                        chunk.snippet,
                        encode_f16(&chunk.vector)
                    ])
                    .context("insert chunk")?;
            }
        }
        tx.commit().context("commit file write")?;
        Ok(())
    }

    pub fn set_complete(&self, root: &str, complete: bool) -> Result<()> {
        self.set_meta(
            &format!("complete_{root}"),
            if complete { "1" } else { "0" },
        )
    }

    pub fn is_complete(&self, root: &str) -> Result<Option<bool>> {
        Ok(self
            .get_meta(&format!("complete_{root}"))?
            .map(|value| value == "1"))
    }

    /// Roots that have at least one embed_files row, sorted.
    pub fn roots(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT root FROM embed_files ORDER BY root")
            .context("prepare roots query")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn stats(&self) -> Result<StoreStats> {
        let count = |sql: &str| -> Result<u64> {
            Ok(self.conn.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64)
        };
        let files = count("SELECT COUNT(*) FROM embed_files")?;
        let chunks = count("SELECT COUNT(*) FROM chunks")?;
        let skipped = count(&format!(
            "SELECT COUNT(*) FROM embed_files WHERE status = {}",
            FileStatus::Skipped as i64
        ))?;
        let failed = count(&format!(
            "SELECT COUNT(*) FROM embed_files WHERE status = {}",
            FileStatus::Failed as i64
        ))?;
        let mut sidecar_bytes = file_len(&self.path);
        sidecar_bytes += file_len(&PathBuf::from(format!("{}-wal", self.path.display())));
        Ok(StoreStats {
            files,
            chunks,
            skipped,
            failed,
            sidecar_bytes,
        })
    }

    /// Deletes all file and chunk rows. Meta (model and dims) is kept.
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM chunks; DELETE FROM embed_files;")
            .context("clear embeddings rows")
    }

    /// Snippets for `(path, ordinal)` pairs. A missing chunk yields an empty string.
    pub fn snippets_for(&self, keys: &[(String, u32)]) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT snippet FROM chunks WHERE path = ?1 AND ordinal = ?2")
            .context("prepare snippet query")?;
        let mut out = Vec::with_capacity(keys.len());
        for (path, ordinal) in keys {
            let snippet: Option<String> = stmt
                .query_row(params![path, *ordinal as i64], |row| row.get(0))
                .optional()
                .context("read snippet")?;
            out.push(snippet.unwrap_or_default());
        }
        Ok(out)
    }

    /// Number of chunk rows (used by the availability check).
    /// Changes whenever another connection commits to the sidecar. Used to notice new results.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?)
    }

    pub fn chunk_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
                row.get::<_, i64>(0)
            })? as u64)
    }

    /// Every stored chunk's path, ordinal, and f16 bytes, in insertion order.
    fn all_vectors(&self) -> Result<Vec<(String, u32, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, ordinal, vec FROM chunks ORDER BY path, ordinal")
            .context("prepare vector load")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u32,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredChunk {
    pub path: String,
    pub ordinal: u32,
    pub score: f32,
}

/// All chunk vectors in memory as f16 (2 bytes per component), scored by dot product.
pub struct VectorIndex {
    dims: usize,
    paths: Vec<String>,
    refs: Vec<(u32, u32)>,
    vecs: Vec<u8>,
}

impl VectorIndex {
    /// Loads every chunk vector from the store.
    pub fn load(store: &EmbedStore) -> Result<VectorIndex> {
        let dims = store
            .dims()?
            .ok_or_else(|| anyhow!("embeddings sidecar has no dims"))?;
        let rows = store.all_vectors()?;
        Self::from_rows(dims, rows)
    }

    /// Builds an index from `(path, ordinal, f16 bytes)` rows. Each vector must be `dims` components.
    pub fn from_rows(dims: usize, rows: Vec<(String, u32, Vec<u8>)>) -> Result<VectorIndex> {
        let mut paths: Vec<String> = Vec::new();
        let mut path_ids: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut refs = Vec::with_capacity(rows.len());
        let mut vecs = Vec::with_capacity(rows.len() * dims * 2);
        for (path, ordinal, bytes) in rows {
            if bytes.len() != dims * 2 {
                bail!(
                    "stored vector for {path} has {} bytes, expected {}",
                    bytes.len(),
                    dims * 2
                );
            }
            let path_id = match path_ids.get(&path) {
                Some(id) => *id,
                None => {
                    let id = paths.len() as u32;
                    path_ids.insert(path.clone(), id);
                    paths.push(path);
                    id
                }
            };
            refs.push((path_id, ordinal));
            vecs.extend_from_slice(&bytes);
        }
        Ok(VectorIndex {
            dims,
            paths,
            refs,
            vecs,
        })
    }

    pub fn len(&self) -> usize {
        self.refs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Top `k` chunks by dot product, highest first. `query` must be L2-normalized with `dims` components.
    pub fn top_k(&self, query: &[f32], k: usize) -> Vec<ScoredChunk> {
        let n = self.refs.len();
        if k == 0 || n == 0 || query.len() != self.dims {
            return Vec::new();
        }
        let bytes_per = self.dims * 2;
        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .clamp(1, MAX_SEARCH_WORKERS);
        let per_worker = n.div_ceil(workers);
        let vecs = &self.vecs;

        let mut candidates: Vec<(f32, usize)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .filter_map(|worker| {
                    let start = worker * per_worker;
                    if start >= n {
                        return None;
                    }
                    let end = (start + per_worker).min(n);
                    Some(scope.spawn(move || {
                        let mut scored: Vec<(f32, usize)> = (start..end)
                            .map(|row| {
                                let slice = &vecs[row * bytes_per..(row + 1) * bytes_per];
                                (dot_f16(query, slice), row)
                            })
                            .collect();
                        if scored.len() > k {
                            scored.select_nth_unstable_by(k - 1, |a, b| {
                                b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal)
                            });
                            scored.truncate(k);
                        }
                        scored
                    }))
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("vector search worker panicked"))
                .collect()
        });

        candidates.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| self.path_of(a.1).cmp(self.path_of(b.1)))
        });
        candidates.truncate(k);
        candidates
            .into_iter()
            .map(|(score, row)| {
                let (path_id, ordinal) = self.refs[row];
                ScoredChunk {
                    path: self.paths[path_id as usize].clone(),
                    ordinal,
                    score,
                }
            })
            .collect()
    }

    fn path_of(&self, row: usize) -> &str {
        &self.paths[self.refs[row].0 as usize]
    }
}

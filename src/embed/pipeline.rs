//! The embedding pass. Three stages run concurrently:
//!
//! 1. Extract workers read and chunk files (no model involved).
//! 2. The embed stage runs on the calling thread, the only thread that touches the model.
//!    It batches chunks adaptively and hands finished files to the writer.
//! 3. The writer thread stores each file in the sidecar in one transaction.
//!
//! Shutdown: the embed stage returns when its input disconnects or the cancel flag is set. Returning
//! drops the work receiver, which unblocks any extract worker stuck on a full queue. Dropping the
//! output sender lets the writer drain and exit. Every thread is joined inside `thread::scope`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use super::chunk::{
    chunk_text, classify_and_extract, Chunk, Extracted, SkipReason, MAX_CHUNKS_PER_FILE,
};
use super::runtime::{document_input, with_embedder, Embedder, EmbedderConfig};
use super::skip::{detail_for_rule, dir_has_vendor_manifest, never_read_rule, sort_for_embedding};
use super::store::{sidecar_path_for_index, EmbedStore, FileStamp, FileStatus, StoredChunk};
use super::vector::truncate_and_normalize;
use crate::db::{Database, EmbeddableFile};

/// Stored with the vectors. Changing the model or the prefixes requires changing this string.
/// Fingerprint of the vectors. Changes whenever the same input would give different vectors:
/// `img140`: images are resized to the 140-token budget (DigUp sizing) before the vision encoder;
/// `tok2048`: text inputs keep up to 2048 tokens (was 1024); `imgfix1`: image vectors come from a
/// single mixed batch (before, every image got the same vector). A sidecar with another id is rebuilt.
pub const MODEL_ID: &str = "embeddinggemma-2-Q8_0-img140-tok2048-imgfix1";

const WORK_QUEUE: usize = 64;
const WRITE_QUEUE: usize = 16;
const INITIAL_BATCH: usize = 8;
const MIN_BATCH: usize = 2;
const MAX_BATCH: usize = 32;
const FAST_BATCH: Duration = Duration::from_millis(700);
const SLOW_BATCH: Duration = Duration::from_secs(4);
const IDLE_WAIT: Duration = Duration::from_millis(200);
const IDLE_FLUSH: Duration = Duration::from_millis(50);
const MAX_EXTRACT_WORKERS: usize = 8;

/// Chunk ordinal where each stage ends (exclusive). Stage 1 is the first 8 chunks of every file, stage 2
/// the next 24, stage 3 the rest (up to `MAX_CHUNKS_PER_FILE`).
pub const STAGE_ENDS: [usize; 3] = [8, 32, MAX_CHUNKS_PER_FILE];

/// How many passes (stages) a run does. Quick is pass 1 only (the first 8 chunks of every file),
/// balanced passes 1 and 2 (up to 32 chunks), deep all three (up to 128). Quick is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    #[default]
    Quick,
    Balanced,
    Deep,
}

impl Depth {
    pub fn stages(self) -> usize {
        match self {
            Depth::Quick => 1,
            Depth::Balanced => 2,
            Depth::Deep => 3,
        }
    }

    /// Parses `quick`, `balanced` or `deep` (case-insensitive). `None` for anything else.
    pub fn parse(value: &str) -> Option<Depth> {
        match value.trim().to_ascii_lowercase().as_str() {
            "quick" => Some(Depth::Quick),
            "balanced" => Some(Depth::Balanced),
            "deep" => Some(Depth::Deep),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Depth::Quick => "quick",
            Depth::Balanced => "balanced",
            Depth::Deep => "deep",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PassRequest {
    /// Final `index.sqlite`.
    pub index_path: PathBuf,
    /// Root as passed to scan; canonicalized inside.
    pub root: PathBuf,
    pub dims: usize,
    pub text_model: Option<PathBuf>,
    pub vision_model: Option<PathBuf>,
    pub fake: bool,
    /// How deep each file is embedded in this pass (`Depth::Deep` embeds every chunk up to 128).
    pub depth: Depth,
}

#[derive(Debug, Clone)]
pub enum PassEvent {
    Started {
        total_files: usize,
        backend: String,
        resumed: bool,
    },
    FileStarted {
        path: String,
    },
    /// Vectors are already truncated to the store's dims.
    ChunksEmbedded {
        path: String,
        kind: String,
        vectors: Vec<Vec<f32>>,
    },
    FileFinished {
        path: String,
        status: FileStatus,
    },
    /// A stage (1-based, of `stages`) begins. It embeds chunk ordinals `first_chunk..end_chunk` of
    /// the `files` it covers.
    StageStarted {
        stage: usize,
        stages: usize,
        first_chunk: usize,
        end_chunk: usize,
        files: usize,
    },
    /// A stage finished without being cancelled.
    StageFinished {
        stage: usize,
        stages: usize,
    },
    Finished(PassSummary),
}

/// Where a pass spent its time. Printed under `LCTR_TIMING=1`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PassTiming {
    /// Model load, before the pass starts.
    pub model_load: Duration,
    /// Sum over extract workers of time reading and chunking files.
    pub extract: Duration,
    /// Time the writer thread spent storing files.
    pub writer: Duration,
    /// Time inside the embedder (GPU busy for the text model).
    pub gpu: Duration,
    /// Time the embed stage waited for input from the extract workers.
    pub embed_wait: Duration,
    /// Tokens sent through the embedder.
    pub tokens: u64,
    /// Embedder calls (text batches and images).
    pub calls: usize,
    /// Chunks whose vector was reused from an identical chunk earlier in the pass.
    pub deduplicated: usize,
    /// Files skipped by low-value rules, by rule name.
    pub low_value: Vec<(&'static str, usize)>,
    /// Time from the start of the pass to the end of stage 1 (None if the pass stopped first).
    pub first_stage: Option<Duration>,
    /// Figures per stage, in stage order.
    pub stages: Vec<StageTiming>,
}

/// One stage of a pass: its wall time, chunks written and tokens sent through the embedder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageTiming {
    pub stage: usize,
    pub wall: Duration,
    pub chunks: usize,
    pub tokens: u64,
}

/// Compile-time check that the literal numbers in the config description and `--depth` help match
/// `STAGE_ENDS`. If the constants change, this fails and those texts must be updated with them.
const _: () = assert!(STAGE_ENDS[0] == 8 && STAGE_ENDS[1] == 32 && STAGE_ENDS[2] == 128);

/// What one pass covers, in users' numbering (chunks count from 1): pass 1 is the first 8 chunks of
/// each file, pass 2 chunks 9 to 32, pass 3 chunks 33 to 128. Ordinals in the code count from 0.
pub fn pass_range_text(pass: usize) -> String {
    let pass = pass.clamp(1, STAGE_ENDS.len());
    let end = STAGE_ENDS[pass - 1];
    let start = if pass == 1 { 0 } else { STAGE_ENDS[pass - 2] };
    if start == 0 {
        format!("first {end} chunks of each file")
    } else {
        format!("chunks {} to {end} of each file", start + 1)
    }
}

/// "pass 2 of 3: chunks 9 to 32 of each file": pass `stage` of the `stages` passes this run does.
pub fn stage_label(stage: usize, stages: usize) -> String {
    format!("pass {stage} of {stages}: {}", pass_range_text(stage))
}

/// Shown when a pass completes.
pub fn pass_done_text(stage: usize, stages: usize) -> String {
    if stage < stages {
        format!(
            "Pass {stage} of {stages} done. Search already works; the remaining passes add depth."
        )
    } else {
        format!("Pass {stage} of {stages} done.")
    }
}

/// The depth line used by the plain output and the summary card. Says what was done and how to go
/// deeper, or where a stopped run stopped and how to resume it. Built from `STAGE_ENDS`.
pub fn depth_text(depth: Depth, stages_done: usize, cancelled: bool) -> String {
    let [first, second, third] = STAGE_ENDS;
    if cancelled {
        return format!(
            "depth: {} stopped in pass {} of {}. Progress is saved; rerun the same command to resume.",
            depth.as_str(),
            stages_done + 1,
            depth.stages()
        );
    }
    match depth {
        Depth::Quick => format!(
            "depth: quick (first {first} chunks of each file). --depth balanced adds chunks {} to {second} and --depth deep adds chunks up to {third}.",
            first + 1
        ),
        Depth::Balanced => format!(
            "depth: balanced (up to {second} chunks per file). --depth deep adds chunks up to {third}."
        ),
        Depth::Deep => format!("depth: deep (up to {third} chunks per file)."),
    }
}

impl PassTiming {
    /// Adds another stage's counters to this one (the per-stage fields are not touched).
    fn absorb(&mut self, other: &PassTiming) {
        self.gpu += other.gpu;
        self.embed_wait += other.embed_wait;
        self.tokens += other.tokens;
        self.calls += other.calls;
        self.deduplicated += other.deduplicated;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassSummary {
    /// Distinct files that gained chunks in this pass.
    pub files_embedded: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub files_unchanged: usize,
    pub chunks: usize,
    pub cancelled: bool,
    pub elapsed: Duration,
    pub sidecar_bytes: u64,
    pub timing: PassTiming,
    /// The depth this pass was asked for, and how many of its stages finished.
    pub depth: Depth,
    pub stages_done: usize,
}

/// Runs the embedding pass with the model selected by `request` (fake when `request.fake` or
/// `LCTR_EMBED_FAKE=1`). Progress goes to `events`. Setting `cancel` stops the pass; everything
/// written so far is kept and the next run resumes.
pub fn run_pass(
    request: &PassRequest,
    events: Sender<PassEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<PassSummary> {
    let config = EmbedderConfig {
        text_model: request.text_model.clone(),
        vision_model: request.vision_model.clone(),
        fake: request.fake,
    };
    let load_started = Instant::now();
    with_embedder(&config, |embedder| {
        let model_load = load_started.elapsed();
        let mut summary =
            run_pass_with_embedder(request, events.clone(), cancel.clone(), embedder)?;
        summary.timing.model_load = model_load;
        Ok(summary)
    })
}

/// Same as `run_pass` with a caller-supplied embedder (tests use recording and fake embedders).
#[doc(hidden)]
pub fn run_pass_with_embedder(
    request: &PassRequest,
    events: Sender<PassEvent>,
    cancel: Arc<AtomicBool>,
    embedder: &mut dyn Embedder,
) -> Result<PassSummary> {
    let started = Instant::now();
    let root = request
        .root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", request.root.display()))?;
    let root_string = root.to_string_lossy().to_string();

    let db = Database::open_readonly(&request.index_path)
        .with_context(|| format!("open index {}", request.index_path.display()))?;
    // The index's size and mtime are not reliable for change detection, so both come from the
    // filesystem now. Files that have vanished since the scan are dropped (and pruned below).
    let files: Vec<EmbeddableFile> = db
        .embeddable_files(&root_string)?
        .into_iter()
        .filter_map(refresh_from_disk)
        .collect();
    drop(db);

    let sidecar = sidecar_path_for_index(&request.index_path);
    let store = EmbedStore::open(&sidecar)?;
    store.ensure_compatible(MODEL_ID, request.dims)?;
    let resumed = store.stats()?.files > 0;
    let active: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
    store.prune_to_active(&root_string, &active)?;
    store.set_complete(&root_string, false)?;

    // Stage by stage: every file gets stage 1 before any file gets stage 2, so the first results
    // arrive early. Each stage re-reads the files it needs (extraction is cheap). A stage that is
    // cancelled keeps what it wrote, and the next pass resumes at the earliest incomplete stage.
    let home = dirs::home_dir();
    let stages = request.depth.stages();
    let mut totals = WriterCounts::default();
    let mut extract_total = Duration::ZERO;
    let mut timing = PassTiming::default();
    let mut files_unchanged = 0;
    let mut cancelled = false;
    let mut stages_done = 0;
    for stage in 0..stages {
        let first_chunk = if stage == 0 { 0 } else { STAGE_ENDS[stage - 1] };
        let end_chunk = STAGE_ENDS[stage];
        let stage_started = Instant::now();
        let mut todo: Vec<EmbeddableFile> = Vec::new();
        for file in &files {
            let stamp = stamp_of(file);
            let needed = if stage == 0 {
                store.needs_embedding(&stamp, end_chunk)?
            } else {
                store.needs_deeper(&stamp, end_chunk)?
            };
            if needed {
                todo.push(file.clone());
            }
        }
        if stage == 0 {
            files_unchanged = files.len() - todo.len();
        }
        // Low-value files are recorded as skipped without being read (stage 1 only). The rest are
        // ordered by priority, so stopping early keeps the most useful files.
        let mut rule_by_path: HashMap<String, &'static str> = HashMap::new();
        let mut low_value_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for file in &todo {
            let absolute = Path::new(&file.path);
            let relative = absolute.strip_prefix(&root).unwrap_or(absolute);
            if let Some(rule) = never_read_rule(
                relative,
                absolute,
                home.as_deref(),
                &dir_has_vendor_manifest,
            ) {
                rule_by_path.insert(file.path.clone(), rule);
                *low_value_counts.entry(rule).or_default() += 1;
            }
        }
        if stage == 0 {
            timing.low_value = low_value_counts.into_iter().collect();
        }
        sort_for_embedding(&mut todo);
        let rules: Vec<Option<&'static str>> = todo
            .iter()
            .map(|file| rule_by_path.get(&file.path).copied())
            .collect();
        if stage == 0 {
            let _ = events.send(PassEvent::Started {
                total_files: todo.len(),
                backend: embedder.backend_label().to_string(),
                resumed,
            });
        }
        let _ = events.send(PassEvent::StageStarted {
            stage: stage + 1,
            stages,
            first_chunk,
            end_chunk,
            files: todo.len(),
        });

        let (counts, extract, embed_timing) = run_stages(
            embedder,
            &todo,
            &rules,
            request.dims,
            &sidecar,
            &root_string,
            &events,
            &cancel,
            (first_chunk, end_chunk),
        )?;
        let stage_cancelled = counts.handled != todo.len();
        extract_total += extract;
        timing.absorb(&embed_timing);
        timing.stages.push(StageTiming {
            stage: stage + 1,
            wall: stage_started.elapsed(),
            chunks: counts.chunks,
            tokens: embed_timing.tokens,
        });
        totals.embedded += counts.embedded;
        totals.skipped += counts.skipped;
        totals.failed += counts.failed;
        totals.chunks += counts.chunks;
        totals.write += counts.write;
        totals.files_done.extend(counts.files_done);
        if stage_cancelled {
            cancelled = true;
            break;
        }
        if stage == 0 {
            timing.first_stage = Some(started.elapsed());
        }
        stages_done = stage + 1;
        let _ = events.send(PassEvent::StageFinished {
            stage: stage + 1,
            stages,
        });
    }
    if !cancelled {
        store.set_complete(&root_string, true)?;
    }

    let summary = PassSummary {
        files_embedded: totals.files_done.len(),
        files_skipped: totals.skipped,
        files_failed: totals.failed,
        files_unchanged,
        chunks: totals.chunks,
        cancelled,
        elapsed: started.elapsed(),
        sidecar_bytes: store.stats()?.sidecar_bytes,
        timing: PassTiming {
            extract: extract_total,
            writer: totals.write,
            ..timing
        },
        depth: request.depth,
        stages_done,
    };
    let _ = events.send(PassEvent::Finished(summary.clone()));
    Ok(summary)
}

fn refresh_from_disk(file: EmbeddableFile) -> Option<EmbeddableFile> {
    // Follows symlinks, like the extractor does, so the stamp matches what gets read.
    let metadata = std::fs::metadata(&file.path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|age| age.as_secs() as i64)
        .unwrap_or(0);
    Some(EmbeddableFile {
        size_bytes: metadata.len(),
        modified_at,
        ..file
    })
}

fn stamp_of(file: &EmbeddableFile) -> FileStamp {
    FileStamp {
        path: file.path.clone(),
        mtime: file.modified_at,
        size: file.size_bytes as i64,
    }
}

/// Work produced by an extract worker. `index` points into the `todo` list.
enum Work {
    /// `chunks` are the file's chunks for this stage; `start` is the first ordinal among them and
    /// `chunks_total` how many the file has.
    Text {
        index: usize,
        chunks: Vec<Chunk>,
        start: usize,
        chunks_total: usize,
    },
    Image {
        index: usize,
    },
    Skipped {
        index: usize,
        reason: SkipReason,
    },
    Failed {
        index: usize,
        message: String,
    },
    LowValue {
        index: usize,
        detail: &'static str,
    },
}

/// Final per-file record sent to the writer.
struct Out {
    path: String,
    kind: String,
    stamp: FileStamp,
    status: FileStatus,
    chunks: Vec<StoredChunk>,
    detail: Option<String>,
    /// First ordinal of `chunks` (0 for a whole file) and the file's total chunk count.
    start: usize,
    chunks_total: usize,
}

#[derive(Debug, Default, Clone)]
struct WriterCounts {
    /// Outcomes received, one per file in the stage (fewer when the stage was cancelled).
    handled: usize,
    embedded: usize,
    skipped: usize,
    failed: usize,
    chunks: usize,
    write: Duration,
    /// Distinct files that gained chunks.
    files_done: HashSet<String>,
}

fn extract_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .clamp(1, MAX_EXTRACT_WORKERS)
}

/// Runs one stage over `todo`: extract workers read and chunk each file (keeping ordinals in
/// `range`), the embed stage embeds, and the writer stores the results. Returns when all threads
/// have finished.
#[allow(clippy::too_many_arguments)]
fn run_stages(
    embedder: &mut dyn Embedder,
    todo: &[EmbeddableFile],
    rules: &[Option<&'static str>],
    dims: usize,
    sidecar: &Path,
    root: &str,
    events: &Sender<PassEvent>,
    cancel: &Arc<AtomicBool>,
    range: (usize, usize),
) -> Result<(WriterCounts, Duration, PassTiming)> {
    let (work_tx, work_rx) = mpsc::sync_channel::<Work>(WORK_QUEUE);
    let (out_tx, out_rx) = mpsc::sync_channel::<Out>(WRITE_QUEUE);
    let next = AtomicUsize::new(0);
    let cancel_flag: &AtomicBool = cancel.as_ref();
    let workers = extract_worker_count();

    thread::scope(|scope| {
        let mut worker_handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let work_tx = work_tx.clone();
            let events = events.clone();
            let next = &next;
            worker_handles.push(scope.spawn(move || {
                extract_worker(todo, rules, next, cancel_flag, work_tx, events, range)
            }));
        }
        drop(work_tx);

        let writer_events = events.clone();
        let writer =
            scope.spawn(move || writer_loop(sidecar, root, out_rx, writer_events, range.0));

        // Consumes work_rx and out_tx; both are dropped when this returns.
        let embed_result = embed_stage(embedder, todo, dims, work_rx, out_tx, cancel_flag);

        let mut worker_panicked = false;
        let mut extract = Duration::ZERO;
        for handle in worker_handles {
            match handle.join() {
                Ok(busy) => extract += busy,
                Err(_) => worker_panicked = true,
            }
        }
        let writer_result = writer
            .join()
            .map_err(|_| anyhow!("embedding writer thread panicked"))?;
        if worker_panicked {
            return Err(anyhow!("extract worker thread panicked"));
        }
        let counts = writer_result?;
        let embed_timing = embed_result?;
        Ok((counts, extract, embed_timing))
    })
}

fn extract_worker(
    todo: &[EmbeddableFile],
    rules: &[Option<&'static str>],
    next: &AtomicUsize,
    cancel: &AtomicBool,
    work_tx: SyncSender<Work>,
    events: Sender<PassEvent>,
    range: (usize, usize),
) -> Duration {
    let mut busy = Duration::ZERO;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let index = next.fetch_add(1, Ordering::Relaxed);
        if index >= todo.len() {
            break;
        }
        let file = &todo[index];
        let _ = events.send(PassEvent::FileStarted {
            path: file.path.clone(),
        });
        if let Some(rule) = rules[index] {
            let detail = detail_for_rule(rule);
            if work_tx.send(Work::LowValue { index, detail }).is_err() {
                break;
            }
            continue;
        }
        let started = Instant::now();
        let work = catch_unwind(AssertUnwindSafe(|| extract_one(index, file, range)))
            .unwrap_or_else(|_| Work::Failed {
                index,
                message: "unexpected failure while reading the file".to_string(),
            });
        busy += started.elapsed();
        if work_tx.send(work).is_err() {
            break;
        }
    }
    busy
}

fn extract_one(index: usize, file: &EmbeddableFile, range: (usize, usize)) -> Work {
    let extracted = classify_and_extract(
        Path::new(&file.path),
        &file.kind,
        file.extension.as_deref(),
        file.size_bytes,
    );
    match extracted {
        Extracted::Text(text) => {
            let all = chunk_text(&text);
            let chunks_total = all.len();
            let (start, end) = range;
            let chunks: Vec<Chunk> = all
                .into_iter()
                .skip(start)
                .take(end.saturating_sub(start))
                .collect();
            if chunks.is_empty() {
                // Nothing in this stage: a file with no text, or one shorter than the stage start.
                Work::Skipped {
                    index,
                    reason: SkipReason::Empty,
                }
            } else {
                Work::Text {
                    index,
                    chunks,
                    start,
                    chunks_total,
                }
            }
        }
        Extracted::Image => Work::Image { index },
        Extracted::Skipped(reason) => Work::Skipped { index, reason },
        Extracted::Failed(message) => Work::Failed { index, message },
    }
}

/// Stores each file's outcome. `stage_start` is the first ordinal of this stage: after the first stage
/// a file that fails or is skipped keeps what it has, and the next pass retries it.
fn writer_loop(
    sidecar: &Path,
    root: &str,
    out_rx: Receiver<Out>,
    events: Sender<PassEvent>,
    stage_start: usize,
) -> Result<WriterCounts> {
    let store = EmbedStore::open(sidecar)?;
    let mut counts = WriterCounts::default();
    for out in out_rx {
        counts.handled += 1;
        if stage_start > 0 && out.status != FileStatus::Done {
            if out.status == FileStatus::Failed {
                counts.failed += 1;
            }
            let _ = events.send(PassEvent::FileFinished {
                path: out.path,
                status: out.status,
            });
            continue;
        }
        match out.status {
            FileStatus::Done => {
                counts.embedded += 1;
                counts.chunks += out.chunks.len();
                counts.files_done.insert(out.path.clone());
                let _ = events.send(PassEvent::ChunksEmbedded {
                    path: out.path.clone(),
                    kind: out.kind.clone(),
                    vectors: out
                        .chunks
                        .iter()
                        .map(|chunk| chunk.vector.clone())
                        .collect(),
                });
            }
            FileStatus::Skipped => counts.skipped += 1,
            FileStatus::Failed => counts.failed += 1,
        }
        let write_started = Instant::now();
        store.write_stage(
            root,
            &out.stamp,
            out.status,
            &out.chunks,
            out.detail.as_deref(),
            out.start,
            out.chunks_total,
        )?;
        counts.write += write_started.elapsed();
        let _ = events.send(PassEvent::FileFinished {
            path: out.path,
            status: out.status,
        });
    }
    Ok(counts)
}

/// A chunk waiting for its vector.
struct QueuedChunk {
    file: usize,
    chunk: usize,
    input: String,
}

/// A file whose chunks are partly embedded.
struct Pending {
    chunks: Vec<Chunk>,
    vectors: Vec<Option<Vec<f32>>>,
    remaining: usize,
    kind: String,
    start: usize,
    chunks_total: usize,
}

struct EmbedState {
    batch: usize,
    queue: VecDeque<QueuedChunk>,
    pending: HashMap<usize, Pending>,
    timing: PassTiming,
    /// Raw vectors of chunks already embedded in this pass, keyed by text hash.
    cache: HashMap<u64, (String, Vec<f32>)>,
}

/// Maximum distinct chunk texts remembered for dedupe (about 60 MB of vectors at 768 dims).
const MAX_CACHE: usize = 20_000;

/// FNV-1a over the chunk text. A hash hit is still confirmed by comparing the text.
fn text_key(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn embed_stage(
    embedder: &mut dyn Embedder,
    todo: &[EmbeddableFile],
    dims: usize,
    work_rx: Receiver<Work>,
    out_tx: SyncSender<Out>,
    cancel: &AtomicBool,
) -> Result<PassTiming> {
    let mut state = EmbedState {
        batch: INITIAL_BATCH,
        queue: VecDeque::new(),
        pending: HashMap::new(),
        timing: PassTiming::default(),
        cache: HashMap::new(),
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(state.timing);
        }
        let wait = if state.queue.is_empty() {
            IDLE_WAIT
        } else {
            IDLE_FLUSH
        };
        let waited = Instant::now();
        let received = work_rx.recv_timeout(wait);
        state.timing.embed_wait += waited.elapsed();
        let (idle, disconnected) = match received {
            Ok(work) => {
                state.accept(embedder, todo, dims, work, &out_tx)?;
                (false, false)
            }
            Err(RecvTimeoutError::Timeout) => (true, false),
            Err(RecvTimeoutError::Disconnected) => (false, true),
        };
        while !state.queue.is_empty() && (state.queue.len() >= state.batch || idle || disconnected)
        {
            if cancel.load(Ordering::Relaxed) {
                return Ok(state.timing);
            }
            state.flush(embedder, todo, dims, &out_tx)?;
        }
        if disconnected && state.queue.is_empty() {
            return Ok(state.timing);
        }
    }
}

impl EmbedState {
    fn accept(
        &mut self,
        embedder: &mut dyn Embedder,
        todo: &[EmbeddableFile],
        dims: usize,
        work: Work,
        out_tx: &SyncSender<Out>,
    ) -> Result<()> {
        match work {
            Work::Text {
                index,
                chunks,
                start,
                chunks_total,
            } => {
                let kind = todo[index].kind.clone();
                let count = chunks.len();
                let mut reused: Vec<(usize, Vec<f32>)> = Vec::new();
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    match self.cache.get(&text_key(&chunk.text)) {
                        Some((text, vector)) if *text == chunk.text => {
                            reused.push((chunk_index, vector.clone()));
                        }
                        _ => self.queue.push_back(QueuedChunk {
                            file: index,
                            chunk: chunk_index,
                            input: document_input(&chunk.text),
                        }),
                    }
                }
                self.timing.deduplicated += reused.len();
                self.pending.insert(
                    index,
                    Pending {
                        chunks,
                        vectors: vec![None; count],
                        remaining: count,
                        kind,
                        start,
                        chunks_total,
                    },
                );
                for (chunk_index, vector) in reused {
                    self.assign(index, chunk_index, vector, todo, dims, out_tx)?;
                }
                Ok(())
            }
            Work::LowValue { index, detail } => send_out(
                out_tx,
                out_for(
                    todo,
                    index,
                    FileStatus::Skipped,
                    Vec::new(),
                    Some(detail.to_string()),
                    &todo[index].kind,
                ),
            ),
            Work::Image { index } => {
                if !embedder.supports_images() {
                    return send_out(
                        out_tx,
                        out_for(
                            todo,
                            index,
                            FileStatus::Skipped,
                            Vec::new(),
                            Some("vision model not installed".to_string()),
                            "image",
                        ),
                    );
                }
                let path = Path::new(&todo[index].path);
                let image_started = Instant::now();
                let outcome = catch_unwind(AssertUnwindSafe(|| embedder.embed_image(path)));
                self.timing.gpu += image_started.elapsed();
                self.timing.calls += 1;
                match outcome {
                    Ok(Ok(vector)) => {
                        let chunk = StoredChunk {
                            ordinal: 0,
                            start_byte: 0,
                            snippet: file_name_of(&todo[index].path),
                            vector: truncate_and_normalize(&vector, dims),
                        };
                        let mut out =
                            out_for(todo, index, FileStatus::Done, vec![chunk], None, "image");
                        out.chunks_total = 1;
                        send_out(out_tx, out)
                    }
                    Ok(Err(error)) => send_out(
                        out_tx,
                        out_for(
                            todo,
                            index,
                            FileStatus::Failed,
                            Vec::new(),
                            Some(format!("{error:#}")),
                            "image",
                        ),
                    ),
                    Err(_) => send_out(
                        out_tx,
                        out_for(
                            todo,
                            index,
                            FileStatus::Failed,
                            Vec::new(),
                            Some("image embedding panicked".to_string()),
                            "image",
                        ),
                    ),
                }
            }
            Work::Skipped { index, reason } => send_out(
                out_tx,
                out_for(
                    todo,
                    index,
                    FileStatus::Skipped,
                    Vec::new(),
                    Some(skip_detail(reason).to_string()),
                    &todo[index].kind,
                ),
            ),
            Work::Failed { index, message } => send_out(
                out_tx,
                out_for(
                    todo,
                    index,
                    FileStatus::Failed,
                    Vec::new(),
                    Some(message),
                    &todo[index].kind,
                ),
            ),
        }
    }

    /// Embeds one batch from the queue. On error the batch is retried one file at a time, so
    /// only the file that causes the failure is marked Failed.
    fn flush(
        &mut self,
        embedder: &mut dyn Embedder,
        todo: &[EmbeddableFile],
        dims: usize,
        out_tx: &SyncSender<Out>,
    ) -> Result<()> {
        let take = self.batch.min(self.queue.len());
        let items: Vec<QueuedChunk> = self.queue.drain(..take).collect();
        let inputs: Vec<String> = items.iter().map(|item| item.input.clone()).collect();

        let tokens_before = embedder.tokens_embedded();
        let started = Instant::now();
        let outcome = guarded_embed(embedder, &inputs);
        let elapsed = started.elapsed();
        self.timing.gpu += elapsed;
        self.timing.calls += 1;
        self.timing.tokens += embedder.tokens_embedded().saturating_sub(tokens_before);
        if elapsed < FAST_BATCH && !self.queue.is_empty() {
            self.batch = (self.batch * 2).min(MAX_BATCH);
        } else if elapsed > SLOW_BATCH {
            self.batch = (self.batch / 2).max(MIN_BATCH);
        }

        match outcome {
            Ok(vectors) => {
                for (item, vector) in items.iter().zip(vectors) {
                    self.assign(item.file, item.chunk, vector, todo, dims, out_tx)?;
                }
            }
            Err(_) => {
                let mut groups: BTreeMap<usize, Vec<(usize, String)>> = BTreeMap::new();
                for item in &items {
                    groups
                        .entry(item.file)
                        .or_default()
                        .push((item.chunk, item.input.clone()));
                }
                for (file, group) in groups {
                    let inputs: Vec<String> =
                        group.iter().map(|(_, input)| input.clone()).collect();
                    match guarded_embed(embedder, &inputs) {
                        Ok(vectors) => {
                            for ((chunk, _), vector) in group.iter().zip(vectors) {
                                self.assign(file, *chunk, vector, todo, dims, out_tx)?;
                            }
                        }
                        Err(message) => self.fail_file(file, message, todo, out_tx)?,
                    }
                }
            }
        }
        Ok(())
    }

    fn assign(
        &mut self,
        file: usize,
        chunk: usize,
        vector: Vec<f32>,
        todo: &[EmbeddableFile],
        dims: usize,
        out_tx: &SyncSender<Out>,
    ) -> Result<()> {
        let Some(pending) = self.pending.get_mut(&file) else {
            return Ok(());
        };
        if self.cache.len() < MAX_CACHE {
            if let Some(info) = pending.chunks.get(chunk) {
                let key = text_key(&info.text);
                self.cache
                    .entry(key)
                    .or_insert_with(|| (info.text.clone(), vector.clone()));
            }
        }
        pending.vectors[chunk] = Some(vector);
        pending.remaining = pending.remaining.saturating_sub(1);
        if pending.remaining > 0 {
            return Ok(());
        }
        let Pending {
            chunks,
            vectors,
            kind,
            start,
            chunks_total,
            ..
        } = self
            .pending
            .remove(&file)
            .expect("pending entry is present");
        let finished: Option<Vec<StoredChunk>> = chunks
            .into_iter()
            .zip(vectors)
            .map(|(chunk, vector)| {
                vector.map(|vector| StoredChunk {
                    ordinal: chunk.ordinal,
                    start_byte: chunk.start_byte,
                    snippet: chunk.snippet,
                    vector: truncate_and_normalize(&vector, dims),
                })
            })
            .collect();
        match finished {
            Some(stored) => {
                let mut out = out_for(todo, file, FileStatus::Done, stored, None, &kind);
                out.start = start;
                out.chunks_total = chunks_total;
                send_out(out_tx, out)
            }
            None => self.fail_file(
                file,
                "embedding left a chunk without a vector".to_string(),
                todo,
                out_tx,
            ),
        }
    }

    fn fail_file(
        &mut self,
        file: usize,
        message: String,
        todo: &[EmbeddableFile],
        out_tx: &SyncSender<Out>,
    ) -> Result<()> {
        let kind = self
            .pending
            .remove(&file)
            .map(|pending| pending.kind)
            .unwrap_or_else(|| todo[file].kind.clone());
        self.queue.retain(|item| item.file != file);
        send_out(
            out_tx,
            out_for(
                todo,
                file,
                FileStatus::Failed,
                Vec::new(),
                Some(message),
                &kind,
            ),
        )
    }
}

/// Runs `embed_batch` and turns errors and panics into a message.
fn guarded_embed(
    embedder: &mut dyn Embedder,
    inputs: &[String],
) -> std::result::Result<Vec<Vec<f32>>, String> {
    match catch_unwind(AssertUnwindSafe(|| embedder.embed_batch(inputs))) {
        Ok(Ok(vectors)) if vectors.len() == inputs.len() => Ok(vectors),
        Ok(Ok(vectors)) => Err(format!(
            "embedder returned {} vectors for {} inputs",
            vectors.len(),
            inputs.len()
        )),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(_) => Err("embedder panicked".to_string()),
    }
}

fn out_for(
    todo: &[EmbeddableFile],
    index: usize,
    status: FileStatus,
    chunks: Vec<StoredChunk>,
    detail: Option<String>,
    kind: &str,
) -> Out {
    let file = &todo[index];
    Out {
        path: file.path.clone(),
        kind: kind.to_string(),
        stamp: stamp_of(file),
        status,
        chunks,
        detail,
        start: 0,
        chunks_total: 0,
    }
}

fn send_out(out_tx: &SyncSender<Out>, out: Out) -> Result<()> {
    out_tx
        .send(out)
        .map_err(|_| anyhow!("embedding writer stopped unexpectedly"))
}

fn skip_detail(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::TooLarge => "file too large",
        SkipReason::Binary => "binary file",
        SkipReason::UnsupportedKind => "unsupported file type",
        SkipReason::Empty => "no text to embed",
        SkipReason::PrivateKey => "private key",
    }
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

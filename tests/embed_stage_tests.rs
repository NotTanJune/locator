//! Staged embedding passes (fake embedder, no model): stage order, quick and balanced depths,
//! cancel and resume, modified files, deeper and shallower reruns, search while a pass runs, and
//! migration of an old-schema sidecar.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use anyhow::Result;
use locator::db::Database;
use locator::embed::fake::HashEmbedder;
use locator::embed::pipeline::{
    run_pass_with_embedder, Depth, PassEvent, PassRequest, PassSummary, MODEL_ID, STAGE_ENDS,
};
use locator::embed::runtime::Embedder;
use locator::embed::store::{
    sidecar_path_for_index, EmbedStore, FileStamp, FileStatus, StoredChunk, VectorIndex,
};
use locator::embed::vector::{encode_f16, truncate_and_normalize};
use locator::scanner::{scan_root, ScanOptions};
use rusqlite::Connection;

const DIMS: usize = 768;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    index: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    let index = dir.path().join("index.sqlite");
    Fixture {
        _dir: dir,
        root,
        index,
    }
}

/// Paragraphs of about 900 characters, so chunks split at paragraph boundaries.
fn long_text(tag: &str, paragraphs: usize) -> String {
    let filler =
        "the quick brown fox jumps over the lazy dog while the team reviews budgets and plans "
            .repeat(11);
    (0..paragraphs)
        .map(|i| format!("{tag} section {i}: {filler}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Three files: a long one (more than 32 chunks), a medium one (between 8 and 32), a short one.
fn write_tree(root: &Path) {
    fs::write(root.join("big_a.txt"), long_text("alpha", 400)).expect("big_a");
    fs::write(root.join("big_b.txt"), long_text("beta", 60)).expect("big_b");
    fs::write(root.join("small.txt"), long_text("gamma", 3)).expect("small");
}

fn scan_into_index(root: &Path, index: &Path) {
    let db = Database::open(index).expect("open index");
    scan_root(&db, root, ScanOptions::default()).expect("scan");
    db.checkpoint().expect("checkpoint");
}

/// Counts the texts it embeds, can cancel the pass after a count, and can pause the pass at a count
/// (so a test can read the sidecar while the pass is in progress).
struct Probe {
    inner: HashEmbedder,
    seen: Arc<AtomicUsize>,
    cancel_at: Option<(usize, Arc<AtomicBool>)>,
    gate: Option<Gate>,
}

struct Gate {
    at: usize,
    reached: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
    taken: bool,
}

impl Probe {
    fn new() -> Probe {
        Probe {
            inner: HashEmbedder,
            seen: Arc::new(AtomicUsize::new(0)),
            cancel_at: None,
            gate: None,
        }
    }
}

impl Embedder for Probe {
    fn backend_label(&self) -> &str {
        "fake"
    }
    fn supports_images(&self) -> bool {
        true
    }
    fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let before = self.seen.fetch_add(inputs.len(), Ordering::SeqCst);
        if let Some((limit, flag)) = &self.cancel_at {
            if before + inputs.len() >= *limit {
                flag.store(true, Ordering::SeqCst);
            }
        }
        if let Some(gate) = self.gate.as_mut() {
            if !gate.taken && before >= gate.at {
                gate.taken = true;
                let _ = gate.reached.send(());
                let _ = gate.resume.recv();
            }
        }
        self.inner.embed_batch(inputs)
    }
    fn embed_image(&mut self, path: &Path) -> Result<Vec<f32>> {
        self.inner.embed_image(path)
    }
}

fn request(fixture: &Fixture, depth: Depth) -> PassRequest {
    PassRequest {
        index_path: fixture.index.clone(),
        root: fixture.root.clone(),
        dims: DIMS,
        text_model: None,
        vision_model: None,
        fake: true,
        depth,
    }
}

/// Runs one pass with `probe`, returning the summary and every event in order.
fn run(fixture: &Fixture, depth: Depth, probe: &mut Probe) -> (PassSummary, Vec<PassEvent>) {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let summary = run_pass_with_embedder(&request(fixture, depth), tx, cancel, probe)
        .expect("embedding pass");
    (summary, rx.try_iter().collect())
}

/// Texts embedded by a pass with a fresh probe, and the summary.
fn run_counted(fixture: &Fixture, depth: Depth) -> (PassSummary, usize) {
    let mut probe = Probe::new();
    let (summary, _) = run(fixture, depth, &mut probe);
    (summary, probe.seen.load(Ordering::SeqCst))
}

fn sidecar(fixture: &Fixture) -> PathBuf {
    sidecar_path_for_index(&fixture.index)
}

/// (file name, depth_done, chunks_total, chunk_count) for every row in the sidecar.
fn progress(fixture: &Fixture) -> Vec<(String, i64, i64, i64)> {
    let conn = Connection::open(sidecar(fixture)).expect("open sidecar");
    let mut statement = conn
        .prepare(
            "SELECT path, depth_done, chunks_total, chunk_count FROM embed_files ORDER BY path",
        )
        .expect("prepare");
    let rows = statement
        .query_map([], |row| {
            let path: String = row.get(0)?;
            let name = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(path);
            Ok((name, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query");
    rows.map(|row| row.expect("row")).collect()
}

fn depth_of(fixture: &Fixture, name: &str) -> (i64, i64, i64) {
    progress(fixture)
        .into_iter()
        .find(|(file, ..)| file == name)
        .map(|(_, done, total, count)| (done, total, count))
        .expect("file row")
}

fn total_chunks(fixture: &Fixture) -> u64 {
    EmbedStore::open(&sidecar(fixture))
        .expect("open")
        .stats()
        .expect("stats")
        .chunks
}

/// Texts that stage 1 embeds for this tree: the first min(total, 8) chunks of every file.
fn stage_one_inputs(reference: &Fixture) -> usize {
    let (_, count) = run_counted(reference, Depth::Quick);
    count
}

fn index_of(events: &[PassEvent], wanted: impl Fn(&PassEvent) -> bool) -> usize {
    events.iter().position(wanted).expect("event is present")
}

#[test]
fn every_file_gets_stage_one_before_any_file_gets_stage_two() {
    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    let mut probe = Probe::new();
    let (summary, events) = run(&fx, Depth::Deep, &mut probe);
    assert_eq!(summary.stages_done, 3);
    assert!(!summary.cancelled);

    let stage_two = index_of(&events, |e| {
        matches!(e, PassEvent::StageStarted { stage: 2, .. })
    });
    let stage_three = index_of(&events, |e| {
        matches!(e, PassEvent::StageStarted { stage: 3, .. })
    });
    assert!(stage_two < stage_three);
    for (position, event) in events.iter().enumerate() {
        if let PassEvent::ChunksEmbedded { path, vectors, .. } = event {
            if position < stage_two {
                assert!(
                    vectors.len() <= STAGE_ENDS[0],
                    "{path}: stage 1 batch of {}",
                    vectors.len()
                );
            } else {
                assert!(!vectors.is_empty(), "{path}: a later stage must add chunks");
            }
        }
    }
    // Every file that has chunks is written in stage 1, before any stage-2 write.
    for name in ["big_a.txt", "big_b.txt", "small.txt"] {
        let wrote_in_stage_one = events[..stage_two].iter().any(
            |event| matches!(event, PassEvent::ChunksEmbedded { path, .. } if path.ends_with(name)),
        );
        assert!(wrote_in_stage_one, "{name} has no stage-1 write");
    }
}

#[test]
fn quick_stops_after_the_first_eight_chunks() {
    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    let (summary, _) = run_counted(&fx, Depth::Quick);
    assert_eq!(summary.stages_done, 1);
    assert_eq!(summary.depth, Depth::Quick);
    let (done, total, count) = depth_of(&fx, "big_a.txt");
    assert_eq!((done, count), (8, 8));
    assert!(total > 32, "big_a has {total} chunks");
    let (done, total, _) = depth_of(&fx, "small.txt");
    assert_eq!(
        done, total,
        "a file shorter than 8 chunks is complete after stage 1"
    );
    let store = EmbedStore::open(&sidecar(&fx)).expect("open");
    assert_eq!(
        store
            .is_complete(&fx.root.canonicalize().expect("canonical").to_string_lossy())
            .expect("flag"),
        Some(true)
    );
}

#[test]
fn cancel_in_stage_two_then_resume_embeds_only_what_is_missing() {
    let reference = fixture();
    write_tree(&reference.root);
    scan_into_index(&reference.root, &reference.index);
    let (full, _) = run_counted(&reference, Depth::Deep);
    assert!(!full.cancelled);
    let all_chunks = total_chunks(&reference);
    let stage_one = stage_one_inputs(&fixture_with_tree());

    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    let flag = Arc::new(AtomicBool::new(false));
    let mut first = Probe::new();
    first.cancel_at = Some((stage_one + 5, Arc::clone(&flag)));
    let (tx, _rx) = mpsc::channel();
    let cancelled = run_pass_with_embedder(&request(&fx, Depth::Deep), tx, flag, &mut first)
        .expect("first pass");
    assert!(cancelled.cancelled, "the pass should stop early");
    let stored = total_chunks(&fx);
    assert!(
        stored as usize >= stage_one,
        "stage 1 finished before the cancel: {stored}"
    );
    assert!(stored < all_chunks);

    let (resumed, inputs) = run_counted(&fx, Depth::Deep);
    assert!(!resumed.cancelled);
    assert_eq!(
        inputs as u64,
        all_chunks - stored,
        "only the missing chunks are embedded"
    );
    assert_eq!(total_chunks(&fx), all_chunks);
}

#[test]
fn modified_file_restarts_at_ordinal_zero() {
    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    run_counted(&fx, Depth::Deep);
    let (_, total_before, _) = depth_of(&fx, "big_a.txt");
    assert!(total_before > 32);

    fs::write(fx.root.join("big_a.txt"), long_text("alpha changed", 12)).expect("rewrite");
    scan_into_index(&fx.root, &fx.index);
    let (summary, _) = run_counted(&fx, Depth::Deep);
    assert!(summary.files_embedded >= 1);
    let (done, total, count) = depth_of(&fx, "big_a.txt");
    assert_eq!(done, total, "the rewritten file is complete again");
    assert_eq!(count, total);
    assert!(total < total_before, "the new text has fewer chunks");
    let conn = Connection::open(sidecar(&fx)).expect("open");
    let (lowest, rows): (i64, i64) = conn
        .query_row(
            "SELECT MIN(ordinal), COUNT(*) FROM chunks WHERE path LIKE '%big_a.txt'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("chunk rows");
    assert_eq!((lowest, rows), (0, total));
}

#[test]
fn deeper_reruns_add_only_the_missing_chunks() {
    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    let (quick, quick_inputs) = run_counted(&fx, Depth::Quick);
    assert_eq!(quick.stages_done, 1);
    let totals: Vec<i64> = progress(&fx)
        .iter()
        .map(|(_, _, total, _)| *total)
        .collect();

    let expected_balanced: i64 = totals
        .iter()
        .map(|&t| t.min(32) - t.min(8))
        .filter(|n| *n > 0)
        .sum();
    let (balanced, balanced_inputs) = run_counted(&fx, Depth::Balanced);
    assert_eq!(balanced.stages_done, 2);
    assert_eq!(balanced_inputs as i64, expected_balanced);

    let expected_deep: i64 = totals
        .iter()
        .map(|&t| t.min(128) - t.min(32))
        .filter(|n| *n > 0)
        .sum();
    let (deep, deep_inputs) = run_counted(&fx, Depth::Deep);
    assert_eq!(deep.stages_done, 3);
    assert_eq!(deep_inputs as i64, expected_deep);
    assert_eq!(
        total_chunks(&fx) as i64,
        totals.iter().map(|&t| t.min(128)).sum::<i64>()
    );
    assert!(quick_inputs > 0);
}

#[test]
fn shallower_reruns_embed_nothing_and_delete_nothing() {
    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    run_counted(&fx, Depth::Deep);
    let before = total_chunks(&fx);
    for depth in [Depth::Balanced, Depth::Quick] {
        let (summary, inputs) = run_counted(&fx, depth);
        assert_eq!(inputs, 0, "{depth:?} embeds nothing after deep");
        assert_eq!(summary.files_embedded, 0);
        assert_eq!(total_chunks(&fx), before, "{depth:?} deletes nothing");
    }
}

#[test]
fn search_sees_stage_one_while_stage_two_is_running() {
    let reference = fixture();
    write_tree(&reference.root);
    scan_into_index(&reference.root, &reference.index);
    let stage_one = stage_one_inputs(&reference);

    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let request = request(&fx, Depth::Deep);
    let worker = thread::spawn(move || {
        let mut probe = Probe::new();
        probe.gate = Some(Gate {
            at: stage_one,
            reached: reached_tx,
            resume: resume_rx,
            taken: false,
        });
        let (tx, _rx) = mpsc::channel();
        run_pass_with_embedder(&request, tx, Arc::new(AtomicBool::new(false)), &mut probe)
            .expect("pass")
    });
    reached_rx.recv().expect("stage 2 reached");
    let store = EmbedStore::open_readonly(&sidecar(&fx))
        .expect("open readonly")
        .expect("sidecar exists");
    let index = VectorIndex::load(&store).expect("load");
    assert_eq!(
        index.len(),
        stage_one,
        "stage-1 chunks are visible during stage 2"
    );
    resume_tx.send(()).expect("resume");
    let summary = worker.join().expect("worker");
    assert!(!summary.cancelled);
}

/// A tree written to a fresh directory, for the stage-one count.
fn fixture_with_tree() -> Fixture {
    let fx = fixture();
    write_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    fx
}

#[test]
fn old_schema_sidecar_migrates_in_place_without_wiping() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("embeddings.sqlite");
    let vector =
        truncate_and_normalize(&(0..DIMS).map(|i| (i % 7) as f32).collect::<Vec<_>>(), DIMS);
    {
        // The schema before staged passes: no chunks_total or depth_done columns.
        let conn = Connection::open(&path).expect("create");
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE embed_files (
                 path TEXT PRIMARY KEY, root TEXT NOT NULL, mtime INTEGER NOT NULL,
                 size INTEGER NOT NULL, status INTEGER NOT NULL, chunk_count INTEGER NOT NULL,
                 detail TEXT);
             CREATE TABLE chunks (
                 id INTEGER PRIMARY KEY, path TEXT NOT NULL, ordinal INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL, snippet TEXT NOT NULL, vec BLOB NOT NULL,
                 UNIQUE(path, ordinal));",
        )
        .expect("old schema");
        for (key, value) in [
            ("model", MODEL_ID),
            ("dims", "768"),
            ("schema_version", "1"),
        ] {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                [key, value],
            )
            .expect("meta");
        }
        conn.execute(
            "INSERT INTO embed_files (path, root, mtime, size, status, chunk_count) VALUES ('/r/a.txt', '/r', 1, 1, 1, 2)",
            [],
        )
        .expect("row");
        for ordinal in 0..2 {
            conn.execute(
                "INSERT INTO chunks (path, ordinal, start_byte, snippet, vec) VALUES ('/r/a.txt', ?1, 0, 'x', ?2)",
                rusqlite::params![ordinal, encode_f16(&vector)],
            )
            .expect("chunk");
        }
    }

    let store = EmbedStore::open(&path).expect("migrate");
    assert_eq!(store.stats().expect("stats").chunks, 2, "vectors are kept");
    assert!(
        !store.ensure_compatible(MODEL_ID, DIMS).expect("same model"),
        "not wiped"
    );
    let stamp = FileStamp {
        path: "/r/a.txt".to_string(),
        mtime: 1,
        size: 1,
    };
    assert!(!store.needs_embedding(&stamp, STAGE_ENDS[2]).expect("needs"));
    assert!(!store.needs_deeper(&stamp, STAGE_ENDS[2]).expect("deeper"));
    let conn = Connection::open(&path).expect("reopen");
    let (done, total): (i64, i64) = conn
        .query_row(
            "SELECT depth_done, chunks_total FROM embed_files WHERE path = '/r/a.txt'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("progress");
    assert_eq!((done, total), (2, 2));
    // Opening again changes nothing.
    drop(store);
    EmbedStore::open(&path).expect("second open");
}

#[test]
fn stage_writes_append_and_refuse_a_wrong_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = EmbedStore::open(&dir.path().join("s.sqlite")).expect("open");
    let stamp = FileStamp {
        path: "/r/b.txt".to_string(),
        mtime: 5,
        size: 9,
    };
    let chunk = |ordinal: u32| StoredChunk {
        ordinal,
        start_byte: u64::from(ordinal),
        snippet: "s".to_string(),
        vector: truncate_and_normalize(&vec![1.0; DIMS], DIMS),
    };
    let first: Vec<StoredChunk> = (0..8).map(chunk).collect();
    store
        .write_stage("/r", &stamp, FileStatus::Done, &first, None, 0, 40)
        .expect("stage 1");
    assert!(store.needs_deeper(&stamp, 32).expect("deeper"));
    let second: Vec<StoredChunk> = (8..32).map(chunk).collect();
    assert!(
        store
            .write_stage("/r", &stamp, FileStatus::Done, &second, None, 4, 40)
            .is_err(),
        "a stage must start at the stored depth"
    );
    store
        .write_stage("/r", &stamp, FileStatus::Done, &second, None, 8, 40)
        .expect("stage 2");
    assert_eq!(store.stats().expect("stats").chunks, 32);
    assert!(!store.needs_deeper(&stamp, 32).expect("done to 32"));
    assert!(store.needs_deeper(&stamp, 128).expect("more to go"));
}

#[test]
fn depth_parses_and_stage_wording_is_stable() {
    assert_eq!(Depth::parse("Quick"), Some(Depth::Quick));
    assert_eq!(Depth::parse(" balanced "), Some(Depth::Balanced));
    assert_eq!(Depth::parse("deep"), Some(Depth::Deep));
    assert_eq!(Depth::parse("shallow"), None);
    assert_eq!(Depth::default(), Depth::Quick);
    assert_eq!(
        (
            Depth::Quick.stages(),
            Depth::Balanced.stages(),
            Depth::Deep.stages()
        ),
        (1, 2, 3)
    );
    assert_eq!(STAGE_ENDS, [8, 32, 128]);
    assert_eq!(
        locator::embed::pipeline::stage_label(1, 3),
        "pass 1 of 3: first 8 chunks of each file"
    );
    assert_eq!(
        locator::embed::pipeline::stage_label(2, 3),
        "pass 2 of 3: chunks 9 to 32 of each file"
    );
}

#[test]
fn rows_without_progress_columns_count_as_complete_as_far_as_they_go() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("null.sqlite");
    let store = EmbedStore::open(&path).expect("open");
    store.ensure_compatible(MODEL_ID, DIMS).expect("stamp");
    drop(store);
    {
        // Rows as an older build writes them: no chunks_total or depth_done values at all.
        let conn = Connection::open(&path).expect("raw");
        conn.execute(
            "INSERT INTO embed_files (path, root, mtime, size, status, chunk_count, detail)
             VALUES ('/r/done.txt', '/r', 1, 1, 1, 8, NULL)",
            [],
        )
        .expect("old row");
        conn.execute(
            "INSERT INTO embed_files (path, root, mtime, size, status, chunk_count, detail)
             VALUES ('/r/small.txt', '/r', 2, 2, 1, 3, NULL)",
            [],
        )
        .expect("old small row");
        let vector = encode_f16(&truncate_and_normalize(&vec![1.0; DIMS], DIMS));
        for (path, count) in [("/r/done.txt", 8), ("/r/small.txt", 3)] {
            for ordinal in 0..count {
                conn.execute(
                    "INSERT INTO chunks (path, ordinal, start_byte, snippet, vec) VALUES (?1, ?2, 0, 's', ?3)",
                    rusqlite::params![path, ordinal, vector],
                )
                .expect("old chunk");
            }
        }
    }
    let store = EmbedStore::open(&path).expect("reopen");
    let done = FileStamp {
        path: "/r/done.txt".to_string(),
        mtime: 1,
        size: 1,
    };
    let small = FileStamp {
        path: "/r/small.txt".to_string(),
        mtime: 2,
        size: 2,
    };
    // 8 chunks were stored, the file is treated as having 8: quick, balanced and deep do no work.
    assert!(!store.needs_embedding(&done, STAGE_ENDS[0]).expect("quick"));
    assert!(!store.needs_deeper(&done, STAGE_ENDS[1]).expect("balanced"));
    assert!(!store.needs_deeper(&done, STAGE_ENDS[2]).expect("deep"));
    assert!(!store.needs_embedding(&small, STAGE_ENDS[2]).expect("small"));
    // A later stage cannot start on such a row unless it begins where the stored depth ends.
    let extra: Vec<StoredChunk> = (8..10)
        .map(|ordinal| StoredChunk {
            ordinal,
            start_byte: 0,
            snippet: "s".to_string(),
            vector: truncate_and_normalize(&vec![1.0; DIMS], DIMS),
        })
        .collect();
    assert!(store
        .write_stage("/r", &done, FileStatus::Done, &extra, None, 4, 40)
        .is_err());
    assert_eq!(
        store.stats().expect("stats").chunks,
        11,
        "nothing was lost or redone"
    );
}

/// Every user-facing depth sentence is built from STAGE_ENDS, so the numbers cannot drift from the
/// passes the code actually runs (the config description and the --depth help are checked against
/// the same numbers in tests/embed_cli_tests.rs and by a compile-time assertion in pipeline.rs).
#[test]
fn depth_wording_is_derived_from_stage_ends() {
    use locator::embed::pipeline::{depth_text, pass_range_text};
    let [first, second, third] = STAGE_ENDS;
    assert_eq!(
        pass_range_text(1),
        format!("first {first} chunks of each file")
    );
    assert_eq!(
        pass_range_text(2),
        format!("chunks {} to {second} of each file", first + 1)
    );
    assert_eq!(
        pass_range_text(3),
        format!("chunks {} to {third} of each file", second + 1)
    );
    assert_eq!(
        depth_text(Depth::Quick, 1, false),
        format!(
            "depth: quick (first {first} chunks of each file). --depth balanced adds chunks {} to {second} and --depth deep adds chunks up to {third}.",
            first + 1
        )
    );
    assert_eq!(
        depth_text(Depth::Balanced, 2, false),
        format!("depth: balanced (up to {second} chunks per file). --depth deep adds chunks up to {third}.")
    );
    assert_eq!(
        depth_text(Depth::Deep, 3, false),
        format!("depth: deep (up to {third} chunks per file).")
    );
    assert_eq!(
        depth_text(Depth::Deep, 1, true),
        "depth: deep stopped in pass 2 of 3. Progress is saved; rerun the same command to resume."
    );
    assert_eq!(
        depth_text(Depth::Quick, 0, true),
        "depth: quick stopped in pass 1 of 1. Progress is saved; rerun the same command to resume."
    );
    let description = locator::config::key_description("embed_depth");
    assert!(
        description.contains(&format!("first {first} chunks")),
        "{description}"
    );
    assert!(
        description.contains(&format!("up to {second} chunks per file")),
        "{description}"
    );
    assert!(
        description.contains(&format!("up to {third} chunks per file")),
        "{description}"
    );
    assert!(description.contains("quick (default)"), "{description}");
}

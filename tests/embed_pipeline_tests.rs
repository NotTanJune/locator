use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use locator::db::Database;
use locator::embed::fake::HashEmbedder;
use locator::embed::pipeline::{
    run_pass, run_pass_with_embedder, PassEvent, PassRequest, PassSummary,
};
use locator::embed::runtime::{document_input, query_input, Embedder};
use locator::embed::store::{
    sidecar_path_for_index, EmbedStore, FileStamp, FileStatus, StoredChunk, VectorIndex,
};
use locator::embed::vector::truncate_and_normalize;
use locator::scanner::{scan_root, ScanOptions};

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

fn write_standard_tree(root: &Path) {
    fs::write(
        root.join("invoice.txt"),
        "budget spreadsheet for marketing, quarterly spend and invoices",
    )
    .expect("invoice");
    fs::write(
        root.join("soup.md"),
        "# Soup\n\nTomato soup recipe with basil and garlic.",
    )
    .expect("soup");
    fs::write(
        root.join("server.rs"),
        "fn start_http_server(port: u16) { listen(port) }",
    )
    .expect("server");
    fs::write(root.join("blob.bin"), b"\x00\x01binary\x00data").expect("blob");
    fs::write(root.join("garbage.pdf"), b"this is not a pdf").expect("pdf");
    image::RgbImage::from_pixel(1, 1, image::Rgb([10, 20, 30]))
        .save(root.join("pixel.png"))
        .expect("png");
}

fn scan_into_index(root: &Path, index: &Path) {
    let db = Database::open(index).expect("open index");
    scan_root(&db, root, ScanOptions::default()).expect("scan");
    db.checkpoint().expect("checkpoint");
}

fn request(fixture: &Fixture) -> PassRequest {
    PassRequest {
        index_path: fixture.index.clone(),
        root: fixture.root.clone(),
        dims: DIMS,
        text_model: None,
        vision_model: None,
        fake: true,
        depth: locator::embed::pipeline::Depth::Deep,
    }
}

fn run_fake(fixture: &Fixture) -> (PassSummary, Vec<PassEvent>) {
    let (tx, rx) = mpsc::channel();
    let summary = run_pass(&request(fixture), tx, Arc::new(AtomicBool::new(false))).expect("pass");
    (summary, rx.try_iter().collect())
}

fn store_for(fixture: &Fixture) -> EmbedStore {
    EmbedStore::open(&sidecar_path_for_index(&fixture.index)).expect("open sidecar")
}

fn canonical(root: &Path) -> String {
    root.canonicalize()
        .expect("canonical")
        .to_string_lossy()
        .to_string()
}

/// Records every input it is asked to embed, then delegates to the fake embedder.
struct RecordingEmbedder {
    inner: HashEmbedder,
    inputs: Arc<Mutex<Vec<String>>>,
}

impl Embedder for RecordingEmbedder {
    fn backend_label(&self) -> &str {
        "fake"
    }
    fn supports_images(&self) -> bool {
        true
    }
    fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.inputs
            .lock()
            .expect("lock")
            .extend(inputs.iter().cloned());
        self.inner.embed_batch(inputs)
    }
    fn embed_image(&mut self, path: &Path) -> Result<Vec<f32>> {
        self.inner.embed_image(path)
    }
}

/// Sleeps per batch so the cancel test has work left when the flag is set.
struct SlowEmbedder {
    inner: HashEmbedder,
}

impl Embedder for SlowEmbedder {
    fn backend_label(&self) -> &str {
        "fake"
    }
    fn supports_images(&self) -> bool {
        true
    }
    fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        thread::sleep(Duration::from_millis(100));
        self.inner.embed_batch(inputs)
    }
    fn embed_image(&mut self, path: &Path) -> Result<Vec<f32>> {
        self.inner.embed_image(path)
    }
}

/// Fails any batch that contains the poison word, like a broken document would.
struct PoisonEmbedder {
    inner: HashEmbedder,
}

impl Embedder for PoisonEmbedder {
    fn backend_label(&self) -> &str {
        "fake"
    }
    fn supports_images(&self) -> bool {
        true
    }
    fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.iter().any(|input| input.contains("POISON")) {
            bail!("model rejected the input");
        }
        self.inner.embed_batch(inputs)
    }
    fn embed_image(&mut self, path: &Path) -> Result<Vec<f32>> {
        self.inner.embed_image(path)
    }
}

fn drain(events: &mpsc::Receiver<PassEvent>) -> Vec<PassEvent> {
    events.try_iter().collect()
}

#[test]
fn full_pass_embeds_skips_and_fails_as_expected() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);

    let (tx, rx) = mpsc::channel();
    let summary = run_pass(&request(&fx), tx, Arc::new(AtomicBool::new(false))).expect("pass");
    let events = drain(&rx);

    assert_eq!(summary.files_embedded, 4, "invoice, soup, server, pixel");
    assert_eq!(summary.files_skipped, 1, "binary blob");
    assert_eq!(summary.files_failed, 1, "garbage pdf");
    assert_eq!(summary.files_unchanged, 0);
    assert!(!summary.cancelled);
    assert!(summary.chunks >= 4);

    let finished = events
        .iter()
        .filter(|event| matches!(event, PassEvent::FileFinished { .. }))
        .count();
    assert_eq!(finished, 6);
    assert!(events
        .iter()
        .any(|event| matches!(event, PassEvent::Finished(_))));

    let store = store_for(&fx);
    assert_eq!(
        store.is_complete(&canonical(&fx.root)).expect("complete"),
        Some(true)
    );
    let stats = store.stats().expect("stats");
    assert_eq!(stats.files, 6);
    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.chunks, summary.chunks as u64);
}

#[test]
fn second_run_without_changes_writes_nothing_new() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    let (first, _) = run_fake(&fx);
    let chunks_before = store_for(&fx).stats().expect("stats").chunks;

    let (second, _) = run_fake(&fx);
    // Done and skipped rows are unchanged; the failed pdf is retried, as needs_embedding requires.
    assert_eq!(second.files_unchanged, 5);
    assert_eq!(second.files_embedded, 0);
    assert_eq!(second.files_failed, 1);
    assert_eq!(second.chunks, 0);
    assert_eq!(first.files_embedded, 4);
    assert_eq!(store_for(&fx).stats().expect("stats").chunks, chunks_before);
}

#[test]
fn modified_file_is_the_only_one_reembedded() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);

    fs::write(
        fx.root.join("invoice.txt"),
        "budget spreadsheet for marketing, now with a much longer body of text about invoices and office chairs",
    )
    .expect("rewrite");
    scan_into_index(&fx.root, &fx.index);
    let (summary, _) = run_fake(&fx);
    assert_eq!(summary.files_embedded, 1);
    assert!(summary.chunks >= 1);
    assert_eq!(summary.files_unchanged, 4);
}

#[test]
fn deleted_file_is_pruned_with_its_chunks() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);
    // Join with the platform separator: the scanner stores `\` paths on Windows.
    let soup = fx
        .root
        .canonicalize()
        .expect("canonical")
        .join("soup.md")
        .to_string_lossy()
        .to_string();
    assert!(!store_for(&fx)
        .snippets_for(&[(soup.clone(), 0)])
        .expect("before")[0]
        .is_empty());

    fs::remove_file(fx.root.join("soup.md")).expect("delete");
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);

    let store = store_for(&fx);
    assert_eq!(store.snippets_for(&[(soup, 0)]).expect("after")[0], "");
    assert_eq!(store.stats().expect("stats").files, 5);
}

#[test]
fn file_embedded_before_a_rule_covered_it_loses_its_chunks() {
    let fx = fixture();
    let build = fx.root.join("DerivedData-mac");
    fs::create_dir_all(&build).expect("build dir");
    fs::write(build.join("main.d"), "dependencies: main.o main.c").expect("dep file");
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);
    let path = fx
        .root
        .canonicalize()
        .expect("canonical")
        .join("DerivedData-mac")
        .join("main.d")
        .to_string_lossy()
        .to_string();
    let store = store_for(&fx);
    assert!(store.is_skipped(&path).expect("skipped"));

    // What an older build that lacked the rule left behind: the file embedded and current.
    let metadata = fs::metadata(&path).expect("metadata");
    let stamp = FileStamp {
        path: path.clone(),
        mtime: metadata
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs() as i64,
        size: metadata.len() as i64,
    };
    let chunk = StoredChunk {
        ordinal: 0,
        start_byte: 0,
        snippet: "dependencies".to_string(),
        vector: vec![0.0; DIMS],
    };
    store
        .write_file(
            &canonical(&fx.root),
            &stamp,
            FileStatus::Done,
            &[chunk],
            None,
        )
        .expect("write old row");
    drop(store);

    run_fake(&fx);

    let store = store_for(&fx);
    assert!(store.is_skipped(&path).expect("skipped again"));
    assert_eq!(store.snippets_for(&[(path, 0)]).expect("snippet")[0], "");
}

#[test]
fn cancel_keeps_progress_and_a_rerun_finishes_the_rest() {
    let fx = fixture();
    for index in 0..30 {
        fs::write(
            fx.root.join(format!("note-{index:02}.txt")),
            format!("note number {index} about quarterly planning and budget item {index}"),
        )
        .expect("note");
    }
    scan_into_index(&fx.root, &fx.index);

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let slow_request = request(&fx);
    let worker_cancel = Arc::clone(&cancel);
    let worker = thread::spawn(move || {
        let mut embedder = SlowEmbedder {
            inner: HashEmbedder,
        };
        run_pass_with_embedder(&slow_request, tx, worker_cancel, &mut embedder)
    });
    for event in rx.iter() {
        if matches!(event, PassEvent::FileFinished { .. }) {
            cancel.store(true, Ordering::Relaxed);
            break;
        }
    }
    let first = worker
        .join()
        .expect("worker thread")
        .expect("cancelled pass");
    assert!(first.cancelled);
    assert!(first.files_embedded < 30);
    assert_eq!(
        store_for(&fx)
            .is_complete(&canonical(&fx.root))
            .expect("complete"),
        Some(false)
    );

    let (second, _) = run_fake(&fx);
    assert!(!second.cancelled);
    assert_eq!(second.files_unchanged, first.files_embedded);
    assert_eq!(first.files_embedded + second.files_embedded, 30);
    assert_eq!(
        store_for(&fx)
            .is_complete(&canonical(&fx.root))
            .expect("complete"),
        Some(true)
    );
}

#[test]
fn changing_dims_wipes_and_reembeds_at_the_new_width() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);

    let mut smaller = request(&fx);
    smaller.dims = 256;
    let (tx, _rx) = mpsc::channel();
    let summary = run_pass(&smaller, tx, Arc::new(AtomicBool::new(false))).expect("pass");
    assert_eq!(summary.files_unchanged, 0);
    assert_eq!(summary.files_embedded, 4);

    let store = store_for(&fx);
    assert_eq!(store.dims().expect("dims"), Some(256));
    let index = VectorIndex::load(&store).expect("load");
    let mut fake = HashEmbedder;
    let raw = fake
        .embed_batch(&[query_input("budget marketing")])
        .expect("embed query")
        .remove(0);
    let query = truncate_and_normalize(&raw, 256);
    let hits = index.top_k(&query, 3);
    assert_eq!(
        hits.first().map(|hit| hit.path.ends_with("invoice.txt")),
        Some(true)
    );
}

#[test]
fn every_text_input_carries_the_document_prefix() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    scan_into_index(&fx.root, &fx.index);

    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut embedder = RecordingEmbedder {
        inner: HashEmbedder,
        inputs: Arc::clone(&inputs),
    };
    let (tx, _rx) = mpsc::channel();
    run_pass_with_embedder(
        &request(&fx),
        tx,
        Arc::new(AtomicBool::new(false)),
        &mut embedder,
    )
    .expect("pass");

    let recorded = inputs.lock().expect("lock").clone();
    assert!(!recorded.is_empty());
    for input in &recorded {
        assert!(input.starts_with("title: none | text: "), "input {input:?}");
    }
    assert!(recorded.contains(&document_input(
        "budget spreadsheet for marketing, quarterly spend and invoices"
    )));
}

#[test]
fn a_poisoned_file_fails_alone_and_the_rest_are_embedded() {
    let fx = fixture();
    fs::write(fx.root.join("good-one.txt"), "quarterly budget planning").expect("good");
    fs::write(fx.root.join("good-two.txt"), "tomato soup with basil").expect("good");
    fs::write(fx.root.join("poison.txt"), "this document contains POISON").expect("poison");
    scan_into_index(&fx.root, &fx.index);

    let mut embedder = PoisonEmbedder {
        inner: HashEmbedder,
    };
    let (tx, _rx) = mpsc::channel();
    let summary = run_pass_with_embedder(
        &request(&fx),
        tx,
        Arc::new(AtomicBool::new(false)),
        &mut embedder,
    )
    .expect("pass");
    assert_eq!(summary.files_embedded, 2);
    assert_eq!(summary.files_failed, 1);
    assert!(!summary.cancelled);
    let store = store_for(&fx);
    assert_eq!(store.stats().expect("stats").failed, 1);
}

#[test]
fn image_is_embedded_when_vision_is_available_and_skipped_otherwise() {
    let fx = fixture();
    image::RgbImage::from_pixel(4, 4, image::Rgb([200, 0, 0]))
        .save(fx.root.join("red.png"))
        .expect("png");
    scan_into_index(&fx.root, &fx.index);

    let (summary, _) = run_fake(&fx);
    assert_eq!(summary.files_embedded, 1);

    struct NoVision(HashEmbedder);
    impl Embedder for NoVision {
        fn backend_label(&self) -> &str {
            "fake"
        }
        fn supports_images(&self) -> bool {
            false
        }
        fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            self.0.embed_batch(inputs)
        }
        fn embed_image(&mut self, _path: &Path) -> Result<Vec<f32>> {
            bail!("no vision model")
        }
    }
    let other = fixture();
    image::RgbImage::from_pixel(4, 4, image::Rgb([0, 200, 0]))
        .save(other.root.join("green.png"))
        .expect("png");
    scan_into_index(&other.root, &other.index);
    let mut embedder = NoVision(HashEmbedder);
    let (tx, _rx) = mpsc::channel();
    let summary = run_pass_with_embedder(
        &request(&other),
        tx,
        Arc::new(AtomicBool::new(false)),
        &mut embedder,
    )
    .expect("pass");
    assert_eq!(summary.files_skipped, 1);
    assert_eq!(summary.files_embedded, 0);
}

#[test]
fn identical_chunks_are_embedded_once_per_pass() {
    let fx = fixture();
    for index in 0..40 {
        fs::write(
            fx.root.join(format!("license-{index:02}.txt")),
            "Copyright (c) 2026 Example Authors. Licensed under the terms in LICENSE.",
        )
        .expect("write");
    }
    scan_into_index(&fx.root, &fx.index);

    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut embedder = RecordingEmbedder {
        inner: HashEmbedder,
        inputs: Arc::clone(&inputs),
    };
    let (tx, _rx) = mpsc::channel();
    let summary = run_pass_with_embedder(
        &request(&fx),
        tx,
        Arc::new(AtomicBool::new(false)),
        &mut embedder,
    )
    .expect("pass");

    assert_eq!(
        summary.files_embedded, 40,
        "every file still gets its chunk"
    );
    assert_eq!(store_for(&fx).stats().expect("stats").chunks, 40);
    let embedded = inputs.lock().expect("lock").len();
    assert_eq!(
        summary.timing.deduplicated,
        40 - embedded,
        "reused chunks are not embedded"
    );
    // The first batch of 8 is embedded before the cache holds anything, so at least the
    // later files reuse a vector.
    assert!(embedded <= 12, "embedded {embedded} of 40 identical chunks");
}

#[test]
fn low_value_files_are_recorded_skipped_and_not_retried() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    fs::create_dir_all(fx.root.join("app/coverage")).expect("dir");
    fs::write(
        fx.root.join("app/coverage/report.md"),
        "coverage report notes",
    )
    .expect("write");
    fs::write(fx.root.join("yarn.lock"), "lock contents").expect("write");
    scan_into_index(&fx.root, &fx.index);

    let (first, _) = run_fake(&fx);
    assert!(
        first
            .timing
            .low_value
            .iter()
            .any(|(rule, count)| *rule == "dependency or build directory" && *count == 1),
        "{:?}",
        first.timing.low_value
    );
    assert!(
        first
            .timing
            .low_value
            .iter()
            .any(|(rule, count)| *rule == "lock file" && *count == 1),
        "{:?}",
        first.timing.low_value
    );
    assert_eq!(
        first.files_skipped,
        2 + 1,
        "blob plus the two low-value files"
    );

    let (second, _) = run_fake(&fx);
    assert_eq!(
        second
            .timing
            .low_value
            .iter()
            .map(|(_, c)| c)
            .sum::<usize>(),
        0,
        "skipped rows are not retried"
    );
    assert!(second.files_unchanged >= 2);
}

/// Reads one file's row and chunk count straight from the sidecar (test-only inspection).
fn file_row(fixture: &Fixture, suffix: &str) -> (Option<String>, i64, i64) {
    let conn = rusqlite::Connection::open(sidecar_path_for_index(&fixture.index)).expect("open");
    let (detail, chunk_count): (Option<String>, i64) = conn
        .query_row(
            "SELECT detail, chunk_count FROM embed_files WHERE path LIKE ?1",
            [format!("%{suffix}")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("file row");
    let chunks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE path LIKE ?1",
            [format!("%{suffix}")],
            |row| row.get(0),
        )
        .expect("chunk count");
    (detail, chunk_count, chunks)
}

#[test]
fn secret_name_is_recorded_skipped_and_never_embedded() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    fs::write(
        fx.root.join(".env"),
        "DATABASE_PASSWORD=hunter2 and a long line of ordinary looking settings text",
    )
    .expect("write");
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);
    let (detail, chunk_count, chunks) = file_row(&fx, ".env");
    assert_eq!(detail.as_deref(), Some("secret or credential file"));
    assert_eq!((chunk_count, chunks), (0, 0));
}

#[test]
fn file_that_now_holds_a_private_key_loses_its_chunks_on_next_pass() {
    let fx = fixture();
    write_standard_tree(&fx.root);
    fs::write(
        fx.root.join("deploy.txt"),
        "deploy notes for the staging cluster, plenty of ordinary words for one chunk",
    )
    .expect("write");
    scan_into_index(&fx.root, &fx.index);
    run_fake(&fx);
    let (_, _, before) = file_row(&fx, "deploy.txt");
    assert!(before >= 1, "file should be embedded on the first pass");

    fs::write(
        fx.root.join("deploy.txt"),
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n",
    )
    .expect("rewrite");
    scan_into_index(&fx.root, &fx.index);
    let (summary, _) = run_fake(&fx);
    assert_eq!(summary.files_embedded, 0);
    let (detail, chunk_count, chunks) = file_row(&fx, "deploy.txt");
    assert_eq!(detail.as_deref(), Some("private key"));
    assert_eq!((chunk_count, chunks), (0, 0));
}

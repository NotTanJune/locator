use std::fs;
use std::time::{Duration, Instant};

use locator::embed::runtime::{document_input, Embedder};
use locator::embed::store::{
    sidecar_path_for_index, EmbedStore, FileStamp, FileStatus, StoredChunk, VectorIndex,
};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use locator::db::Database;
use locator::embed::fake::HashEmbedder;
use locator::embed::pipeline::{run_pass, PassRequest};
use locator::embed::search::SemanticSearcher;
use locator::query::{QueryMode, SearchOptions};
use locator::scanner::{scan_root, ScanOptions};

fn write_corpus(root: &Path) {
    fs::write(
        root.join("budget.txt"),
        "quarterly budget marketing spend spreadsheet",
    )
    .expect("budget");
    fs::write(
        root.join("soup.md"),
        "tomato soup recipe with basil and garlic",
    )
    .expect("soup");
    fs::write(root.join("server.rs"), "fn start_http_server listen port").expect("server");
}

fn index_and_embed(root: &Path, index: &Path) {
    let db = Database::open(index).expect("open index");
    scan_root(&db, root, ScanOptions::default()).expect("scan");
    db.checkpoint().expect("checkpoint");
    drop(db);
    let request = PassRequest {
        index_path: index.to_path_buf(),
        root: root.to_path_buf(),
        dims: 768,
        text_model: None,
        vision_model: None,
        fake: true,
        depth: locator::embed::pipeline::Depth::Deep,
    };
    let (tx, _rx) = mpsc::channel();
    run_pass(&request, tx, Arc::new(AtomicBool::new(false))).expect("embedding pass");
}

fn query(text: &str) -> SearchOptions {
    SearchOptions::new(text).with_mode(QueryMode::Semantic)
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}

#[test]
fn ranks_the_file_that_shares_the_query_words_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    write_corpus(&root);
    let index = dir.path().join("index.sqlite");
    index_and_embed(&root, &index);

    let mut searcher = SemanticSearcher::open(&index)
        .expect("open")
        .expect("has embeddings");
    let mut embedder = HashEmbedder;

    let budget = searcher
        .search(&mut embedder, &query("marketing budget"))
        .expect("search");
    assert_eq!(file_name(&budget[0].result.path), "budget.txt");
    assert!(!budget[0].snippet.is_empty());

    let soup = searcher
        .search(&mut embedder, &query("tomato basil"))
        .expect("search");
    assert_eq!(file_name(&soup[0].result.path), "soup.md");
}

#[test]
fn scores_are_descending_and_limit_is_respected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    write_corpus(&root);
    let index = dir.path().join("index.sqlite");
    index_and_embed(&root, &index);

    let mut searcher = SemanticSearcher::open(&index)
        .expect("open")
        .expect("has embeddings");
    let mut embedder = HashEmbedder;
    let mut options = query("budget soup server");
    options.limit = 2;
    let hits = searcher.search(&mut embedder, &options).expect("search");
    assert_eq!(hits.len(), 2);
    assert!(hits[0].score >= hits[1].score);
}

#[test]
fn extension_filter_applies_to_semantic_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    write_corpus(&root);
    let index = dir.path().join("index.sqlite");
    index_and_embed(&root, &index);

    let mut searcher = SemanticSearcher::open(&index)
        .expect("open")
        .expect("has embeddings");
    let mut embedder = HashEmbedder;
    let mut options = query("tomato basil");
    options.filters.exts = vec!["rs".to_string()];
    let hits = searcher.search(&mut embedder, &options).expect("search");
    assert!(hits.iter().all(|hit| hit.result.path.ends_with(".rs")));
    assert!(hits
        .iter()
        .any(|hit| file_name(&hit.result.path) == "server.rs"));
}

#[test]
fn deleted_file_drops_out_of_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    write_corpus(&root);
    let index = dir.path().join("index.sqlite");
    index_and_embed(&root, &index);

    fs::remove_file(root.join("soup.md")).expect("delete");
    let db = Database::open(&index).expect("open index");
    scan_root(&db, &root, ScanOptions::default()).expect("rescan");
    db.checkpoint().expect("checkpoint");
    drop(db);

    let mut searcher = SemanticSearcher::open(&index)
        .expect("open")
        .expect("has embeddings");
    let mut embedder = HashEmbedder;
    let hits = searcher
        .search(&mut embedder, &query("tomato basil"))
        .expect("search");
    assert!(hits
        .iter()
        .all(|hit| file_name(&hit.result.path) != "soup.md"));
}

#[test]
fn empty_query_returns_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    write_corpus(&root);
    let index = dir.path().join("index.sqlite");
    index_and_embed(&root, &index);

    let mut searcher = SemanticSearcher::open(&index)
        .expect("open")
        .expect("has embeddings");
    let mut embedder = HashEmbedder;
    assert!(searcher
        .search(&mut embedder, &query("   "))
        .expect("search")
        .is_empty());
}

#[test]
fn no_sidecar_means_no_searcher() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = dir.path().join("index.sqlite");
    Database::open(&index).expect("create index");
    assert!(SemanticSearcher::open(&index).expect("open").is_none());
    assert!(!locator::embed::semantic_available(&index));
}

#[test]
fn search_sees_files_committed_by_another_connection_after_the_check_interval() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    fs::create_dir_all(&root).expect("root");
    write_corpus(&root);
    let index = dir.path().join("index.sqlite");
    index_and_embed(&root, &index);

    let mut searcher = SemanticSearcher::open(&index)
        .expect("open")
        .expect("has embeddings");
    let mut embedder = HashEmbedder;
    let before = searcher
        .search(&mut embedder, &query("zebra crossing"))
        .expect("search");
    assert!(before
        .iter()
        .all(|hit| file_name(&hit.result.path) != "zebra.txt"));

    // A concurrent `lctr scan --em` writes a new file through its own connection.
    fs::write(root.join("zebra.txt"), "zebra crossing near the school").expect("zebra");
    let db = Database::open(&index).expect("open index");
    scan_root(&db, &root, ScanOptions::default()).expect("rescan");
    db.checkpoint().expect("checkpoint");
    drop(db);
    let canonical = root.canonicalize().expect("canonical");
    let zebra_path = canonical.join("zebra.txt").to_string_lossy().to_string();
    let vector = embedder
        .embed_batch(&[document_input("zebra crossing near the school")])
        .expect("embed")
        .remove(0);
    let writer =
        EmbedStore::open(&sidecar_path_for_index(&index)).expect("open sidecar for writing");
    writer
        .write_file(
            &canonical.to_string_lossy(),
            &FileStamp {
                path: zebra_path.clone(),
                mtime: 1,
                size: 1,
            },
            FileStatus::Done,
            &[StoredChunk {
                ordinal: 0,
                start_byte: 0,
                snippet: "zebra crossing near the school".to_string(),
                vector,
            }],
            None,
        )
        .expect("commit");

    // Inside the 2 second window the searcher still answers from its loaded vectors.
    let still_old = searcher
        .search(&mut embedder, &query("zebra crossing"))
        .expect("search");
    assert!(still_old.iter().all(|hit| hit.result.path != zebra_path));

    std::thread::sleep(Duration::from_millis(2100));
    let after = searcher
        .search(&mut embedder, &query("zebra crossing"))
        .expect("search");
    assert!(
        after.iter().any(|hit| hit.result.path == zebra_path),
        "new file not found after reload"
    );
}

/// Reload cost on a real sidecar: `LCTR_REAL_SIDECAR=<path> cargo test --release reload_cost -- --ignored --nocapture`.
#[test]
#[ignore]
fn reload_cost_on_a_real_sidecar() {
    let path = std::env::var("LCTR_REAL_SIDECAR").expect("set LCTR_REAL_SIDECAR");
    let store = EmbedStore::open_readonly(Path::new(&path))
        .expect("open")
        .expect("sidecar exists");
    let started = Instant::now();
    let index = VectorIndex::load(&store).expect("load");
    println!(
        "reload of {} chunks from {}: {:.1} ms",
        index.len(),
        path,
        started.elapsed().as_secs_f64() * 1000.0
    );
}

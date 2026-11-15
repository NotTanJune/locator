use locator::live_search::{search_live, search_live_with_options, LiveSearchOptions};
use locator::query::{QueryMode, SearchOptions, SortField};
use tempfile::tempdir;

#[test]
fn live_search_finds_filename_matches_without_index() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("QuarterlyBudget.xlsx"), "fake").expect("write match");
    std::fs::write(root.path().join("notes.txt"), "fake").expect("write miss");

    let results = search_live(root.path(), "quarterly", LiveSearchOptions { limit: 50 })
        .expect("live search succeeds");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "QuarterlyBudget.xlsx");
}

#[test]
fn live_search_skips_noise_directories() {
    let root = tempdir().expect("root");
    std::fs::create_dir(root.path().join("node_modules")).expect("create noise dir");
    std::fs::write(root.path().join("node_modules").join("report.pdf"), "fake")
        .expect("write skipped file");
    std::fs::write(root.path().join("report-final.pdf"), "fake").expect("write match");

    let results = search_live(root.path(), "report", LiveSearchOptions { limit: 50 })
        .expect("live search succeeds");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "report-final.pdf");
}

#[test]
fn live_search_honors_query_mode_filters_and_sorting() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("alpha-report.pdf"), "fake").expect("write alpha");
    std::fs::write(root.path().join("beta-report.md"), "fake").expect("write beta");
    std::fs::write(root.path().join("reporting.txt"), "fake").expect("write text");

    let options = SearchOptions::new("report")
        .with_mode(QueryMode::Suffix)
        .with_sort(SortField::Name)
        .with_limit(50)
        .with_exts("pdf,md")
        .expect("extensions parse");
    let results = search_live_with_options(root.path(), &options).expect("live search succeeds");

    assert_eq!(
        results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha-report.pdf", "beta-report.md"]
    );
}

#[test]
fn live_search_ignores_matches_in_the_root_path() {
    let parent = tempdir().expect("parent");
    let root = parent.path().join("needleroot");
    std::fs::create_dir(&root).expect("create root");
    for name in ["alpha.txt", "beta.md", "needle-note.txt"] {
        std::fs::write(root.join(name), "x").expect("write file");
    }

    let results = search_live_with_options(&root, &SearchOptions::new("needle").with_limit(100))
        .expect("live search succeeds");

    assert_eq!(
        results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["needle-note.txt"]
    );
}

#[test]
fn live_search_fuzzy_relevance_orders_by_score_not_modified_time() {
    let root = tempdir().expect("root");
    let best = root.path().join("abc.txt");
    let weak = root.path().join("a_x_b_x_c.txt");
    std::fs::write(&best, "x").expect("write best");
    std::fs::write(&weak, "x").expect("write weak");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let new = old + std::time::Duration::from_secs(1_000_000);
    std::fs::File::options()
        .write(true)
        .open(&best)
        .and_then(|f| f.set_modified(old))
        .expect("set old mtime");
    std::fs::File::options()
        .write(true)
        .open(&weak)
        .and_then(|f| f.set_modified(new))
        .expect("set new mtime");

    let options = SearchOptions::new("abc")
        .with_mode(QueryMode::Fuzzy)
        .with_sort(SortField::Relevance)
        .with_limit(50);
    let results = search_live_with_options(root.path(), &options).expect("live search succeeds");

    assert_eq!(results.first().map(|r| r.name.as_str()), Some("abc.txt"));
}

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

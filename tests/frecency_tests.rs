use locator::db::{Database, FileRecord};
use locator::query::SearchOptions;

fn record(path: &str, name: &str) -> FileRecord {
    FileRecord {
        path: path.to_string(),
        name: name.to_string(),
        parent: "/tmp".to_string(),
        extension: Some("txt".to_string()),
        root: "/tmp".to_string(),
        volume: "local".to_string(),
        kind: "text".to_string(),
        size_bytes: 1,
        created_at: None,
        modified_at: None,
    }
}

fn names(db: &Database, query: &str) -> Vec<String> {
    db.search_with_options(&SearchOptions::new(query).with_limit(10))
        .expect("search")
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn frecently_opened_files_rank_higher_under_relevance() {
    let db = Database::open_in_memory().expect("open in-memory db");
    db.upsert_file(&record("/tmp/alpha_report.txt", "alpha_report.txt"))
        .unwrap();
    db.upsert_file(&record("/tmp/beta_report.txt", "beta_report.txt"))
        .unwrap();

    // Same relevance tier; ties break alphabetically, so alpha leads initially.
    let before = names(&db, "report");
    assert_eq!(before.first().map(String::as_str), Some("alpha_report.txt"));

    // Opening beta repeatedly should lift it above alpha within the tier.
    for _ in 0..5 {
        db.record_access("/tmp/beta_report.txt").unwrap();
    }

    let after = names(&db, "report");
    assert_eq!(after.first().map(String::as_str), Some("beta_report.txt"));
}

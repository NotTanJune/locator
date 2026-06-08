use chrono::{TimeZone, Utc};
use locator::query::{
    CompiledQuery, FileKind, QueryMode, QueryScorer, SearchFilters, SearchOptions, SizeBound,
    SortField,
};

#[test]
fn parses_size_filters_with_units() {
    let min = SizeBound::parse("500kb").expect("min size parses");
    let max = SizeBound::parse("2gb").expect("max size parses");

    assert_eq!(min.bytes, 500_000);
    assert_eq!(max.bytes, 2_000_000_000);
}

#[test]
fn rejects_invalid_size_filter() {
    let err = SizeBound::parse("12llamas").expect_err("invalid unit must fail");

    assert!(err.to_string().contains("invalid size"));
}

#[test]
fn parses_filter_extensions_and_dates() {
    let filters = SearchFilters::new()
        .with_exts("jpg,png,PDF")
        .expect("extensions parse")
        .with_modified_after("2024-01-01")
        .expect("date parses")
        .with_kind("pdf")
        .expect("kind parses");

    assert_eq!(filters.exts, vec!["jpg", "png", "pdf"]);
    assert_eq!(
        filters.modified_after,
        Some(Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap())
    );
    assert_eq!(filters.kind, Some(FileKind::Pdf));
}

#[test]
fn parses_shared_search_options_for_find_and_tui() {
    let options = SearchOptions::new("Report")
        .with_mode(QueryMode::Prefix)
        .with_sort(SortField::Modified)
        .with_reverse(true)
        .with_limit(25)
        .with_exts("pdf,md")
        .expect("extensions parse");

    assert_eq!(options.query, "Report");
    assert_eq!(options.mode, QueryMode::Prefix);
    assert_eq!(options.sort, SortField::Modified);
    assert!(options.reverse);
    assert_eq!(options.limit, 25);
    assert_eq!(options.filters.exts, vec!["pdf", "md"]);
}

#[test]
fn query_modes_match_filename_and_path_candidates() {
    assert!(QueryMode::Contains
        .matches("port", "QuarterlyReport.pdf")
        .expect("contains validates"));
    assert!(QueryMode::Exact
        .matches("quarterlyreport.pdf", "QuarterlyReport.pdf")
        .expect("exact validates"));
    assert!(QueryMode::Prefix
        .matches("quarterly", "QuarterlyReport.pdf")
        .expect("prefix validates"));
    assert!(QueryMode::Suffix
        .matches(".pdf", "QuarterlyReport.pdf")
        .expect("suffix validates"));
    assert!(QueryMode::Fuzzy
        .matches("qtrpdf", "QuarterlyReport.pdf")
        .expect("fuzzy validates"));
    assert!(QueryMode::Regex
        .matches(r"(?i)^quarterly.*\.pdf$", "QuarterlyReport.pdf")
        .expect("regex validates"));
    assert!(QueryMode::Glob
        .matches("Quarterly*.pdf", "QuarterlyReport.pdf")
        .expect("glob validates"));
}

#[test]
fn invalid_pattern_modes_return_errors() {
    assert!(QueryMode::Regex.matches("[", "report.pdf").is_err());
    assert!(QueryMode::Glob.matches("[", "report.pdf").is_err());
}

#[test]
fn match_positions_report_highlight_ranges() {
    let mut scorer = QueryScorer::new();

    // Contains: the substring "port" of "Report" sits at char indices 11..15.
    let contains = CompiledQuery::compile(QueryMode::Contains, "port").unwrap();
    assert_eq!(
        contains.match_positions(&mut scorer, "QuarterlyReport.pdf"),
        vec![11, 12, 13, 14]
    );

    // Prefix highlights the leading run.
    let prefix = CompiledQuery::compile(QueryMode::Prefix, "quar").unwrap();
    assert_eq!(
        prefix.match_positions(&mut scorer, "QuarterlyReport.pdf"),
        vec![0, 1, 2, 3]
    );

    // Fuzzy returns the matched subsequence positions, sorted and deduped.
    let fuzzy = CompiledQuery::compile(QueryMode::Fuzzy, "qrep").unwrap();
    let positions = fuzzy.match_positions(&mut scorer, "QuarterlyReport.pdf");
    assert!(!positions.is_empty());
    assert!(positions.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(positions.len(), 4);

    // Empty query highlights nothing.
    let empty = CompiledQuery::compile(QueryMode::Contains, "").unwrap();
    assert!(empty
        .match_positions(&mut scorer, "QuarterlyReport.pdf")
        .is_empty());
}

#[test]
fn fuzzy_rank_orders_better_matches_higher() {
    let mut scorer = QueryScorer::new();
    let compiled = CompiledQuery::compile(QueryMode::Fuzzy, "report").unwrap();

    let tight = compiled.fuzzy_rank(&mut scorer, ["report.pdf"]);
    let loose = compiled.fuzzy_rank(&mut scorer, ["quarterly_report_archive.pdf"]);

    assert!(tight.is_some() && loose.is_some());
    assert!(tight.unwrap() >= loose.unwrap());
}

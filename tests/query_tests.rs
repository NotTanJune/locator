use chrono::{DateTime, Duration, TimeZone, Utc};
use locator::query::{parse_date, parse_date_at};
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

#[test]
fn semantic_mode_joins_the_cycle_after_glob() {
    use locator::query::QueryMode;
    assert_eq!(QueryMode::Glob.next(), QueryMode::Semantic);
    assert_eq!(QueryMode::Semantic.next(), QueryMode::Contains);
    assert_eq!(QueryMode::Semantic.label(), "semantic");
}

#[test]
fn next_available_skips_semantic_when_unavailable() {
    use locator::query::QueryMode;
    let mut mode = QueryMode::Contains;
    for _ in 0..12 {
        mode = mode.next_available(false);
        assert_ne!(mode, QueryMode::Semantic);
    }
    assert_eq!(QueryMode::Glob.next_available(false), QueryMode::Contains);
    assert_eq!(QueryMode::Glob.next_available(true), QueryMode::Semantic);
    assert_eq!(
        QueryMode::Semantic.next_available(true),
        QueryMode::Contains
    );
}

#[test]
fn compiling_semantic_mode_is_an_error() {
    use locator::query::{CompiledQuery, QueryMode};
    let error = match CompiledQuery::compile(QueryMode::Semantic, "x") {
        Ok(_) => panic!("semantic mode must not compile"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("--em"), "{error}");
}

#[test]
fn database_search_rejects_semantic_mode_loudly() {
    use locator::db::Database;
    use locator::query::{QueryMode, SearchOptions};
    let db = Database::open_in_memory().expect("db");
    let mut options = SearchOptions::new("x");
    options.mode = QueryMode::Semantic;
    let error = db
        .search_with_options(&options)
        .expect_err("semantic is not a SQL mode");
    assert!(error.to_string().contains("embed::search"), "{error}");
}

#[test]
fn glob_negated_class_matches_anything_but_listed() {
    let q = CompiledQuery::compile(QueryMode::Glob, "[!a]bc").expect("compile");
    let mut scorer = QueryScorer::new();
    assert!(q.is_match(&mut scorer, "xbc"));
    assert!(!q.is_match(&mut scorer, "abc"));
    assert!(q.is_match(&mut scorer, "!bc"));
}

#[test]
fn fuzzy_positions_are_char_indices_for_decomposed_accents() {
    let q = CompiledQuery::compile(QueryMode::Fuzzy, "notes").expect("compile");
    let mut scorer = QueryScorer::new();
    assert_eq!(
        q.match_positions(&mut scorer, "cafe\u{301}-notes.txt"),
        vec![6, 7, 8, 9, 10]
    );
    assert_eq!(
        q.match_positions(&mut scorer, "notes.txt"),
        vec![0, 1, 2, 3, 4]
    );
}

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
}

#[test]
fn parse_date_at_keeps_absolute_dates() {
    let now = at(2024, 3, 31, 15, 30, 45);

    assert_eq!(
        parse_date_at("2020-02-29", now).unwrap(),
        at(2020, 2, 29, 0, 0, 0)
    );
    assert_eq!(
        parse_date_at("  2020-02-29 ", now).unwrap(),
        at(2020, 2, 29, 0, 0, 0)
    );
}

#[test]
fn parse_date_at_resolves_today_and_yesterday() {
    let now = at(2024, 3, 1, 15, 30, 45);

    assert_eq!(
        parse_date_at("today", now).unwrap(),
        at(2024, 3, 1, 0, 0, 0)
    );
    assert_eq!(
        parse_date_at(" Yesterday ", now).unwrap(),
        at(2024, 2, 29, 0, 0, 0)
    );
    assert_eq!(
        parse_date_at("TODAY", now).unwrap(),
        at(2024, 3, 1, 0, 0, 0)
    );
}

#[test]
fn parse_date_at_subtracts_relative_amounts() {
    let now = at(2024, 3, 31, 15, 30, 45);

    assert_eq!(
        parse_date_at("5h", now).unwrap(),
        at(2024, 3, 31, 10, 30, 45)
    );
    assert_eq!(
        parse_date_at("7d", now).unwrap(),
        at(2024, 3, 24, 15, 30, 45)
    );
    assert_eq!(
        parse_date_at("7 d", now).unwrap(),
        parse_date_at("7d", now).unwrap()
    );
    assert_eq!(
        parse_date_at("2w", now).unwrap(),
        at(2024, 3, 17, 15, 30, 45)
    );
    assert_eq!(
        parse_date_at("1mo", now).unwrap(),
        at(2024, 2, 29, 15, 30, 45)
    );
    assert_eq!(
        parse_date_at("1y", now).unwrap(),
        at(2023, 3, 31, 15, 30, 45)
    );
    assert_eq!(
        parse_date_at("  3 MO ", now).unwrap(),
        at(2023, 12, 31, 15, 30, 45)
    );
    assert_eq!(
        parse_date_at("1y", at(2024, 2, 29, 1, 2, 3)).unwrap(),
        at(2023, 2, 28, 1, 2, 3)
    );
    assert_eq!(
        parse_date_at("12mo", now).unwrap(),
        parse_date_at("1y", now).unwrap()
    );
}

#[test]
fn parse_date_at_zero_amount_returns_now() {
    let now = at(2024, 3, 31, 15, 30, 45);

    for value in ["0h", "0d", "0w", "0mo", "0y", "000d"] {
        assert_eq!(parse_date_at(value, now).unwrap(), now, "{value}");
    }
}

#[test]
fn parse_date_at_rejects_invalid_input() {
    let now = at(2024, 3, 31, 15, 30, 45);

    for value in [
        "",
        "   ",
        "-7d",
        "1.5d",
        "d",
        "7",
        "7x",
        "7dd",
        "7m",
        "+7d",
        "7 7d",
        "tomorrow",
        "7d ago",
        "2024-13-01",
        "abc",
    ] {
        assert!(
            parse_date_at(value, now).is_err(),
            "{value:?} should be rejected"
        );
    }
}

#[test]
fn parse_date_at_points_months_users_to_mo() {
    let now = at(2024, 3, 31, 15, 30, 45);
    let message = parse_date_at("7m", now).unwrap_err().to_string();

    assert!(message.contains("mo"), "{message}");
    let message = parse_date_at("nonsense", now).unwrap_err().to_string();
    assert!(
        message.contains("YYYY-MM-DD") && message.contains("yesterday"),
        "{message}"
    );
}

#[test]
fn parse_date_at_returns_errors_instead_of_overflowing() {
    let now = at(2024, 3, 31, 15, 30, 45);

    for value in [
        "99999999999999999999d",
        "999999999999d",
        "999999999999999h",
        "9223372036854775807d",
        "9223372036854775808d",
        "999999999999w",
        "99999999999mo",
        "4294967295mo",
        "4294967295y",
        "999999999y",
        "99999999999999999999y",
    ] {
        assert!(
            parse_date_at(value, now).is_err(),
            "{value} should overflow to an error"
        );
    }
}

#[test]
fn parse_date_matches_parse_date_at_for_absolute_and_relative_values() {
    assert_eq!(parse_date("2024-01-01").unwrap(), at(2024, 1, 1, 0, 0, 0));

    let before = Utc::now();
    let parsed = parse_date("1d").unwrap();
    let after = Utc::now();
    assert!(parsed >= before - Duration::days(1) && parsed <= after - Duration::days(1));
    assert!(parse_date("7m").is_err());
}

#[test]
fn filter_builders_accept_relative_dates() {
    let before = Utc::now();
    let filters = SearchFilters::new()
        .with_created_after("7d")
        .unwrap()
        .with_created_before("yesterday")
        .unwrap()
        .with_modified_after("1mo")
        .unwrap()
        .with_modified_before("2024-01-01")
        .unwrap();
    let after = Utc::now();

    let created_after = filters.created_after.unwrap();
    assert!(
        created_after >= before - Duration::days(7) && created_after <= after - Duration::days(7)
    );
    assert_eq!(
        filters.created_before.unwrap(),
        parse_date("yesterday").unwrap()
    );
    assert!(filters.modified_after.unwrap() < before - Duration::days(27));
    assert_eq!(filters.modified_before, Some(at(2024, 1, 1, 0, 0, 0)));

    let options = SearchOptions::new("report")
        .with_created_after("2h")
        .unwrap()
        .with_created_before("today")
        .unwrap()
        .with_modified_after("1w")
        .unwrap()
        .with_modified_before("1y")
        .unwrap();
    assert!(options.filters.created_after.is_some());
    assert!(options.filters.created_before.is_some());
    assert!(options.filters.modified_after.is_some());
    assert!(options.filters.modified_before.is_some());
}

#[test]
fn filter_builders_reject_invalid_relative_dates() {
    for value in ["7m", "-7d", "", "99999999999999999999d", "7"] {
        assert!(
            SearchFilters::new().with_created_after(value).is_err(),
            "{value:?}"
        );
        assert!(
            SearchFilters::new().with_created_before(value).is_err(),
            "{value:?}"
        );
        assert!(
            SearchFilters::new().with_modified_after(value).is_err(),
            "{value:?}"
        );
        assert!(
            SearchFilters::new().with_modified_before(value).is_err(),
            "{value:?}"
        );
        assert!(
            SearchOptions::new("x").with_modified_before(value).is_err(),
            "{value:?}"
        );
    }
}

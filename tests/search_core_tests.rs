//! `lctr find` (`Database::search_with_options`) against rows written the way a scan writes
//! them: names only, with size 0 and no timestamps. Size and date filters and sorts have to
//! use the files' real metadata, and every matching row has to be considered, not only the
//! first few thousand.
//!
//! No test here changes a `LCTR_*` variable, so the file needs no env guard.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use locator::db::{Database, FileRecord, SearchResult};
use locator::name_file::{name_file_path, NameFileBuilder};
use locator::query::{
    CompiledQuery, QueryMode, QueryScorer, SearchFilters, SearchOptions, SortField,
};
use locator::scanner::{scan_root, ScanOptions};
use tempfile::{tempdir, TempDir};

/// A record as the scanner stores it: no size and no timestamps.
fn light_record(root: &str, path: &str) -> FileRecord {
    let as_path = Path::new(path);
    FileRecord {
        path: path.to_string(),
        name: as_path
            .file_name()
            .expect("record path has a name")
            .to_string_lossy()
            .to_string(),
        parent: as_path
            .parent()
            .map(|parent| parent.to_string_lossy().to_string())
            .unwrap_or_default(),
        extension: as_path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase()),
        root: root.to_string(),
        volume: "local".to_string(),
        kind: "file".to_string(),
        size_bytes: 0,
        created_at: None,
        modified_at: None,
    }
}

/// Installs `records` the way a finished scan does: each record's root is replaced by the
/// records that name it, and only the path and the kind are kept.
fn install_light(db: &Database, records: &[FileRecord]) {
    let mut roots: Vec<&str> = records.iter().map(|record| record.root.as_str()).collect();
    roots.sort_unstable();
    roots.dedup();
    for root in roots {
        let mut rows = NameFileBuilder::default();
        for record in records.iter().filter(|record| record.root == root) {
            rows.push(&record.path, &record.kind).expect("push row");
        }
        db.install_scan(root, rows, 1).expect("install scan");
    }
}

fn write_file(dir: &Path, name: &str, size: usize, modified: Option<SystemTime>) {
    let path = dir.join(name);
    fs::write(&path, vec![b'x'; size]).expect("write fixture file");
    if let Some(modified) = modified {
        File::options()
            .write(true)
            .open(&path)
            .expect("reopen fixture file")
            .set_modified(modified)
            .expect("set modified time");
    }
}

/// Scans a directory of real files into an in-memory index.
fn scanned(files: &[(&str, usize, Option<SystemTime>)]) -> (TempDir, Database) {
    let dir = tempdir().expect("temp dir");
    for (name, size, modified) in files {
        write_file(dir.path(), name, *size, *modified);
    }
    let db = Database::open_in_memory().expect("db opens");
    let stats = scan_root(&db, dir.path(), ScanOptions::default()).expect("scan succeeds");
    assert_eq!(stats.indexed_files, files.len() as u64);
    (dir, db)
}

fn names(results: &[SearchResult]) -> Vec<&str> {
    results.iter().map(|result| result.name.as_str()).collect()
}

fn year(year: u64) -> SystemTime {
    // Close enough for bounds that sit years apart.
    SystemTime::UNIX_EPOCH + Duration::from_secs((year - 1970) * 365 * 24 * 60 * 60)
}

#[test]
fn size_filters_use_real_file_sizes_after_a_scan() {
    let (_dir, db) = scanned(&[
        ("big-report.dat", 300_000, None),
        ("small-report.dat", 10, None),
    ]);

    let at_least = db
        .search_with_options(&SearchOptions::new("report").with_min_size("100kb").unwrap())
        .expect("search works");
    assert_eq!(names(&at_least), ["big-report.dat"]);

    let at_most = db
        .search_with_options(&SearchOptions::new("report").with_max_size("1kb").unwrap())
        .expect("search works");
    assert_eq!(names(&at_most), ["small-report.dat"]);
}

#[test]
fn date_filters_use_real_modification_times_after_a_scan() {
    let (_dir, db) = scanned(&[
        ("old-report.txt", 1, Some(year(2001))),
        ("new-report.txt", 1, Some(year(2024))),
    ]);

    let after = db
        .search_with_options(
            &SearchOptions::new("report")
                .with_modified_after("2020-01-01")
                .unwrap(),
        )
        .expect("search works");
    assert_eq!(names(&after), ["new-report.txt"]);

    let before = db
        .search_with_options(
            &SearchOptions::new("report")
                .with_modified_before("2010-01-01")
                .unwrap(),
        )
        .expect("search works");
    assert_eq!(names(&before), ["old-report.txt"]);
}

#[test]
fn size_and_date_sorts_use_real_metadata_after_a_scan() {
    // Name order (a, b, c) differs from size order and from age order.
    let (_dir, db) = scanned(&[
        ("a-report.txt", 30, Some(year(2010))),
        ("b-report.txt", 10, Some(year(2020))),
        ("c-report.txt", 20, Some(year(2000))),
    ]);

    let by_size = db
        .search_with_options(&SearchOptions::new("report").with_sort(SortField::Size))
        .expect("search works");
    assert_eq!(
        names(&by_size),
        ["b-report.txt", "c-report.txt", "a-report.txt"]
    );

    let largest_first = db
        .search_with_options(
            &SearchOptions::new("report")
                .with_sort(SortField::Size)
                .with_reverse(true)
                .with_limit(1),
        )
        .expect("search works");
    assert_eq!(names(&largest_first), ["a-report.txt"]);

    let newest_first = db
        .search_with_options(
            &SearchOptions::new("report")
                .with_sort(SortField::Modified)
                .with_reverse(true),
        )
        .expect("search works");
    assert_eq!(
        names(&newest_first),
        ["b-report.txt", "a-report.txt", "c-report.txt"]
    );
}

/// 21,000 rows whose names sort before the one row the queries are after. That is more rows
/// than any candidate cap `find` used to apply (20,000 at most).
fn large_index() -> Database {
    let db = Database::open_in_memory().expect("db opens");
    let mut records: Vec<FileRecord> = (0..21_000)
        .map(|index| light_record("/big", &format!("/big/filler/data-{index:05}.bin")))
        .collect();
    records.push(light_record("/big", "/big/archive/zz-needle.cfg"));
    install_light(&db, &records);
    db
}

#[test]
fn find_examines_every_row_in_regex_glob_and_fuzzy_modes() {
    let db = large_index();
    for (mode, query) in [
        (QueryMode::Glob, "zz-needle*"),
        (QueryMode::Glob, "*.cfg"),
        (QueryMode::Regex, "^zz-needle"),
        (QueryMode::Fuzzy, "zzndlcfg"),
    ] {
        let results = db
            .search_with_options(&SearchOptions::new(query).with_mode(mode))
            .expect("search works");
        assert_eq!(
            names(&results),
            ["zz-needle.cfg"],
            "{mode:?} query {query:?}"
        );
    }
}

#[test]
fn exact_name_match_ranks_first_however_many_other_matches_sort_before_it() {
    let db = Database::open_in_memory().expect("db opens");
    let mut records: Vec<FileRecord> = (0..6_000)
        .map(|index| light_record("/r", &format!("/r/annual/annual-report-{index:05}.txt")))
        .collect();
    records.push(light_record("/r", "/r/archive/report"));
    records.push(light_record("/r", "/r/archive/report.txt"));
    install_light(&db, &records);

    let results = db
        .search_with_options(&SearchOptions::new("report").with_limit(3))
        .expect("search works");
    assert_eq!(
        names(&results),
        ["report", "report.txt", "annual-report-00000.txt"]
    );
}

#[test]
fn query_whitespace_does_not_change_ranking() {
    let db = Database::open_in_memory().expect("db opens");
    install_light(
        &db,
        &[
            light_record("/w", "/w/annual-report.txt"),
            light_record("/w", "/w/report"),
            light_record("/w", "/w/report-card.txt"),
        ],
    );

    let trimmed = db
        .search_with_options(&SearchOptions::new("report"))
        .expect("search works");
    let padded = db
        .search_with_options(&SearchOptions::new("  report "))
        .expect("search works");
    assert_eq!(
        names(&trimmed),
        ["report", "report-card.txt", "annual-report.txt"]
    );
    assert_eq!(names(&padded), names(&trimmed));
}

#[test]
fn name_sort_orders_equal_names_by_path() {
    let db = Database::open_in_memory().expect("db opens");
    // Inserted in the opposite of path order, so insertion order cannot produce the result.
    install_light(
        &db,
        &[
            light_record("/t", "/t/zeta/notes.txt"),
            light_record("/t", "/t/mid/notes.txt"),
            light_record("/t", "/t/alpha/notes.txt"),
        ],
    );

    let ascending = db
        .search_with_options(&SearchOptions::new("notes").with_sort(SortField::Name))
        .expect("search works");
    let paths: Vec<&str> = ascending
        .iter()
        .map(|result| result.path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "/t/alpha/notes.txt",
            "/t/mid/notes.txt",
            "/t/zeta/notes.txt"
        ]
    );

    let descending = db
        .search_with_options(
            &SearchOptions::new("notes")
                .with_sort(SortField::Name)
                .with_reverse(true),
        )
        .expect("search works");
    let paths: Vec<&str> = descending
        .iter()
        .map(|result| result.path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "/t/zeta/notes.txt",
            "/t/mid/notes.txt",
            "/t/alpha/notes.txt"
        ]
    );
}

#[test]
fn rows_for_missing_files_do_not_use_up_result_slots() {
    let dir = tempdir().expect("temp dir");
    let root = dir.path().to_string_lossy().to_string();
    // 1,100 indexed rows whose files are gone, all ranked ahead of the five that exist.
    let mut records: Vec<FileRecord> = (0..1_100)
        .map(|index| {
            let path = dir.path().join(format!("hit-{index:04}.txt"));
            light_record(&root, &path.to_string_lossy())
        })
        .collect();
    for index in 9_990..9_995 {
        let name = format!("hit-{index:04}.txt");
        write_file(dir.path(), &name, 1, None);
        records.push(light_record(
            &root,
            &dir.path().join(&name).to_string_lossy(),
        ));
    }
    let db_dir = tempdir().expect("db dir");
    let db = Database::open(db_dir.path().join("index.sqlite"))
        .expect("db opens")
        .with_search_path_verification();
    install_light(&db, &records);

    let results = db
        .search_with_options(
            &SearchOptions::new("hit")
                .with_sort(SortField::Name)
                .with_limit(5),
        )
        .expect("search works");
    assert_eq!(
        names(&results),
        [
            "hit-9990.txt",
            "hit-9991.txt",
            "hit-9992.txt",
            "hit-9993.txt",
            "hit-9994.txt"
        ]
    );
}

/// The SQL predicate for each mode is only a first cut; `CompiledQuery` decides. This checks
/// the first cut never drops a row the matcher accepts, across the query shapes that take
/// different SQL paths (index lookup, name range, trigram, full scan).
#[test]
fn sql_predicates_never_drop_a_row_the_matcher_accepts() {
    let paths = [
        "/data/report",
        "/data/Report.TXT",
        "/data/report.txt.bak",
        "/data/annual-report.pdf",
        "/data/reports/summary.md",
        "/data/src/main.rs",
        "/data/src/main.rs.orig",
        "/data/src/lib.RS",
        "/data/rs",
        "/data/a_b.txt",
        "/data/axb.txt",
        "/data/100%.txt",
        "/data/100x.txt",
        "/data/.bashrc",
        "/data/archive.tar.gz",
        "/data/CAF\u{c9}.txt",
        "/data/caf\u{e9}.txt",
        "/data/re",
        "/data/r",
        "/data/config/settings.toml",
        "/data/Config",
        "/data/configure.ac",
    ];
    let records: Vec<FileRecord> = paths
        .iter()
        .map(|path| light_record("/data", path))
        .collect();
    let db = Database::open_in_memory().expect("db opens");
    install_light(&db, &records);

    let queries = [
        "report",
        "REPORT",
        "re",
        "r",
        "rs",
        ".rs",
        "main.rs",
        "src/main",
        "/data",
        "/data/src",
        "/data/report",
        "a_b",
        "100%",
        "config",
        "configure.ac",
        ".bashrc",
        "tar.gz",
        "CAF\u{c9}",
        "caf\u{e9}",
        "\u{e9}",
        "txt",
        " report ",
    ];
    let modes = [
        QueryMode::Contains,
        QueryMode::Exact,
        QueryMode::Prefix,
        QueryMode::Suffix,
        QueryMode::Fuzzy,
    ];
    let mut scorer = QueryScorer::new();
    let mut checked = 0;
    for mode in modes {
        for query in queries {
            let compiled = CompiledQuery::compile(mode, query).expect("query compiles");
            let expected: BTreeSet<String> = records
                .iter()
                .filter(|record| {
                    compiled.matches_any(&mut scorer, [record.name.as_str(), record.path.as_str()])
                })
                .map(|record| record.path.clone())
                .collect();
            let actual: BTreeSet<String> = db
                .search_with_options(
                    &SearchOptions::new(query)
                        .with_mode(mode)
                        .with_limit(usize::MAX),
                )
                .expect("search works")
                .into_iter()
                .map(|result| result.path)
                .collect();
            if mode == QueryMode::Contains && !query.contains('/') {
                // Contains without a path separator matches names only. Every name match
                // must come back, and nothing the matcher rejects may. A query with a path
                // separator also looks at the path and must find every path that contains it.
                let by_name: BTreeSet<String> = records
                    .iter()
                    .filter(|record| compiled.is_match(&mut scorer, &record.name))
                    .map(|record| record.path.clone())
                    .collect();
                assert!(
                    by_name.is_subset(&actual) && actual.is_subset(&expected),
                    "{mode:?} query {query:?}: got {actual:?}, name matches {by_name:?}"
                );
            } else {
                assert_eq!(actual, expected, "{mode:?} query {query:?}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, modes.len() * queries.len());
}

/// A database on disk with enough rows that a full scan is split across several readers.
/// (An in-memory database is always read by one.)
#[test]
fn a_full_scan_split_across_readers_finds_each_match_once() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(dir.path().join("index.sqlite")).expect("db opens");
    let records: Vec<FileRecord> = (0..120_000)
        .map(|index| match index % 1_000 {
            0 => light_record(
                "/big",
                &format!("/big/dir-{}/needle-{index}.cfg", index % 7),
            ),
            _ => light_record("/big", &format!("/big/dir-{}/file-{index}.dat", index % 7)),
        })
        .collect();
    install_light(&db, &records);

    for (mode, query, expected) in [
        (QueryMode::Regex, r"^needle-\d+\.cfg$", 120),
        (QueryMode::Glob, "needle-*.cfg", 120),
        (QueryMode::Fuzzy, "ndl9000cfg", 21),
        (QueryMode::Contains, "dir-3/needle", 17),
    ] {
        let found: Vec<String> = db
            .search_with_options(
                &SearchOptions::new(query)
                    .with_mode(mode)
                    .with_limit(usize::MAX),
            )
            .expect("search works")
            .into_iter()
            .map(|result| result.path)
            .collect();
        let unique: BTreeSet<&String> = found.iter().collect();
        assert_eq!(
            (found.len(), unique.len()),
            (expected, expected),
            "{mode:?} {query:?}"
        );
    }
}

// The tests below cover the name file (`index.names`) that searches read, and the changes
// table that keeps it correct between compactions.

fn paths_of(db: &Database, options: SearchOptions) -> BTreeSet<String> {
    db.search_with_options(&options.with_limit(usize::MAX))
        .expect("search works")
        .into_iter()
        .map(|result| result.path)
        .collect()
}

fn path_set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|path| path.to_string()).collect()
}

fn change_count(db_path: &Path) -> i64 {
    rusqlite::Connection::open(db_path)
        .expect("plain connection opens")
        .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
        .expect("count change rows")
}

/// A file-backed index of `paths` whose name file is built and has no changes.
fn file_backed(paths: &[&str]) -> (TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    let records: Vec<FileRecord> = paths.iter().map(|path| light_record("/r", path)).collect();
    install_light(&db, &records);
    (dir, db_path)
}

#[test]
fn name_file_search_agrees_with_the_matcher() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(dir.path().join("index.sqlite")).expect("db opens");
    let bases = [
        "Report",
        "report",
        "CAF\u{c9}",
        "caf\u{e9}",
        "notes",
        "my file",
        "data.v2",
        "main",
    ];
    let extensions = ["rs", "txt", "cfg", "RS", "md", ""];
    let records: Vec<FileRecord> = (0..60_000)
        .map(|index| {
            let extension = extensions[(index / 8) % extensions.len()];
            let dot = if extension.is_empty() { "" } else { "." };
            let name = format!("{}-{index}{dot}{extension}", bases[index % bases.len()]);
            light_record("/corpus", &format!("/corpus/src-{}/{name}", index % 53))
        })
        .collect();
    install_light(&db, &records);

    let modes = [
        QueryMode::Contains,
        QueryMode::Exact,
        QueryMode::Prefix,
        QueryMode::Suffix,
        QueryMode::Fuzzy,
        QueryMode::Regex,
        QueryMode::Glob,
    ];
    let queries = [
        "report",
        "REPORT",
        "r",
        "zq",
        "1234",
        ".rs",
        "caf\u{e9}",
        "CAF\u{c9}",
        "src-7/",
        "/corpus/src-3",
        "report-12",
        "^report",
        "report.*7$",
        "*.cfg",
        "  notes  ",
        "data.v2",
    ];
    let mut scorer = QueryScorer::new();
    let mut checked = 0;
    let mut matched_rows = 0;
    for mode in modes {
        for query in queries {
            // A query the mode cannot compile (a leading `*` as a regex) has nothing to check.
            let Ok(compiled) = CompiledQuery::compile(mode, query) else {
                continue;
            };
            // The rule: Contains without a path separator in the query tests the name only.
            // Every other case tests the name and the path.
            let name_only = mode == QueryMode::Contains && !query.trim().contains(['/', '\\']);
            let expected: BTreeSet<String> = records
                .iter()
                .filter(|record| {
                    if name_only {
                        compiled.is_match(&mut scorer, &record.name)
                    } else {
                        compiled
                            .matches_any(&mut scorer, [record.name.as_str(), record.path.as_str()])
                    }
                })
                .map(|record| record.path.clone())
                .collect();
            let actual = paths_of(&db, SearchOptions::new(query).with_mode(mode));
            assert_eq!(actual, expected, "{mode:?} query {query:?}");
            matched_rows += expected.len();
            checked += 1;
        }
    }
    assert!(checked >= 100, "only {checked} combinations ran");
    assert!(matched_rows > 0, "no query matched any row");
}

#[test]
fn a_changed_row_is_found_without_rewriting_the_name_file() {
    let (_dir, db_path) = file_backed(&["/r/old.txt"]);
    let db = Database::open(&db_path).expect("db reopens");
    let name_file = name_file_path(&db_path);
    let before = locator::name_file::NameFile::open(&name_file)
        .expect("name file opens")
        .snapshot_id();

    db.upsert_file(&light_record("/r", "/r/fresh-file.txt"))
        .expect("upsert works");
    let found = paths_of(&db, SearchOptions::new("fresh-file"));
    assert_eq!(found, path_set(&["/r/fresh-file.txt"]));

    let after = locator::name_file::NameFile::open(&name_file)
        .expect("name file still opens")
        .snapshot_id();
    assert_eq!(before, after, "a changed row must not cause a rewrite");
    assert_eq!(change_count(&db_path), 1);
}

#[test]
fn a_removal_hides_a_row_and_its_subtree() {
    let (_dir, db_path) = file_backed(&["/r/dir/a.txt", "/r/dir/sub/b.txt", "/r/dirx/c.txt"]);
    let db = Database::open(&db_path).expect("db reopens");
    assert_eq!(
        paths_of(&db, SearchOptions::new("txt")),
        path_set(&["/r/dir/a.txt", "/r/dir/sub/b.txt", "/r/dirx/c.txt"])
    );

    db.mark_path_deleted("/r/dir").expect("delete works");
    assert_eq!(
        paths_of(&db, SearchOptions::new("txt")),
        path_set(&["/r/dirx/c.txt"])
    );
}

#[test]
fn a_deleted_then_recreated_path_is_found_once() {
    let (_dir, db_path) = file_backed(&["/r/a.txt", "/r/dir/b.txt"]);
    let db = Database::open(&db_path).expect("db reopens");

    db.mark_path_deleted("/r/a.txt").expect("delete works");
    db.upsert_file(&light_record("/r", "/r/a.txt"))
        .expect("recreate works");
    // The folder goes away and a file comes back beneath it: found once, not twice, though
    // both a subtree entry and a one-path entry name it.
    db.mark_path_deleted("/r/dir").expect("delete works");
    db.upsert_file(&light_record("/r", "/r/dir/b.txt"))
        .expect("recreate works");

    let results = db
        .search_with_options(&SearchOptions::new("txt").with_limit(usize::MAX))
        .expect("search works");
    let mut paths: Vec<&str> = results.iter().map(|result| result.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["/r/a.txt", "/r/dir/b.txt"]);
}

#[test]
fn a_corrupt_name_file_is_an_error_not_an_empty_result() {
    let (_dir, db_path) = file_backed(&["/r/alpha.txt", "/r/beta.txt", "/r/gamma.md"]);
    let name_file = name_file_path(&db_path);

    fs::write(&name_file, b"this is not a name file").expect("overwrite the name file");
    let db = Database::open(&db_path).expect("db reopens");
    let error = db
        .search_with_options(&SearchOptions::new("txt"))
        .expect_err("a name file that fails validation is an error");
    assert!(
        format!("{error:#}").contains("name file"),
        "the error names the file: {error:#}"
    );
}

#[test]
fn a_read_only_database_without_a_name_file_searches_without_writing_one() {
    let (_dir, db_path) = file_backed(&["/r/alpha.txt", "/r/beta.txt", "/r/gamma.md"]);
    let name_file = name_file_path(&db_path);
    fs::remove_file(&name_file).expect("remove the name file");

    let db = Database::open_readonly(&db_path).expect("read-only open works");
    assert!(
        paths_of(&db, SearchOptions::new("txt")).is_empty(),
        "the rows are lost with the file"
    );
    assert!(
        !name_file.exists(),
        "a read-only search must not write the file"
    );
}

#[test]
fn filters_apply_to_name_file_rows() {
    let kinded = |path: &str, kind: &str| {
        let mut record = light_record("/f", path);
        record.kind = kind.to_string();
        record
    };
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let db = Database::open(&db_path).expect("db opens");
    install_light(
        &db,
        &[
            kinded("/f/alpha.txt", "text"),
            kinded("/f/beta.rs", "code"),
            kinded("/f/alpha-notes.md", "text"),
        ],
    );
    // These two are not in the name file yet.
    db.upsert_files(&[
        kinded("/f/alpha-new.rs", "code"),
        kinded("/f/gamma.txt", "text"),
    ])
    .expect("upsert works");
    assert_eq!(change_count(&db_path), 2);

    let by_kind = paths_of(&db, SearchOptions::new("a").with_kind("text").unwrap());
    assert_eq!(
        by_kind,
        path_set(&["/f/alpha.txt", "/f/alpha-notes.md", "/f/gamma.txt"])
    );
    let by_ext = paths_of(&db, SearchOptions::new("a").with_exts("rs").unwrap());
    assert_eq!(by_ext, path_set(&["/f/beta.rs", "/f/alpha-new.rs"]));
    let by_name = paths_of(&db, SearchOptions::new("a").with_name("ALPHA").unwrap());
    assert_eq!(
        by_name,
        path_set(&["/f/alpha.txt", "/f/alpha-notes.md", "/f/alpha-new.rs"])
    );
}

#[test]
fn more_than_2000_changes_are_folded_into_the_name_file_on_search() {
    let (_dir, db_path) = file_backed(&["/r/old.txt"]);
    let db = Database::open(&db_path).expect("db reopens");
    let records: Vec<FileRecord> = (0..2_100)
        .map(|index| light_record("/r", &format!("/r/new/item-{index}.log")))
        .collect();
    db.upsert_files(&records).expect("upsert works");
    assert_eq!(change_count(&db_path), 2_100);

    let found = paths_of(&db, SearchOptions::new("item-"));
    assert_eq!(found.len(), 2_100);
    assert!(found.contains("/r/new/item-2099.log"));
    assert_eq!(change_count(&db_path), 0, "the search should have folded");
    assert_eq!(
        paths_of(&db, SearchOptions::new("old.txt")),
        path_set(&["/r/old.txt"])
    );
}

/// Builds a 1,000,000 row index the way a finished scan does, prints how long that took and
/// how big the two files are, then times each kind of search against it. Run by hand with
/// `--ignored --nocapture`; it asserts nothing.
#[test]
#[ignore]
fn name_file_bench() {
    use std::time::Instant;

    const ROWS: usize = 1_000_000;
    const DIRS: usize = 5_000;
    const EXTS: [&str; 8] = ["rs", "txt", "md", "pdf", "png", "json", "toml", "log"];
    const WORDS: [&str; 10] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    ];

    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("index.db");
    let root = "/bench/root";
    let db = Database::open(&db_path).expect("open");

    let build_start = Instant::now();
    let mut rows = NameFileBuilder::default();
    for i in 0..ROWS {
        let d = i % DIRS;
        // About 1% of names carry "needle"; one name is unique.
        let stem = if i % 100 == 7 {
            format!("needle_{i}")
        } else if i == 123_456 {
            "uniquefile".to_string()
        } else {
            format!(
                "{}_{}_{i}",
                WORDS[i % WORDS.len()],
                WORDS[(i / 10) % WORDS.len()]
            )
        };
        let ext = EXTS[i % EXTS.len()];
        let path = format!(
            "{root}/projects/area{:02}/group{:03}/module{d:04}/sub{:02}/{stem}.{ext}",
            d % 50,
            d % 500,
            i % 20
        );
        rows.push(&path, "file").expect("push row");
    }
    let push_wall = build_start.elapsed();

    let install_start = Instant::now();
    db.install_scan(root, rows, 1_000).expect("install scan");
    let install_wall = install_start.elapsed();
    drop(db);
    let sqlite_bytes = fs::metadata(&db_path).expect("database file").len();
    let names_bytes = fs::metadata(name_file_path(&db_path))
        .expect("name file")
        .len();
    eprintln!("rows pushed: {ROWS} in {push_wall:?}");
    eprintln!("install_scan wall {install_wall:?}");
    eprintln!("build and install {:?}", build_start.elapsed());
    eprintln!("database file {sqlite_bytes} bytes");
    eprintln!("name file {names_bytes} bytes");

    let db = Database::open(&db_path).expect("reopen");
    let cases: Vec<(&str, SearchOptions)> = vec![
        ("substring (~1%)", SearchOptions::new("needle")),
        (
            "exact name",
            SearchOptions::new("uniquefile.rs").with_mode(QueryMode::Exact),
        ),
        (
            "prefix",
            SearchOptions::new("needle_9").with_mode(QueryMode::Prefix),
        ),
        (
            "suffix",
            SearchOptions::new("_999.log").with_mode(QueryMode::Suffix),
        ),
        (
            "regex",
            SearchOptions::new(r"^needle_\d+7\.pdf$").with_mode(QueryMode::Regex),
        ),
        (
            "glob",
            SearchOptions::new("needle_*.pdf").with_mode(QueryMode::Glob),
        ),
        (
            "fuzzy",
            SearchOptions::new("ndlpdf").with_mode(QueryMode::Fuzzy),
        ),
        ("one letter", SearchOptions::new("e")),
        ("with slash", SearchOptions::new("area07/group007")),
    ];
    for (label, options) in cases {
        // One warm-up run, then the median of five.
        let warm = db.search_with_options(&options).expect("warm-up search");
        let mut times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let found = db.search_with_options(&options).expect("search");
            times.push(start.elapsed());
            assert_eq!(found.len(), warm.len());
        }
        times.sort();
        eprintln!(
            "search {label:<16} median {:>10?} results {}",
            times[2],
            warm.len()
        );
    }
}

#[test]
fn extension_filter_and_exact_needles_lose_no_rows() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(dir.path().join("index.sqlite")).expect("db opens");
    let extensions = ["toml", "cfg", "rs", "", "TOML", "bak"];
    let bases = ["alpha", "Settings", "beta", "gamma"];
    let mut records: Vec<FileRecord> = (0..60_000)
        .map(|index| {
            let extension = extensions[(index / 4) % extensions.len()];
            let dot = if extension.is_empty() { "" } else { "." };
            let name = format!("{}-{index}{dot}{extension}", bases[index % bases.len()]);
            light_record("/big", &format!("/big/dir-{}/{name}", index % 24))
        })
        .collect();
    for path in [
        "/big/dir-1/SETTINGS.TOML",
        "/big/dir-2/settings.toml",
        "/big/dir-3/.toml",
        "/big/dir-4/x.toml.bak",
        "/big/dir-5/config",
        "/big/dir-6/CONFIG",
        "/big/dir-7/myconfig",
        "/big/dir-8/config.toml",
        "/big/dir.toml/plain",
        "/big/dir.toml/config",
        "/big/dir-9/a.cfg",
        "/big/dir-9/fig",
    ] {
        records.push(light_record("/big", path));
    }
    install_light(&db, &records);

    let name_only = |mode: QueryMode, query: &str| {
        mode == QueryMode::Contains && !query.trim().contains(['/', '\\'])
    };
    let mut scorer = QueryScorer::new();
    let mut searches: Vec<(QueryMode, &str, &str)> = vec![
        (QueryMode::Fuzzy, "cfg", "toml"),
        (QueryMode::Regex, "^s", "toml"),
        (QueryMode::Glob, "*", "toml"),
        (QueryMode::Contains, "a", "toml"),
        (QueryMode::Contains, "settings", "toml"),
        (QueryMode::Regex, ".", "toml,cfg"),
        (QueryMode::Exact, "config", ""),
        (QueryMode::Exact, "CONFIG", ""),
        (QueryMode::Exact, "config.toml", ""),
        (QueryMode::Exact, "fig", ""),
    ];
    if SearchOptions::new("").validate().is_ok() {
        searches.push((QueryMode::Contains, "", "toml"));
    }
    let mut matched_rows = 0;
    for (mode, query, exts) in searches {
        let wanted: Vec<&str> = exts.split(',').filter(|ext| !ext.is_empty()).collect();
        let compiled = CompiledQuery::compile(mode, query).expect("query compiles");
        let expected: BTreeSet<String> = records
            .iter()
            .filter(|record| {
                if !wanted.is_empty() {
                    let extension = Path::new(&record.name)
                        .extension()
                        .map(|ext| ext.to_string_lossy().to_ascii_lowercase());
                    if !extension.is_some_and(|ext| wanted.contains(&ext.as_str())) {
                        return false;
                    }
                }
                if name_only(mode, query) {
                    compiled.is_match(&mut scorer, &record.name)
                } else {
                    compiled.matches_any(&mut scorer, [record.name.as_str(), record.path.as_str()])
                }
            })
            .map(|record| record.path.clone())
            .collect();
        let mut options = SearchOptions::new(query).with_mode(mode);
        if !wanted.is_empty() {
            options = options.with_filters(SearchFilters::new().with_exts(exts).expect("exts"));
        }
        let actual = paths_of(&db, options);
        assert_eq!(actual, expected, "{mode:?} query {query:?} exts {exts:?}");
        matched_rows += expected.len();
    }
    assert!(matched_rows > 0, "no search matched any row");
}

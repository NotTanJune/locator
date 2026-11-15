use std::collections::BTreeMap;
use std::path::MAIN_SEPARATOR as SEP;

use locator::name_file::{
    install, name_file_path, LookupCache, NameFile, NameFileBuilder, BLOCK_ROWS, INDEX_FORMAT,
};

fn sep() -> String {
    SEP.to_string()
}

fn build(rows: &[(String, String)]) -> NameFile {
    let mut builder = NameFileBuilder::default();
    for (path, kind) in rows {
        builder.push(path, kind).unwrap();
    }
    NameFile::from_bytes(builder.finish(7).unwrap()).unwrap()
}

fn all_rows(file: &NameFile) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut reader = file.reader();
    reader.for_each(0..file.len(), &mut |row| {
        assert!(row.path.ends_with(row.name));
        found.push((row.path.to_string(), row.kind.to_string()));
        true
    });
    found
}

fn under(file: &NameFile, path: &str) -> Vec<String> {
    let mut found = Vec::new();
    file.reader().for_each_under(path, &mut |row| {
        found.push(row.path.to_string());
        true
    });
    found.sort();
    found
}

fn sorted(mut rows: Vec<(String, String)>) -> Vec<(String, String)> {
    rows.sort();
    rows
}

fn pair(path: &str, kind: &str) -> (String, String) {
    (path.to_string(), kind.to_string())
}

fn many_rows() -> Vec<(String, String)> {
    let names = [
        "Readme.md",
        "main.RS",
        "caf\u{e9} Menu.txt",
        "a.b.c",
        "Z",
        "x y.TXT",
        "\u{c9}t\u{e9}.doc",
    ];
    let kinds = ["file", "folder", "symlink"];
    let mut rows = Vec::new();
    for index in 0..10_000usize {
        let folder = index % 300;
        let name = format!("{}{}", names[index % names.len()], index);
        rows.push((
            format!("{SEP}root{SEP}d{folder}{SEP}sub{}{SEP}{name}", folder % 7),
            kinds[index % kinds.len()].to_string(),
        ));
    }
    rows
}

#[test]
fn rows_round_trip_with_case_and_kind() {
    let rows = many_rows();
    assert!(rows.len() > 2 * BLOCK_ROWS);
    let file = build(&rows);
    assert_eq!(file.len(), rows.len());
    assert_eq!(sorted(all_rows(&file)), sorted(rows));
}

#[test]
fn paths_with_and_without_a_leading_separator_stay_distinct() {
    let rows = vec![
        pair("x.txt", "file"),
        pair(&format!("{SEP}x.txt"), "folder"),
        pair(&format!("{SEP}a{SEP}x.txt"), "symlink"),
    ];
    let file = build(&rows);
    assert_eq!(sorted(all_rows(&file)), sorted(rows));
}

#[test]
fn a_later_push_of_the_same_path_wins() {
    let path = format!("{SEP}a{SEP}b.txt");
    let file = build(&[
        pair(&path, "file"),
        pair(&format!("{SEP}a{SEP}c"), "file"),
        pair(&path, "folder"),
    ]);
    let rows = sorted(all_rows(&file));
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&pair(&path, "folder")));
}

#[test]
fn names_of_a_block_hold_every_row_once() {
    let file = build(&many_rows());
    assert!(file.block_count() >= 3);
    let mut reader = file.reader();
    let mut by_row = Vec::new();
    for block in 0..file.block_count() {
        let mut scratch = Vec::new();
        let names = reader.names(block, &mut scratch).expect("names decode");
        let expected = (file.len() - block * BLOCK_ROWS).min(BLOCK_ROWS);
        assert_eq!(names.iter().filter(|byte| **byte == 0).count(), expected);
        let (mut start, mut index) = (0, 0);
        while start < names.len() {
            let end = start + names[start..].iter().position(|byte| *byte == 0).unwrap();
            let row = reader.row(block, index, &names[start..end], start).unwrap();
            by_row.push((row.path.to_string(), row.kind.to_string()));
            start = end + 1;
            index += 1;
        }
        assert_eq!(index, expected);
    }
    assert_eq!(by_row, all_rows(&file));
}

#[test]
fn a_file_that_keeps_names_returns_the_same_rows_from_its_own_copy() {
    let mut builder = NameFileBuilder::default();
    for (path, kind) in &many_rows() {
        builder.push(path, kind).unwrap();
    }
    let image = builder.finish(7).unwrap();
    let plain = NameFile::from_bytes(image.clone()).unwrap();
    let keeping = NameFile::from_bytes(image).unwrap().keeping_names();

    assert_eq!(all_rows(&keeping), all_rows(&plain));
    assert_eq!(all_rows(&keeping), all_rows(&plain));

    let (keeping_reader, plain_reader) = (keeping.reader(), plain.reader());
    for block in 0..keeping.block_count() {
        let (mut kept_scratch, mut plain_scratch) = (Vec::new(), Vec::new());
        let first = keeping_reader.names(block, &mut kept_scratch).unwrap();
        let first_ptr = first.as_ptr();
        let first_bytes = first.to_vec();
        let second = keeping_reader.names(block, &mut kept_scratch).unwrap();
        assert_eq!(first_ptr, second.as_ptr());
        assert_ne!(first_ptr, kept_scratch.as_ptr());

        let decoded = plain_reader.names(block, &mut plain_scratch).unwrap();
        let (decoded_ptr, decoded_bytes) = (decoded.as_ptr(), decoded.to_vec());
        assert_eq!(decoded_ptr, plain_scratch.as_ptr());
        assert_eq!(decoded_bytes, first_bytes);
    }
}

#[test]
fn for_each_under_returns_the_exact_subtree() {
    let rows = ["dir", "dir-2", "dir.txt"]
        .iter()
        .map(|name| pair(&format!("{SEP}r{SEP}{name}"), "file"))
        .chain([
            pair(&format!("{SEP}r{SEP}dir{SEP}a.txt"), "file"),
            pair(&format!("{SEP}r{SEP}dir{SEP}deep{SEP}b.txt"), "file"),
            pair(&format!("{SEP}r{SEP}dir-2{SEP}c.txt"), "file"),
            pair(&format!("{SEP}r{SEP}dir0{SEP}d.txt"), "file"),
        ])
        .collect::<Vec<_>>();
    let file = build(&rows);
    let dir = format!("{SEP}r{SEP}dir");
    let expected = vec![
        dir.clone(),
        format!("{dir}{SEP}a.txt"),
        format!("{dir}{SEP}deep{SEP}b.txt"),
    ];
    assert_eq!(under(&file, &dir), expected);
    assert_eq!(
        under(&file, &format!("{SEP}r{SEP}dir.txt")),
        vec![format!("{SEP}r{SEP}dir.txt")]
    );
    assert!(under(&file, &format!("{SEP}r{SEP}nothing")).is_empty());
}

#[test]
fn kind_of_finds_rows_and_misses_others() {
    let rows = many_rows();
    let file = build(&rows);
    let mut cache = LookupCache::default();
    let expected: BTreeMap<_, _> = rows.iter().cloned().collect();
    for (path, kind) in &expected {
        assert_eq!(
            file.kind_of(&mut cache, path),
            Some(kind.as_str()),
            "{path}"
        );
    }
    let sample = rows
        .iter()
        .find(|(path, _)| path.contains("Readme"))
        .unwrap()
        .0
        .clone();
    assert!(file
        .kind_of(&mut cache, &sample.to_ascii_uppercase())
        .is_none());
    assert!(file
        .kind_of(&mut cache, &sample.to_ascii_lowercase())
        .is_none());
    assert!(file
        .kind_of(&mut cache, &format!("{SEP}root{SEP}missing.txt"))
        .is_none());
    assert!(file.kind_of(&mut cache, "missing").is_none());
}

#[test]
fn an_empty_builder_gives_an_empty_valid_file() {
    let file = NameFile::from_bytes(NameFileBuilder::default().finish(0).unwrap()).unwrap();
    assert!(file.is_empty());
    assert_eq!(file.len(), 0);
    assert_eq!(file.block_count(), 0);
    assert_eq!(file.snapshot_id(), 1);
    assert!(all_rows(&file).is_empty());
    assert!(under(&file, "x").is_empty());
    assert!(file.kind_of(&mut LookupCache::default(), "x").is_none());
}

#[test]
fn a_damaged_file_is_rejected_or_skipped_without_a_panic() {
    let rows = many_rows();
    let mut builder = NameFileBuilder::default();
    for (path, kind) in &rows {
        builder.push(path, kind).unwrap();
    }
    let image = builder.finish(3).unwrap();
    for step in 1..=12 {
        let cut = image.len() * step / 13;
        // A cut file may fail to open; if it opens, reading it must not panic.
        if let Ok(file) = NameFile::from_bytes(image[..cut].to_vec()) {
            let _ = all_rows(&file);
        }
    }
    assert!(NameFile::from_bytes(Vec::new()).is_err());
    assert!(NameFile::from_bytes(image[..10].to_vec()).is_err());

    let file = NameFile::from_bytes(image.clone()).unwrap();
    // The blocks section is the last one, so the last third of the image is block data.
    for position in [
        image.len() - 5,
        image.len() * 3 / 4,
        image.len() - image.len() / 8,
    ] {
        let mut damaged = image.clone();
        damaged[position] ^= 0xFF;
        if let Ok(opened) = NameFile::from_bytes(damaged) {
            let found = all_rows(&opened);
            assert!(found.len() <= file.len());
            let mut cache = LookupCache::default();
            let _ = opened.kind_of(&mut cache, &rows[0].0);
            let _ = under(&opened, &format!("{SEP}root"));
        }
    }
}

#[test]
fn a_newer_format_is_refused_with_the_update_hint() {
    let mut image = NameFileBuilder::default().finish(1).unwrap();
    image[8..12].copy_from_slice(&(INDEX_FORMAT + 1).to_le_bytes());
    let error = NameFile::from_bytes(image).err().unwrap();
    assert!(error.to_string().contains("run `lctr update`"), "{error}");
}

#[test]
fn install_replaces_the_file_and_leaves_no_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let target = name_file_path(&dir.path().join("index.db"));
    assert_eq!(target.file_name().unwrap(), "index.names");
    let mut first = NameFileBuilder::default();
    first.push("a.txt", "file").unwrap();
    install(&first.finish(1).unwrap(), &target).unwrap();
    let old = NameFile::open(&target).unwrap();

    let mut second = NameFileBuilder::default();
    second.push("b.txt", "file").unwrap();
    second.push("c.txt", "file").unwrap();
    install(&second.finish(2).unwrap(), &target).unwrap();

    // The mapping opened before the rename still shows the old rows.
    assert_eq!(all_rows(&old), vec![pair("a.txt", "file")]);
    let new = NameFile::open(&target).unwrap();
    assert_eq!(new.snapshot_id(), 2);
    assert_eq!(new.len(), 2);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn folders_without_rows_and_deep_trees_work() {
    // Only the leaf folder has rows; every ancestor must still be in the table.
    let deep = (0..40)
        .map(|level| format!("d{level}"))
        .collect::<Vec<_>>()
        .join(&sep());
    let path = format!("{SEP}{deep}{SEP}leaf.txt");
    let file = build(&[pair(&path, "file"), pair("top", "file")]);
    assert_eq!(
        sorted(all_rows(&file)),
        sorted(vec![pair(&path, "file"), pair("top", "file")])
    );
    assert_eq!(under(&file, &format!("{SEP}d0")), vec![path.clone()]);
    assert_eq!(
        file.kind_of(&mut LookupCache::default(), &path),
        Some("file")
    );
}

#[test]
fn a_name_with_a_zero_byte_or_too_many_kinds_fails_in_finish() {
    let mut builder = NameFileBuilder::default();
    builder.push("a\0b", "file").unwrap();
    assert!(builder.finish(1).is_err());
    let mut builder = NameFileBuilder::default();
    for kind in 0..256 {
        builder
            .push(&format!("f{kind}"), &format!("kind{kind}"))
            .unwrap();
    }
    assert!(builder.finish(1).is_err());
}

#[test]
fn has_rows_beneath_sees_rows_and_folders_only_under_the_path() {
    let mut builder = NameFileBuilder::default();
    builder
        .push(&format!("{SEP}r{SEP}dir{SEP}a.txt"), "file")
        .unwrap();
    builder
        .push(&format!("{SEP}r{SEP}dir-2{SEP}b.txt"), "file")
        .unwrap();
    let ask = |path: String| builder.has_rows_beneath(&path);

    assert!(ask(format!("{SEP}r{SEP}dir")));
    assert!(ask(format!("{SEP}r")));
    assert!(ask(format!("{SEP}r{SEP}dir{SEP}a.txt")));
    assert!(!ask(format!("{SEP}r{SEP}di")));
    assert!(!ask(format!("{SEP}r{SEP}dir{SEP}a")));
    assert!(!ask(format!("{SEP}r{SEP}dir{SEP}other")));
    assert!(!ask(format!("{SEP}elsewhere")));

    let mut sibling = NameFileBuilder::default();
    sibling
        .push(&format!("{SEP}r{SEP}dir-2{SEP}b.txt"), "file")
        .unwrap();
    assert!(!sibling.has_rows_beneath(&format!("{SEP}r{SEP}dir")));
    assert!(sibling.has_rows_beneath(&format!("{SEP}r{SEP}dir-2")));
}

#[test]
fn a_block_whose_names_do_not_decode_is_skipped_without_repeating_rows() {
    let rows = many_rows();
    let mut builder = NameFileBuilder::default();
    for (path, kind) in &rows {
        builder.push(path, kind).unwrap();
    }
    let mut image = builder.finish(5).unwrap();

    // The block index is section 5; each block has 48 bytes there. Making the raw length of
    // block 1, part 0 one too long makes that part fail to decode.
    let index_offset = u64::from_le_bytes(image[40 + 5 * 16..40 + 5 * 16 + 8].try_into().unwrap());
    let at = index_offset as usize + 48 + 12;
    let raw = u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
    image[at..at + 4].copy_from_slice(&(raw + 1).to_le_bytes());

    let file = NameFile::from_bytes(image).unwrap();
    let found = all_rows(&file);

    // The decode buffer is reused between blocks, so a failed block must not hand back the
    // rows of the block before it.
    assert_eq!(found.len(), rows.len() - BLOCK_ROWS);
    let mut paths: Vec<&String> = found.iter().map(|(path, _)| path).collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), found.len());
    let originals: std::collections::HashSet<&String> = rows.iter().map(|(path, _)| path).collect();
    assert!(paths.iter().all(|path| originals.contains(*path)));
}

use std::fs;

use locator::preview::{preview_for, Preview};

#[test]
fn classifies_text_binary_and_directory() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Text source file -> highlighted text preview.
    let source = dir.path().join("main.rs");
    fs::write(&source, "fn main() {\n    println!(\"hi\");\n}\n").expect("write source");
    assert!(matches!(preview_for(&source, 50), Preview::Text(_)));

    // Binary file (embedded NUL) -> info/metadata fallback.
    let binary = dir.path().join("blob.bin");
    fs::write(&binary, [0u8, 1, 2, 3, 0, 255, 7]).expect("write binary");
    assert!(matches!(preview_for(&binary, 50), Preview::Info(_)));

    // Directory -> info listing.
    assert!(matches!(preview_for(dir.path(), 50), Preview::Info(_)));
}

#[test]
fn large_files_are_not_fully_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let big = dir.path().join("huge.txt");
    // 3 MiB of text exceeds the preview byte ceiling.
    fs::write(&big, "a".repeat(3 * 1024 * 1024)).expect("write big");
    assert!(matches!(preview_for(&big, 50), Preview::Info(_)));
}

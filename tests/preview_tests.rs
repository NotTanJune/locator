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

fn info_text(preview: Preview) -> String {
    match preview {
        Preview::Info(lines) => lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        Preview::Text(_) => panic!("expected Info preview, got Text"),
        Preview::Image { .. } => panic!("expected Info preview, got Image"),
    }
}

#[test]
fn oversized_pdf_is_rejected_before_extraction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let big = dir.path().join("big.pdf");
    // Junk bytes are fine: the size gate fires before any PDF parsing.
    fs::write(&big, vec![0u8; 2 * 1024 * 1024 + 1]).expect("write big pdf");
    let text = info_text(preview_for(&big, 50));
    assert!(text.contains("too large"), "got: {text}");
}

#[test]
fn oversized_image_is_rejected_before_decode() {
    let dir = tempfile::tempdir().expect("temp dir");
    let big = dir.path().join("big.png");
    // Sparse file: set_len records the size without writing 50 MiB of data.
    let file = fs::File::create(&big).expect("create big png");
    file.set_len(50 * 1024 * 1024 + 1).expect("set sparse len");
    drop(file);
    let text = info_text(preview_for(&big, 50));
    assert!(text.contains("too large"), "got: {text}");
}

#[cfg(unix)]
#[test]
fn symlink_to_large_file_respects_size_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let huge = dir.path().join("huge.txt");
    fs::write(&huge, "a".repeat(3 * 1024 * 1024)).expect("write huge");
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&huge, &link).expect("symlink");
    assert!(info_text(preview_for(&link, 50)).contains("too large"));
}

#[cfg(unix)]
#[test]
fn symlink_to_small_text_still_previews() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("main.rs");
    fs::write(&source, "fn main() {}\n").expect("write source");
    let link = dir.path().join("link.rs");
    std::os::unix::fs::symlink(&source, &link).expect("symlink");
    assert!(matches!(preview_for(&link, 50), Preview::Text(_)));
}

#[cfg(unix)]
#[test]
fn broken_symlink_reports_info() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = dir.path().join("dangling.txt");
    std::os::unix::fs::symlink(dir.path().join("missing.txt"), &link).expect("symlink");
    assert!(info_text(preview_for(&link, 50)).contains("broken symlink"));
}

#[cfg(unix)]
#[test]
fn fifo_is_not_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fifo_path = dir.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("run mkfifo");
    assert!(status.success());

    let (tx, rx) = std::sync::mpsc::channel();
    let path = fifo_path.clone();
    std::thread::spawn(move || {
        let _ = tx.send(info_text(preview_for(&path, 50)));
    });
    let text = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("preview_for must not block on a FIFO");
    assert!(text.contains("special file"));
}

#[test]
fn directory_listing_is_capped() {
    let dir = tempfile::tempdir().expect("temp dir");
    for i in 0..5_050 {
        fs::write(dir.path().join(format!("f{i:05}")), "").expect("write entry");
    }
    let preview = preview_for(dir.path(), 10);
    assert!(matches!(preview, Preview::Info(_)));
    assert!(info_text(preview).contains("showing first 5000 entries"));
}

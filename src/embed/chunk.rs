//! Text extraction and chunking. Pure functions except for reading the file in
//! `classify_and_extract`.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

pub const TARGET_CHARS: usize = 1800;
pub const OVERLAP_CHARS: usize = 200;
pub const MAX_CHUNKS_PER_FILE: usize = 128;
pub const MAX_TEXT_BYTES: u64 = 1_048_576;
pub const MAX_PDF_BYTES: u64 = 25 * 1_048_576;
pub const MAX_IMAGE_BYTES: u64 = 25 * 1_048_576;
pub const SNIPPET_CHARS: usize = 240;
const SNIFF_BYTES: usize = 8 * 1024;
const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "bmp", "gif"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ordinal: u32,
    pub start_byte: u64,
    pub text: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extracted {
    Text(String),
    Image,
    Skipped(SkipReason),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    TooLarge,
    Binary,
    UnsupportedKind,
    Empty,
    /// A private key block or an AWS access key id in the first 8 KiB.
    PrivateKey,
}

/// Splits `text` into overlapping chunks of about `TARGET_CHARS` characters,
/// cutting at paragraph, line, sentence, or word boundaries when possible.
pub fn chunk_text(text: &str) -> Vec<Chunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    // offs[i] is the byte offset of character i; offs[n] is text.len().
    let offs: Vec<usize> = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    let n = offs.len() - 1;

    let mut chunks = Vec::new();
    let mut start = 0usize;
    loop {
        if chunks.len() >= MAX_CHUNKS_PER_FILE {
            break;
        }
        let hard_end = (start + TARGET_CHARS).min(n);
        let end = if hard_end == n {
            n
        } else {
            find_cut(text, &offs, start, hard_end)
        };
        let slice = &text[offs[start]..offs[end]];
        if !slice.trim().is_empty() {
            chunks.push(Chunk {
                ordinal: chunks.len() as u32,
                start_byte: offs[start] as u64,
                text: slice.to_string(),
                snippet: snippet_of(slice),
            });
        }
        if end >= n {
            break;
        }
        start = end.saturating_sub(OVERLAP_CHARS).max(start + 1);
    }
    chunks
}

/// Picks the cut for a window `[start, hard_end)` (character indices). It prefers
/// the last paragraph break, then line break, sentence end, and space, searching only
/// the last 20 percent of the window. Returns a character index strictly after `start`.
fn find_cut(text: &str, offs: &[usize], start: usize, hard_end: usize) -> usize {
    let window = hard_end - start;
    let search_from = hard_end - window / 5;
    let region = &text[offs[search_from]..offs[hard_end]];
    for separator in ["\n\n", "\n", ". ", " "] {
        if let Some(position) = region.rfind(separator) {
            let cut_byte = offs[search_from] + position + separator.len();
            if let Ok(index) = offs.binary_search(&cut_byte) {
                if index > start {
                    return index;
                }
            }
        }
    }
    hard_end
}

fn snippet_of(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(SNIPPET_CHARS)
        .collect()
}

/// Decides how a file is embedded and extracts its text (or marks it as an image).
pub fn classify_and_extract(
    path: &Path,
    kind: &str,
    extension: Option<&str>,
    size_bytes: u64,
) -> Extracted {
    match kind {
        "folder" | "video" | "audio" | "archive" => Extracted::Skipped(SkipReason::UnsupportedKind),
        "image" => {
            let lower = extension.map(str::to_ascii_lowercase).unwrap_or_default();
            if !IMAGE_EXTENSIONS.contains(&lower.as_str()) {
                Extracted::Skipped(SkipReason::UnsupportedKind)
            } else if size_bytes > MAX_IMAGE_BYTES {
                Extracted::Skipped(SkipReason::TooLarge)
            } else {
                Extracted::Image
            }
        }
        "pdf" => extract_pdf(path, size_bytes),
        _ => extract_text_file(path, size_bytes),
    }
}

fn regular_file(path: &Path) -> Result<fs::Metadata, String> {
    // Follows symlinks so the target is checked, as src/preview.rs does.
    let metadata = fs::metadata(path).map_err(|error| format!("cannot read metadata: {error}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".to_string());
    }
    Ok(metadata)
}

fn extract_pdf(path: &Path, size_bytes: u64) -> Extracted {
    if size_bytes > MAX_PDF_BYTES {
        return Extracted::Skipped(SkipReason::TooLarge);
    }
    if let Err(message) = regular_file(path) {
        return Extracted::Failed(message);
    }
    // A panic on a malformed file only fails this file.
    match crate::preview::extract_pdf_text(path) {
        Ok(text) if text.trim().is_empty() => Extracted::Skipped(SkipReason::Empty),
        Ok(text) => Extracted::Text(text),
        Err(message) => Extracted::Failed(message),
    }
}

/// Content guard for text files: a PEM private key block or an AWS access key id in the first
/// `SNIFF_BYTES`. Later bytes are not checked.
fn holds_credential(sniff: &[u8]) -> bool {
    static AWS_ACCESS_KEY_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let text = String::from_utf8_lossy(sniff);
    if text.contains("PRIVATE KEY-----") {
        return true;
    }
    AWS_ACCESS_KEY_ID
        .get_or_init(|| regex::Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("static regex"))
        .is_match(&text)
}

fn extract_text_file(path: &Path, size_bytes: u64) -> Extracted {
    if let Err(message) = regular_file(path) {
        return Extracted::Failed(message);
    }
    let truncated = size_bytes > MAX_TEXT_BYTES;
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|file| file.take(MAX_TEXT_BYTES).read_to_end(&mut bytes));
    if let Err(error) = read {
        return Extracted::Failed(format!("read failed: {error}"));
    }
    if bytes.is_empty() {
        return Extracted::Skipped(SkipReason::Empty);
    }
    let sniff = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if sniff.contains(&0) {
        return Extracted::Skipped(SkipReason::Binary);
    }
    if holds_credential(sniff) {
        return Extracted::Skipped(SkipReason::PrivateKey);
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_string(),
        // A truncated read may end inside a multi-byte sequence; keep the valid prefix.
        Err(error) if truncated && error.error_len().is_none() => {
            match std::str::from_utf8(&bytes[..error.valid_up_to()]) {
                Ok(text) => text.to_string(),
                Err(_) => return Extracted::Skipped(SkipReason::Binary),
            }
        }
        Err(_) => return Extracted::Skipped(SkipReason::Binary),
    };
    if text.trim().is_empty() {
        return Extracted::Skipped(SkipReason::Empty);
    }
    Extracted::Text(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_boundaries_are_valid_utf8_for_multibyte_text() {
        let text = "héllo wörld 日本語のテキスト。 ".repeat(400);
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(text.is_char_boundary(chunk.start_byte as usize));
            assert!(text.get(chunk.start_byte as usize..).is_some());
            assert!(!chunk.text.is_empty());
        }
    }

    #[test]
    fn consecutive_chunks_overlap() {
        let text = "word ".repeat(2000);
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2);
        let first_end = chunks[0].start_byte as usize + chunks[0].text.len();
        assert!((chunks[1].start_byte as usize) < first_end);
    }

    #[test]
    fn max_chunks_per_file_is_enforced() {
        let text = "x".repeat(TARGET_CHARS * (MAX_CHUNKS_PER_FILE + 20));
        assert_eq!(chunk_text(&text).len(), MAX_CHUNKS_PER_FILE);
    }

    #[test]
    fn whitespace_only_text_has_no_chunks() {
        assert!(chunk_text("  \n\t  ").is_empty());
        assert!(chunk_text("").is_empty());
    }

    #[test]
    fn snippet_collapses_whitespace_and_is_bounded() {
        let text = format!("alpha   beta\n\ngamma {}", "z".repeat(500));
        let chunks = chunk_text(&text);
        assert!(chunks[0].snippet.starts_with("alpha beta gamma"));
        assert!(chunks[0].snippet.chars().count() <= SNIPPET_CHARS);
    }

    #[test]
    fn nul_containing_file_is_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blob.dat");
        fs::write(&path, b"hello\0world").expect("write");
        let result = classify_and_extract(&path, "file", Some("dat"), 11);
        assert_eq!(result, Extracted::Skipped(SkipReason::Binary));
    }

    #[test]
    fn large_text_file_is_read_only_up_to_one_mib() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.txt");
        let mut body = "a".repeat(MAX_TEXT_BYTES as usize);
        body.push_str(&"TAILMARKER ".repeat(200_000));
        fs::write(&path, &body).expect("write");
        match classify_and_extract(&path, "text", Some("txt"), body.len() as u64) {
            Extracted::Text(text) => {
                assert!(text.len() <= MAX_TEXT_BYTES as usize);
                assert!(!text.contains("TAILMARKER"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn rust_source_labelled_file_is_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server.rs");
        fs::write(&path, "fn start_http_server(port: u16) {}\n").expect("write");
        match classify_and_extract(&path, "file", Some("rs"), 33) {
            Extracted::Text(text) => assert!(text.contains("start_http_server")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn unknown_binary_extension_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("data.bin");
        fs::write(&path, [0xFFu8, 0xFE, 0x00, 0x81, 0x82]).expect("write");
        let result = classify_and_extract(&path, "file", Some("bin"), 5);
        assert_eq!(result, Extracted::Skipped(SkipReason::Binary));
    }

    #[test]
    fn whitespace_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blank.txt");
        fs::write(&path, "   \n\n  ").expect("write");
        let result = classify_and_extract(&path, "text", Some("txt"), 7);
        assert_eq!(result, Extracted::Skipped(SkipReason::Empty));
    }

    #[test]
    fn garbage_pdf_fails_without_panicking_the_caller() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.pdf");
        fs::write(&path, b"this is not a pdf at all").expect("write");
        let result = classify_and_extract(&path, "pdf", Some("pdf"), 24);
        assert!(matches!(result, Extracted::Failed(_)), "{result:?}");
    }

    #[test]
    fn images_and_unsupported_kinds_are_classified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let png = dir.path().join("pic.png");
        fs::write(&png, [0u8; 4]).expect("write");
        assert_eq!(
            classify_and_extract(&png, "image", Some("png"), 4),
            Extracted::Image
        );
        assert_eq!(
            classify_and_extract(&png, "image", Some("tiff"), 4),
            Extracted::Skipped(SkipReason::UnsupportedKind)
        );
        assert_eq!(
            classify_and_extract(&png, "video", Some("mp4"), 4),
            Extracted::Skipped(SkipReason::UnsupportedKind)
        );
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn extract(name: &str, body: &str) -> Extracted {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        fs::write(&path, body).expect("write");
        let size = fs::metadata(&path).expect("metadata").len();
        classify_and_extract(&path, "text", Some("txt"), size)
    }

    #[test]
    fn pem_private_key_block_is_skipped() {
        let body = "notes\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n";
        assert_eq!(
            extract("plan.txt", body),
            Extracted::Skipped(SkipReason::PrivateKey)
        );
        let pkcs8 = "-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n";
        assert_eq!(
            extract("x.txt", pkcs8),
            Extracted::Skipped(SkipReason::PrivateKey)
        );
    }

    #[test]
    fn aws_access_key_id_is_skipped() {
        let body = "aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n";
        assert_eq!(
            extract("creds.txt", body),
            Extracted::Skipped(SkipReason::PrivateKey)
        );
    }

    #[test]
    fn public_certificate_and_short_akia_text_are_not_skipped() {
        let cert = "-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n";
        assert!(matches!(extract("cert-note.txt", cert), Extracted::Text(_)));
        assert!(matches!(
            extract("short.txt", "AKIA12345 is too short to be a key id\n"),
            Extracted::Text(_)
        ));
    }

    #[test]
    fn credential_past_the_sniff_window_is_not_checked() {
        let filler = "a".repeat(SNIFF_BYTES + 10);
        let body = format!("{filler}\n-----BEGIN PRIVATE KEY-----\n");
        assert!(matches!(extract("late.txt", &body), Extracted::Text(_)));
    }
}

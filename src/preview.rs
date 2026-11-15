//! File preview generation for the search TUI's preview pane. Produces
//! syntax-highlighted text (via `syntect`), decoded images (rendered inline by
//! `ratatui-image` in the TUI), extracted PDF text, directory listings, or a
//! metadata fallback for binaries. Heavy work (decode, highlight) is bounded and
//! the TUI caches the result per selected path so it is not recomputed each tick.

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Maximum bytes read for text/binary sniffing and PDF size guarding.
const MAX_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;

/// Formats other than JPEG are decoded in full before downscaling, so cap the file size
/// we are willing to decode. Larger than the text cap because real photos are big.
const MAX_IMAGE_PREVIEW_BYTES: u64 = 50 * 1024 * 1024;

/// Decoded images are shrunk to fit this many pixels per side before they leave the worker
/// thread. A terminal pane shows far fewer pixels, and the UI thread hashes and resizes
/// whatever it receives (a 12000x8000 photo is 288 MB decoded).
const MAX_IMAGE_PREVIEW_SIDE: u32 = 1024;

/// Text lines are cut to this many characters before highlighting. Syntect's cost grows with
/// line length, and a minified file can be one 2 MB line; the pane shows far less.
const MAX_PREVIEW_LINE_CHARS: usize = 500;

/// Directory previews stop reading after this many entries so a huge folder
/// cannot stall the UI thread. Sorting applies to the entries read.
const MAX_DIRECTORY_PREVIEW_ENTRIES: usize = 5_000;

/// What a preview resolved to. Text/Info carry ready-to-render lines; Image
/// carries a decoded image the TUI turns into a terminal graphics protocol.
pub enum Preview {
    Text(Vec<Line<'static>>),
    Image {
        image: Box<image::DynamicImage>,
        meta: Vec<Line<'static>>,
    },
    Info(Vec<Line<'static>>),
}

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

fn highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .expect("at least one default syntect theme");
        Highlighter { syntaxes, theme }
    })
}

/// Build a preview for `path`, capping text/PDF output at `max_lines`.
pub fn preview_for(path: &Path, max_lines: usize) -> Preview {
    // Follow symlinks: every reader below opens the target, so size caps and
    // file-type checks must apply to the target too, not the link.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            let is_link = std::fs::symlink_metadata(path)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false);
            let message = if is_link {
                format!("broken symlink: {error}")
            } else {
                format!("cannot read: {error}")
            };
            return Preview::Info(info_lines(&message));
        }
    };

    if metadata.is_dir() {
        return directory_preview(path, max_lines);
    }
    // FIFOs, sockets, and devices can block forever or never end when read.
    if !metadata.is_file() {
        return Preview::Info(info_lines("special file (not previewed)"));
    }

    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if is_image_ext(&extension) {
        return image_preview(path, &metadata);
    }
    if extension == "pdf" {
        return pdf_preview(path, &metadata, max_lines);
    }

    text_or_binary_preview(path, &metadata, &extension, max_lines)
}

fn directory_preview(path: &Path, max_lines: usize) -> Preview {
    let mut lines = vec![Line::from(Span::styled(
        format!("directory: {}", path.display()),
        Style::default().fg(Color::Cyan),
    ))];
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut names = entries
                .filter_map(|entry| entry.ok())
                .take(MAX_DIRECTORY_PREVIEW_ENTRIES)
                .map(|entry| {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let name = entry.file_name().to_string_lossy().to_string();
                    if is_dir {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect::<Vec<_>>();
            let truncated = names.len() == MAX_DIRECTORY_PREVIEW_ENTRIES;
            names.sort();
            for name in names.into_iter().take(max_lines) {
                lines.push(Line::from(name));
            }
            if truncated {
                lines.push(Line::from(format!(
                    "(showing first {MAX_DIRECTORY_PREVIEW_ENTRIES} entries, unsorted beyond that)"
                )));
            }
        }
        Err(error) => lines.push(Line::from(format!("cannot list: {error}"))),
    }
    Preview::Info(lines)
}

fn image_preview(path: &Path, metadata: &std::fs::Metadata) -> Preview {
    if metadata.len() > MAX_IMAGE_PREVIEW_BYTES {
        return Preview::Info(info_lines(&format!(
            "image too large to preview ({})",
            human_size(metadata.len())
        )));
    }
    let Some((mut image, (width, height))) = decode_for_preview(path) else {
        return Preview::Info(info_lines("image could not be decoded"));
    };
    let meta = vec![
        Line::from(Span::styled(
            format!("image {width}x{height}"),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(format_size_line(metadata.len())),
    ];
    if image.width() > MAX_IMAGE_PREVIEW_SIDE || image.height() > MAX_IMAGE_PREVIEW_SIDE {
        image = image.thumbnail(MAX_IMAGE_PREVIEW_SIDE, MAX_IMAGE_PREVIEW_SIDE);
    }
    Preview::Image {
        image: Box::new(image),
        meta,
    }
}

/// Decodes an image for the pane and returns it with the size of the original.
fn decode_for_preview(path: &Path) -> Option<(image::DynamicImage, (u32, u32))> {
    if let Some(scaled) = decode_scaled_jpeg(path) {
        return Some(scaled);
    }
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let size = (image.width(), image.height());
    Some((image, size))
}

/// Decodes a JPEG at 1/2, 1/4 or 1/8 size when the pane needs no more, so a 96 MP photo
/// never exists at full size in memory. `None` for anything else, including the JPEG
/// kinds this decoder does not turn into 8-bit grey or RGB.
fn decode_scaled_jpeg(path: &Path) -> Option<(image::DynamicImage, (u32, u32))> {
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut decoder = jpeg_decoder::Decoder::new(file);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    let original = (u32::from(info.width), u32::from(info.height));
    let side = MAX_IMAGE_PREVIEW_SIDE as u16;
    let (width, height) = decoder.scale(side, side).ok()?;
    let (width, height) = (u32::from(width), u32::from(height));
    let pixels = decoder.decode().ok()?;
    let image = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, pixels)?)
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, pixels)?)
        }
        _ => return None,
    };
    Some((image, original))
}

/// `pdf_extract::extract_text` that turns a panic (common on malformed PDFs) into `Err`
/// without the default hook printing over the TUI. Other threads' panics still print.
pub fn extract_pdf_text(path: &Path) -> Result<String, String> {
    let outcome = crate::panic_guard::catch_quiet(|| pdf_extract::extract_text(path));
    match outcome {
        Ok(result) => result.map_err(|error| format!("pdf text extraction failed: {error}")),
        Err(_) => Err("pdf text extraction panicked".to_string()),
    }
}

fn pdf_preview(path: &Path, metadata: &std::fs::Metadata, max_lines: usize) -> Preview {
    if metadata.len() > MAX_PREVIEW_BYTES {
        return Preview::Info(info_lines(&format!(
            "pdf too large to preview ({})",
            human_size(metadata.len())
        )));
    }
    match extract_pdf_text(path) {
        Ok(text) => {
            let mut lines = vec![Line::from(Span::styled(
                "pdf (extracted text)",
                Style::default().fg(Color::Cyan),
            ))];
            for line in text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(max_lines)
            {
                lines.push(Line::from(line.to_string()));
            }
            if lines.len() == 1 {
                lines.push(Line::from("(no extractable text)"));
            }
            Preview::Text(lines)
        }
        Err(_) => Preview::Info(info_lines("pdf text could not be extracted")),
    }
}

fn text_or_binary_preview(
    path: &Path,
    metadata: &std::fs::Metadata,
    extension: &str,
    max_lines: usize,
) -> Preview {
    if metadata.len() > MAX_PREVIEW_BYTES {
        return Preview::Info(info_lines(&format!(
            "file too large to preview ({})",
            human_size(metadata.len())
        )));
    }

    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| {
        use std::io::Read;
        file.take(MAX_PREVIEW_BYTES + 1).read_to_end(&mut bytes)
    });
    if let Err(error) = read {
        return Preview::Info(info_lines(&format!("cannot read: {error}")));
    }
    if bytes.len() as u64 > MAX_PREVIEW_BYTES {
        return Preview::Info(info_lines("file too large to preview"));
    }

    if looks_binary(&bytes) {
        let mut lines = vec![Line::from(Span::styled(
            "binary file",
            Style::default().fg(Color::Yellow),
        ))];
        lines.push(Line::from(format_size_line(metadata.len())));
        return Preview::Info(lines);
    }

    let text = String::from_utf8_lossy(&bytes);
    Preview::Text(highlight_text(&text, extension, max_lines))
}

fn highlight_text(text: &str, extension: &str, max_lines: usize) -> Vec<Line<'static>> {
    let hl = highlighter();
    let syntax = hl
        .syntaxes
        .find_syntax_by_extension(extension)
        .unwrap_or_else(|| hl.syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &hl.theme);

    let mut lines = Vec::new();
    for raw_line in LinesWithEndings::from(text).take(max_lines) {
        let cut = raw_line.char_indices().nth(MAX_PREVIEW_LINE_CHARS);
        let raw_line = match cut {
            Some((end, _)) => Cow::Owned(format!("{}\n", &raw_line[..end])),
            None => Cow::Borrowed(raw_line),
        };
        match highlighter.highlight_line(&raw_line, &hl.syntaxes) {
            Ok(ranges) => lines.push(syntect_line_to_ratatui(&ranges)),
            Err(_) => lines.push(Line::from(raw_line.trim_end_matches('\n').to_string())),
        }
    }
    lines
}

fn syntect_line_to_ratatui(ranges: &[(SynStyle, &str)]) -> Line<'static> {
    let spans = ranges
        .iter()
        .map(|(style, text)| {
            Span::styled(
                text.trim_end_matches('\n').to_string(),
                Style::default().fg(Color::Rgb(
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                )),
            )
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.contains(&0) {
        return true;
    }
    // High proportion of non-text control bytes suggests binary.
    let suspicious = sample
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20))
        .count();
    !sample.is_empty() && suspicious * 100 / sample.len() > 30
}

fn is_image_ext(extension: &str) -> bool {
    matches!(
        extension,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tiff" | "tif"
    )
}

fn info_lines(message: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        message.to_string(),
        Style::default().fg(Color::DarkGray),
    ))]
}

fn format_size_line(bytes: u64) -> Span<'static> {
    Span::styled(human_size(bytes), Style::default().fg(Color::DarkGray))
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

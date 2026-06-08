//! File preview generation for the search TUI's preview pane. Produces
//! syntax-highlighted text (via `syntect`), decoded images (rendered inline by
//! `ratatui-image` in the TUI), extracted PDF text, directory listings, or a
//! metadata fallback for binaries. Heavy work (decode, highlight) is bounded and
//! the TUI caches the result per selected path so it is not recomputed each tick.

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
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return Preview::Info(info_lines(&format!("cannot read: {error}"))),
    };

    if metadata.is_dir() {
        return directory_preview(path, max_lines);
    }

    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if is_image_ext(&extension) {
        return image_preview(path, &metadata);
    }
    if extension == "pdf" {
        return pdf_preview(path, max_lines);
    }

    text_or_binary_preview(path, &extension, max_lines)
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
            names.sort();
            for name in names.into_iter().take(max_lines) {
                lines.push(Line::from(name));
            }
        }
        Err(error) => lines.push(Line::from(format!("cannot list: {error}"))),
    }
    Preview::Info(lines)
}

fn image_preview(path: &Path, metadata: &std::fs::Metadata) -> Preview {
    match image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok())
        .and_then(|reader| reader.decode().ok())
    {
        Some(image) => {
            let meta = vec![
                Line::from(Span::styled(
                    format!("image {}x{}", image.width(), image.height()),
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(format_size_line(metadata.len())),
            ];
            Preview::Image {
                image: Box::new(image),
                meta,
            }
        }
        None => Preview::Info(info_lines("image could not be decoded")),
    }
}

fn pdf_preview(path: &Path, max_lines: usize) -> Preview {
    match pdf_extract::extract_text(path) {
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

fn text_or_binary_preview(path: &Path, extension: &str, max_lines: usize) -> Preview {
    let metadata = std::fs::symlink_metadata(path).ok();
    if let Some(meta) = &metadata {
        if meta.len() > MAX_PREVIEW_BYTES {
            return Preview::Info(info_lines(&format!(
                "file too large to preview ({})",
                human_size(meta.len())
            )));
        }
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return Preview::Info(info_lines(&format!("cannot read: {error}"))),
    };

    if looks_binary(&bytes) {
        let mut lines = vec![Line::from(Span::styled(
            "binary file",
            Style::default().fg(Color::Yellow),
        ))];
        if let Some(meta) = &metadata {
            lines.push(Line::from(format_size_line(meta.len())));
        }
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
        match highlighter.highlight_line(raw_line, &hl.syntaxes) {
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

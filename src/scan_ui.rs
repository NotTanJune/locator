use std::f64::consts::PI;
use std::time::Duration;

use crate::scanner::{ScanBackend, ScanPhase, ScanProgress};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanAnimation {
    art: String,
}

impl ScanAnimation {
    pub fn frame(index: usize) -> Self {
        Self {
            art: render_magnifier_orbit(index),
        }
    }
}

pub fn render_scan_frame(progress: &ScanProgress, animation: ScanAnimation) -> String {
    render_scan_frame_with_eta(progress, animation, true)
}

pub fn render_scan_frame_with_eta(
    progress: &ScanProgress,
    animation: ScanAnimation,
    show_eta: bool,
) -> String {
    let percent = progress
        .percent_complete()
        .map(|value| format!("{value:>3}%"))
        .unwrap_or_else(|| " --%".to_string());
    let eta = if show_eta {
        progress
            .eta()
            .map(format_duration)
            .unwrap_or_else(|| "estimating".to_string())
    } else {
        "off".to_string()
    };
    let total = progress
        .total_files
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_string());
    let count_label = if matches!(progress.phase, ScanPhase::Discovering) {
        "discovered"
    } else {
        "indexed"
    };

    format!(
        "{art}\n{phase} {bar} {percent}  ETA {eta}\n{count_label} {indexed} / {total} files  {rate:.1} files/s  {mb:.1} MB/s\nskipped {skipped}  errors {errors}  backend {backend}\n{path}",
        art = animation.art,
        phase = progress.phase.label(),
        bar = progress_bar(progress.percent_complete()),
        percent = percent,
        eta = eta,
        count_label = count_label,
        indexed = progress.indexed_files,
        total = total,
        rate = progress.files_per_second(),
        mb = progress.megabytes_per_second(),
        skipped = progress.skipped_entries,
        errors = progress.error_entries,
        backend = progress.backend.resolved_name(matches!(progress.backend, ScanBackend::Native)),
        path = compact_path(&progress.current_path),
    )
}

fn render_magnifier_orbit(frame: usize) -> String {
    const ROWS: usize = 14;
    const COLS: usize = 44;
    const CY: isize = 7;
    const CX: isize = 22;
    const RX: f64 = 16.0;
    const RY: f64 = 5.0;
    const ANGULAR_SPEED: f64 = 0.16;
    const TRAIL_LEN: isize = 6;
    const TRAIL_CHARS: [char; 6] = ['.', '.', ':', ':', 'o', 'o'];

    let mut grid = vec![vec![' '; COLS]; ROWS];
    draw_mac_folder(&mut grid, CY, CX);

    for i in (1..=TRAIL_LEN).rev() {
        let (x, y, _) = orbit_pos(frame as isize - i, RX, RY, ANGULAR_SPEED, CX, CY);
        if is_inside_folder(x, y, CX, CY) {
            continue;
        }
        set_cell(&mut grid, y, x, TRAIL_CHARS[(TRAIL_LEN - i) as usize]);
    }

    let (x, y, angle) = orbit_pos(frame as isize, RX, RY, ANGULAR_SPEED, CX, CY);
    place_text(&mut grid, y, x - 1, "(O)");

    let handle_side = if angle.cos() > 0.0 { 2 } else { -2 };
    let handle_char = if angle.cos() > 0.0 { '\\' } else { '/' };
    set_cell(&mut grid, y + 1, x + handle_side, handle_char);

    grid.into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn orbit_pos(
    frame: isize,
    rx: f64,
    ry: f64,
    speed: f64,
    cx: isize,
    cy: isize,
) -> (isize, isize, f64) {
    let angle = (frame as f64 * speed) % (PI * 2.0);
    let x = cx + (angle.cos() * rx).round() as isize;
    let y = cy + (angle.sin() * ry).round() as isize;
    (x, y, angle)
}

fn draw_mac_folder(grid: &mut [Vec<char>], cy: isize, cx: isize) {
    place_text(grid, cy - 3, cx - 8, "   ______");
    place_text(grid, cy - 2, cx - 8, "  /      \\________");
    place_text(grid, cy - 1, cx - 8, " /                \\");
    place_text(grid, cy, cx - 8, "|                  |");
    place_text(grid, cy + 1, cx - 8, "|                  |");
    place_text(grid, cy + 2, cx - 8, "|__________________|");
}

fn place_text(grid: &mut [Vec<char>], row: isize, col: isize, text: &str) {
    for (index, ch) in text.chars().enumerate() {
        set_cell(grid, row, col + index as isize, ch);
    }
}

fn set_cell(grid: &mut [Vec<char>], row: isize, col: isize, ch: char) {
    if row >= 0 && col >= 0 {
        if let Some(cell) = grid
            .get_mut(row as usize)
            .and_then(|line| line.get_mut(col as usize))
        {
            *cell = ch;
        }
    }
}

fn is_inside_folder(x: isize, y: isize, cx: isize, cy: isize) -> bool {
    x > cx - 9 && x < cx + 9 && y > cy - 4 && y < cy + 3
}

fn progress_bar(percent: Option<u64>) -> String {
    let percent = percent.unwrap_or(0).min(100) as usize;
    let filled = percent / 5;
    let empty = 20usize.saturating_sub(filled);
    format!("[{}{}]", "#".repeat(filled), ".".repeat(empty))
}

fn compact_path(path: &str) -> String {
    const MAX_LEN: usize = 84;
    if path.chars().count() <= MAX_LEN {
        return path.to_string();
    }

    let tail = path
        .chars()
        .rev()
        .take(MAX_LEN - 3)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("...{tail}")
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let minutes = total / 60;
    let seconds = total % 60;
    format!("{minutes:02}:{seconds:02}")
}

impl ScanPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Discovering => "discovering",
            Self::Indexing => "indexing",
            Self::Optimizing => "optimizing",
            Self::Done => "done",
        }
    }
}

use std::f64::consts::PI;
use std::path::Path;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, BorderType, Borders, Gauge, Paragraph, Widget, Wrap,
};

use crate::scanner::{ScanBackend, ScanPhase, ScanProgress, ScanStats};

const DEFAULT_DASHBOARD_WIDTH: u16 = 112;
const MIN_DASHBOARD_WIDTH: u16 = 78;
const MAX_DASHBOARD_WIDTH: u16 = 148;
const LIVE_HEIGHT: u16 = 28;
const SUMMARY_HEIGHT: u16 = 32;
const SUMMARY_DETAIL_HEIGHT: u16 = 44;
const TITLE: &str = " locator ";

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
    render_scan_frame_with_eta_and_root(progress, animation, true, None)
}

pub fn render_scan_frame_with_eta(
    progress: &ScanProgress,
    animation: ScanAnimation,
    show_eta: bool,
) -> String {
    render_scan_frame_with_eta_and_root(progress, animation, show_eta, None)
}

pub fn render_scan_frame_with_eta_and_root(
    progress: &ScanProgress,
    animation: ScanAnimation,
    show_eta: bool,
    root: Option<&Path>,
) -> String {
    render_scan_frame_dashboard(
        progress,
        animation,
        show_eta,
        root,
        false,
        DEFAULT_DASHBOARD_WIDTH,
    )
}

pub fn render_scan_frame_for_terminal(
    progress: &ScanProgress,
    animation: ScanAnimation,
    show_eta: bool,
    root: &Path,
) -> String {
    render_scan_frame_dashboard(
        progress,
        animation,
        show_eta,
        Some(root),
        true,
        terminal_dashboard_width(),
    )
}

#[derive(Debug, Clone, Copy)]
pub struct ScanSummary<'a> {
    pub stats: &'a ScanStats,
    pub root: &'a Path,
    pub index_path: &'a Path,
    pub staged: bool,
    pub detail: bool,
}

pub fn render_scan_summary(summary: &ScanSummary<'_>) -> String {
    render_scan_summary_dashboard(summary, false, DEFAULT_DASHBOARD_WIDTH)
}

pub fn render_scan_summary_for_terminal(summary: &ScanSummary<'_>) -> String {
    render_scan_summary_dashboard(summary, true, terminal_dashboard_width())
}

fn render_scan_frame_dashboard(
    progress: &ScanProgress,
    animation: ScanAnimation,
    show_eta: bool,
    root: Option<&Path>,
    color: bool,
    width: u16,
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
    let root_label = root
        .map(|path| compact_path(&path.display().to_string()))
        .unwrap_or_else(|| "current root".to_string());
    let backend = progress
        .backend
        .resolved_name(matches!(progress.backend, ScanBackend::Native));

    let mut buffer = Buffer::empty(Rect::new(0, 0, width, LIVE_HEIGHT));
    let shell = Rect::new(0, 0, width, LIVE_HEIGHT);
    Block::default()
        .title(Line::from(vec![
            Span::styled(TITLE, style_accent().add_modifier(Modifier::BOLD)),
            Span::styled("scan running ", style_muted()),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_accent())
        .render(shell, &mut buffer);

    let shell_inner = shell.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(16),
            Constraint::Length(4),
            Constraint::Length(3),
        ])
        .split(shell_inner);
    render_header(
        chunks[0],
        &mut buffer,
        progress.phase.label(),
        &root_label,
        backend,
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(48), Constraint::Min(0)])
        .split(chunks[1]);
    Paragraph::new(animation.art)
        .block(panel(" scanner "))
        .style(style_muted())
        .render(body[0], &mut buffer);

    let stats_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(6),
        ])
        .split(body[1]);
    let gauge_label = format!("{percent}  ETA {eta}");
    let gauge = Gauge::default()
        .block(panel(" progress "))
        .gauge_style(style_ok().add_modifier(Modifier::BOLD))
        .label(Span::styled(gauge_label, style_text()))
        .ratio(progress.percent_complete().unwrap_or(0) as f64 / 100.0)
        .use_unicode(true);
    gauge.render(stats_chunks[0], &mut buffer);

    let counts = vec![
        Line::from(vec![
            Span::styled(count_label, style_muted()),
            Span::raw(format!(" {} / {} files", progress.indexed_files, total)),
        ]),
        Line::from(format!("{:.1} files/s", progress.files_per_second())),
        Line::from(format!("{:.1} MB/s", progress.megabytes_per_second())),
        Line::from(vec![
            Span::styled("backend ", style_muted()),
            Span::raw(backend.to_string()),
        ]),
    ];
    Paragraph::new(counts)
        .block(panel(" rate "))
        .style(style_text())
        .render(stats_chunks[1], &mut buffer);

    let health = vec![
        Line::from(vec![
            Span::styled("skipped ", style_warn()),
            Span::raw(progress.skipped_entries.to_string()),
        ]),
        Line::from(vec![
            Span::styled("warnings ", warning_style(progress.error_entries)),
            Span::raw(progress.error_entries.to_string()),
        ]),
        Line::from(vec![
            Span::styled("phase   ", style_muted()),
            Span::raw(progress.phase.label()),
        ]),
    ];
    Paragraph::new(health)
        .block(panel(" health "))
        .style(style_text())
        .render(stats_chunks[2], &mut buffer);

    let current_path = compact_path(&progress.current_path);
    Paragraph::new(vec![
        Line::from(vec![
            Span::styled("current ", style_muted()),
            Span::raw(current_path),
        ]),
        Line::from("Ctrl-C cancels the scan"),
    ])
    .block(panel(" current path "))
    .style(style_text())
    .wrap(Wrap { trim: true })
    .render(chunks[2], &mut buffer);

    Paragraph::new(Line::from(vec![
        Span::styled("status ", style_muted()),
        Span::raw(format!(
            "{} {}  {} files/s  backend {}",
            progress.phase.label(),
            progress_bar(progress.percent_complete()),
            format_number(progress.files_per_second() as u64),
            backend
        )),
    ]))
    .block(panel(" compact "))
    .style(style_text())
    .render(chunks[3], &mut buffer);

    render_buffer(&buffer, color)
}

fn render_scan_summary_dashboard(summary: &ScanSummary<'_>, color: bool, width: u16) -> String {
    let stats = summary.stats;
    let show_detail = summary.detail || !stats.error_summaries.is_empty();
    let height = if show_detail {
        SUMMARY_DETAIL_HEIGHT
    } else {
        SUMMARY_HEIGHT
    };
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let shell = Rect::new(0, 0, width, height);
    Block::default()
        .title(Span::styled(
            TITLE,
            style_accent().add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_ok())
        .render(shell, &mut buffer);

    let shell_inner = shell.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let constraints = if show_detail {
        vec![
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(0),
        ]
    } else {
        vec![
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Min(0),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(shell_inner);

    render_summary_hero(chunks[0], &mut buffer, summary);
    let timing_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(chunks[1]);
    render_timing_chart(timing_chunks[0], &mut buffer, stats);
    render_next_steps(timing_chunks[1], &mut buffer, summary.root);
    render_scan_flow(chunks[2], &mut buffer, stats);
    render_error_summary(chunks[3], &mut buffer, stats);

    if summary.detail {
        render_profile_detail(chunks[4], &mut buffer, stats);
    } else if !stats.error_summaries.is_empty() {
        render_error_samples(chunks[4], &mut buffer, stats);
    }

    render_buffer(&buffer, color)
}

fn render_summary_hero(area: Rect, buffer: &mut Buffer, summary: &ScanSummary<'_>) {
    let stats = summary.stats;
    let total = stats.profile.total.as_secs_f64();
    let files_per_second = if total > 0.0 {
        stats.indexed_files as f64 / total
    } else {
        0.0
    };
    let mb_per_second = if total > 0.0 {
        (stats.profile.indexed_bytes as f64 / 1_000_000.0) / total
    } else {
        0.0
    };
    let index_mode = if summary.staged {
        "search-ready index"
    } else {
        "updated index"
    };
    let root = compact_path(&summary.root.display().to_string());

    let lines = vec![
        Line::from(Span::styled(
            "scan complete",
            style_ok().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![Span::styled(
            format!("{} files indexed", format_number(stats.indexed_files)),
            style_accent().add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![
            Span::styled(format_duration(stats.profile.total), style_text()),
            Span::styled(" total  ", style_muted()),
            Span::styled(format!("{files_per_second:.1} files/s"), style_ok()),
            Span::styled("  ", style_muted()),
            Span::styled(format!("{mb_per_second:.1} MB/s"), style_muted()),
        ]),
        Line::from(vec![
            Span::styled(index_mode, style_accent()),
            Span::styled(" for ", style_muted()),
            Span::raw(root),
        ]),
        Line::from(vec![
            Span::styled("skipped ", style_muted()),
            Span::raw(stats.skipped_entries.to_string()),
            Span::styled("   warnings ", style_muted()),
            Span::styled(
                stats.error_entries.to_string(),
                warning_style(stats.error_entries),
            ),
        ]),
    ];

    Paragraph::new(lines)
        .block(panel(" scan complete "))
        .style(style_text())
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(area, buffer);
}

fn render_scan_flow(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let optimize_known = stats
        .profile
        .fts_rebuild
        .saturating_add(stats.profile.index_rebuild)
        .saturating_add(stats.profile.stale_mark)
        .saturating_add(stats.profile.trigger_recreate);
    let finalize = stats.profile.cleanup.saturating_sub(optimize_known);
    let flow = Line::from(vec![
        Span::styled("[walk]", style_ok().add_modifier(Modifier::BOLD)),
        Span::styled(" -> ", style_muted()),
        Span::styled("[metadata]", style_ok().add_modifier(Modifier::BOLD)),
        Span::styled(" -> ", style_muted()),
        Span::styled("[sqlite]", style_warn().add_modifier(Modifier::BOLD)),
        Span::styled(" -> ", style_muted()),
        Span::styled(
            "[search-ready index]",
            style_accent().add_modifier(Modifier::BOLD),
        ),
    ]);
    let lines = vec![
        flow,
        Line::from(vec![
            Span::styled("walk+metadata ", style_muted()),
            Span::raw(format_seconds(stats.profile.walk)),
            Span::styled("   sqlite writes ", style_muted()),
            Span::raw(format_seconds(stats.profile.sqlite_writes)),
            Span::styled("   filename index ", style_muted()),
            Span::raw(format_seconds(stats.profile.fts_rebuild)),
            Span::styled("   sort/filter indexes ", style_muted()),
            Span::raw(format_seconds(stats.profile.index_rebuild)),
        ]),
        Line::from(vec![
            Span::styled("finish ", style_muted()),
            Span::raw(format_seconds(finalize)),
            Span::styled(" after the fast file walk; this is where locator builds reusable filename and sort indexes", style_muted()),
        ]),
    ];

    Paragraph::new(lines)
        .block(panel(" indexing flow "))
        .style(style_text())
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(area, buffer);
}

fn render_header(area: Rect, buffer: &mut Buffer, phase: &str, root: &str, backend: &str) {
    Paragraph::new(vec![
        Line::from(vec![
            Span::styled("root ", style_muted()),
            Span::raw(root.to_string()),
        ]),
        Line::from(vec![
            Span::styled("phase ", style_muted()),
            Span::styled(phase.to_string(), phase_style(phase)),
            Span::raw("   "),
            Span::styled("backend ", style_muted()),
            Span::raw(backend.to_string()),
        ]),
    ])
    .block(panel(" locator "))
    .style(style_text())
    .wrap(Wrap { trim: true })
    .render(area, buffer);
}

fn render_timing_chart(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let max_value = [
        stats.profile.discovery,
        stats.profile.walk,
        stats.profile.sqlite_writes,
        stats.profile.cleanup,
    ]
    .into_iter()
    .map(|duration| duration.as_millis().max(1) as u64)
    .max()
    .unwrap_or(1)
    .max(1);
    let bars = [
        timing_bar("discover", stats.profile.discovery, style_accent()),
        timing_bar("walk", stats.profile.walk, style_ok()),
        timing_bar("writes", stats.profile.sqlite_writes, style_warn()),
        timing_bar("optimize", stats.profile.cleanup, style_accent()),
    ];
    let chart = BarChart::default()
        .block(panel(" timing breakdown "))
        .data(BarGroup::default().bars(&bars))
        .bar_width(8)
        .bar_gap(2)
        .max(max_value)
        .value_style(style_text().add_modifier(Modifier::BOLD))
        .label_style(style_muted());
    chart.render(area, buffer);
}

fn render_next_steps(area: Rect, buffer: &mut Buffer, root: &Path) {
    let root = root.display().to_string();
    Paragraph::new(vec![
        Line::from(vec![
            Span::styled("search ", style_muted()),
            Span::raw(format!("lctr search {root}")),
        ]),
        Line::from(vec![
            Span::styled("find   ", style_muted()),
            Span::raw("lctr find <query>"),
        ]),
        Line::from(vec![
            Span::styled("delete ", style_muted()),
            Span::raw(format!("lctr delete-index {root}")),
        ]),
    ])
    .block(panel(" next commands "))
    .style(style_text())
    .wrap(Wrap { trim: true })
    .render(area, buffer);
}

fn render_error_summary(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let text = if stats.error_summaries.is_empty() {
        vec![Line::from(vec![
            Span::styled("warnings ", style_ok()),
            Span::raw("none"),
        ])]
    } else {
        let sample_width = area.width.saturating_sub(6).max(24) as usize;
        let mut lines = vec![Line::from(Span::styled(
            "warning summary:",
            style_warn().add_modifier(Modifier::BOLD),
        ))];
        for (kind, summary) in &stats.error_summaries {
            lines.push(Line::from(vec![
                Span::styled(kind.label(), style_warn()),
                Span::raw(format!(": {}", summary.count)),
            ]));
            for sample in summary.samples.iter().take(2) {
                for (index, segment) in
                    wrap_path_segments(&sample.display().to_string(), sample_width)
                        .into_iter()
                        .enumerate()
                {
                    let prefix = if index == 0 { "  " } else { "    " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, style_muted()),
                        Span::styled(segment, style_warn()),
                    ]));
                }
            }
        }
        lines
    };
    Paragraph::new(text)
        .block(panel(" health "))
        .style(style_text())
        .wrap(Wrap { trim: false })
        .render(area, buffer);
}

fn render_error_samples(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let mut lines = Vec::new();
    for (kind, summary) in &stats.error_summaries {
        lines.push(Line::from(vec![
            Span::styled(kind.label(), warning_style(summary.count)),
            Span::raw(format!(" ({})", summary.count)),
        ]));
        for sample in summary.samples.iter().take(3) {
            lines.push(Line::from(compact_path(&sample.display().to_string())));
        }
    }
    Paragraph::new(lines)
        .block(panel(" warning samples "))
        .style(style_text())
        .wrap(Wrap { trim: true })
        .render(area, buffer);
}

fn render_profile_detail(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let chunks = if area.width >= 112 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
    };

    render_optimizer_profile(chunks[0], buffer, stats);
    render_filesystem_profile(chunks[1], buffer, stats);
}

fn render_optimizer_profile(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let known = stats
        .profile
        .fts_rebuild
        .saturating_add(stats.profile.index_rebuild)
        .saturating_add(stats.profile.stale_mark)
        .saturating_add(stats.profile.trigger_recreate);
    let finalize = stats.profile.cleanup.saturating_sub(known);
    let max_value = [
        stats.profile.fts_rebuild,
        stats.profile.index_rebuild,
        stats.profile.stale_mark,
        stats.profile.trigger_recreate,
        finalize,
    ]
    .into_iter()
    .map(|duration| duration.as_millis().max(1) as u64)
    .max()
    .unwrap_or(1);
    let lines = vec![
        metric_bar_line(
            "filename index",
            stats.profile.fts_rebuild.as_millis().max(1) as u64,
            max_value,
            format_seconds(stats.profile.fts_rebuild),
            style_accent(),
        ),
        metric_bar_line(
            "sort/filter",
            stats.profile.index_rebuild.as_millis().max(1) as u64,
            max_value,
            format_seconds(stats.profile.index_rebuild),
            style_ok(),
        ),
        metric_bar_line(
            "stale cleanup",
            stats.profile.stale_mark.as_millis().max(1) as u64,
            max_value,
            format_seconds(stats.profile.stale_mark),
            style_warn(),
        ),
        metric_bar_line(
            "triggers",
            stats.profile.trigger_recreate.as_millis().max(1) as u64,
            max_value,
            format_seconds(stats.profile.trigger_recreate),
            style_muted(),
        ),
        metric_bar_line(
            "finalize",
            finalize.as_millis().max(1) as u64,
            max_value,
            format_seconds(finalize),
            style_text(),
        ),
    ];
    Paragraph::new(lines)
        .block(panel(" post-scan index build "))
        .style(style_text())
        .render(area, buffer);
}

fn render_filesystem_profile(area: Rect, buffer: &mut Buffer, stats: &ScanStats) {
    let max_value = [
        stats.profile.native_dirs_opened,
        stats.profile.native_files_seen.max(stats.indexed_files),
        stats.profile.native_entries_seen,
        stats.profile.native_getattr_calls,
    ]
    .into_iter()
    .max()
    .unwrap_or(1)
    .max(1);
    let lines = vec![
        metric_bar_line(
            "dirs opened",
            stats.profile.native_dirs_opened,
            max_value,
            format_number(stats.profile.native_dirs_opened),
            style_accent(),
        ),
        metric_bar_line(
            "files indexed",
            stats.profile.native_files_seen.max(stats.indexed_files),
            max_value,
            format_number(stats.profile.native_files_seen.max(stats.indexed_files)),
            style_ok(),
        ),
        metric_bar_line(
            "entries seen",
            stats.profile.native_entries_seen,
            max_value,
            format_number(stats.profile.native_entries_seen),
            style_warn(),
        ),
        metric_bar_line(
            "metadata reads",
            stats.profile.native_getattr_calls,
            max_value,
            format_number(stats.profile.native_getattr_calls),
            style_text(),
        ),
    ];
    Paragraph::new(lines)
        .block(panel(" filesystem scan counts "))
        .style(style_text())
        .render(area, buffer);
}

fn timing_bar<'a>(label: &'a str, duration: Duration, style: Style) -> Bar<'a> {
    let millis = duration.as_millis().max(1) as u64;
    Bar::default()
        .label(Line::from(label))
        .value(millis)
        .text_value(format_seconds(duration))
        .style(style)
        .value_style(style_text().add_modifier(Modifier::BOLD))
}

fn metric_bar_line(
    label: &'static str,
    value: u64,
    max_value: u64,
    text: String,
    style: Style,
) -> Line<'static> {
    const BAR_WIDTH: usize = 18;
    let filled = if max_value == 0 {
        0
    } else {
        ((value as f64 / max_value as f64) * BAR_WIDTH as f64).round() as usize
    }
    .clamp(1, BAR_WIDTH);
    let empty = BAR_WIDTH.saturating_sub(filled);
    Line::from(vec![
        Span::styled(format!("{label:<16}"), style_muted()),
        Span::styled("█".repeat(filled), style),
        Span::styled("░".repeat(empty), style_muted()),
        Span::raw(" "),
        Span::styled(text, style_text().add_modifier(Modifier::BOLD)),
    ])
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_muted())
}

fn render_buffer(buffer: &Buffer, color: bool) -> String {
    let mut output = String::new();
    for y in 0..buffer.area.height {
        let width = trimmed_width(buffer, y);
        let mut active = CellStyle::default();
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let style = CellStyle::from_cell(cell);
            if color && style != active {
                output.push_str("\x1b[0m");
                output.push_str(&style.ansi_prefix());
                active = style;
            }
            output.push_str(cell.symbol());
        }
        if color && active != CellStyle::default() {
            output.push_str("\x1b[0m");
        }
        if y + 1 < buffer.area.height {
            output.push('\n');
        }
    }
    output
}

fn terminal_dashboard_width() -> u16 {
    crossterm::terminal::size()
        .map(|(cols, _)| dashboard_width_for_columns(cols))
        .unwrap_or(DEFAULT_DASHBOARD_WIDTH)
}

fn dashboard_width_for_columns(columns: u16) -> u16 {
    columns
        .saturating_sub(2)
        .clamp(MIN_DASHBOARD_WIDTH, MAX_DASHBOARD_WIDTH)
}

fn trimmed_width(buffer: &Buffer, y: u16) -> u16 {
    let mut width = buffer.area.width;
    while width > 0 {
        let cell = &buffer[(width - 1, y)];
        if cell.symbol() != " " {
            break;
        }
        width -= 1;
    }
    width
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: Color,
    bg: Color,
    modifier: Modifier,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            modifier: Modifier::empty(),
        }
    }
}

impl CellStyle {
    fn from_cell(cell: &ratatui::buffer::Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            modifier: cell.modifier,
        }
    }

    fn ansi_prefix(self) -> String {
        let mut codes = Vec::new();
        if let Some(code) = color_code(self.fg, false) {
            codes.push(code);
        }
        if let Some(code) = color_code(self.bg, true) {
            codes.push(code);
        }
        if self.modifier.contains(Modifier::BOLD) {
            codes.push("1".to_string());
        }
        if self.modifier.contains(Modifier::DIM) {
            codes.push("2".to_string());
        }
        if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        }
    }
}

fn color_code(color: Color, background: bool) -> Option<String> {
    let base = if background { 40 } else { 30 };
    let bright_base = if background { 100 } else { 90 };
    Some(match color {
        Color::Reset => return None,
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray | Color::White => (base + 7).to_string(),
        Color::DarkGray => bright_base.to_string(),
        Color::LightRed => (bright_base + 1).to_string(),
        Color::LightGreen => (bright_base + 2).to_string(),
        Color::LightYellow => (bright_base + 3).to_string(),
        Color::LightBlue => (bright_base + 4).to_string(),
        Color::LightMagenta => (bright_base + 5).to_string(),
        Color::LightCyan => (bright_base + 6).to_string(),
        Color::Rgb(r, g, b) if background => format!("48;2;{r};{g};{b}"),
        Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        Color::Indexed(index) if background => format!("48;5;{index}"),
        Color::Indexed(index) => format!("38;5;{index}"),
    })
}

fn style_accent() -> Style {
    Style::default().fg(Color::Rgb(93, 173, 226))
}

fn style_ok() -> Style {
    Style::default().fg(Color::Rgb(82, 190, 128))
}

fn style_warn() -> Style {
    Style::default().fg(Color::Rgb(245, 176, 65))
}

fn style_text() -> Style {
    Style::default().fg(Color::Rgb(225, 232, 236))
}

fn style_muted() -> Style {
    Style::default().fg(Color::Rgb(145, 164, 174))
}

fn warning_style(count: u64) -> Style {
    if count == 0 {
        style_ok()
    } else {
        style_warn().add_modifier(Modifier::BOLD)
    }
}

fn phase_style(phase: &str) -> Style {
    match phase {
        "done" => style_ok().add_modifier(Modifier::BOLD),
        "optimizing" => style_warn().add_modifier(Modifier::BOLD),
        _ => style_accent().add_modifier(Modifier::BOLD),
    }
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

fn wrap_path_segments(path: &str, width: usize) -> Vec<String> {
    let width = width.max(16);
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for ch in path.chars() {
        if current_width >= width {
            segments.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += 1;
    }

    if !current.is_empty() {
        segments.push(current);
    }

    segments
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

fn format_seconds(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

fn format_number(value: u64) -> String {
    let raw = value.to_string();
    let mut grouped = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
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

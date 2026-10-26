//! The embedding galaxy: the full-screen view for `lctr scan --em` on an interactive terminal.
//! Each embedded chunk is a point in a slowly orbiting 3D cloud (PCA projection of its vector),
//! next to a panel with counters, throughput, an ETA, and a patience message.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Points};
use ratatui::widgets::{Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::Config;
use crate::embed::pipeline::{
    pass_done_text, run_pass, stage_label, PassEvent, PassRequest, PassSummary,
};
use crate::embed::projection::{orbit, project_point, Camera, Projector};
use crate::tui::theme::Theme;
use crate::tui::TerminalGuard;

const MAX_POINTS: usize = 6000;
const MAX_SAMPLES: usize = 2000;
const REFIT_EVERY: usize = 256;
const EASE: f32 = 0.15;
const FLASH_SECONDS: f32 = 0.6;
const PLACEHOLDER_POINTS: usize = 60;
const PATIENCE_SECONDS: f64 = 8.0;
const FINAL_HOLD: Duration = Duration::from_millis(600);
const FRAME_POLL: Duration = Duration::from_millis(50);
const RATE_WINDOW_SECONDS: f64 = 30.0;
const RATE_SAMPLE_EVERY: f64 = 1.0;
const ETA_MIN_FILES: usize = 5;
const WIDE_MIN_WIDTH: u16 = 70;
const WIDE_MIN_HEIGHT: u16 = 16;
const MODEL_LABEL: &str = "EmbeddingGemma 2 | Q8_0";
const RANDOM_SEED: u64 = 0x2545_F491_4F6C_DD1D;

const PATIENCE_MESSAGES: [&str; 4] = [
    "Reading your files and mapping their meaning. This takes a while.",
    "Have patience. This is a one-time pass, later scans only embed what changed.",
    "Similar files drift together. Watch the clusters form.",
    "Almost there. Search will understand your files soon.",
];

/// Coarse file category, used for point colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointKind {
    Text,
    Pdf,
    Image,
    Other,
}

impl PointKind {
    pub fn from_file_kind(kind: &str) -> Self {
        match kind {
            "text" | "file" => PointKind::Text,
            "pdf" => PointKind::Pdf,
            "image" => PointKind::Image,
            _ => PointKind::Other,
        }
    }

    fn index(self) -> usize {
        match self {
            PointKind::Text => 0,
            PointKind::Pdf => 1,
            PointKind::Image => 2,
            PointKind::Other => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GalaxyPoint {
    pub pos: [f32; 3],
    pub target: [f32; 3],
    /// Galaxy clock value when the point appeared (drives the white flash).
    pub born: f32,
    pub hue: PointKind,
    /// The embedding, kept so the point can be re-projected after a refit. Empty for placeholders.
    pub vector: Vec<f32>,
}

/// The point cloud: PCA projection, reservoir of samples for fitting, and the orbiting camera.
pub struct Galaxy {
    pub points: Vec<GalaxyPoint>,
    projector: Projector,
    samples: Vec<Vec<f32>>,
    seen_samples: u64,
    pub camera: Camera,
    pub elapsed: f32,
    new_since_refit: usize,
    placeholder: bool,
    rng: u64,
}

impl Galaxy {
    pub fn new(dims: usize) -> Self {
        let mut galaxy = Galaxy {
            points: Vec::new(),
            projector: Projector::new(dims),
            samples: Vec::new(),
            seen_samples: 0,
            camera: Camera::default(),
            elapsed: 0.0,
            new_since_refit: 0,
            placeholder: true,
            rng: RANDOM_SEED,
        };
        galaxy.points = (0..PLACEHOLDER_POINTS)
            .map(|_| {
                let pos = random_sphere_point(&mut galaxy.rng);
                GalaxyPoint {
                    pos,
                    target: pos,
                    born: -1000.0,
                    hue: PointKind::Other,
                    vector: Vec::new(),
                }
            })
            .collect();
        galaxy
    }

    pub fn is_placeholder(&self) -> bool {
        self.placeholder
    }

    /// Adds one embedded file's chunk vectors as points.
    pub fn add_vectors(&mut self, kind: PointKind, vectors: Vec<Vec<f32>>) {
        if self.placeholder {
            self.points.clear();
            self.placeholder = false;
        }
        for vector in vectors {
            self.push_sample(&vector);
            let pos = self.projector.project(&vector);
            let point = GalaxyPoint {
                pos,
                target: pos,
                born: self.elapsed,
                hue: kind,
                vector,
            };
            self.insert_point(point);
            self.new_since_refit += 1;
            if self.new_since_refit >= self.refit_interval() {
                self.refit();
            }
        }
    }

    fn insert_point(&mut self, point: GalaxyPoint) {
        if self.points.len() < MAX_POINTS {
            self.points.push(point);
        } else {
            let index = (self.next_rand() % MAX_POINTS as u64) as usize;
            self.points[index] = point;
        }
    }

    fn push_sample(&mut self, vector: &[f32]) {
        self.seen_samples += 1;
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(vector.to_vec());
        } else {
            let index = self.next_rand() % self.seen_samples;
            if (index as usize) < MAX_SAMPLES {
                self.samples[index as usize] = vector.to_vec();
            }
        }
    }

    /// Chunks between refits. The first refits come early, so the cloud is centred from the start.
    fn refit_interval(&self) -> usize {
        match self.samples.len() {
            0..=63 => 8,
            64..=255 => 32,
            _ => REFIT_EVERY,
        }
    }

    /// Refits the projection on the sample reservoir and moves every point's target.
    fn refit(&mut self) {
        self.new_since_refit = 0;
        self.projector.fit(&self.samples);
        for point in &mut self.points {
            if !point.vector.is_empty() {
                point.target = self.projector.project(&point.vector);
            }
        }
    }

    /// Advances the galaxy clock, orbits the camera, and eases every point toward its target.
    pub fn tick(&mut self, dt_seconds: f32) {
        let dt = dt_seconds.max(0.0);
        self.elapsed += dt;
        orbit(&mut self.camera, dt, self.elapsed);
        for point in &mut self.points {
            ease(point);
        }
    }

    fn next_rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
}

fn ease(point: &mut GalaxyPoint) {
    for axis in 0..3 {
        point.pos[axis] += (point.target[axis] - point.pos[axis]) * EASE;
    }
}

fn random_sphere_point(rng: &mut u64) -> [f32; 3] {
    let mut next = || {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        ((*rng % 2_000_001) as f32 / 1_000_000.0) - 1.0
    };
    let raw = [next(), next(), next()];
    let length = (raw[0] * raw[0] + raw[1] * raw[1] + raw[2] * raw[2])
        .sqrt()
        .max(1e-3);
    [
        raw[0] / length * 0.9,
        raw[1] / length * 0.9,
        raw[2] / length * 0.9,
    ]
}

/// Everything the screen shows, as plain data so it can be tested without a terminal.
pub struct EmbedUiState {
    pub backend: String,
    pub total_files: usize,
    pub files_embedded: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub chunks: usize,
    pub current_file: Option<String>,
    pub stopping: bool,
    pub galaxy: Galaxy,
    /// The stage in progress (1-based) of `stages`, and its wording for the panel.
    pub stage: usize,
    pub stages: usize,
    pub stage_label: String,
    /// Files finished in the current stage (the gauge and ETA are per stage).
    stage_processed: usize,
    /// Elapsed seconds when the current stage began.
    stage_started_at: f64,
    /// Shown in place of the patience message once a stage completes.
    stage_note: Option<String>,
    /// Seconds since the screen started, with the chunk count at that moment (30 s window).
    rate_samples: VecDeque<(f64, usize)>,
    elapsed_seconds: f64,
    last_rate_sample: f64,
}

impl EmbedUiState {
    pub fn new(dims: usize) -> Self {
        EmbedUiState {
            backend: "loading model".to_string(),
            total_files: 0,
            files_embedded: 0,
            files_skipped: 0,
            files_failed: 0,
            chunks: 0,
            current_file: None,
            stopping: false,
            galaxy: Galaxy::new(dims),
            stage: 0,
            stages: 0,
            stage_label: String::new(),
            stage_processed: 0,
            stage_started_at: 0.0,
            stage_note: None,
            rate_samples: VecDeque::new(),
            elapsed_seconds: 0.0,
            last_rate_sample: f64::NEG_INFINITY,
        }
    }

    /// Files finished in the current stage.
    pub fn processed(&self) -> usize {
        self.stage_processed
    }

    pub fn apply(&mut self, event: PassEvent) {
        match event {
            PassEvent::Started {
                total_files,
                backend,
                ..
            } => {
                self.total_files = total_files;
                self.backend = backend;
            }
            PassEvent::StageStarted {
                stage,
                stages,
                files,
                ..
            } => {
                self.stage = stage;
                self.stages = stages;
                self.stage_label = stage_label(stage, stages);
                self.total_files = files;
                self.stage_processed = 0;
                self.stage_started_at = self.elapsed_seconds;
                self.stage_note = None;
            }
            PassEvent::StageFinished { stage, stages } => {
                self.stage_note = Some(pass_done_text(stage, stages));
            }
            PassEvent::FileStarted { .. } => {}
            PassEvent::ChunksEmbedded {
                path,
                kind,
                vectors,
            } => {
                self.current_file = Some(file_name(&path));
                self.chunks += vectors.len();
                self.galaxy
                    .add_vectors(PointKind::from_file_kind(&kind), vectors);
            }
            PassEvent::FileFinished { path, status } => {
                self.current_file = Some(file_name(&path));
                self.stage_processed += 1;
                match status {
                    crate::embed::store::FileStatus::Done => self.files_embedded += 1,
                    crate::embed::store::FileStatus::Skipped => self.files_skipped += 1,
                    crate::embed::store::FileStatus::Failed => self.files_failed += 1,
                }
            }
            PassEvent::Finished(_) => {}
        }
    }

    /// Advances the clock by `dt_seconds` and records throughput samples.
    pub fn tick(&mut self, dt_seconds: f64) {
        self.elapsed_seconds += dt_seconds.max(0.0);
        self.galaxy.tick(dt_seconds as f32);
        if self.elapsed_seconds - self.last_rate_sample >= RATE_SAMPLE_EVERY {
            self.last_rate_sample = self.elapsed_seconds;
            self.rate_samples
                .push_back((self.elapsed_seconds, self.chunks));
            while let Some(&(at, _)) = self.rate_samples.front() {
                if self.elapsed_seconds - at > RATE_WINDOW_SECONDS {
                    self.rate_samples.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    pub fn chunks_per_second(&self) -> f64 {
        chunks_per_second(&self.rate_samples)
    }

    pub fn elapsed_seconds(&self) -> f64 {
        self.elapsed_seconds
    }

    pub fn progress_fraction(&self) -> f64 {
        if self.total_files == 0 {
            0.0
        } else {
            (self.processed() as f64 / self.total_files as f64).min(1.0)
        }
    }
}

/// Chunks per second across the window: (last count - first count) / (last time - first time).
pub fn chunks_per_second(samples: &VecDeque<(f64, usize)>) -> f64 {
    match (samples.front(), samples.back()) {
        (Some(&(first_at, first_count)), Some(&(last_at, last_count))) if last_at > first_at => {
            last_count.saturating_sub(first_count) as f64 / (last_at - first_at)
        }
        _ => 0.0,
    }
}

/// ETA text. Hidden (estimating) until `ETA_MIN_FILES` files have finished.
pub fn eta_text(processed: usize, total: usize, elapsed_seconds: f64) -> String {
    if processed < ETA_MIN_FILES || elapsed_seconds <= 0.0 || total <= processed {
        return "estimating...".to_string();
    }
    let files_per_second = processed as f64 / elapsed_seconds;
    let remaining = (total - processed) as f64 / files_per_second;
    format!("ETA {}", format_clock(remaining.round() as u64))
}

fn format_clock(seconds: u64) -> String {
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Rotating patience message. Message 4 is only used once progress reaches 80 percent.
pub fn patience_message(elapsed_seconds: f64, progress_fraction: f64) -> &'static str {
    let usable = if progress_fraction >= 0.8 {
        PATIENCE_MESSAGES.len()
    } else {
        PATIENCE_MESSAGES.len() - 1
    };
    let slot = ((elapsed_seconds.max(0.0) / PATIENCE_SECONDS) as usize) % usable;
    PATIENCE_MESSAGES[slot]
}

/// What a key press means on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Stop,
    Ignore,
}

/// `q`, `Esc`, and `Ctrl-C` stop the pass; everything else is ignored.
pub fn key_action(key: &KeyEvent) -> KeyAction {
    if key.kind != KeyEventKind::Press {
        return KeyAction::Ignore;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Stop,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyAction::Stop,
        _ => KeyAction::Ignore,
    }
}

/// Screen regions for a terminal of the given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenLayout {
    /// Galaxy on the left, panel on the right.
    Wide,
    /// Panel only, no canvas.
    Compact,
}

pub fn screen_layout(width: u16, height: u16) -> ScreenLayout {
    if width >= WIDE_MIN_WIDTH && height >= WIDE_MIN_HEIGHT {
        ScreenLayout::Wide
    } else {
        ScreenLayout::Compact
    }
}

/// Renders the whole screen into `frame`. Never panics for any area size.
pub fn draw(frame: &mut Frame, state: &EmbedUiState, theme: &Theme) {
    let area = frame.area();
    match screen_layout(area.width, area.height) {
        ScreenLayout::Wide => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
                .split(area);
            draw_galaxy(frame, columns[0], state, theme);
            draw_panel(frame, columns[1], state, theme);
        }
        ScreenLayout::Compact => draw_panel(frame, area, state, theme),
    }
}

fn kind_color(kind: PointKind, theme: &Theme) -> Color {
    match kind {
        PointKind::Text => theme.accent,
        PointKind::Pdf => theme.warn,
        PointKind::Image => theme.ok,
        PointKind::Other => theme.text,
    }
}

/// Dot centres for each layer, in canvas units. Shared by the drawing code and its tests.
#[derive(Debug, Default)]
pub struct CloudLayers {
    pub flash: Vec<(f64, f64)>,
    pub near: [Vec<(f64, f64)>; 4],
    pub mid: [Vec<(f64, f64)>; 4],
    pub far: Vec<(f64, f64)>,
}

/// Buckets projected points by age and depth. Points younger than the flash window go to `flash`.
pub fn cloud_layers(
    points: &[GalaxyPoint],
    camera: &Camera,
    elapsed: f32,
    placeholder: bool,
) -> CloudLayers {
    let mut layers = CloudLayers::default();
    for point in points {
        let (x, y, depth) = project_point(point.pos, camera);
        let coords = (f64::from(x), f64::from(y));
        if !placeholder && elapsed - point.born < FLASH_SECONDS {
            layers.flash.push(coords);
        } else if depth < 0.34 {
            layers.near[point.hue.index()].push(coords);
        } else if depth < 0.67 {
            layers.mid[point.hue.index()].push(coords);
        } else {
            layers.far.push(coords);
        }
    }
    layers
}

/// Size in canvas units of one braille dot. A cell holds 2 dots across and 4 down.
fn dot_step(x_range: f64, cols: f64, rows: f64) -> (f64, f64) {
    ((2.0 * x_range) / (cols * 2.0), 2.4 / (rows * 4.0))
}

/// Each point becomes a `size` by `size` block of dots, centred on the point.
fn clustered(points: &[(f64, f64)], size: usize, step: (f64, f64)) -> Vec<(f64, f64)> {
    let half = (size as f64 - 1.0) / 2.0;
    let mut out = Vec::with_capacity(points.len() * size * size);
    for &(x, y) in points {
        for i in 0..size {
            for j in 0..size {
                out.push((
                    x + (i as f64 - half) * step.0,
                    y + (j as f64 - half) * step.1,
                ));
            }
        }
    }
    out
}

fn draw_galaxy(frame: &mut Frame, area: Rect, state: &EmbedUiState, theme: &Theme) {
    let camera = state.galaxy.camera;
    let layers = cloud_layers(
        &state.galaxy.points,
        &camera,
        state.galaxy.elapsed,
        state.galaxy.is_placeholder(),
    );
    let cols = f64::from(area.width.max(1));
    let rows = f64::from(area.height.max(1));
    // A braille cell is about twice as tall as it is wide, so scale the x range to keep circles round.
    let aspect = (cols / (2.0 * rows)).clamp(0.25, 4.0);
    let x_range = 1.2 * aspect;
    let step = dot_step(x_range, cols, rows);
    let far = clustered(&layers.far, 2, step);
    let flash = clustered(&layers.flash, 3, step);
    let mid = layers.mid.map(|points| clustered(&points, 2, step));
    let near = layers.near.map(|points| clustered(&points, 2, step));
    let kinds = [
        PointKind::Text,
        PointKind::Pdf,
        PointKind::Image,
        PointKind::Other,
    ];
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-x_range, x_range])
        .y_bounds([-1.2, 1.2])
        .paint(move |ctx| {
            ctx.draw(&Points {
                coords: &far,
                color: theme.muted,
            });
            for kind in kinds {
                let index = kind.index();
                ctx.draw(&Points {
                    coords: &mid[index],
                    color: kind_color(kind, theme),
                });
                ctx.draw(&Points {
                    coords: &near[index],
                    color: kind_color(kind, theme),
                });
            }
            ctx.draw(&Points {
                coords: &flash,
                color: Color::White,
            });
        });
    frame.render_widget(canvas, area);
}

fn draw_panel(frame: &mut Frame, area: Rect, state: &EmbedUiState, theme: &Theme) {
    let inner_width = usize::from(area.width.saturating_sub(2));
    let backend_style = if state.backend == "Metal" {
        Style::default().fg(theme.ok)
    } else {
        Style::default().fg(theme.text)
    };
    let title = Line::from(Span::styled(
        "lctr embedding",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    let model_line = Line::from(vec![
        Span::styled(format!("{MODEL_LABEL} | "), Style::default().fg(theme.text)),
        Span::styled(state.backend.clone(), backend_style),
    ]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(title), rows[0]);
    frame.render_widget(Paragraph::new(model_line), rows[1]);

    let processed = state.processed();
    let ratio = state.progress_fraction();
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(theme.ok).bg(theme.panel_bg))
        .ratio(ratio)
        .label(format!("{processed}/{} files", state.total_files));
    frame.render_widget(gauge, rows[2]);

    let rate = state.chunks_per_second();
    let mut lines = vec![
        Line::from(format!("chunks {}", state.chunks)),
        Line::from(format!("files {processed}/{}", state.total_files)),
        Line::from(format!("{rate:.1} chunks/s")),
        Line::from(eta_text(
            processed,
            state.total_files,
            state.elapsed_seconds() - state.stage_started_at,
        )),
    ];
    if !state.stage_label.is_empty() {
        lines.insert(0, Line::from(state.stage_label.clone()));
    }
    let now = state
        .current_file
        .as_deref()
        .map(|name| truncate(name, inner_width.saturating_sub(5)))
        .unwrap_or_else(|| "starting".to_string());
    lines.push(Line::from(format!("last: {now}")));
    if state.files_skipped > 0 || state.files_failed > 0 {
        lines.push(Line::from(format!(
            "skipped {}, failed {}",
            state.files_skipped, state.files_failed
        )));
    }
    lines.push(Line::from(""));
    let note = match &state.stage_note {
        Some(note) => note.clone(),
        None => patience_message(state.elapsed_seconds(), state.progress_fraction()).to_string(),
    };
    lines.push(Line::from(Span::styled(
        note,
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));
    let footer = if state.stopping {
        "stopping, progress is saved".to_string()
    } else {
        "q  stop and keep progress".to_string()
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), rows[3]);
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('~');
    out
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Runs the pass on a worker thread and shows the galaxy until it finishes or the user stops it.
/// The terminal is restored before this returns, on every path. The caller prints the summary.
pub fn run_embedding_screen(request: PassRequest) -> Result<PassSummary> {
    let dims = request.dims;
    let theme = Theme::load_with_default(&Config::load().theme);
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<PassEvent>();
    let worker_cancel = Arc::clone(&cancel);
    let worker = thread::spawn(move || run_pass(&request, tx, worker_cancel));

    let guard = TerminalGuard::enter_default_cursor().context("enter terminal")?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("create terminal")?;
    let mut state = EmbedUiState::new(dims);
    let mut last_frame = Instant::now();
    let mut worker_done_at: Option<Instant> = None;
    loop {
        for event in rx.try_iter() {
            state.apply(event);
        }
        let now = Instant::now();
        state.tick(now.duration_since(last_frame).as_secs_f64());
        last_frame = now;
        terminal.draw(|frame| draw(frame, &state, &theme))?;

        if worker.is_finished() {
            let done_at = *worker_done_at.get_or_insert(now);
            if now.duration_since(done_at) >= FINAL_HOLD {
                break;
            }
        }
        if event::poll(FRAME_POLL)? {
            if let Event::Key(key) = event::read()? {
                if key_action(&key) == KeyAction::Stop && !state.stopping {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    state.stopping = true;
                }
            }
        }
    }
    drop(terminal);
    drop(guard);
    worker
        .join()
        .map_err(|_| anyhow!("embedding pass panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::fake::HashEmbedder;
    use crate::embed::runtime::Embedder;
    use crossterm::event::KeyEventState;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn theme() -> Theme {
        Theme::from_name(crate::tui::theme::ThemeName::parse("default"))
    }

    fn synthetic_vectors(count: usize, seed: u32) -> Vec<Vec<f32>> {
        let mut embedder = HashEmbedder;
        let texts: Vec<String> = (0..count)
            .map(|index| {
                format!(
                    "synthetic chunk {seed} {index} with words {}",
                    index * 7 + seed as usize
                )
            })
            .collect();
        embedder.embed_batch(&texts).expect("embed")
    }

    #[test]
    fn panel_names_the_stage_and_says_search_works_after_stage_one() {
        let mut state = EmbedUiState::new(768);
        state.backend = "Metal".to_string();
        state.apply(PassEvent::StageStarted {
            stage: 1,
            stages: 3,
            first_chunk: 0,
            end_chunk: 8,
            files: 40,
        });
        let text = render(&state, 120, 36);
        assert!(text.contains("pass 1 of 3: first 8 chunks"), "{text}");
        assert!(text.contains("files 0/40"), "{text}");

        state.apply(PassEvent::FileFinished {
            path: "/r/a.txt".to_string(),
            status: crate::embed::store::FileStatus::Done,
        });
        state.apply(PassEvent::StageFinished {
            stage: 1,
            stages: 3,
        });
        let text = render(&state, 120, 36);
        assert!(text.contains("files 1/40"), "{text}");
        assert!(text.contains("Pass 1 of 3 done."), "{text}");
        // The note wraps inside the panel, so check its two halves.
        assert!(text.contains("Search already"), "{text}");
        assert!(text.contains("remaining passes"), "{text}");
    }

    fn render(state: &EmbedUiState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, state, &theme()))
            .expect("draw");
        buffer_text(terminal.backend().buffer())
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn state_with_points(count: usize) -> EmbedUiState {
        let mut state = EmbedUiState::new(768);
        state.backend = "Metal".to_string();
        state.total_files = 40;
        state.apply(PassEvent::ChunksEmbedded {
            path: "/a/b.txt".to_string(),
            kind: "text".to_string(),
            vectors: synthetic_vectors(count, 1),
        });
        state.chunks = count;
        state.files_embedded = 12;
        state.current_file = Some("notes.md".to_string());
        state
    }

    #[test]
    fn wide_render_with_five_hundred_points_has_panel_text() {
        let state = state_with_points(500);
        assert_eq!(state.galaxy.points.len(), 500);
        let text = render(&state, 100, 30);
        assert!(text.contains("EmbeddingGemma 2"), "{text}");
        assert!(text.contains("chunks"), "{text}");
        dump_if_requested("wide-100x30-500points", &text);
    }

    #[test]
    fn narrow_render_is_the_compact_layout_and_shows_chunks() {
        let state = state_with_points(40);
        assert_eq!(screen_layout(50, 12), ScreenLayout::Compact);
        let text = render(&state, 50, 12);
        assert!(text.contains("chunks"), "{text}");
        dump_if_requested("compact-50x12", &text);
    }

    #[test]
    fn other_sizes_render_without_panicking() {
        let state = state_with_points(60);
        for (width, height) in [(120, 35), (70, 20), (20, 5), (1, 1), (0, 0)] {
            let text = render(&state, width, height);
            if width >= 20 && height >= 5 {
                dump_if_requested(&format!("size-{width}x{height}"), &text);
            }
        }
    }

    #[test]
    fn placeholder_screen_renders_before_the_first_vector() {
        let state = EmbedUiState::new(768);
        let text = render(&state, 100, 30);
        assert!(text.contains("chunks"), "{text}");
        assert!(state.galaxy.is_placeholder());
    }

    /// Writes each rendered frame into $LCTR_UI_DUMP_DIR for review. Does nothing otherwise.
    fn dump_if_requested(name: &str, text: &str) {
        if let Ok(dir) = std::env::var("LCTR_UI_DUMP_DIR") {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{name}.txt")), text);
        }
    }

    #[test]
    fn eta_is_hidden_before_five_files_and_shown_after() {
        assert_eq!(eta_text(4, 40, 20.0), "estimating...");
        let shown = eta_text(5, 40, 20.0);
        assert!(shown.starts_with("ETA "), "{shown}");
        assert_eq!(eta_text(0, 40, 0.0), "estimating...");
    }

    #[test]
    fn galaxy_caps_points_at_six_thousand() {
        let mut galaxy = Galaxy::new(768);
        // One real batch still runs projection and refit. Further points go through
        // insert_point directly: add_vectors on 7,500 x 768-dim vectors refits the
        // whole cloud often enough to dominate `cargo test` in debug.
        galaxy.add_vectors(PointKind::Text, synthetic_vectors(250, 0));
        for _ in 0..(MAX_POINTS + 500) {
            galaxy.insert_point(GalaxyPoint {
                pos: [0.0, 0.0, 0.0],
                target: [0.0, 0.0, 0.0],
                born: 0.0,
                hue: PointKind::Text,
                vector: Vec::new(),
            });
        }
        assert_eq!(galaxy.points.len(), MAX_POINTS);
    }

    #[test]
    fn ease_moves_strictly_toward_the_target() {
        let mut point = GalaxyPoint {
            pos: [0.0, 0.0, 0.0],
            target: [1.0, -1.0, 0.5],
            born: 0.0,
            hue: PointKind::Text,
            vector: Vec::new(),
        };
        let before = (point.target[0] - point.pos[0]).abs();
        ease(&mut point);
        let after = (point.target[0] - point.pos[0]).abs();
        assert!(after < before, "{after} !< {before}");
        assert!(point.pos[0] > 0.0 && point.pos[1] < 0.0);
    }

    #[test]
    fn patience_message_four_needs_eighty_percent() {
        let fourth = PATIENCE_MESSAGES[3];
        for step in 0..200 {
            assert_ne!(patience_message(f64::from(step), 0.5), fourth);
        }
        let reached = (0..200).any(|step| patience_message(f64::from(step), 0.9) == fourth);
        assert!(reached);
    }

    #[test]
    fn keys_stop_the_pass_only_for_q_esc_and_ctrl_c() {
        let press = |code: KeyCode, modifiers: KeyModifiers| KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(
            key_action(&press(KeyCode::Char('q'), KeyModifiers::NONE)),
            KeyAction::Stop
        );
        assert_eq!(
            key_action(&press(KeyCode::Esc, KeyModifiers::NONE)),
            KeyAction::Stop
        );
        assert_eq!(
            key_action(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Stop
        );
        assert_eq!(
            key_action(&press(KeyCode::Char('c'), KeyModifiers::NONE)),
            KeyAction::Ignore
        );
        assert_eq!(
            key_action(&press(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Ignore
        );
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Char('q'), KeyModifiers::NONE)
        };
        assert_eq!(key_action(&release), KeyAction::Ignore);
    }

    #[test]
    fn rate_uses_the_window_between_first_and_last_sample() {
        let samples: VecDeque<(f64, usize)> = VecDeque::from(vec![(1.0, 10), (6.0, 60)]);
        assert!((chunks_per_second(&samples) - 10.0).abs() < 1e-9);
        assert_eq!(chunks_per_second(&VecDeque::new()), 0.0);
    }

    #[test]
    fn layout_falls_back_below_seventy_by_sixteen() {
        assert_eq!(screen_layout(70, 16), ScreenLayout::Wide);
        assert_eq!(screen_layout(69, 16), ScreenLayout::Compact);
        assert_eq!(screen_layout(70, 15), ScreenLayout::Compact);
    }

    /// Points on a sphere of radius 0.9, spread evenly (Fibonacci lattice). Symmetric about the origin.
    fn sphere_points(count: usize) -> Vec<GalaxyPoint> {
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        (0..count)
            .map(|i| {
                let y = 1.0 - 2.0 * (i as f32 + 0.5) / count as f32;
                let radius = (1.0 - y * y).max(0.0).sqrt();
                let theta = golden * i as f32;
                let pos = [
                    0.9 * radius * theta.cos(),
                    0.9 * y,
                    0.9 * radius * theta.sin(),
                ];
                GalaxyPoint {
                    pos,
                    target: pos,
                    born: -1000.0,
                    hue: PointKind::Text,
                    vector: Vec::new(),
                }
            })
            .collect()
    }

    fn bounds_of(points: &[(f64, f64)]) -> (f64, f64, f64, f64) {
        let min_x = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let max_x = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let min_y = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        (min_x, max_x, min_y, max_y)
    }

    #[test]
    fn drawn_cloud_is_centred_and_fills_the_canvas_height() {
        let points = sphere_points(400);
        let layers = cloud_layers(&points, &Camera::default(), 0.0, false);
        let mut centres: Vec<(f64, f64)> = Vec::new();
        centres.extend(&layers.far);
        centres.extend(&layers.flash);
        for index in 0..4 {
            centres.extend(&layers.mid[index]);
            centres.extend(&layers.near[index]);
        }
        assert_eq!(centres.len(), points.len());
        // The drawn dots use the same step at any canvas size, so check the centres in canvas units.
        let (min_x, max_x, min_y, max_y) = bounds_of(&centres);
        let centre_x = (min_x + max_x) / 2.0;
        let centre_y = (min_y + max_y) / 2.0;
        assert!(centre_x.abs() < 0.05, "horizontal centre {centre_x}");
        assert!(centre_y.abs() < 0.05, "vertical centre {centre_y}");
        // The canvas is 2.4 units tall. A cloud of radius 0.9 spans most of it.
        assert!(max_y - min_y > 0.6 * 2.4, "height span {}", max_y - min_y);
        assert!(max_x - min_x > 0.6 * 2.4, "width span {}", max_x - min_x);
    }

    #[test]
    fn fitted_cloud_is_centred_on_its_mean_and_scaled_to_the_canvas() {
        let mut galaxy = Galaxy::new(768);
        galaxy.add_vectors(PointKind::Text, synthetic_vectors(300, 7));
        let count = galaxy.points.len() as f32;
        let mut mean = [0.0f32; 3];
        let mut max_radius = 0.0f32;
        for point in &galaxy.points {
            for (total, value) in mean.iter_mut().zip(point.target) {
                *total += value / count;
            }
            let radius = point.target.iter().map(|v| v * v).sum::<f32>().sqrt();
            max_radius = max_radius.max(radius);
        }
        for value in mean {
            assert!(value.abs() < 0.1, "mean {mean:?}");
        }
        assert!(max_radius <= 0.95, "max radius {max_radius}");
        assert!(max_radius > 0.5, "max radius {max_radius}");
    }

    #[test]
    fn now_label_follows_embedded_and_finished_files_not_extract_starts() {
        let mut state = EmbedUiState::new(768);
        state.apply(PassEvent::FileStarted {
            path: "/d/z-last-alphabetically.md".to_string(),
        });
        assert_eq!(state.current_file, None);
        state.apply(PassEvent::ChunksEmbedded {
            path: "/d/a.md".to_string(),
            kind: "text".to_string(),
            vectors: synthetic_vectors(2, 3),
        });
        assert_eq!(state.current_file.as_deref(), Some("a.md"));
        state.apply(PassEvent::FileFinished {
            path: "/d/b.md".to_string(),
            status: crate::embed::store::FileStatus::Done,
        });
        assert_eq!(state.current_file.as_deref(), Some("b.md"));
        state.apply(PassEvent::FileStarted {
            path: "/d/c.md".to_string(),
        });
        assert_eq!(state.current_file.as_deref(), Some("b.md"));
    }

    #[test]
    fn draw_timing_at_the_point_cap() {
        let mut state = EmbedUiState::new(768);
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % 2_000_001) as f32 / 1_000_000.0 - 1.0
        };
        state.galaxy.placeholder = false;
        state.galaxy.points = (0..MAX_POINTS)
            .map(|_| GalaxyPoint {
                pos: [next() * 0.9, next() * 0.9, next() * 0.9],
                target: [0.0; 3],
                born: -1000.0,
                hue: PointKind::Pdf,
                vector: Vec::new(),
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
        let frames = 20;
        let started = Instant::now();
        for _ in 0..frames {
            state.tick(0.05);
            terminal
                .draw(|frame| draw(frame, &state, &theme()))
                .expect("draw");
        }
        let per_frame = started.elapsed() / frames;
        println!(
            "draw {} points at 120x36: {:.2} ms per frame (debug or release per the build used)",
            MAX_POINTS,
            per_frame.as_secs_f64() * 1000.0
        );
        assert!(per_frame < Duration::from_millis(500), "{per_frame:?}");
    }
}

/// Width of the embedding summary card: the terminal width minus 2, clamped to 60 to 140.
pub fn card_width_for_columns(columns: u16) -> u16 {
    columns
        .saturating_sub(2)
        .clamp(CARD_MIN_WIDTH, CARD_MAX_WIDTH)
}

const CARD_MIN_WIDTH: u16 = 60;
const CARD_MAX_WIDTH: u16 = 140;

/// Inputs for the embedding summary card.
pub struct CardInput<'a> {
    pub summary: &'a PassSummary,
    pub root: &'a Path,
    pub backend: &'a str,
    pub width: u16,
}

/// The end-of-pass summary: three rounded panels sized to their content, in the same style as the
/// scan summary (bordered panels rendered to a string). Plain text, no colour.
pub fn render_embedding_card(input: &CardInput<'_>) -> String {
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Margin, Rect};
    use ratatui::widgets::{Block, BorderType, Borders, Widget};

    let summary = input.summary;
    let width = input.width.clamp(CARD_MIN_WIDTH, CARD_MAX_WIDTH);
    let inner_width = usize::from(width.saturating_sub(4));
    let seconds = summary.elapsed.as_secs_f64();
    let rate = if seconds > 0.0 {
        summary.chunks as f64 / seconds
    } else {
        0.0
    };
    let low_value: usize = summary
        .timing
        .low_value
        .iter()
        .map(|(_, count)| count)
        .sum();
    let other_skipped = summary.files_skipped.saturating_sub(low_value);

    let mut hero = vec![
        format!(
            "{} chunks from {} files",
            summary.chunks, summary.files_embedded
        ),
        format!(
            "elapsed {} | {:.2} chunks/s",
            format_clock(summary.elapsed.as_secs()),
            rate
        ),
    ];
    hero.push(depth_text(summary));
    if summary.cancelled {
        hero.push("stopped early; progress is saved".to_string());
    }
    let stats = vec![
        format!("embedded       {}", summary.files_embedded),
        format!("unchanged      {}", summary.files_unchanged),
        format!("skipped        {}", other_skipped),
        format!("low-value      {}", low_value),
        format!("failed         {}", summary.files_failed),
        format!("deduplicated   {} chunks", summary.timing.deduplicated),
        format!(
            "sidecar        {:.1} MB",
            summary.sidecar_bytes as f64 / 1_000_000.0
        ),
        format!("backend        {}", input.backend),
    ];
    let mut next = vec![
        format!("lctr search {}", input.root.display()),
        "  then press m until the mode reads semantic".to_string(),
        "lctr find \"<query>\" --mode semantic".to_string(),
        "lctr embed status".to_string(),
    ];
    if summary.cancelled {
        next.push("run the same command to resume".to_string());
    }

    let panels: [(&str, &[String]); 3] = [
        (
            if summary.cancelled {
                "embedding stopped"
            } else {
                "embedding complete"
            },
            &hero,
        ),
        ("stats", &stats),
        ("next", &next),
    ];
    let heights: Vec<u16> = panels
        .iter()
        .map(|(_, lines)| lines.len() as u16 + 2)
        .collect();
    let total: u16 = heights.iter().sum();
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, total));
    let mut top = 0u16;
    for ((title, lines), height) in panels.iter().zip(&heights) {
        let area = Rect::new(0, top, width, *height);
        Block::default()
            .title(format!(" {title} "))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .render(area, &mut buffer);
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        for (row, line) in lines.iter().enumerate() {
            let text = truncate(line, inner_width);
            buffer.set_stringn(
                inner.x,
                inner.y + row as u16,
                &text,
                usize::from(inner.width),
                Style::default(),
            );
        }
        top += height;
    }
    buffer_rows_to_string(&buffer)
}

/// One line on how deep the pass went and how to go deeper.
fn depth_text(summary: &PassSummary) -> String {
    crate::embed::pipeline::depth_text(summary.depth, summary.stages_done, summary.cancelled)
}

fn buffer_rows_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut rows = Vec::with_capacity(usize::from(area.height));
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buffer[(x, y)].symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use crate::embed::pipeline::PassTiming;
    use std::time::Duration;

    fn summary(cancelled: bool) -> PassSummary {
        PassSummary {
            files_embedded: 98,
            files_skipped: 12,
            files_failed: 1,
            files_unchanged: 3,
            chunks: 979,
            cancelled,
            elapsed: Duration::from_secs(371),
            sidecar_bytes: 1_234_567,
            timing: PassTiming {
                deduplicated: 42,
                low_value: vec![("lock file", 7), ("svg image", 5)],
                ..PassTiming::default()
            },
            depth: crate::embed::pipeline::Depth::Quick,
            stages_done: 1,
        }
    }

    #[test]
    fn card_has_three_panels_and_the_key_numbers_at_each_width() {
        for width in [60u16, 100, 140] {
            let text = render_embedding_card(&CardInput {
                summary: &summary(false),
                root: Path::new("/data/docs"),
                backend: "Metal",
                width,
            });
            assert!(text.contains("embedding complete"), "{text}");
            assert!(text.contains("979 chunks from 98 files"), "{text}");
            assert!(text.contains("deduplicated   42 chunks"), "{text}");
            assert!(text.contains("low-value      12"), "{text}");
            assert!(text.contains("lctr embed status"), "{text}");
            assert!(!text.contains("resume"), "{text}");
            for line in text.lines() {
                assert!(
                    line.chars().count() <= usize::from(width),
                    "line wider than {width}: {line}"
                );
            }
            // Three panels, each sized to its content: hero 5 (three lines, with the depth line) + 10 + 6
            // rows, no empty tail.
            assert_eq!(text.lines().count(), 5 + 10 + 6, "width {width}");
            assert!(
                text.contains("depth: quick (first 8 chunks of each file)"),
                "{text}"
            );
            if width == 140 {
                assert!(
                    text.contains(
                        "depth: quick (first 8 chunks of each file). --depth balanced adds chunks 9 to 32 and --depth deep adds chunks up to 128."
                    ),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn stopped_card_says_so_and_offers_to_resume() {
        let text = render_embedding_card(&CardInput {
            summary: &summary(true),
            root: Path::new("/data/docs"),
            backend: "CPU",
            width: 100,
        });
        assert!(text.contains("embedding stopped"), "{text}");
        assert!(text.contains("run the same command to resume"), "{text}");
    }

    #[test]
    fn card_width_follows_the_terminal_and_is_clamped() {
        assert_eq!(card_width_for_columns(40), 60);
        assert_eq!(card_width_for_columns(100), 98);
        assert_eq!(card_width_for_columns(400), 140);
    }
}

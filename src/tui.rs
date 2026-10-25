use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::embed::runtime::{with_embedder, Embedder};
use crate::embed::search::{embedder_config_for_search, SemanticHit, SemanticSearcher};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;

use crate::config::Config;
use crate::db::{
    existing_index_for_working_dir, sort_results_compiled, Database, ScanCompletion, SearchResult,
    SearchStreamStatus, SEARCH_BATCH_SIZE,
};
use crate::live_index::LiveIndex;
#[cfg(test)]
use crate::live_search::search_live_with_options;
use crate::live_search::{search_live_streaming_batches_with_options, LiveSearchStatus};
use crate::open::{copy_path, open_file, FinderRevealSession};
use crate::preview::{self, Preview};
use crate::query::{
    CompiledQuery, QueryMode, QueryScorer, SearchFilters, SearchOptions, SortField,
};

mod input_trace;
pub mod theme;

use theme::Theme;

const TUI_RESULT_LIMIT: usize = usize::MAX;
const NAVIGATION_BURST_WINDOW: Duration = Duration::from_millis(50);
const FRAGMENTED_ESCAPE_WINDOW: Duration = Duration::from_millis(50);
const INPUT_DRAIN_LIMIT: usize = 4096;
const INPUT_DRAIN_BUDGET: Duration = Duration::from_millis(5);
const APPLE_SILICON: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(8);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Search,
    Results,
}

#[derive(Default)]
struct NavigationBurst {
    last: Option<(KeyCode, Instant)>,
}

impl NavigationBurst {
    fn reset(&mut self) {
        self.last = None;
    }

    fn accepts(&mut self, code: KeyCode, now: Instant) -> bool {
        if let Some((last_code, last_at)) = self.last {
            if last_code == code && now.duration_since(last_at) < NAVIGATION_BURST_WINDOW {
                self.last = Some((code, now));
                return false;
            }
        }

        self.last = Some((code, now));
        true
    }
}

enum PendingEscape {
    Escape {
        key: KeyEvent,
        at: Instant,
    },
    Prefix {
        escape: KeyEvent,
        prefix: KeyEvent,
        at: Instant,
    },
    SgrMouse {
        payload: String,
        at: Instant,
    },
}

#[derive(Default)]
struct InputNormalizer {
    pending: Option<PendingEscape>,
    ready: VecDeque<Event>,
}

impl InputNormalizer {
    fn push(&mut self, event: Event, now: Instant) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.push_key(key, now),
            event => {
                self.flush_pending();
                self.ready.push_back(event);
            }
        }
    }

    fn push_key(&mut self, key: KeyEvent, now: Instant) {
        if let Some(pending) = self.pending.take() {
            if now.saturating_duration_since(pending_at(&pending)) < FRAGMENTED_ESCAPE_WINDOW {
                match pending {
                    PendingEscape::Escape { key: escape, .. }
                        if is_fragment_char(key, '[') || is_fragment_char(key, 'O') =>
                    {
                        self.pending = Some(PendingEscape::Prefix {
                            escape,
                            prefix: key,
                            at: now,
                        });
                        return;
                    }
                    PendingEscape::Prefix { escape, prefix, .. } => {
                        if let Some(code) = fragmented_arrow_code(key) {
                            self.ready
                                .push_back(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                            return;
                        }
                        if is_fragment_char(prefix, '[') && is_fragment_char(key, '<') {
                            self.pending = Some(PendingEscape::SgrMouse {
                                payload: String::new(),
                                at: now,
                            });
                            return;
                        }
                        self.ready.push_back(Event::Key(escape));
                        self.ready.push_back(Event::Key(prefix));
                    }
                    PendingEscape::SgrMouse { mut payload, .. } => {
                        if let KeyCode::Char(ch) = key.code {
                            if ch.is_ascii_digit() || ch == ';' {
                                payload.push(ch);
                                self.pending = Some(PendingEscape::SgrMouse { payload, at: now });
                                return;
                            }
                            if matches!(ch, 'M' | 'm') {
                                if let Some(mouse) = fragmented_sgr_mouse(&payload, ch) {
                                    self.ready.push_back(Event::Mouse(mouse));
                                }
                                return;
                            }
                        }
                    }
                    PendingEscape::Escape { key: escape, .. } => {
                        self.ready.push_back(Event::Key(escape));
                    }
                }
            } else {
                self.push_pending(pending);
            }
        }

        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            self.pending = Some(PendingEscape::Escape { key, at: now });
        } else {
            self.ready.push_back(Event::Key(key));
        }
    }

    fn push_pending(&mut self, pending: PendingEscape) {
        match pending {
            PendingEscape::Escape { key, .. } => self.ready.push_back(Event::Key(key)),
            PendingEscape::Prefix { escape, prefix, .. } => {
                self.ready.push_back(Event::Key(escape));
                self.ready.push_back(Event::Key(prefix));
            }
            PendingEscape::SgrMouse { .. } => {}
        }
    }

    fn flush_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.push_pending(pending);
        }
    }

    fn flush_expired(&mut self, now: Instant) {
        let expired = self.pending.as_ref().is_some_and(|pending| {
            now.saturating_duration_since(pending_at(pending)) >= FRAGMENTED_ESCAPE_WINDOW
        });
        if expired {
            self.flush_pending();
        }
    }

    fn poll_timeout(&self, fallback: Duration, now: Instant) -> Duration {
        self.pending.as_ref().map_or(fallback, |pending| {
            fallback.min(
                FRAGMENTED_ESCAPE_WINDOW
                    .saturating_sub(now.saturating_duration_since(pending_at(pending))),
            )
        })
    }

    fn pop_ready(&mut self) -> Option<Event> {
        self.ready.pop_front()
    }

    fn push_ready_front(&mut self, event: Event) {
        self.ready.push_front(event);
    }

    fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    fn discard_ready_navigation(&mut self, code: KeyCode) {
        while self.ready.front().is_some_and(|event| {
            matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == code)
        }) {
            self.ready.pop_front();
        }
    }
}

fn pending_at(pending: &PendingEscape) -> Instant {
    match pending {
        PendingEscape::Escape { at, .. }
        | PendingEscape::Prefix { at, .. }
        | PendingEscape::SgrMouse { at, .. } => *at,
    }
}

fn is_fragment_char(key: KeyEvent, expected: char) -> bool {
    key.code == KeyCode::Char(expected)
        && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
}

fn fragmented_arrow_code(key: KeyEvent) -> Option<KeyCode> {
    if !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::Char('A') => Some(KeyCode::Up),
        KeyCode::Char('B') => Some(KeyCode::Down),
        KeyCode::Char('C') => Some(KeyCode::Right),
        KeyCode::Char('D') => Some(KeyCode::Left),
        KeyCode::Char('H') => Some(KeyCode::Home),
        KeyCode::Char('F') => Some(KeyCode::End),
        _ => None,
    }
}

fn fragmented_sgr_mouse(payload: &str, terminator: char) -> Option<event::MouseEvent> {
    if terminator != 'M' {
        return None;
    }
    let mut fields = payload.split(';');
    let button_code = fields.next()?.parse::<u16>().ok()?;
    let column = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    if fields.next().is_some() {
        return None;
    }

    let kind = match button_code & !(4 | 8 | 16) {
        64 => MouseEventKind::ScrollUp,
        65 => MouseEventKind::ScrollDown,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::NONE;
    if button_code & 4 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if button_code & 8 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if button_code & 16 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some(event::MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn read_normalized_input(
    normalizer: &mut InputNormalizer,
    input_trace: &mut input_trace::InputTrace,
    focus: Focus,
    input: &SearchInput,
    selected: &TableState,
    fallback_timeout: Duration,
) -> Result<Option<Event>> {
    let now = Instant::now();
    normalizer.flush_expired(now);
    if let Some(event) = normalizer.pop_ready() {
        return Ok(Some(event));
    }

    let timeout = normalizer.poll_timeout(fallback_timeout, now);
    if !event::poll(timeout)? {
        return Ok(None);
    }

    let raw_event = event::read()?;
    input_trace.record(
        &raw_event,
        focus,
        input.as_str(),
        input.cursor_column(),
        selected.selected(),
    )?;
    let now = Instant::now();
    normalizer.push(raw_event, now);
    normalizer.flush_expired(now);
    Ok(normalizer.pop_ready())
}

fn scroll_delta(kind: MouseEventKind) -> isize {
    match kind {
        MouseEventKind::ScrollUp => -1,
        MouseEventKind::ScrollDown => 1,
        _ => 0,
    }
}

fn move_selection_if_changed(selected: &mut TableState, result_count: usize, delta: isize) -> bool {
    if delta == 0 {
        return false;
    }
    let before = selected.selected();
    move_selection(selected, result_count, delta);
    selected.selected() != before
}

fn drain_apple_scroll_burst(
    normalizer: &mut InputNormalizer,
    input_trace: &mut input_trace::InputTrace,
    focus: Focus,
    input: &SearchInput,
    selected: &mut TableState,
    result_count: usize,
    initial_delta: isize,
) -> Result<bool> {
    let mut changed = move_selection_if_changed(selected, result_count, initial_delta);
    for _ in 1..INPUT_DRAIN_LIMIT {
        let next = if let Some(ready) = normalizer.pop_ready() {
            Some(ready)
        } else if event::poll(Duration::ZERO)? {
            let raw_event = event::read()?;
            input_trace.record(
                &raw_event,
                focus,
                input.as_str(),
                input.cursor_column(),
                selected.selected(),
            )?;
            let now = Instant::now();
            normalizer.push(raw_event, now);
            normalizer.flush_expired(now);
            normalizer.pop_ready()
        } else {
            None
        };

        let Some(next) = next else {
            if event::poll(Duration::ZERO)? {
                continue;
            }
            break;
        };
        match next {
            Event::Mouse(mouse) => {
                changed |=
                    move_selection_if_changed(selected, result_count, scroll_delta(mouse.kind));
            }
            event => {
                normalizer.push_ready_front(event);
                break;
            }
        }
    }
    Ok(changed)
}

fn drain_same_direction_input(
    normalizer: &mut InputNormalizer,
    input_trace: &mut input_trace::InputTrace,
    focus: Focus,
    input: &SearchInput,
    selected: &TableState,
    code: KeyCode,
) -> Result<()> {
    normalizer.discard_ready_navigation(code);
    if normalizer.has_ready() {
        return Ok(());
    }

    let started = Instant::now();
    for _ in 0..INPUT_DRAIN_LIMIT {
        if started.elapsed() >= INPUT_DRAIN_BUDGET || !event::poll(Duration::ZERO)? {
            break;
        }

        let raw_event = event::read()?;
        input_trace.record(
            &raw_event,
            focus,
            input.as_str(),
            input.cursor_column(),
            selected.selected(),
        )?;
        let now = Instant::now();
        normalizer.push(raw_event, now);
        normalizer.flush_expired(now);
        normalizer.discard_ready_navigation(code);
        if normalizer.has_ready() {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchBackend {
    Indexed { db_path: PathBuf, root: PathBuf },
    Hybrid { db_path: PathBuf, root: PathBuf },
    Live { root: PathBuf },
}

impl SearchBackend {
    fn label(&self) -> &'static str {
        match self {
            Self::Indexed { .. } => "indexed",
            Self::Hybrid { .. } => "hybrid",
            Self::Live { .. } => "live",
        }
    }

    fn root_label(&self) -> String {
        match self {
            Self::Indexed { root, .. } | Self::Hybrid { root, .. } | Self::Live { root } => {
                root.display().to_string()
            }
        }
    }

    /// `(root, db_path)` for backends backed by a persistent index, so a live
    /// filesystem watcher can keep that index current. `None` for `Live`.
    fn watch_target(&self) -> Option<(PathBuf, PathBuf)> {
        match self {
            Self::Indexed { db_path, root } | Self::Hybrid { db_path, root } => {
                Some((root.clone(), db_path.clone()))
            }
            Self::Live { .. } => None,
        }
    }
}

pub fn search_backend_for_directory(start: impl AsRef<Path>) -> Result<SearchBackend> {
    let root = start
        .as_ref()
        .canonicalize()
        .with_context(|| format!("resolve search directory {}", start.as_ref().display()))?;
    if let Some(index) = existing_index_for_working_dir(&root)? {
        let db = open_search_database(&index.db_path)?;
        let root_string = index.root.to_string_lossy().to_string();
        return Ok(match db.scan_completion_for_root(&root_string)? {
            ScanCompletion::Complete => SearchBackend::Indexed {
                db_path: index.db_path,
                root: index.root,
            },
            ScanCompletion::Incomplete | ScanCompletion::Unknown => SearchBackend::Hybrid {
                db_path: index.db_path,
                root: index.root,
            },
        });
    }
    Ok(SearchBackend::Live { root })
}

pub fn run_for_current_dir() -> Result<()> {
    run_for_directory(
        std::env::current_dir().context("locate current directory")?,
        false,
    )
}

pub fn run_for_directory(root: impl AsRef<Path>, update_check_disabled: bool) -> Result<()> {
    let backend = search_backend_for_directory(root)?;
    run_with_backend(backend, update_check_disabled)
}

pub fn run(db: &Database, db_path: PathBuf) -> Result<()> {
    let _ = db;
    run_with_backend(
        SearchBackend::Indexed {
            db_path,
            root: std::env::current_dir().context("locate current directory")?,
        },
        false,
    )
}

/// Restores the terminal (raw mode off, default cursor, main screen) when
/// dropped, so panics and early returns inside the TUI cannot leave the
/// user's terminal in raw mode on the alternate screen.
pub(crate) struct TerminalGuard {
    /// Whether we pushed kitty keyboard-enhancement flags; only pop on drop if
    /// we actually pushed, so we never disturb a terminal that lacked support.
    keyboard_enhanced: bool,
    mouse_captured: bool,
}

impl TerminalGuard {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            SetCursorStyle::BlinkingBar,
            Show
        )?;
        let keyboard_enhanced = enable_keyboard_enhancement();
        let mouse_captured = if APPLE_SILICON {
            execute!(io::stdout(), EnableMouseCapture).is_ok()
        } else {
            false
        };
        Ok(Self {
            keyboard_enhanced,
            mouse_captured,
        })
    }

    /// Like [`TerminalGuard::enter`] but leaves the cursor style untouched,
    /// for UIs that never show a text cursor (e.g. the config editor).
    pub(crate) fn enter_default_cursor() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let keyboard_enhanced = enable_keyboard_enhancement();
        Ok(Self {
            keyboard_enhanced,
            mouse_captured: false,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.mouse_captured {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            discard_pending_terminal_input();
        }
        if self.keyboard_enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen,
            Show
        );
    }
}

#[cfg(unix)]
fn discard_pending_terminal_input() {
    // Mouse tracking reports can still be queued when the app exits. Clear
    // them while raw mode is active so the parent shell cannot read them.
    unsafe {
        let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

#[cfg(not(unix))]
fn discard_pending_terminal_input() {}

/// Request the kitty keyboard protocol's escape-code disambiguation. This makes
/// a lone `Esc` register on the first press instead of being held back by the
/// parser as the possible prefix of an arrow or function-key sequence. The
/// request is non-blocking, and unsupported terminals ignore the private CSI
/// sequence. Returns whether the flags were pushed so the guard knows to pop
/// them on teardown.
fn enable_keyboard_enhancement() -> bool {
    // crossterm's support probe waits for a terminal response for up to two
    // seconds. The kitty keyboard sequence is a private CSI request, so
    // unsupported terminals can ignore it without a blocking probe.
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
}

pub fn run_with_backend(search_backend: SearchBackend, update_check_disabled: bool) -> Result<()> {
    let guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut finder_reveal = run_loop(&mut terminal, search_backend, update_check_disabled)?;
    // Restore normal signal handling before waiting for external Finder cleanup.
    drop(guard);
    finder_reveal.close().context("close Finder reveal session")
}

fn is_quit_key(key: KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && (key.code == KeyCode::Esc
            || (matches!(key.code, KeyCode::Char('c' | 'C'))
                && key.modifiers.contains(KeyModifiers::CONTROL)))
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    search_backend: SearchBackend,
    update_check_disabled: bool,
) -> Result<FinderRevealSession> {
    let mut input = SearchInput::default();
    let mut results = Vec::new();
    let mut result_paths = HashSet::new();
    let mut reset_pending = false;
    let mut selection_anchor: Option<String> = None;
    let mut results_stamp: u64 = 0;
    let mut row_cache = RowCache::empty();
    let mut selected = TableState::default();
    let mut viewport_start = 0usize;
    let mut status = String::from("Type to search. Tab or Down to navigate results.");
    let mut search_state = SearchState::default();
    let backend_label = search_backend.label();
    let root_label = search_backend.root_label();
    let config = Config::load();
    let mut theme = Theme::load_with_default(&config.theme);
    let mut mode = QueryMode::Contains;
    let mut sort = SortField::Relevance;
    let mut reverse = matches!(sort, SortField::Modified);
    let mut filters = SearchFilters::new();
    let watch_target = search_backend.watch_target();
    let semantic_available = watch_target
        .as_ref()
        .is_some_and(|(_, db_path)| crate::embed::semantic_available(db_path));
    let mut semantic_loaded = false;
    let mut live_index = watch_target.as_ref().and_then(|(root, db_path)| {
        LiveIndex::spawn_background(root.clone(), db_path.clone()).ok()
    });
    let mut watch_enabled = live_index.is_some();
    let mut live_generation = live_index.as_ref().map_or(0, LiveIndex::generation);
    // Do not probe terminal capabilities before the first frame. The probe
    // waits for a response on stdin and can block for seconds in terminals
    // that do not implement the graphics query, while also racing input
    // handling after its timeout. Half-block rendering remains available for
    // image previews without delaying TUI startup.
    let mut picker = Picker::halfblocks();
    let preview_worker = PreviewWorker::spawn();
    let mut preview_requested: Option<String> = None;
    let mut preview_state: Option<PreviewState> = None;
    let mut preview_target: Option<String> = None;
    let mut preview_pending_since = Instant::now();
    let mut show_help = false;
    let mut focus = Focus::Search;
    // Env var overrides config; config is the persistent default.
    let icons_enabled = icons_env_override().unwrap_or(config.icons);
    let preview_enabled = config.preview;
    let mut search_worker = SearchWorker::spawn(search_backend)?;
    let mut loading_query: Option<String> = None;
    let mut last_edit = Instant::now();
    let update_rx = crate::update_check::check_async(update_check_disabled);
    let mut update_status: Option<crate::update_check::UpdateStatus> = None;
    let mut spinner_frame: usize = 0;
    let mut finder_reveal = FinderRevealSession::new();
    let mut input_trace = input_trace::InputTrace::from_env()?;
    let mut input_normalizer = InputNormalizer::default();
    let mut navigation_burst = NavigationBurst::default();
    let mut needs_draw = true;
    let mut next_spinner_deadline = Instant::now();

    loop {
        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        while let Some(response) = finder_reveal.try_reveal_response() {
            match response.result {
                Ok(()) => {
                    let path = response.path.to_string_lossy().to_string();
                    record_access_if_indexed(&watch_target, &path);
                    status = format!("revealed {path}");
                }
                Err(error) => {
                    status = format!("reveal failed: {error}");
                }
            }
            needs_draw = true;
        }

        // Yield to keyboard input even when a broad query keeps producing batches.
        for _ in 0..8 {
            let Some(update) = search_worker.try_recv() else {
                break;
            };
            let query = input.as_str();
            match update {
                SearchUpdate::Reset {
                    request_id,
                    options,
                } if search_state.accepts_update(request_id, query, &options) => {
                    selection_anchor = selected_path(&selected, &results).map(str::to_string);
                    reset_pending = true;
                    loading_query = Some(options.query.clone());
                    next_spinner_deadline = Instant::now();
                    status = format!("searching {backend_label} for {}", options.query);
                    needs_draw = true;
                }
                SearchUpdate::Append {
                    request_id,
                    options,
                    results: batch,
                    count,
                } if search_state.accepts_update(request_id, query, &options) => {
                    let current_anchor = selected_path(&selected, &results)
                        .map(str::to_string)
                        .or_else(|| selection_anchor.clone());
                    if reset_pending {
                        results.clear();
                        result_paths.clear();
                        selected.select(None);
                        reset_pending = false;
                    }
                    for result in batch {
                        if result_paths.insert(result.path.clone()) {
                            results.push(result);
                        }
                    }
                    restore_selection_by_path(&mut selected, &results, current_anchor.as_deref());
                    selection_anchor = None;
                    results_stamp = results_stamp.wrapping_add(1);
                    loading_query = Some(options.query.clone());
                    status = format!("{} results, searching…", format_count(count));
                    needs_draw = true;
                }
                SearchUpdate::Complete {
                    request_id,
                    options,
                    count,
                    final_results,
                    live_backfill,
                } if search_state.accepts_update(request_id, query, &options) => {
                    let current_anchor = selected_path(&selected, &results)
                        .map(str::to_string)
                        .or_else(|| selection_anchor.clone());
                    if let Some(final_results) = final_results {
                        results.clear();
                        result_paths.clear();
                        for result in final_results {
                            if result_paths.insert(result.path.clone()) {
                                results.push(result);
                            }
                        }
                    } else if reset_pending {
                        results.clear();
                        result_paths.clear();
                    }
                    reset_pending = false;
                    restore_selection_by_path(&mut selected, &results, current_anchor.as_deref());
                    selection_anchor = None;
                    results_stamp = results_stamp.wrapping_add(1);
                    loading_query = None;
                    next_spinner_deadline = Instant::now();
                    if options.mode == QueryMode::Semantic {
                        semantic_loaded = true;
                    }
                    if live_backfill {
                        search_state.mark_live_complete(options.query.clone());
                    }
                    status = format!("{} results complete", format_count(count));
                    needs_draw = true;
                }
                SearchUpdate::Error {
                    request_id,
                    options,
                    error,
                } if search_state.accepts_update(request_id, query, &options) => {
                    loading_query = None;
                    reset_pending = false;
                    selection_anchor = None;
                    status = error;
                    needs_draw = true;
                }
                _ => {}
            }
        }

        let query = input.as_str();
        if search_state.should_auto_submit(query, backend_label, last_edit.elapsed()) {
            let options = tui_search_options(query)
                .with_mode(mode)
                .with_sort(sort)
                .with_reverse(reverse)
                .with_filters(filters.clone());
            if let Ok(request_id) = search_worker.submit(SearchRequest {
                options: options.clone(),
            }) {
                search_state.mark_submitted_with_id(options, false, request_id);
                loading_query = Some(query.to_string());
                status = format!("searching {backend_label} index for {query}");
                needs_draw = true;
            }
        }

        // When the live watcher reports the index changed, re-run the current
        // query so new/removed files surface without a keystroke.
        if let Some(live) = live_index.as_ref() {
            let current = live.generation();
            // A busy filesystem must not continually cancel an active search.
            if current != live_generation && loading_query.is_none() {
                live_generation = current;
                let query = input.as_str();
                if should_show_results(query) {
                    let options = tui_search_options(query)
                        .with_mode(mode)
                        .with_sort(sort)
                        .with_reverse(reverse)
                        .with_filters(filters.clone());
                    if let Ok(request_id) = search_worker.submit(SearchRequest {
                        options: options.clone(),
                    }) {
                        search_state.mark_submitted_with_id(options, false, request_id);
                        loading_query = Some(query.to_string());
                        needs_draw = true;
                    }
                }
            }
        }

        if update_status.is_none() {
            if let Ok(Some(s)) = update_rx.try_recv() {
                update_status = Some(s);
                needs_draw = true;
            }
        }

        // Debounced preview: only build once the selection has been still for
        // PREVIEW_DEBOUNCE. This keeps fast arrow-key scrolling smooth -- images
        // and PDFs you scroll past are never decoded, only the row you land on.
        let preview_path = selected
            .selected()
            .and_then(|index| results.get(index))
            .map(|result| result.path.clone());
        if preview_path != preview_target {
            preview_target = preview_path.clone();
            preview_pending_since = Instant::now();
            preview_requested = None;
            preview_state = None;
            needs_draw = true;
        }
        while let Ok((path, preview)) = preview_worker.rx.try_recv() {
            if preview_enabled && preview_target.as_deref() == Some(path.as_str()) {
                preview_state = Some(build_preview(&mut picker, preview));
                needs_draw = true;
            }
        }
        match &preview_target {
            Some(path) => {
                if preview_enabled
                    && preview_requested.as_deref() != Some(path.as_str())
                    && preview_pending_since.elapsed() >= PREVIEW_DEBOUNCE
                {
                    preview_worker
                        .tx
                        .send(path.clone())
                        .context("request preview")?;
                    preview_requested = Some(path.clone());
                }
            }
            None => {
                if preview_state.take().is_some() {
                    needs_draw = true;
                }
            }
        }

        let spinner_due = loading_query.is_some() && Instant::now() >= next_spinner_deadline;
        if spinner_due {
            next_spinner_deadline = Instant::now() + Duration::from_millis(100);
            needs_draw = true;
        }
        if needs_draw {
            spinner_frame = spinner_frame.wrapping_add(1);
            terminal.draw(|frame| {
                // Fill the entire terminal area with the theme background.
                frame.render_widget(
                    Block::default().style(Style::default().bg(theme.bg)),
                    frame.area(),
                );

                let query = input.as_str();
                let has_detail = frame.area().height >= 20;
                let detail_rows = {
                    let semantic_details = search_worker.semantic_details();
                    let semantic_map = semantic_details
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let selected_path = selected
                        .selected()
                        .and_then(|index| results.get(index))
                        .map(|result| result.path.as_str());
                    let has_hit = selected_has_semantic_hit(&semantic_map, selected_path);
                    detail_height(frame.area().height, has_hit)
                };
                let top_args = TopPanelArgs {
                    query,
                    root_label: &root_label,
                    backend_label,
                    result_count: results.len(),
                    watch_enabled,
                    watch_errors: live_index.as_ref().map_or(0, LiveIndex::write_errors),
                    mode,
                    sort,
                    reverse,
                    filters: &filters,
                    theme,
                    status: status.as_str(),
                };
                let show_banner = update_status.is_some();
                let all_chunks = if show_banner {
                    Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(1),
                            Constraint::Length(top_chrome_height(&top_args)),
                            Constraint::Min(6),
                            Constraint::Length(detail_rows),
                        ])
                        .split(frame.area())
                } else {
                    Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(top_chrome_height(&top_args)),
                            Constraint::Min(6),
                            Constraint::Length(detail_rows),
                        ])
                        .split(frame.area())
                };
                let offset = if show_banner { 1 } else { 0 };
                let chunks = &all_chunks[offset..];

                if show_banner {
                    if let Some(ref s) = update_status {
                        let banner_text = format!(
                            "\u{2728} lctr {} available, run `{}`",
                            s.latest, s.update_cmd
                        );
                        let banner = Paragraph::new(banner_text)
                            .style(Style::default().fg(theme.warn).add_modifier(Modifier::BOLD));
                        frame.render_widget(banner, all_chunks[0]);
                    }
                }

                // Compact chrome: 1-row header band + bordered search bar (focal) +
                // one status line + one controls line.
                // Full key help lives in the `?` overlay.
                let top_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Length(3),
                        Constraint::Length(1),
                        Constraint::Length(1),
                    ])
                    .split(chunks[0]);

                // Header band: wordmark left, root + backend right.
                let (wordmark, right_label) = header_segments(&root_label, backend_label);
                let band_width = top_chunks[0].width as usize;
                let right_len = right_label.len();
                let left_len = wordmark.len() + 2; // " lctr " padding
                let padding = if band_width > left_len + right_len + 2 {
                    " ".repeat(band_width - left_len - right_len)
                } else {
                    " ".to_string()
                };
                let band_line = Line::from(vec![
                    Span::styled(
                        format!(" {wordmark} "),
                        Style::default()
                            .fg(theme.accent)
                            .bg(theme.panel_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(padding, Style::default().bg(theme.panel_bg).fg(theme.muted)),
                    Span::styled(
                        right_label,
                        Style::default().fg(theme.muted).bg(theme.panel_bg),
                    ),
                ]);
                frame.render_widget(Paragraph::new(band_line), top_chunks[0]);

                let search_focused = focus == Focus::Search;
                let search_border_style = if search_focused {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.muted)
                };
                let search_panel = Paragraph::new(search_bar_line(&top_args)).block(
                    Block::default()
                        .title("search")
                        .title_style(
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        )
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(search_border_style)
                        .style(Style::default().bg(theme.panel_bg)),
                );
                frame.render_widget(search_panel, top_chunks[1]);
                if search_focused {
                    frame.set_cursor_position(Position {
                        x: top_chunks[1].x + 1 + input.cursor_column() as u16,
                        y: top_chunks[1].y + 1,
                    });
                }

                frame.render_widget(Paragraph::new(top_status_line(&top_args)), top_chunks[2]);
                frame.render_widget(Paragraph::new(top_controls_line(&top_args)), top_chunks[3]);

                // Split the results region into table + preview when wide enough and
                // a preview is available; otherwise the table takes the full width.
                let (results_area, preview_area) = if preview_enabled
                    && preview_state.is_some()
                    && chunks[1].width >= PREVIEW_MIN_WIDTH
                {
                    let parts = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .split(chunks[1]);
                    (parts[0], Some(parts[1]))
                } else {
                    (chunks[1], None)
                };

                if search_state.should_render_results(query) {
                    let capacity = results_area.height.saturating_sub(3).max(1) as usize;
                    let (next_viewport_start, viewport_end) = viewport_range(
                        results.len(),
                        selected.selected(),
                        capacity,
                        viewport_start,
                    );
                    viewport_start = next_viewport_start;
                    if cache_stale_for_viewport(
                        &row_cache,
                        input.as_str(),
                        mode,
                        results_stamp,
                        viewport_start,
                        viewport_end,
                    ) {
                        row_cache = rebuild_viewport_row_cache(
                            &results,
                            input.as_str(),
                            mode,
                            results_stamp,
                            viewport_start,
                            viewport_end,
                        );
                    }
                    let rows = results[viewport_start..viewport_end]
                        .iter()
                        .zip(row_cache.rows.iter())
                        .map(|(result, row_data)| {
                            result_row(result, backend_label, mode, &theme, row_data, icons_enabled)
                        })
                        .collect::<Vec<_>>();
                    let table = Table::new(
                        rows,
                        [
                            Constraint::Length(2),
                            Constraint::Percentage(24),
                            Constraint::Length(10),
                            Constraint::Length(10),
                            Constraint::Length(17),
                            Constraint::Length(8),
                            Constraint::Length(9),
                            Constraint::Min(18),
                        ],
                    )
                    .header(
                        Row::new([
                            Cell::from(""),
                            Cell::from("name"),
                            Cell::from("kind"),
                            Cell::from("size"),
                            Cell::from("modified"),
                            Cell::from("source"),
                            Cell::from("match"),
                            Cell::from("path"),
                        ])
                        .style(Style::default().fg(theme.muted)),
                    )
                    .block(
                        Block::default()
                            .title(match &loading_query {
                                Some(active) if active == query => {
                                    format!("{} searching {active}", spinner_glyph(spinner_frame))
                                }
                                _ => format!("results ({})", format_count(results.len())),
                            })
                            .title_style(Style::default().fg(theme.ok).add_modifier(Modifier::BOLD))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(if focus == Focus::Results {
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme.muted)
                            })
                            .style(Style::default().bg(theme.panel_bg)),
                    )
                    .row_highlight_style(
                        Style::default()
                            .bg(theme.selected_bg)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("\u{258c} ");
                    let mut viewport_state = TableState::default();
                    if let Some(index) = selected.selected() {
                        if index >= viewport_start && index < viewport_end {
                            viewport_state.select(Some(index - viewport_start));
                        }
                    }
                    frame.render_stateful_widget(table, results_area, &mut viewport_state);
                } else if !query.is_empty() {
                    let hint = Paragraph::new(if should_show_results(query) {
                        match backend_label {
                            "indexed" => "Indexed results update while typing",
                            "hybrid" => {
                                "Indexed results update while typing. Tab or Down for results"
                            }
                            _ => "Press Enter to search live filenames",
                        }
                    } else {
                        "Type at least 2 letters to search"
                    })
                    .style(Style::default().fg(theme.muted))
                    .block(
                        Block::default()
                            .title("waiting")
                            .title_style(Style::default().fg(theme.ok))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(theme.muted))
                            .style(Style::default().bg(theme.panel_bg)),
                    );
                    frame.render_widget(hint, results_area);
                } else {
                    let card_lines = empty_state_lines(&root_label)
                        .into_iter()
                        .map(|s| Line::from(Span::styled(s, Style::default().fg(theme.muted))))
                        .collect::<Vec<_>>();
                    let card = Paragraph::new(card_lines).block(
                        Block::default()
                            .title(" search ")
                            .title_style(
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(theme.accent))
                            .style(Style::default().bg(theme.panel_bg)),
                    );
                    frame.render_widget(card, results_area);
                }

                if let Some(area) = preview_area {
                    if let Some(state) = preview_state.as_mut() {
                        render_preview(frame, area, state, &theme);
                    }
                }

                if has_detail {
                    let hint_line = Line::from(Span::styled(
                        footer_with_position(focus, selected.selected(), results.len()),
                        Style::default().fg(theme.muted).bg(theme.bg),
                    ));
                    let mut detail_lines = vec![hint_line];
                    let detail_text = selected_detail(&selected, &results, &theme);
                    for line in detail_text.lines {
                        detail_lines.push(line);
                    }
                    let semantic_details = search_worker.semantic_details();
                    let semantic_map = semantic_details
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    if let Some(result) = selected.selected().and_then(|index| results.get(index)) {
                        detail_lines.extend(semantic_detail_lines(
                            &result.path,
                            &semantic_map,
                            &theme,
                            usize::from(chunks[2].width),
                        ));
                    }
                    let detail = Paragraph::new(detail_lines)
                        .style(Style::default().fg(theme.muted))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::TOP)
                                .border_style(Style::default().fg(theme.muted)),
                        );
                    frame.render_widget(detail, chunks[2]);
                }

                if show_help {
                    render_help_overlay(frame, &theme);
                }
            })?;
            needs_draw = false;
        }

        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        let finder_pending = finder_reveal.reveal_pending();
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64", not(test))))]
        let finder_pending = false;
        let poll_timeout = if APPLE_SILICON && (loading_query.is_some() || finder_pending) {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        let Some(event) = read_normalized_input(
            &mut input_normalizer,
            &mut input_trace,
            focus,
            &input,
            &selected,
            poll_timeout,
        )?
        else {
            continue;
        };
        if matches!(&event, Event::Resize(_, _)) {
            needs_draw = true;
            continue;
        }
        if let Event::Mouse(mouse) = event {
            if APPLE_SILICON && focus == Focus::Results {
                let initial_delta = scroll_delta(mouse.kind);
                if initial_delta != 0 {
                    let changed = drain_apple_scroll_burst(
                        &mut input_normalizer,
                        &mut input_trace,
                        focus,
                        &input,
                        &mut selected,
                        results.len(),
                        initial_delta,
                    )?;
                    needs_draw |= changed;
                }
            }
            continue;
        }
        if let Event::Key(key) = event {
            if is_quit_key(key) {
                break;
            }
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // Help overlay is modal: any key dismisses it.
            if show_help {
                show_help = false;
                continue;
            }
            let navigation_code =
                matches!(key.code, KeyCode::Up | KeyCode::Down).then_some(key.code);
            if !APPLE_SILICON {
                let is_vertical_navigation = navigation_code.is_some();
                if focus != Focus::Results || !is_vertical_navigation {
                    navigation_burst.reset();
                } else if !navigation_burst.accepts(key.code, Instant::now()) {
                    if let Some(code) = navigation_code {
                        drain_same_direction_input(
                            &mut input_normalizer,
                            &mut input_trace,
                            focus,
                            &input,
                            &selected,
                            code,
                        )?;
                    }
                    continue;
                }
            }
            needs_draw = true;
            match focus {
                Focus::Search => match key.code {
                    KeyCode::Backspace if input.backspace() => {
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        if backend_label == "live" {
                            clear_results(&mut results, &mut result_paths, &mut selected);
                        }
                        normalize_selection(&mut selected, results.len());
                        status = edit_status(backend_label);
                    }
                    KeyCode::Char(ch) => {
                        input.insert(ch);
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        if backend_label == "live" {
                            clear_results(&mut results, &mut result_paths, &mut selected);
                        }
                        normalize_selection(&mut selected, results.len());
                        status = edit_status(backend_label);
                    }
                    KeyCode::Left => input.move_left(),
                    KeyCode::Right => input.move_right(),
                    KeyCode::Tab | KeyCode::Down if !results.is_empty() => {
                        focus = Focus::Results;
                        normalize_selection(&mut selected, results.len());
                    }
                    KeyCode::Up => move_selection(&mut selected, results.len(), -1),
                    KeyCode::PageDown => move_selection(&mut selected, results.len(), 10),
                    KeyCode::PageUp => move_selection(&mut selected, results.len(), -10),
                    KeyCode::F(1) => {
                        show_help = true;
                    }
                    KeyCode::Enter => {
                        let query = input.as_str();
                        let live_backfill =
                            backend_label != "indexed" && !search_state.live_complete_for(query);
                        if selected_path(&selected, &results).is_some() {
                            if let Some(path) = selected_path(&selected, &results) {
                                match open_file(Path::new(path)) {
                                    Ok(()) => {
                                        record_access_if_indexed(&watch_target, path);
                                        status = format!("opened {path}");
                                    }
                                    Err(error) => {
                                        status = format!("open failed: {error:#}");
                                    }
                                }
                            }
                        } else if live_backfill && should_show_results(query) {
                            let options = tui_search_options(query)
                                .with_mode(mode)
                                .with_sort(sort)
                                .with_reverse(reverse)
                                .with_filters(filters.clone());
                            let request_id = search_worker.submit(SearchRequest {
                                options: options.clone(),
                            })?;
                            search_state.mark_submitted_with_id(options, live_backfill, request_id);
                            loading_query = Some(query.to_string());
                            if backend_label == "live" || live_backfill {
                                clear_results(&mut results, &mut result_paths, &mut selected);
                            }
                            normalize_selection(&mut selected, results.len());
                            status = if live_backfill {
                                format!("searching live filenames for {query}")
                            } else {
                                format!("searching {backend_label} filenames for {query}")
                            };
                        } else {
                            status = "Type to search, then select a result".to_string();
                        }
                    }
                    _ => {}
                },
                Focus::Results => match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        move_selection(&mut selected, results.len(), 1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        move_selection(&mut selected, results.len(), -1);
                    }
                    KeyCode::Char('g') if !results.is_empty() => {
                        selected.select(Some(0));
                    }
                    KeyCode::Char('G') if !results.is_empty() => {
                        selected.select(Some(results.len() - 1));
                    }
                    KeyCode::PageDown => move_selection(&mut selected, results.len(), 10),
                    KeyCode::PageUp => move_selection(&mut selected, results.len(), -10),
                    KeyCode::Char('o') | KeyCode::Enter => {
                        if let Some(path) = selected_path(&selected, &results) {
                            match open_file(Path::new(path)) {
                                Ok(()) => {
                                    record_access_if_indexed(&watch_target, path);
                                    status = format!("opened {path}");
                                }
                                Err(error) => {
                                    status = format!("open failed: {error:#}");
                                }
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Some(path) = selected_path(&selected, &results) {
                            #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
                            {
                                match finder_reveal.request_reveal(Path::new(path)) {
                                    Ok(()) => status = format!("revealing {path}"),
                                    Err(error) => status = format!("reveal failed: {error:#}"),
                                }
                            }
                            #[cfg(not(all(
                                target_os = "macos",
                                target_arch = "aarch64",
                                not(test)
                            )))]
                            match finder_reveal.reveal(Path::new(path)) {
                                Ok(()) => {
                                    record_access_if_indexed(&watch_target, path);
                                    status = format!("revealed {path}");
                                }
                                Err(error) => {
                                    status = format!("reveal failed: {error:#}");
                                }
                            }
                        }
                    }
                    KeyCode::Char('y') => {
                        if let Some(path) = selected_path(&selected, &results) {
                            match copy_path(Path::new(path)) {
                                Ok(()) => {
                                    record_access_if_indexed(&watch_target, path);
                                    status = format!("copied {path}");
                                }
                                Err(error) => {
                                    status = format!("copy failed: {error:#}");
                                }
                            }
                        }
                    }
                    KeyCode::Char('m') => {
                        mode = mode.next_available(semantic_available);
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        status = format!("mode: {}", mode.label());
                        if mode == QueryMode::Semantic && !semantic_loaded {
                            status =
                                "mode: semantic (loads the model on the first search)".to_string();
                        }
                    }
                    KeyCode::Char('f') => {
                        filters = cycle_kind_filter(filters);
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        status = "type filter changed".to_string();
                    }
                    KeyCode::Char('s') => {
                        sort = sort.next();
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        status = format!("sort: {}", sort.label());
                    }
                    KeyCode::Char('S') => {
                        reverse = toggle_sort_order(reverse);
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        status = format!("sort order: {}", sort_label(sort, reverse));
                    }
                    KeyCode::Char('t') => {
                        theme = theme.cycle();
                        if let Err(error) = theme.persist() {
                            status = error.to_string();
                        } else {
                            status = format!("theme: {}", theme.name.label());
                        }
                    }
                    KeyCode::Char('w') => {
                        if live_index.is_some() {
                            live_index = None;
                            watch_enabled = false;
                            status = "live watch off".to_string();
                        } else if let Some((root, db_path)) = watch_target.as_ref() {
                            match LiveIndex::spawn_background(root.clone(), db_path.clone()).ok() {
                                Some(live) => {
                                    live_generation = live.generation();
                                    live_index = Some(live);
                                    watch_enabled = true;
                                    status =
                                        "live watch on: index updates as files change".to_string();
                                }
                                None => {
                                    status = "live watch unavailable for this index".to_string();
                                }
                            }
                        } else {
                            status = "live watch not available in live search mode".to_string();
                        }
                    }
                    KeyCode::Char('?') | KeyCode::F(1) => {
                        show_help = true;
                    }
                    KeyCode::Char('/') | KeyCode::Tab => {
                        focus = Focus::Search;
                    }
                    KeyCode::Backspace => {
                        focus = Focus::Search;
                        if input.backspace() {
                            search_state.mark_dirty();
                            last_edit = Instant::now();
                            if backend_label == "live" {
                                clear_results(&mut results, &mut result_paths, &mut selected);
                            }
                            normalize_selection(&mut selected, results.len());
                            status = edit_status(backend_label);
                        }
                    }
                    _ => {}
                },
            }
            if !APPLE_SILICON {
                if let Some(code) = navigation_code {
                    drain_same_direction_input(
                        &mut input_normalizer,
                        &mut input_trace,
                        focus,
                        &input,
                        &selected,
                        code,
                    )?;
                }
            }
        }
    }

    Ok(finder_reveal)
}

#[cfg(test)]
fn search_for_tui(db: &Database, options: &SearchOptions) -> Result<Vec<SearchResult>> {
    if !should_show_results(&options.query) {
        return Ok(Vec::new());
    }
    db.search_with_options(options)
}

fn open_search_database(path: &Path) -> Result<Database> {
    Database::open(path)
        .or_else(|_| Database::open_readonly(path))
        .map(Database::with_search_path_verification)
}

fn open_search_reader(path: &Path) -> Result<Database> {
    // Backend selection already upgrades the schema. Queries only need a reader:
    // no migration writes, and a fresh handle sees a staged index replacement.
    Database::open_readonly(path).map(Database::with_search_path_verification)
}

#[cfg(test)]
fn search_hybrid(db: &Database, root: &Path, options: &SearchOptions) -> Result<Vec<SearchResult>> {
    let indexed = search_for_tui(db, options)?;
    let live = search_live_with_options(root, options)?;
    Ok(merge_results(indexed, live, options.limit))
}

/// Latest semantic hits keyed by path: (similarity, snippet). Shared with the semantic thread.
type SemanticDetails = Arc<Mutex<HashMap<String, (f32, String)>>>;

struct SearchWorker {
    tx: Sender<QueuedSearchRequest>,
    rx: Receiver<SearchUpdate>,
    next_request_id: u64,
    result_tx: Sender<SearchUpdate>,
    semantic_index: Option<PathBuf>,
    semantic_tx: Option<Sender<QueuedSearchRequest>>,
    semantic_details: SemanticDetails,
}

impl SearchWorker {
    fn spawn(search_backend: SearchBackend) -> Result<Self> {
        let (query_tx, query_rx) = mpsc::channel::<QueuedSearchRequest>();
        let (result_tx, result_rx) = mpsc::channel::<SearchUpdate>();
        let semantic_index = search_backend.watch_target().map(|(_, db_path)| db_path);
        let semantic_result_tx = result_tx.clone();

        thread::spawn(move || search_worker_loop(search_backend, query_rx, result_tx));

        Ok(Self {
            tx: query_tx,
            rx: result_rx,
            next_request_id: 1,
            result_tx: semantic_result_tx,
            semantic_index,
            semantic_tx: None,
            semantic_details: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn submit(&mut self, request: SearchRequest) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        if request_is_semantic(&request.options) {
            self.submit_semantic(QueuedSearchRequest {
                request_id,
                request,
            })?;
            return Ok(request_id);
        }
        self.tx
            .send(QueuedSearchRequest {
                request_id,
                request,
            })
            .context("send search request")?;
        Ok(request_id)
    }

    /// Sends a semantic request to the semantic thread, starting that thread on first use.
    fn submit_semantic(&mut self, request: QueuedSearchRequest) -> Result<()> {
        if self.semantic_tx.is_none() {
            let index = self
                .semantic_index
                .clone()
                .context("semantic search needs an index")?;
            let (tx, rx) = mpsc::channel::<QueuedSearchRequest>();
            let result_tx = self.result_tx.clone();
            let details = Arc::clone(&self.semantic_details);
            thread::spawn(move || semantic_worker_loop(index, rx, result_tx, details));
            self.semantic_tx = Some(tx);
        }
        match self.semantic_tx.as_ref() {
            Some(tx) => tx.send(request).context("send semantic search request"),
            None => Err(anyhow::anyhow!("semantic search worker is not running")),
        }
    }

    /// Shared map of the latest semantic hits, read by the detail panel.
    fn semantic_details(&self) -> SemanticDetails {
        Arc::clone(&self.semantic_details)
    }

    fn try_recv(&mut self) -> Option<SearchResponse> {
        self.rx.try_recv().ok()
    }
}

#[derive(Debug, Clone)]
struct SearchRequest {
    options: SearchOptions,
}

struct QueuedSearchRequest {
    request_id: u64,
    request: SearchRequest,
}

enum SearchUpdate {
    Reset {
        request_id: u64,
        options: SearchOptions,
    },
    Append {
        request_id: u64,
        options: SearchOptions,
        results: Vec<SearchResult>,
        count: usize,
    },
    Complete {
        request_id: u64,
        options: SearchOptions,
        count: usize,
        final_results: Option<Vec<SearchResult>>,
        live_backfill: bool,
    },
    Error {
        request_id: u64,
        options: SearchOptions,
        error: String,
    },
}

type SearchResponse = SearchUpdate;

fn search_worker_loop(
    search_backend: SearchBackend,
    query_rx: Receiver<QueuedSearchRequest>,
    result_tx: Sender<SearchUpdate>,
) {
    match search_backend {
        SearchBackend::Indexed { db_path, .. } => {
            while let Ok(request) = query_rx.recv() {
                let mut request = latest_queued_request(request, &query_rx);
                loop {
                    let request_id = request.request_id;
                    let options = request.request.options.clone();
                    if result_tx
                        .send(SearchUpdate::Reset {
                            request_id,
                            options: options.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }

                    let mut next_request = None;
                    let outcome = open_search_reader(&db_path).and_then(|db| {
                        db.search_streaming_with_options(
                            &options,
                            || {
                                while let Ok(newer_request) = query_rx.try_recv() {
                                    next_request = Some(newer_request);
                                }
                                next_request.is_some()
                            },
                            |batch| {
                                result_tx
                                    .send(SearchUpdate::Append {
                                        request_id,
                                        options: options.clone(),
                                        results: batch.results,
                                        count: batch.count,
                                    })
                                    .context("send indexed search batch")
                            },
                        )
                    });

                    match outcome {
                        Ok(SearchStreamStatus::Complete { count }) => {
                            if result_tx
                                .send(SearchUpdate::Complete {
                                    request_id,
                                    options,
                                    count,
                                    final_results: None,
                                    live_backfill: false,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(SearchStreamStatus::Cancelled) => {
                            if let Some(newer_request) = next_request.take() {
                                request = newer_request;
                                continue;
                            }
                        }
                        Err(error) => {
                            if result_tx
                                .send(SearchUpdate::Error {
                                    request_id,
                                    options,
                                    error: error.to_string(),
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    break;
                }
            }
        }
        SearchBackend::Live { root } => {
            while let Ok(request) = query_rx.recv() {
                let mut request = latest_queued_request(request, &query_rx);
                loop {
                    let request_id = request.request_id;
                    let options = request.request.options.clone();
                    if result_tx
                        .send(SearchUpdate::Reset {
                            request_id,
                            options: options.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }

                    let mut next_request = None;
                    let outcome = search_live_streaming_batches_with_options(
                        &root,
                        &options,
                        || {
                            while let Ok(newer_request) = query_rx.try_recv() {
                                next_request = Some(newer_request);
                            }
                            next_request.is_some()
                        },
                        |batch| {
                            result_tx
                                .send(SearchUpdate::Append {
                                    request_id,
                                    options: options.clone(),
                                    results: batch.results,
                                    count: batch.count,
                                })
                                .context("send live search batch")
                        },
                    );

                    match outcome {
                        Ok((LiveSearchStatus::Complete, final_results)) => {
                            let count = final_results.len();
                            if result_tx
                                .send(SearchUpdate::Complete {
                                    request_id,
                                    options,
                                    count,
                                    final_results: Some(final_results),
                                    live_backfill: true,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok((LiveSearchStatus::Cancelled, _)) => {
                            if let Some(newer_request) = next_request.take() {
                                request = newer_request;
                                continue;
                            }
                        }
                        Err(error) => {
                            if result_tx
                                .send(SearchUpdate::Error {
                                    request_id,
                                    options,
                                    error: error.to_string(),
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    break;
                }
            }
        }
        SearchBackend::Hybrid { db_path, root } => {
            while let Ok(request) = query_rx.recv() {
                let mut request = latest_queued_request(request, &query_rx);
                loop {
                    let request_id = request.request_id;
                    let options = request.request.options.clone();
                    if result_tx
                        .send(SearchUpdate::Reset {
                            request_id,
                            options: options.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }

                    let mut next_request = None;
                    let mut seen_paths = HashSet::new();
                    let mut final_results = Vec::new();
                    let indexed_outcome = open_search_reader(&db_path).and_then(|db| {
                        db.search_streaming_with_options(
                            &options,
                            || {
                                while let Ok(newer_request) = query_rx.try_recv() {
                                    next_request = Some(newer_request);
                                }
                                next_request.is_some()
                            },
                            |batch| {
                                for result in &batch.results {
                                    seen_paths.insert(result.path.clone());
                                    final_results.push(result.clone());
                                }
                                result_tx
                                    .send(SearchUpdate::Append {
                                        request_id,
                                        options: options.clone(),
                                        results: batch.results,
                                        count: batch.count,
                                    })
                                    .context("send hybrid indexed batch")
                            },
                        )
                    });

                    match indexed_outcome {
                        Ok(SearchStreamStatus::Cancelled) => {
                            if let Some(newer_request) = next_request.take() {
                                request = newer_request;
                                continue;
                            }
                            break;
                        }
                        Err(error) => {
                            if result_tx
                                .send(SearchUpdate::Error {
                                    request_id,
                                    options,
                                    error: error.to_string(),
                                })
                                .is_err()
                            {
                                return;
                            }
                            break;
                        }
                        Ok(SearchStreamStatus::Complete { .. }) => {}
                    }

                    let mut live_pending = Vec::with_capacity(SEARCH_BATCH_SIZE);
                    let mut count = final_results.len();
                    let live_outcome = search_live_streaming_batches_with_options(
                        &root,
                        &options,
                        || {
                            while let Ok(newer_request) = query_rx.try_recv() {
                                next_request = Some(newer_request);
                            }
                            next_request.is_some()
                        },
                        |batch| {
                            for result in batch.results {
                                if seen_paths.insert(result.path.clone()) {
                                    count += 1;
                                    live_pending.push(result.clone());
                                    final_results.push(result);
                                    if live_pending.len() == SEARCH_BATCH_SIZE {
                                        let delta = std::mem::replace(
                                            &mut live_pending,
                                            Vec::with_capacity(SEARCH_BATCH_SIZE),
                                        );
                                        result_tx
                                            .send(SearchUpdate::Append {
                                                request_id,
                                                options: options.clone(),
                                                results: delta,
                                                count,
                                            })
                                            .context("send hybrid live batch")?;
                                    }
                                }
                            }
                            Ok(())
                        },
                    );

                    match live_outcome {
                        Ok((LiveSearchStatus::Cancelled, _)) => {
                            if let Some(newer_request) = next_request.take() {
                                request = newer_request;
                                continue;
                            }
                        }
                        Ok((LiveSearchStatus::Complete, _)) => {
                            if !live_pending.is_empty()
                                && result_tx
                                    .send(SearchUpdate::Append {
                                        request_id,
                                        options: options.clone(),
                                        results: live_pending,
                                        count,
                                    })
                                    .is_err()
                            {
                                return;
                            }

                            if let Ok(compiled) =
                                CompiledQuery::compile(options.mode, &options.query)
                            {
                                let mut scorer = QueryScorer::new();
                                sort_results_compiled(
                                    &mut final_results,
                                    &options,
                                    &compiled,
                                    &mut scorer,
                                    &HashMap::new(),
                                );
                            }
                            let count = final_results.len();
                            if result_tx
                                .send(SearchUpdate::Complete {
                                    request_id,
                                    options,
                                    count,
                                    final_results: Some(final_results),
                                    live_backfill: true,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => {
                            if result_tx
                                .send(SearchUpdate::Error {
                                    request_id,
                                    options,
                                    error: error.to_string(),
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
}

/// True for requests that go to the semantic thread instead of the filename worker.
fn request_is_semantic(options: &SearchOptions) -> bool {
    options.mode == QueryMode::Semantic
}

/// Map the detail panel reads: path to (similarity, snippet).
fn semantic_detail_map(hits: &[SemanticHit]) -> HashMap<String, (f32, String)> {
    hits.iter()
        .map(|hit| (hit.result.path.clone(), (hit.score, hit.snippet.clone())))
        .collect()
}

/// Two detail lines for a file that has a semantic hit. Nothing for other files.
fn semantic_detail_lines(
    path: &str,
    details: &HashMap<String, (f32, String)>,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let Some((score, snippet)) = details.get(path) else {
        return Vec::new();
    };
    let label = "match ";
    let room = width.saturating_sub(label.chars().count()).max(1);
    let snippet = fit_one_line(snippet, room);
    vec![
        Line::from(vec![
            Span::styled("similarity ", Style::default().fg(theme.muted)),
            Span::styled(format!("{score:.2}"), Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled(label, Style::default().fg(theme.muted)),
            Span::styled(snippet, Style::default().fg(theme.text)),
        ]),
    ]
}

/// Rows for the bottom detail pane. Filename results keep the original 4 rows. A selected semantic
/// hit needs 2 more for its similarity and match lines. Terminals under 20 rows hide the pane.
fn detail_height(frame_height: u16, has_semantic_hit: bool) -> u16 {
    if frame_height < 20 {
        0
    } else if has_semantic_hit {
        6
    } else {
        4
    }
}

/// True when the selected path has a semantic hit entry.
fn selected_has_semantic_hit(
    details: &HashMap<String, (f32, String)>,
    selected_path: Option<&str>,
) -> bool {
    selected_path.is_some_and(|path| details.contains_key(path))
}

/// Collapses whitespace and cuts to `max_chars` characters, ending with an ellipsis when cut.
fn fit_one_line(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = collapsed.chars().take(keep).collect();
    out.push('\u{2026}');
    out
}

/// Owns the embedder for the life of the semantic thread. The index and model load on the first
/// request. Queued requests are coalesced to the newest, as the filename worker does.
fn semantic_worker_loop(
    index_path: PathBuf,
    query_rx: Receiver<QueuedSearchRequest>,
    result_tx: Sender<SearchUpdate>,
    details: SemanticDetails,
) {
    semantic_worker_with(query_rx, result_tx, |pending, query_rx, result_tx| {
        serve_semantic_requests(&index_path, pending, query_rx, result_tx, &details)
    });
}

fn semantic_worker_with(
    query_rx: Receiver<QueuedSearchRequest>,
    result_tx: Sender<SearchUpdate>,
    serve: impl FnOnce(
        &mut Option<QueuedSearchRequest>,
        &Receiver<QueuedSearchRequest>,
        &Sender<SearchUpdate>,
    ) -> Result<()>,
) {
    let mut pending = match query_rx.recv() {
        Ok(request) => Some(request),
        Err(_) => return,
    };
    // Unwind and drop the model session on panic. Keep the active request so it
    // receives an Error update and the UI can stop its loading indicator.
    let outcome = crate::panic_guard::catch_quiet(std::panic::AssertUnwindSafe(|| {
        serve(&mut pending, &query_rx, &result_tx)
    }))
    .unwrap_or_else(|_| {
        Err(anyhow::anyhow!(
            "semantic search failed internally; restart lctr to retry"
        ))
    });
    if let Err(error) = outcome {
        // A failed model session is not reused. Filename search remains available.
        let message = format!("{error:#}");
        while let Some(request) = pending.take() {
            let request = latest_queued_request(request, &query_rx);
            let _ = result_tx.send(SearchUpdate::Error {
                request_id: request.request_id,
                options: request.request.options,
                error: message.clone(),
            });
            pending = query_rx.recv().ok();
        }
    }
}

fn serve_semantic_requests(
    index_path: &Path,
    pending: &mut Option<QueuedSearchRequest>,
    query_rx: &Receiver<QueuedSearchRequest>,
    result_tx: &Sender<SearchUpdate>,
    details: &SemanticDetails,
) -> Result<()> {
    // Open the index before the model, so a missing index never loads the model.
    let mut searcher = SemanticSearcher::open(index_path)?
        .ok_or_else(|| anyhow::anyhow!("no embeddings yet; run lctr scan <dir> --em"))?;
    let config = embedder_config_for_search()?;
    with_embedder(&config, |embedder| {
        while let Some(request) = pending.take() {
            let request = pending.insert(latest_queued_request(request, query_rx));
            run_semantic_request(&mut searcher, embedder, request, result_tx, details);
            *pending = query_rx.recv().ok();
        }
        Ok(())
    })
}

fn run_semantic_request(
    searcher: &mut SemanticSearcher,
    embedder: &mut dyn Embedder,
    request: &QueuedSearchRequest,
    result_tx: &Sender<SearchUpdate>,
    details: &SemanticDetails,
) {
    let request_id = request.request_id;
    let options = request.request.options.clone();
    if let Ok(mut shared) = details.lock() {
        shared.clear();
    }
    let _ = result_tx.send(SearchUpdate::Reset {
        request_id,
        options: options.clone(),
    });
    match searcher.search(embedder, &options) {
        Ok(hits) => {
            if let Ok(mut shared) = details.lock() {
                *shared = semantic_detail_map(&hits);
            }
            let mut results: Vec<SearchResult> = hits.into_iter().map(|hit| hit.result).collect();
            // Relevance order is the score order the searcher returned. Other sorts reorder it.
            if options.sort != SortField::Relevance {
                crate::db::sort_results(&mut results, &options);
            }
            let count = results.len();
            let _ = result_tx.send(SearchUpdate::Append {
                request_id,
                options: options.clone(),
                results,
                count,
            });
            let _ = result_tx.send(SearchUpdate::Complete {
                request_id,
                options,
                count,
                final_results: None,
                live_backfill: false,
            });
        }
        Err(error) => {
            let _ = result_tx.send(SearchUpdate::Error {
                request_id,
                options,
                error: format!("{error:#}"),
            });
        }
    }
}

fn latest_queued_request(
    mut request: QueuedSearchRequest,
    query_rx: &Receiver<QueuedSearchRequest>,
) -> QueuedSearchRequest {
    while let Ok(newer_request) = query_rx.try_recv() {
        request = newer_request;
    }
    request
}

#[cfg(test)]
fn merge_results(
    mut indexed: Vec<SearchResult>,
    live: Vec<SearchResult>,
    limit: usize,
) -> Vec<SearchResult> {
    let mut seen = indexed
        .iter()
        .map(|result| result.path.clone())
        .collect::<std::collections::HashSet<_>>();
    for result in live {
        if indexed.len() >= limit {
            break;
        }
        if seen.insert(result.path.clone()) {
            indexed.push(result);
        }
    }
    indexed
}

fn should_show_results(query: &str) -> bool {
    query.chars().filter(|ch| ch.is_alphanumeric()).count() >= 2
}

fn tui_search_options(query: &str) -> SearchOptions {
    SearchOptions::new(query).with_limit(TUI_RESULT_LIMIT)
}

#[derive(Debug, Clone, Default)]
struct SearchInput {
    text: String,
    cursor: usize,
}

impl SearchInput {
    fn as_str(&self) -> &str {
        self.text.as_str()
    }

    fn cursor_column(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    fn insert(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) -> bool {
        let Some(previous) = self.previous_cursor_boundary() else {
            return false;
        };
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
        true
    }

    fn move_left(&mut self) {
        if let Some(previous) = self.previous_cursor_boundary() {
            self.cursor = previous;
        }
    }

    fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
    }

    fn previous_cursor_boundary(&self) -> Option<usize> {
        if self.cursor == 0 {
            return None;
        }
        self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
    }
}

#[derive(Debug, Clone, Default)]
struct SearchState {
    dirty: bool,
    last_submitted: Option<SearchOptions>,
    last_live_query: Option<String>,
    active_request_id: Option<u64>,
}

impl SearchState {
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_live_query = None;
    }

    fn should_submit(&self, query: &str) -> bool {
        should_show_results(query)
            && (self.dirty
                || self
                    .last_submitted
                    .as_ref()
                    .is_none_or(|options| options.query != query))
    }

    fn should_auto_submit(&self, query: &str, backend_label: &str, elapsed: Duration) -> bool {
        (backend_label != "live" || self.last_submitted.is_some())
            && self.should_submit(query)
            && elapsed >= Duration::from_millis(150)
    }

    #[cfg(test)]
    fn mark_submitted(&mut self, options: SearchOptions, live_backfill: bool) {
        self.mark_submitted_with_id(options, live_backfill, 0);
    }

    fn mark_submitted_with_id(
        &mut self,
        options: SearchOptions,
        live_backfill: bool,
        request_id: u64,
    ) {
        if live_backfill {
            self.last_live_query = None;
        }
        self.last_submitted = Some(options);
        self.active_request_id = Some(request_id);
        self.dirty = false;
    }

    fn accepts_update(
        &self,
        request_id: u64,
        current_query: &str,
        response_options: &SearchOptions,
    ) -> bool {
        !self.dirty
            && current_query == response_options.query
            && self.active_request_id == Some(request_id)
            && self
                .last_submitted
                .as_ref()
                .is_some_and(|options| options == response_options)
    }

    fn should_render_results(&self, query: &str) -> bool {
        should_show_results(query)
            && self
                .last_submitted
                .as_ref()
                .is_some_and(|options| options.query == query)
    }

    fn mark_live_complete(&mut self, query: String) {
        self.last_live_query = Some(query);
    }

    fn live_complete_for(&self, query: &str) -> bool {
        self.last_live_query.as_deref() == Some(query)
    }
}

/// Minimum results-area width before the preview pane is shown; below this the
/// table takes the full width (responsive layout).
const PREVIEW_MIN_WIDTH: u16 = 100;
/// Cap on lines read into a text/PDF preview.
const PREVIEW_MAX_LINES: usize = 300;
/// How long the selection must hold still before its preview is built, so fast
/// scrolling doesn't decode every file (esp. images) it passes over.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(120);

/// Braille spinner frames for the results title while a search is in progress.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) fn spinner_glyph(frame: usize) -> &'static str {
    SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
}

enum PreviewRender {
    Lines(Vec<Line<'static>>),
    Image(Box<StatefulProtocol>),
}

/// A slow filesystem read or decoder must never own the keyboard thread.
/// Dropping the channels does not wait for an in-flight preview.
struct PreviewWorker {
    tx: Sender<String>,
    rx: Receiver<(String, Preview)>,
}

impl PreviewWorker {
    fn spawn() -> Self {
        Self::spawn_with(|path| preview::preview_for(Path::new(path), PREVIEW_MAX_LINES))
    }

    fn spawn_with(render: impl Fn(&str) -> Preview + Send + 'static) -> Self {
        let (tx, requests) = mpsc::channel::<String>();
        let (responses, rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(mut path) = requests.recv() {
                // Skip selections passed while the previous file was decoding.
                while let Ok(newer) = requests.try_recv() {
                    path = newer;
                }
                let preview = render(&path);
                if responses.send((path, preview)).is_err() {
                    break;
                }
            }
        });
        Self { tx, rx }
    }
}

/// Cached preview for the currently-selected path, so decode/highlight runs only
/// when the selection changes rather than every render tick.
struct PreviewState {
    meta: Vec<Line<'static>>,
    render: PreviewRender,
}

fn build_preview(picker: &mut Picker, preview: Preview) -> PreviewState {
    match preview {
        Preview::Text(lines) | Preview::Info(lines) => PreviewState {
            meta: Vec::new(),
            render: PreviewRender::Lines(lines),
        },
        Preview::Image { image, meta } => {
            let protocol = picker.new_resize_protocol(*image);
            PreviewState {
                meta,
                render: PreviewRender::Image(Box::new(protocol)),
            }
        }
    }
}

fn render_preview(frame: &mut Frame, area: Rect, state: &mut PreviewState, theme: &Theme) {
    let block = Block::default()
        .title("preview")
        .title_style(Style::default().fg(theme.accent))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.muted));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match &mut state.render {
        PreviewRender::Lines(lines) => {
            let paragraph = Paragraph::new(lines.clone()).style(Style::default().fg(theme.text));
            frame.render_widget(paragraph, inner);
        }
        PreviewRender::Image(protocol) => {
            let image_area = if !state.meta.is_empty() && inner.height > 2 {
                let meta_area = Rect { height: 1, ..inner };
                frame.render_widget(
                    Paragraph::new(state.meta.clone()).style(Style::default().fg(theme.muted)),
                    meta_area,
                );
                Rect {
                    y: inner.y + 1,
                    height: inner.height - 1,
                    ..inner
                }
            } else {
                inner
            };
            frame.render_stateful_widget(StatefulImage::new(), image_area, protocol.as_mut());
        }
    }
}

/// Centered rectangle covering `percent_x` × `percent_y` of `area`, for modals.
fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn render_help_overlay(frame: &mut Frame, theme: &Theme) {
    let area = popup_area(frame.area(), 60, 70);
    frame.render_widget(ratatui::widgets::Clear, area);

    let heading = |text: &'static str| {
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let entry = |keys: &'static str, desc: &'static str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<14}"), Style::default().fg(theme.ok)),
            Span::styled(desc.to_string(), Style::default().fg(theme.text)),
        ])
    };

    let lines = vec![
        heading("Search focus (default)"),
        entry("type", "filter results"),
        entry("Tab / \u{2193}", "move focus to results list"),
        entry("Enter", "open selected file"),
        entry("Esc / Ctrl+C", "quit from anywhere"),
        entry("F1 / ?", "this help"),
        Line::from(""),
        heading("Results focus (Tab or \u{2193} to enter)"),
        entry("j / k", "move selection"),
        entry("o / Enter", "open file"),
        entry("r", "reveal in Finder"),
        entry("y", "copy path"),
        entry("m", "cycle match mode"),
        entry("m (embedded)", "semantic mode, once lctr scan --em has run"),
        entry("f", "cycle type filter"),
        entry("s / S", "sort field / direction"),
        entry("t", "cycle theme"),
        entry("w", "toggle live watch"),
        entry("/ Tab", "return to search"),
        Line::from(""),
        Line::from(Span::styled(
            "  Esc / Ctrl+C quit; other keys close help",
            Style::default().fg(theme.muted),
        )),
    ];

    let help = Paragraph::new(lines).block(
        Block::default()
            .title(" help ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.panel_bg)),
    );
    frame.render_widget(help, area);
}

/// `LCTR_ICONS` override for Nerd Font icons: `Some(true/false)` when set,
/// `None` when unset (so config provides the default).
fn icons_env_override() -> Option<bool> {
    std::env::var("LCTR_ICONS")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Nerd Font glyph for a file kind / extension. Returns an empty string when the
/// kind is unknown so callers can prepend unconditionally.
fn icon_for(kind: &str, extension: Option<&str>) -> &'static str {
    match kind {
        "folder" => "\u{f07b}",
        "pdf" => "\u{f1c1}",
        "image" => "\u{f1c5}",
        "video" => "\u{f1c8}",
        "audio" => "\u{f1c7}",
        "archive" => "\u{f1c6}",
        "text" => match extension {
            Some("rs") => "\u{e7a8}",
            Some("py") => "\u{e606}",
            Some("js" | "ts" | "jsx" | "tsx") => "\u{e74e}",
            Some("md") => "\u{f48a}",
            Some("json" | "toml" | "yaml" | "yml") => "\u{e60b}",
            _ => "\u{f15c}",
        },
        _ => "\u{f15b}",
    }
}

/// Record a frecency access against the backing index, if one exists. Opening
/// the DB per action is cheap and these actions are rare. Errors are ignored:
/// Per-result display data computed once per query/mode/results change.
/// Storing highlight positions and pre-formatted strings avoids re-running
/// the matcher and `format!` calls inside every TUI draw call.
struct RowData {
    name_positions: Vec<usize>,
    path_positions: Vec<usize>,
    size_text: String,
    date_text: String,
    kind: String,
}

/// Cache for `result_row` inputs. Rebuilt only when (query, mode, results) change.
struct RowCache {
    query: String,
    mode: QueryMode,
    results_stamp: u64,
    viewport_start: usize,
    viewport_end: usize,
    rows: Vec<RowData>,
}

impl RowCache {
    fn empty() -> Self {
        Self {
            query: String::new(),
            mode: QueryMode::Contains,
            results_stamp: u64::MAX,
            viewport_start: 0,
            viewport_end: 0,
            rows: Vec::new(),
        }
    }
}

fn cache_stale(cache: &RowCache, query: &str, mode: QueryMode, stamp: u64) -> bool {
    cache.query != query || cache.mode != mode || cache.results_stamp != stamp
}

fn cache_stale_for_viewport(
    cache: &RowCache,
    query: &str,
    mode: QueryMode,
    stamp: u64,
    viewport_start: usize,
    viewport_end: usize,
) -> bool {
    cache_stale(cache, query, mode, stamp)
        || cache.viewport_start != viewport_start
        || cache.viewport_end != viewport_end
}

fn viewport_range(
    len: usize,
    selected: Option<usize>,
    capacity: usize,
    current_start: usize,
) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let capacity = capacity.max(1).min(len);
    let max_start = len - capacity;
    let mut start = current_start.min(max_start);
    if let Some(selected) = selected {
        let selected = selected.min(len - 1);
        if selected < start {
            start = selected;
        } else if selected >= start + capacity {
            start = selected.saturating_sub(capacity - 1).min(max_start);
        }
    }
    (start, (start + capacity).min(len))
}

#[cfg(test)]
fn rebuild_row_cache(
    results: &[SearchResult],
    query: &str,
    mode: QueryMode,
    stamp: u64,
) -> RowCache {
    rebuild_viewport_row_cache(results, query, mode, stamp, 0, results.len())
}

fn rebuild_viewport_row_cache(
    results: &[SearchResult],
    query: &str,
    mode: QueryMode,
    stamp: u64,
    viewport_start: usize,
    viewport_end: usize,
) -> RowCache {
    let compiled = CompiledQuery::compile(mode, query).ok();
    let mut scorer = QueryScorer::new();
    let viewport_start = viewport_start.min(results.len());
    let viewport_end = viewport_end.min(results.len()).max(viewport_start);
    let rows = results[viewport_start..viewport_end]
        .iter()
        .map(|result| {
            let (name_positions, path_positions) = if let Some(ref c) = compiled {
                (
                    c.match_positions(&mut scorer, &result.name),
                    c.match_positions(&mut scorer, &result.path),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            RowData {
                name_positions,
                path_positions,
                size_text: format_size(result.size_bytes),
                date_text: format_date(result.modified_at),
                kind: result.kind.clone(),
            }
        })
        .collect();
    RowCache {
        query: query.to_string(),
        mode,
        results_stamp: stamp,
        viewport_start,
        viewport_end,
        rows,
    }
}

/// frecency is a best-effort ranking hint, never a hard dependency.
fn record_access_if_indexed(watch_target: &Option<(PathBuf, PathBuf)>, path: &str) {
    if let Some((_, db_path)) = watch_target {
        if let Ok(db) = Database::open(db_path) {
            let _ = db.record_access(path);
        }
    }
}

fn result_row<'a>(
    result: &'a SearchResult,
    source: &'static str,
    mode: QueryMode,
    theme: &Theme,
    row: &RowData,
    icons: bool,
) -> Row<'a> {
    let base_name = Style::default().fg(theme.text);
    let base_path = Style::default().fg(theme.muted);
    let matched = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let mut name_line = highlight_line(&result.name, &row.name_positions, base_name, matched);
    if icons {
        let icon = icon_for(&result.kind, result.extension.as_deref());
        name_line.spans.insert(
            0,
            Span::styled(format!("{icon} "), Style::default().fg(theme.accent)),
        );
    }
    Row::new([
        Cell::from(""),
        Cell::from(name_line),
        Cell::from(row.kind.clone()).style(Style::default().fg(theme.accent)),
        Cell::from(row.size_text.clone()).style(Style::default().fg(theme.warn)),
        Cell::from(row.date_text.clone()).style(Style::default().fg(theme.ok)),
        Cell::from(source).style(Style::default().fg(source_color(source, theme))),
        Cell::from(mode.label()).style(Style::default().fg(theme.muted)),
        Cell::from(highlight_line(
            &result.path,
            &row.path_positions,
            base_path,
            matched,
        )),
    ])
}

/// Build a line where the char positions in `positions` (sorted ascending) use
/// `matched` styling and the rest use `base`, coalescing adjacent runs.
fn highlight_line(text: &str, positions: &[usize], base: Style, matched: Style) -> Line<'static> {
    if positions.is_empty() {
        return Line::from(Span::styled(text.to_string(), base));
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_matched = false;
    for (i, ch) in text.chars().enumerate() {
        let is_matched = positions.binary_search(&i).is_ok();
        if buf.is_empty() {
            buf_matched = is_matched;
        } else if is_matched != buf_matched {
            let style = if buf_matched { matched } else { base };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
            buf_matched = is_matched;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        let style = if buf_matched { matched } else { base };
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

struct TopPanelArgs<'a> {
    query: &'a str,
    root_label: &'a str,
    backend_label: &'static str,
    result_count: usize,
    watch_enabled: bool,
    watch_errors: u64,
    mode: QueryMode,
    sort: SortField,
    reverse: bool,
    filters: &'a SearchFilters,
    theme: Theme,
    status: &'a str,
}

fn top_status_line(args: &TopPanelArgs<'_>) -> Line<'static> {
    Line::from(vec![
        Span::styled("root ", Style::default().fg(args.theme.muted)),
        Span::styled(
            args.root_label.to_string(),
            Style::default().fg(args.theme.text),
        ),
        Span::raw("  "),
        Span::styled(
            args.backend_label,
            Style::default().fg(source_color(args.backend_label, &args.theme)),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} results", format_count(args.result_count)),
            Style::default().fg(args.theme.ok),
        ),
        Span::raw("  "),
        Span::styled(
            if args.watch_errors > 0 {
                format!("watch: {} write errors", args.watch_errors)
            } else if args.watch_enabled {
                "watch on".to_string()
            } else {
                "watch off".to_string()
            },
            Style::default().fg(if args.watch_errors > 0 {
                args.theme.warn
            } else if args.watch_enabled {
                args.theme.ok
            } else {
                args.theme.muted
            }),
        ),
        Span::raw("  "),
        Span::styled(
            args.status.to_string(),
            Style::default().fg(args.theme.muted),
        ),
    ])
}

fn search_bar_line(args: &TopPanelArgs<'_>) -> Line<'static> {
    if args.query.is_empty() {
        Line::from(Span::styled(
            "type query",
            Style::default().fg(args.theme.muted),
        ))
    } else {
        Line::from(Span::styled(
            args.query.to_string(),
            Style::default().fg(args.theme.text),
        ))
    }
}

/// One compact status line of the live mode/sort/filter/theme state, plus a
/// pointer to the `?` overlay for the full key reference.
fn top_controls_line(args: &TopPanelArgs<'_>) -> Line<'static> {
    let field = |label: &str, value: String| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{label}:"), Style::default().fg(args.theme.muted)),
            Span::styled(value, Style::default().fg(args.theme.accent)),
            Span::raw("  "),
        ]
    };
    let mut spans = Vec::new();
    spans.extend(field("mode", args.mode.label().to_string()));
    spans.extend(field("sort", sort_label(args.sort, args.reverse)));
    spans.extend(field("type", filter_label(args.filters)));
    spans.push(Span::styled(
        format!(
            "ext:{} size:{} date:{}",
            ext_filter_label(args.filters),
            size_filter_label(args.filters),
            date_filter_label(args.filters)
        ),
        Style::default().fg(args.theme.muted),
    ));
    spans.push(Span::raw("  "));
    spans.extend(field("theme", args.theme.name.label().to_string()));
    Line::from(spans)
}

fn top_chrome_height(_args: &TopPanelArgs<'_>) -> u16 {
    // header band (1) + bordered search bar (3) + status line (1) + controls line (1)
    6
}

/// Returns the left wordmark and right root/backend label for the header band.
/// Extracted so it can be unit-tested without rendering.
pub(crate) fn header_segments(root_label: &str, backend_label: &str) -> (String, String) {
    (
        "lctr".to_string(),
        format!("{root_label} \u{b7} {backend_label}"),
    )
}

fn toggle_sort_order(reverse: bool) -> bool {
    !reverse
}

fn sort_order_label(reverse: bool) -> &'static str {
    if reverse {
        "desc"
    } else {
        "asc"
    }
}

fn sort_label(sort: SortField, reverse: bool) -> String {
    format!("{} {}", sort.label(), sort_order_label(reverse))
}

fn filter_label(filters: &SearchFilters) -> String {
    format!(
        "type:{}",
        filters
            .kind
            .as_ref()
            .map(|kind| kind.as_str())
            .unwrap_or("all")
    )
}

fn ext_filter_label(filters: &SearchFilters) -> String {
    if filters.exts.is_empty() {
        "all".to_string()
    } else {
        filters.exts.join(",")
    }
}

#[cfg(test)]
fn format_result_summary(result: &SearchResult) -> String {
    format!(
        "{} {} {} Created {} Modified {} {}",
        result.name,
        result.kind,
        format_size(result.size_bytes),
        format_date(result.created_at),
        format_date(result.modified_at),
        result.path
    )
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let bytes = bytes as f64;

    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn format_date(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|date| date.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn normalize_selection(state: &mut TableState, len: usize) {
    if len == 0 {
        state.select(None);
        return;
    }

    let next = state.selected().unwrap_or(0).min(len - 1);
    state.select(Some(next));
}

fn clear_results(
    results: &mut Vec<SearchResult>,
    result_paths: &mut HashSet<String>,
    selected: &mut TableState,
) {
    results.clear();
    result_paths.clear();
    selected.select(None);
}

fn restore_selection_by_path(
    state: &mut TableState,
    results: &[SearchResult],
    anchor: Option<&str>,
) {
    if let Some(path) = anchor {
        if let Some(index) = results.iter().position(|result| result.path == path) {
            state.select(Some(index));
            return;
        }
    }
    normalize_selection(state, results.len());
}

fn format_count(count: usize) -> String {
    let raw = count.to_string();
    let first_group_len = raw.len() % 3;
    let first_group_len = if first_group_len == 0 {
        3
    } else {
        first_group_len
    };
    let mut formatted = String::with_capacity(raw.len() + raw.len() / 3);
    formatted.push_str(&raw[..first_group_len]);
    for chunk in raw.as_bytes()[first_group_len..].chunks(3) {
        formatted.push(',');
        formatted.push_str(std::str::from_utf8(chunk).expect("count is ASCII"));
    }
    formatted
}

fn move_selection(state: &mut TableState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }

    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, (len - 1) as isize);
    state.select(Some(next as usize));
}

fn selected_path<'a>(
    state: &TableState,
    results: &'a [crate::db::SearchResult],
) -> Option<&'a str> {
    state
        .selected()
        .and_then(|index| results.get(index))
        .map(|result| result.path.as_str())
}

fn selected_detail(state: &TableState, results: &[SearchResult], theme: &Theme) -> Text<'static> {
    match state.selected().and_then(|index| results.get(index)) {
        Some(result) => {
            let meta = Line::from(format!(
                "{}  created {}  modified {}  {} bytes",
                result.kind,
                format_date(result.created_at),
                format_date(result.modified_at),
                result.size_bytes,
            ));
            // Path on its own line so the wrapping Paragraph can show all of it
            // even when the list is narrowed by the preview pane.
            let path = Line::from(vec![
                Span::styled("path ", Style::default().fg(theme.muted)),
                Span::styled(result.path.clone(), Style::default().fg(theme.text)),
            ]);
            Text::from(vec![meta, path])
        }
        None => Text::from("No selection"),
    }
}

#[cfg(test)]
fn apply_local_result_options(
    results: &[SearchResult],
    options: &SearchOptions,
) -> Vec<SearchResult> {
    let Ok(compiled) = CompiledQuery::compile(options.mode, &options.query) else {
        return Vec::new();
    };
    let mut scorer = QueryScorer::new();
    let mut visible = results
        .iter()
        .filter(|result| local_filter_matches(result, &options.filters))
        .filter(|result| {
            compiled.matches_any(&mut scorer, [result.name.as_str(), result.path.as_str()])
        })
        .cloned()
        .collect::<Vec<_>>();
    // Frecency ordering is applied by the indexed search itself; the local
    // re-sort here only reacts to UI-only filter/sort toggles on already-ranked
    // results, so it passes an empty boost map.
    sort_results_compiled(
        &mut visible,
        options,
        &compiled,
        &mut scorer,
        &std::collections::HashMap::new(),
    );
    visible.truncate(options.limit);
    visible
}

#[cfg(test)]
fn local_filter_matches(result: &SearchResult, filters: &SearchFilters) -> bool {
    if let Some(kind) = &filters.kind {
        if result.kind != kind.as_str() {
            return false;
        }
    }
    if !filters.exts.is_empty()
        && !result
            .extension
            .as_ref()
            .is_some_and(|ext| filters.exts.iter().any(|filter| filter == ext))
    {
        return false;
    }
    if filters
        .min_size
        .is_some_and(|min_size| result.size_bytes < min_size)
    {
        return false;
    }
    if filters
        .max_size
        .is_some_and(|max_size| result.size_bytes > max_size)
    {
        return false;
    }
    true
}

fn edit_status(backend_label: &str) -> String {
    match backend_label {
        "indexed" => "indexed search updates while typing".to_string(),
        "hybrid" => "indexed search updates while typing; Enter runs live backfill".to_string(),
        _ => "Press Enter to run live search".to_string(),
    }
}

/// Persistent footer hint shown under results. Content depends on current focus.
pub(crate) fn footer_hint(focus: Focus) -> &'static str {
    match focus {
        Focus::Search => "type to filter \u{00b7} \u{2193}/\u{21e5} results \u{00b7} \u{23ce} open \u{00b7} esc quit",
        Focus::Results => "j/k move \u{00b7} o open \u{00b7} r reveal \u{00b7} y copy \u{00b7} m/f/s mode/filter/sort \u{00b7} / search \u{00b7} esc quit",
    }
}

fn footer_with_position(focus: Focus, selected: Option<usize>, total: usize) -> String {
    let position = selected
        .filter(|index| *index < total)
        .map_or(0, |index| index + 1);
    format!(
        "{}/{} \u{00b7} {}",
        format_count(position),
        format_count(total),
        footer_hint(focus)
    )
}

/// Lines shown in the empty-state onboarding card when query is empty.
pub(crate) fn empty_state_lines(root_label: &str) -> Vec<String> {
    vec![
        "Start typing to search".to_string(),
        root_label.to_string(),
        String::new(),
        "\u{21e5}/\u{2193}  results      ?  help      esc  quit".to_string(),
    ]
}

fn source_color(source: &str, theme: &Theme) -> Color {
    match source {
        "indexed" => theme.ok,
        "hybrid" => theme.warn,
        "live" => theme.accent,
        "stale" => theme.stale,
        _ => theme.muted,
    }
}

fn size_filter_label(filters: &SearchFilters) -> String {
    match (filters.min_size, filters.max_size) {
        (Some(min), Some(max)) => format!("{min}-{max}"),
        (Some(min), None) => format!(">{min}"),
        (None, Some(max)) => format!("<{max}"),
        (None, None) => "all".to_string(),
    }
}

fn date_filter_label(filters: &SearchFilters) -> String {
    if filters.created_after.is_some()
        || filters.created_before.is_some()
        || filters.modified_after.is_some()
        || filters.modified_before.is_some()
    {
        "set".to_string()
    } else {
        "all".to_string()
    }
}

fn cycle_kind_filter(filters: SearchFilters) -> SearchFilters {
    let next = match filters.kind.as_ref().map(|kind| kind.as_str()) {
        None => Some("pdf"),
        Some("pdf") => Some("image"),
        Some("image") => Some("text"),
        Some("text") => Some("folder"),
        _ => None,
    };
    SearchFilters {
        kind: next.and_then(|value| crate::query::FileKind::parse(value).ok()),
        ..filters
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashSet;
    use tempfile::tempdir;

    use crate::db::{local_db_path_for_root, Database, FileRecord, SearchResult};
    use crate::embed::search::SemanticHit;
    use crate::query::{
        CompiledQuery, FileKind, QueryMode, SearchFilters, SearchOptions, SortField,
    };
    use crate::tui::theme::Theme;
    use crate::tui::{
        apply_local_result_options, cache_stale, empty_state_lines, footer_hint,
        footer_with_position, format_count, format_result_summary, header_segments,
        rebuild_row_cache, restore_selection_by_path, search_backend_for_directory,
        search_bar_line, search_hybrid, should_show_results, sort_label, spinner_glyph,
        toggle_sort_order, top_chrome_height, top_controls_line, top_status_line,
        tui_search_options, viewport_range, Focus, InputNormalizer, NavigationBurst, RowCache,
        SearchBackend, SearchInput, SearchRequest, SearchState, SearchWorker, TopPanelArgs,
        FRAGMENTED_ESCAPE_WINDOW, TUI_RESULT_LIMIT,
    };
    use crate::tui::{
        detail_height, request_is_semantic, selected_has_semantic_hit, semantic_detail_lines,
        semantic_detail_map, SearchUpdate,
    };
    use ratatui::text::Line;
    use ratatui::widgets::TableState;
    use std::collections::HashMap;
    use std::thread;
    use std::time::{Duration, Instant};

    fn test_result(
        name: &str,
        kind: &str,
        extension: Option<&str>,
        size_bytes: u64,
    ) -> SearchResult {
        SearchResult {
            path: format!("/tmp/{name}"),
            name: name.to_string(),
            extension: extension.map(str::to_string),
            kind: kind.to_string(),
            size_bytes,
            created_at: None,
            modified_at: None,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn result_summary_includes_finder_like_metadata() {
        let result = SearchResult {
            path: "/tmp/report.pdf".to_string(),
            name: "report.pdf".to_string(),
            extension: Some("pdf".to_string()),
            kind: "pdf".to_string(),
            size_bytes: 1_500_000,
            created_at: Some(Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()),
            modified_at: Some(Utc.with_ymd_and_hms(2025, 6, 7, 8, 9, 10).unwrap()),
        };

        let summary = format_result_summary(&result);

        assert!(summary.contains("report.pdf"));
        assert!(summary.contains("pdf"));
        assert!(summary.contains("1.5 MB"));
        assert!(summary.contains("Created 2024-01-02"));
        assert!(summary.contains("Modified 2025-06-07"));
        assert!(summary.contains("/tmp/report.pdf"));
    }

    #[test]
    fn top_chrome_separates_search_bar_from_controls() {
        let theme = Theme::from_name(crate::tui::theme::ThemeName::Default);
        let args = TopPanelArgs {
            query: "archive",
            root_label: "/tmp",
            backend_label: "indexed",
            result_count: 50,
            watch_enabled: false,
            watch_errors: 0,
            mode: crate::query::QueryMode::Contains,
            sort: SortField::Relevance,
            reverse: false,
            filters: &SearchFilters::new(),
            theme,
            status: "ready",
        };
        let status = line_text(&top_status_line(&args));
        let search = line_text(&search_bar_line(&args));
        let controls = line_text(&top_controls_line(&args));

        assert!(status.contains("root /tmp"));
        assert!(status.contains("ready"));
        assert_eq!(search, "archive");
        assert!(!search.contains("mode:contains"));
        assert!(controls.contains("mode:contains"));
        assert!(controls.contains("sort:relevance asc"));
        assert!(controls.contains("type:"));
    }

    #[test]
    fn top_chrome_is_compact_fixed_height() {
        let theme = Theme::from_name(crate::tui::theme::ThemeName::Default);
        let args = TopPanelArgs {
            query: "archive",
            root_label: "/tmp",
            backend_label: "indexed",
            result_count: 50,
            watch_enabled: false,
            watch_errors: 0,
            mode: crate::query::QueryMode::Contains,
            sort: SortField::Relevance,
            reverse: false,
            filters: &SearchFilters::new(),
            theme,
            status: "ready",
        };

        // header band (1) + bordered search bar (3) + status line (1) + controls line (1)
        assert_eq!(top_chrome_height(&args), 6);
    }

    #[test]
    fn empty_query_hides_results() {
        assert!(!should_show_results(""));
        assert!(!should_show_results("   "));
        assert!(!should_show_results("r"));
        assert!(should_show_results("report"));
    }

    #[test]
    fn search_state_requires_explicit_submit_after_typing() {
        let mut state = SearchState::default();

        state.mark_dirty();

        assert!(!state.should_submit("r"));
        assert!(state.should_submit("report"));

        state.mark_submitted(SearchOptions::new("report"), false);

        assert!(!state.should_submit("report"));
    }

    #[test]
    fn dirty_filter_change_keeps_current_results_visible() {
        let mut state = SearchState::default();
        state.mark_submitted(SearchOptions::new("archive"), false);
        state.mark_dirty();

        assert!(state.should_render_results("archive"));
    }

    #[test]
    fn search_state_rejects_late_updates_by_request_id() {
        let mut state = SearchState::default();
        let options = SearchOptions::new("archive");
        state.mark_submitted_with_id(options.clone(), false, 7);

        assert!(state.accepts_update(7, "archive", &options));
        assert!(!state.accepts_update(6, "archive", &options));

        state.mark_submitted_with_id(options.clone(), false, 8);
        assert!(!state.accepts_update(7, "archive", &options));
        assert!(state.accepts_update(8, "archive", &options));
    }

    #[test]
    fn viewport_range_limits_row_work_and_keeps_selection_visible() {
        let (start, end) = viewport_range(10_000, Some(9_999), 20, 0);
        assert_eq!(end - start, 20);
        assert_eq!(start, 9_980);
        assert!(start <= 9_999 && 9_999 < end);

        let (start, end) = viewport_range(10_000, Some(5), 20, 0);
        assert_eq!((start, end), (0, 20));
    }

    #[test]
    fn viewport_range_holds_window_until_selection_reaches_an_edge() {
        assert_eq!(viewport_range(100, Some(50), 5, 46), (46, 51));
        assert_eq!(viewport_range(100, Some(45), 5, 46), (45, 50));
        assert_eq!(viewport_range(100, Some(51), 5, 46), (47, 52));
        assert_eq!(viewport_range(100, Some(0), 5, 46), (0, 5));
        assert_eq!(viewport_range(100, Some(99), 5, 46), (95, 100));
    }

    #[test]
    fn result_counts_are_grouped_for_progress_statuses() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(500), "500");
        assert_eq!(format_count(12_438), "12,438");
    }

    #[test]
    fn selection_restores_by_path_after_final_reorder() {
        let mut state = TableState::default();
        state.select(Some(1));
        let results = vec![
            test_result("new.txt", "text", Some("txt"), 1),
            test_result("selected.txt", "text", Some("txt"), 1),
        ];

        restore_selection_by_path(&mut state, &results, Some("/tmp/selected.txt"));

        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn local_result_options_sort_without_worker_round_trip() {
        let results = vec![
            test_result("beta.txt", "text", Some("txt"), 200),
            test_result("alpha.txt", "text", Some("txt"), 10),
        ];

        let visible = apply_local_result_options(
            &results,
            &SearchOptions::new("txt").with_sort(SortField::Name),
        );

        assert_eq!(
            visible
                .iter()
                .map(|result| result.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha.txt", "beta.txt"]
        );
    }

    #[test]
    fn sort_order_toggle_reverses_current_sort_field() {
        let results = vec![
            test_result("small.txt", "text", Some("txt"), 10),
            test_result("large.txt", "text", Some("txt"), 200),
        ];

        let visible = apply_local_result_options(
            &results,
            &SearchOptions::new("txt")
                .with_sort(SortField::Size)
                .with_reverse(toggle_sort_order(false)),
        );

        assert_eq!(
            visible
                .iter()
                .map(|result| result.name.as_str())
                .collect::<Vec<_>>(),
            vec!["large.txt", "small.txt"]
        );
        assert_eq!(sort_label(SortField::Size, false), "size asc");
        assert_eq!(sort_label(SortField::Size, true), "size desc");
    }

    #[test]
    fn local_result_options_filter_without_worker_round_trip() {
        let results = vec![
            test_result("archive.zip", "archive", Some("zip"), 200),
            test_result("archive.txt", "text", Some("txt"), 10),
        ];
        let options = SearchOptions::new("archive").with_filters(SearchFilters {
            kind: Some(FileKind::Archive),
            ..SearchFilters::new()
        });

        let visible = apply_local_result_options(&results, &options);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "archive.zip");
    }

    #[test]
    fn tui_search_options_use_larger_result_limit() {
        assert_eq!(tui_search_options("archive").limit, TUI_RESULT_LIMIT);
        assert!(TUI_RESULT_LIMIT > SearchOptions::new("archive").limit);
    }

    #[test]
    fn indexed_worker_reports_open_errors_for_submitted_query() {
        let dir = tempdir().expect("temp dir");
        let mut worker = SearchWorker::spawn(SearchBackend::Indexed {
            db_path: dir.path().to_path_buf(),
            root: dir.path().to_path_buf(),
        })
        .expect("worker spawns");

        worker
            .submit(SearchRequest {
                options: SearchOptions::new("archive"),
            })
            .expect("request sends");

        let response = (0..20)
            .find_map(|_| {
                thread::sleep(Duration::from_millis(10));
                match worker.try_recv() {
                    Some(super::SearchUpdate::Error { options, error, .. }) => {
                        Some((options, error))
                    }
                    Some(_) | None => None,
                }
            })
            .expect("worker returns open error");

        assert_eq!(response.0.query, "archive");
        assert!(!response.1.is_empty());
    }

    #[test]
    fn repeated_indexed_search_does_not_wait_for_a_writer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        drop(Database::open(&path).unwrap());
        let mut worker = SearchWorker::spawn(SearchBackend::Indexed {
            db_path: path.clone(),
            root: dir.path().to_path_buf(),
        })
        .unwrap();
        let request = || SearchRequest {
            options: SearchOptions::new("archive"),
        };
        let complete = |worker: &mut SearchWorker| loop {
            match worker
                .rx
                .recv_timeout(Duration::from_secs(2))
                .expect("search must not wait for writer")
            {
                SearchUpdate::Complete { .. } => break,
                SearchUpdate::Error { error, .. } => panic!("{error}"),
                _ => {}
            }
        };
        worker.submit(request()).unwrap();
        complete(&mut worker);
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        worker.submit(request()).unwrap();
        complete(&mut worker);
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn quit_keys_accept_press_and_repeat_but_not_release() {
        use crossterm::event::KeyEventKind;
        for (code, modifiers) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                assert_eq!(
                    super::is_quit_key(KeyEvent::new_with_kind(code, modifiers, kind)),
                    kind != KeyEventKind::Release
                );
            }
        }
        assert!(!super::is_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn semantic_worker_panic_reports_active_and_later_requests() {
        let (tx, requests) = std::sync::mpsc::channel();
        let (responses, rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            super::semantic_worker_with(requests, responses, |pending, _, responses| {
                let active = pending.as_ref().unwrap();
                responses
                    .send(SearchUpdate::Reset {
                        request_id: active.request_id,
                        options: active.request.options.clone(),
                    })
                    .unwrap();
                panic!("injected semantic worker panic");
            });
        });
        let options = SearchOptions::new("budget").with_mode(QueryMode::Semantic);
        tx.send(super::QueuedSearchRequest {
            request_id: 41,
            request: SearchRequest {
                options: options.clone(),
            },
        })
        .unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            SearchUpdate::Reset { request_id: 41, .. }
        ));
        for request_id in [41, 42] {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                SearchUpdate::Error {
                    request_id: actual,
                    options: actual_options,
                    error,
                } => {
                    assert_eq!(actual, request_id);
                    assert_eq!(actual_options, options);
                    assert!(error.contains("restart lctr"));
                }
                _ => panic!("a panic must terminate the loading state with an error"),
            }
            if request_id == 41 {
                tx.send(super::QueuedSearchRequest {
                    request_id: 42,
                    request: SearchRequest {
                        options: options.clone(),
                    },
                })
                .unwrap();
            }
        }
        drop(tx);
        worker.join().expect("semantic worker contains the panic");
    }

    #[test]
    fn preview_worker_skips_superseded_requests_and_drop_does_not_wait() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = super::PreviewWorker::spawn_with(move |path| {
            started_tx.send(path.to_string()).unwrap();
            release_rx.recv().unwrap();
            crate::preview::Preview::Info(Vec::new())
        });
        worker.tx.send("first".into()).unwrap();
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "first"
        );
        worker.tx.send("skipped".into()).unwrap();
        worker.tx.send("latest".into()).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "latest"
        );
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(worker);
            dropped_tx.send(()).unwrap();
        });
        let dropped = dropped_rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        dropped.expect("dropping a preview worker must not wait for file I/O");
    }

    #[test]
    fn live_worker_emits_all_result_deltas_before_completion() {
        let dir = tempdir().expect("temp dir");
        for index in 0..1_001 {
            std::fs::write(dir.path().join(format!("needle-{index:04}.txt")), b"match")
                .expect("write match");
        }
        let mut worker = SearchWorker::spawn(SearchBackend::Live {
            root: dir.path().to_path_buf(),
        })
        .expect("worker spawns");
        let request_id = worker
            .submit(SearchRequest {
                options: SearchOptions::new("needle").with_sort(SortField::Name),
            })
            .expect("request sends");

        let mut batch_sizes = Vec::new();
        let mut final_count = None;
        for _ in 0..200 {
            if let Some(update) = worker.try_recv() {
                match update {
                    super::SearchUpdate::Reset { request_id: id, .. } => {
                        assert_eq!(id, request_id);
                    }
                    super::SearchUpdate::Append {
                        request_id: id,
                        results,
                        ..
                    } => {
                        assert_eq!(id, request_id);
                        batch_sizes.push(results.len());
                    }
                    super::SearchUpdate::Complete {
                        request_id: id,
                        count,
                        final_results: Some(results),
                        ..
                    } => {
                        assert_eq!(id, request_id);
                        assert_eq!(results.len(), 1_001);
                        final_count = Some(count);
                        break;
                    }
                    super::SearchUpdate::Complete { .. } => panic!("missing final ordering"),
                    super::SearchUpdate::Error { error, .. } => panic!("search failed: {error}"),
                }
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        }

        assert_eq!(batch_sizes, vec![500, 500, 1]);
        assert_eq!(final_count, Some(1_001));
    }

    #[test]
    fn hybrid_worker_deduplicates_indexed_paths_from_live_backfill() {
        let dir = tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonical root");
        let db_path = root.join("index.sqlite");
        let indexed_path = root.join("needle-indexed.txt");
        std::fs::write(&indexed_path, b"indexed").expect("write indexed match");
        let db = Database::open(&db_path).expect("open database");
        db.upsert_file(&FileRecord {
            path: indexed_path.to_string_lossy().to_string(),
            name: "needle-indexed.txt".to_string(),
            parent: root.to_string_lossy().to_string(),
            extension: Some("txt".to_string()),
            root: root.to_string_lossy().to_string(),
            volume: "local".to_string(),
            kind: "text".to_string(),
            size_bytes: 7,
            created_at: None,
            modified_at: None,
        })
        .expect("insert indexed match");
        drop(db);
        for index in 0..1_001 {
            std::fs::write(root.join(format!("needle-live-{index:04}.txt")), b"live")
                .expect("write live match");
        }

        let mut worker =
            SearchWorker::spawn(SearchBackend::Hybrid { db_path, root }).expect("worker spawns");
        worker
            .submit(SearchRequest {
                options: SearchOptions::new("needle").with_sort(SortField::Name),
            })
            .expect("request sends");

        let mut batch_paths = Vec::new();
        let mut final_results = None;
        for _ in 0..300 {
            if let Some(update) = worker.try_recv() {
                match update {
                    super::SearchUpdate::Append { results, .. } => {
                        batch_paths.extend(results.into_iter().map(|result| result.path));
                    }
                    super::SearchUpdate::Complete {
                        final_results: Some(results),
                        ..
                    } => {
                        final_results = Some(results);
                        break;
                    }
                    super::SearchUpdate::Complete { .. } => panic!("missing final ordering"),
                    super::SearchUpdate::Reset { .. } => {}
                    super::SearchUpdate::Error { error, .. } => {
                        panic!("hybrid search failed: {error}")
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        }

        let final_results = final_results.expect("hybrid worker completes");
        assert_eq!(final_results.len(), 1_002);
        assert_eq!(
            final_results
                .iter()
                .map(|result| result.path.as_str())
                .collect::<HashSet<_>>()
                .len(),
            1_002
        );
        assert_eq!(
            batch_paths
                .iter()
                .filter(|path| path.as_str() == indexed_path.to_string_lossy())
                .count(),
            1
        );
    }

    #[test]
    fn search_input_moves_cursor_and_edits_at_cursor() {
        let mut input = SearchInput::default();

        for ch in "report".chars() {
            input.insert(ch);
        }
        input.move_left();
        input.move_left();
        input.insert('_');

        assert_eq!(input.as_str(), "repo_rt");
        assert_eq!(input.cursor_column(), 5);

        input.backspace();

        assert_eq!(input.as_str(), "report");
        assert_eq!(input.cursor_column(), 4);

        input.move_right();
        input.move_right();
        input.move_right();

        assert_eq!(input.cursor_column(), 6);
    }

    #[test]
    fn input_normalizer_reassembles_fragmented_csi_arrow() {
        let start = Instant::now();
        let mut normalizer = InputNormalizer::default();

        normalizer.push(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            start,
        );
        normalizer.push(
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
            start + Duration::from_millis(1),
        );
        normalizer.push(
            Event::Key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT)),
            start + Duration::from_millis(2),
        );

        assert_eq!(
            normalizer.pop_ready(),
            Some(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
        );
        assert!(normalizer.pop_ready().is_none());
    }

    #[test]
    fn input_normalizer_reassembles_fragmented_sgr_wheel_packet() {
        let start = Instant::now();
        let mut normalizer = InputNormalizer::default();
        let fragments = [
            '\u{1b}', '[', '<', '6', '5', ';', '1', '1', '9', ';', '4', '5', 'M',
        ];

        for (offset, ch) in fragments.into_iter().enumerate() {
            let code = if ch == '\u{1b}' {
                KeyCode::Esc
            } else {
                KeyCode::Char(ch)
            };
            normalizer.push(
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                start + Duration::from_millis(offset as u64),
            );
        }

        assert!(matches!(
            normalizer.pop_ready(),
            Some(Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 118,
                row: 44,
                modifiers: KeyModifiers::NONE,
            }))
        ));
        assert!(normalizer.pop_ready().is_none());
    }

    #[test]
    fn input_normalizer_discards_fragmented_sgr_motion_packet() {
        let start = Instant::now();
        let mut normalizer = InputNormalizer::default();
        let fragments = [
            '\u{1b}', '[', '<', '3', '5', ';', '1', '1', '9', ';', '4', '5', 'M',
        ];

        for (offset, ch) in fragments.into_iter().enumerate() {
            let code = if ch == '\u{1b}' {
                KeyCode::Esc
            } else {
                KeyCode::Char(ch)
            };
            normalizer.push(
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                start + Duration::from_millis(offset as u64),
            );
        }

        assert!(normalizer.pop_ready().is_none());
    }

    #[test]
    fn input_normalizer_preserves_a_slow_literal_escape_sequence() {
        let start = Instant::now();
        let mut normalizer = InputNormalizer::default();

        normalizer.push(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            start,
        );
        normalizer.push(
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
            start + FRAGMENTED_ESCAPE_WINDOW,
        );
        normalizer.push(
            Event::Key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT)),
            start + FRAGMENTED_ESCAPE_WINDOW + Duration::from_millis(1),
        );

        assert_eq!(
            normalizer.pop_ready(),
            Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        );
        assert_eq!(
            normalizer.pop_ready(),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('['),
                KeyModifiers::NONE
            )))
        );
        assert_eq!(
            normalizer.pop_ready(),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('B'),
                KeyModifiers::SHIFT
            )))
        );
    }

    #[test]
    fn navigation_burst_accepts_one_event_per_ratchet() {
        let start = Instant::now();
        let mut burst = NavigationBurst::default();

        assert!(burst.accepts(KeyCode::Down, start));
        assert!(!burst.accepts(KeyCode::Down, start + Duration::from_millis(3)));
        assert!(!burst.accepts(KeyCode::Down, start + Duration::from_millis(49)));
        assert!(!burst.accepts(KeyCode::Down, start + Duration::from_millis(50)));
        assert!(burst.accepts(KeyCode::Down, start + Duration::from_millis(100)));
    }

    #[test]
    fn navigation_burst_allows_direction_changes_and_resets() {
        let start = Instant::now();
        let mut burst = NavigationBurst::default();

        assert!(burst.accepts(KeyCode::Down, start));
        assert!(burst.accepts(KeyCode::Up, start + Duration::from_millis(3)));
        assert!(!burst.accepts(KeyCode::Up, start + Duration::from_millis(4)));
        burst.reset();
        assert!(burst.accepts(KeyCode::Up, start + Duration::from_millis(5)));
    }

    #[test]
    fn unindexed_directory_uses_live_search_backend() {
        let dir = tempdir().expect("temp dir");

        let backend = search_backend_for_directory(dir.path()).expect("backend resolves");

        assert_eq!(
            backend,
            SearchBackend::Live {
                root: dir.path().canonicalize().expect("canonical root")
            }
        );
    }

    #[test]
    fn indexed_directory_uses_indexed_search_backend() {
        let dir = tempdir().expect("temp dir");
        let db_path = local_db_path_for_root(dir.path()).expect("local db path");
        let db = Database::open(&db_path).expect("create db");
        let root = dir.path().canonicalize().expect("canonical root");
        let root_string = root.to_string_lossy().to_string();
        db.mark_scan_started(&root_string, 10)
            .expect("mark started");
        db.mark_scan_completed(&root_string, 10)
            .expect("mark complete");

        let backend = search_backend_for_directory(dir.path()).expect("backend resolves");

        assert_eq!(
            backend,
            SearchBackend::Indexed {
                db_path,
                root: dir.path().canonicalize().expect("canonical root")
            }
        );
    }

    #[test]
    fn incomplete_index_uses_hybrid_search_backend() {
        let dir = tempdir().expect("temp dir");
        let db_path = local_db_path_for_root(dir.path()).expect("local db path");
        let db = Database::open(&db_path).expect("create db");
        let root = dir.path().canonicalize().expect("canonical root");
        let root_string = root.to_string_lossy().to_string();
        db.mark_scan_started(&root_string, 10)
            .expect("mark started");

        let backend = search_backend_for_directory(dir.path()).expect("backend resolves");

        assert_eq!(
            backend,
            SearchBackend::Hybrid {
                db_path,
                root: dir.path().canonicalize().expect("canonical root")
            }
        );
    }

    #[test]
    fn hybrid_search_returns_live_matches_missing_from_incomplete_index() {
        let dir = tempdir().expect("temp dir");
        let indexed_path = dir.path().join("indexed-report.pdf");
        let live_path = dir.path().join("live-report.pdf");
        std::fs::write(&indexed_path, "indexed").expect("write indexed");
        std::fs::write(&live_path, "live").expect("write live");

        let db = Database::open_in_memory().expect("db opens");
        db.upsert_file(&FileRecord {
            path: indexed_path.to_string_lossy().to_string(),
            name: "indexed-report.pdf".to_string(),
            parent: dir.path().to_string_lossy().to_string(),
            extension: Some("pdf".to_string()),
            root: dir.path().to_string_lossy().to_string(),
            volume: "local".to_string(),
            kind: "pdf".to_string(),
            size_bytes: 7,
            created_at: None,
            modified_at: None,
        })
        .expect("insert indexed row");

        let options = SearchOptions::new("live-report");
        let results = search_hybrid(&db, dir.path(), &options).expect("hybrid search");
        let live_path = live_path
            .canonicalize()
            .expect("canonical live path")
            .to_string_lossy()
            .to_string();

        assert!(results.iter().any(|result| result.path == live_path));
    }

    #[test]
    fn only_semantic_mode_routes_to_the_semantic_thread() {
        let options = SearchOptions::new("budget");
        assert!(!request_is_semantic(&options));
        assert!(request_is_semantic(
            &options.clone().with_mode(QueryMode::Semantic)
        ));
    }

    #[test]
    fn semantic_detail_lines_show_similarity_and_match_for_hit_paths_only() {
        let theme = Theme::load_with_default("default");
        let mut details = HashMap::new();
        details.insert(
            "/docs/budget.txt".to_string(),
            (0.8312_f32, "quarterly budget".to_string()),
        );
        let lines = semantic_detail_lines("/docs/budget.txt", &details, &theme, 80);
        assert_eq!(lines.len(), 2);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(text[0], "similarity 0.83");
        assert_eq!(text[1], "match quarterly budget");
        assert!(semantic_detail_lines("/docs/other.txt", &details, &theme, 80).is_empty());
    }

    #[test]
    fn semantic_match_line_is_one_row_fitted_to_the_width() {
        let theme = Theme::load_with_default("default");
        let mut details = HashMap::new();
        details.insert(
            "/docs/long.md".to_string(),
            (0.5_f32, "a very long snippet ".repeat(40)),
        );
        let lines = semantic_detail_lines("/docs/long.md", &details, &theme, 40);
        let match_text: String = lines[1]
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(match_text.starts_with("match "));
        assert!(match_text.chars().count() <= 40, "{match_text}");
        assert!(match_text.ends_with('\u{2026}'), "{match_text}");
        assert!(!match_text.contains('\n'));
    }

    #[test]
    fn detail_pane_grows_only_for_a_selected_semantic_hit() {
        assert_eq!(detail_height(36, false), 4);
        assert_eq!(detail_height(36, true), 6);
        assert_eq!(detail_height(20, false), 4);
        assert_eq!(detail_height(19, false), 0);
        assert_eq!(detail_height(19, true), 0);
    }

    #[test]
    fn selected_hit_lookup_uses_the_exact_path_key() {
        let mut details = HashMap::new();
        details.insert("/docs/a.md".to_string(), (0.9_f32, "x".to_string()));
        assert!(selected_has_semantic_hit(&details, Some("/docs/a.md")));
        assert!(!selected_has_semantic_hit(&details, Some("/docs/b.md")));
        assert!(!selected_has_semantic_hit(&details, None));
    }

    #[test]
    fn semantic_detail_map_keeps_score_and_snippet_per_path() {
        let hit = SemanticHit {
            result: SearchResult {
                path: "/a.txt".to_string(),
                name: "a.txt".to_string(),
                extension: Some("txt".to_string()),
                kind: "text".to_string(),
                size_bytes: 3,
                created_at: None,
                modified_at: None,
            },
            score: 0.5,
            snippet: "alpha".to_string(),
        };
        let map = semantic_detail_map(&[hit]);
        assert_eq!(map.get("/a.txt"), Some(&(0.5, "alpha".to_string())));
    }

    #[test]
    fn semantic_request_without_an_index_reports_no_embeddings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("index.sqlite");
        Database::open(&db_path).expect("create index");
        let mut worker = SearchWorker::spawn(SearchBackend::Indexed {
            db_path,
            root: dir.path().to_path_buf(),
        })
        .expect("worker spawns");
        let options = SearchOptions::new("budget").with_mode(QueryMode::Semantic);
        worker
            .submit(SearchRequest { options })
            .expect("submit semantic request");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(SearchUpdate::Error { error, .. }) =
                worker.rx.recv_timeout(Duration::from_millis(200))
            {
                assert!(error.contains("no embeddings yet"), "{error}");
                break;
            }
            assert!(Instant::now() < deadline, "no error update arrived");
        }
    }

    #[test]
    fn cache_stale_detects_query_mode_and_stamp_changes() {
        let cache = RowCache {
            query: "foo".to_string(),
            mode: QueryMode::Contains,
            results_stamp: 1,
            viewport_start: 0,
            viewport_end: 0,
            rows: Vec::new(),
        };
        assert!(!cache_stale(&cache, "foo", QueryMode::Contains, 1));
        assert!(cache_stale(&cache, "bar", QueryMode::Contains, 1));
        assert!(cache_stale(&cache, "foo", QueryMode::Fuzzy, 1));
        assert!(cache_stale(&cache, "foo", QueryMode::Contains, 2));
    }

    #[test]
    fn rebuild_row_cache_positions_match_direct_match_positions() {
        use crate::query::QueryScorer;

        let query = "report";
        let mode = QueryMode::Contains;
        let result = SearchResult {
            path: "/tmp/reports/report.pdf".to_string(),
            name: "report.pdf".to_string(),
            extension: Some("pdf".to_string()),
            kind: "pdf".to_string(),
            size_bytes: 1024,
            created_at: None,
            modified_at: None,
        };
        let cache = rebuild_row_cache(std::slice::from_ref(&result), query, mode, 0);
        assert_eq!(cache.rows.len(), 1);

        let compiled = CompiledQuery::compile(mode, query).unwrap();
        let mut scorer = QueryScorer::new();
        let expected_name = compiled.match_positions(&mut scorer, &result.name);
        let expected_path = compiled.match_positions(&mut scorer, &result.path);

        assert_eq!(cache.rows[0].name_positions, expected_name);
        assert_eq!(cache.rows[0].path_positions, expected_path);
        assert!(!cache.rows[0].size_text.is_empty());
        assert!(!cache.rows[0].kind.is_empty());
    }

    #[test]
    fn spinner_glyph_cycles() {
        assert_ne!(spinner_glyph(0), spinner_glyph(1));
        assert_eq!(spinner_glyph(10), spinner_glyph(0));
    }

    #[test]
    fn header_segments_includes_backend() {
        let (wordmark, right) = header_segments("/tmp", "indexed");
        assert_eq!(wordmark, "lctr");
        assert!(right.contains("indexed"));
        assert!(right.contains("/tmp"));
    }

    #[test]
    fn footer_hint_search_and_results_keys() {
        let search_hint = footer_hint(Focus::Search);
        assert!(search_hint.contains("filter"));
        assert!(search_hint.contains("results"));

        let results_hint = footer_hint(Focus::Results);
        assert!(results_hint.contains("move"));
        assert!(results_hint.contains("open"));
        assert!(results_hint.contains("search"));
    }

    #[test]
    fn footer_position_is_one_based_and_formats_total() {
        assert!(footer_with_position(Focus::Results, Some(24), 2_049).starts_with("25/2,049 ·"));
        assert!(footer_with_position(Focus::Search, None, 0).starts_with("0/0 ·"));
    }

    #[test]
    fn empty_state_includes_root_and_help() {
        let lines = empty_state_lines("/tmp");
        let all = lines.join("\n");
        assert!(all.contains("/tmp"));
        assert!(all.contains("results"));
    }
}

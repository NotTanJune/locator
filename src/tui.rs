use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Terminal;

use crate::db::{
    candidate_matches, existing_index_for_working_dir, sort_results, Database, ScanCompletion,
    SearchResult,
};
use crate::live_search::{
    search_live_streaming_with_options, search_live_with_options, LiveSearchStatus,
};
use crate::open::{copy_path, open_file, reveal_in_finder};
use crate::query::{QueryMode, SearchFilters, SearchOptions, SortField};

mod theme;

use theme::Theme;

const INDEXED_INPUT_GRACE: Duration = Duration::from_millis(1500);
const TUI_RESULT_LIMIT: usize = 1000;

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

pub fn run_with_backend(search_backend: SearchBackend, update_check_disabled: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        SetCursorStyle::BlinkingBar,
        Show
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(&mut terminal, search_backend, update_check_disabled);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    search_backend: SearchBackend,
    update_check_disabled: bool,
) -> Result<()> {
    let mut input = SearchInput::default();
    let mut all_results = Vec::new();
    let mut results = Vec::new();
    let mut selected = TableState::default();
    let mut status = String::from("Press / to search. Shortcut keys are active in normal mode.");
    let mut search_state = SearchState::default();
    let backend_label = search_backend.label();
    let root_label = search_backend.root_label();
    let mut theme = Theme::load();
    let mut mode = QueryMode::Contains;
    let mut sort = SortField::Relevance;
    let mut reverse = matches!(sort, SortField::Modified);
    let mut filters = SearchFilters::new();
    let mut input_mode = initial_input_mode();
    let mut watch_enabled = false;
    let mut search_worker = SearchWorker::spawn(search_backend)?;
    let mut loading_query: Option<String> = None;
    let mut last_edit = Instant::now();
    let mut indexed_input_exit_deadline: Option<Instant> = None;
    let update_rx = crate::update_check::check_async(update_check_disabled);
    let mut update_status: Option<crate::update_check::UpdateStatus> = None;

    loop {
        while let Some(response) = search_worker.try_recv() {
            let query = input.as_str();
            if search_state.accepts_response(query, &response.options) {
                match response.results {
                    Ok(next_results) => {
                        all_results = next_results;
                        results = apply_local_result_options(
                            &all_results,
                            &tui_search_options(input.as_str())
                                .with_mode(mode)
                                .with_sort(sort)
                                .with_reverse(reverse)
                                .with_filters(filters.clone()),
                        );
                        normalize_selection(&mut selected, results.len());
                        if response.complete {
                            indexed_input_exit_deadline = indexed_response_exit_deadline(
                                response.complete,
                                response.live_backfill,
                                backend_label,
                                Instant::now(),
                            );
                            loading_query = None;
                            if response.live_backfill {
                                search_state.mark_live_complete(response.options.query.clone());
                            }
                            status = format!(
                                "{backend_label} search complete, {} results",
                                results.len()
                            );
                        } else {
                            status = format!(
                                "{backend_label} search found {} results so far",
                                results.len()
                            );
                        }
                    }
                    Err(error) => {
                        loading_query = None;
                        status = error.to_string();
                    }
                }
            }
        }

        if !input_mode {
            indexed_input_exit_deadline = None;
        } else if !input_mode_after_indexed_grace(
            input_mode,
            indexed_input_exit_deadline,
            Instant::now(),
        ) {
            input_mode = false;
            indexed_input_exit_deadline = None;
            status = "normal mode".to_string();
        }

        let query = input.as_str();
        if search_state.should_auto_submit(query, backend_label, last_edit.elapsed()) {
            let options = tui_search_options(query)
                .with_mode(mode)
                .with_sort(sort)
                .with_reverse(reverse)
                .with_filters(filters.clone());
            if search_worker
                .submit(SearchRequest {
                    options: options.clone(),
                    live_backfill: false,
                })
                .is_ok()
            {
                search_state.mark_submitted(options, false);
                loading_query = Some(query.to_string());
                indexed_input_exit_deadline = None;
                status = format!("searching {backend_label} index for {query}");
            }
        }

        if update_status.is_none() {
            if let Ok(Some(s)) = update_rx.try_recv() {
                update_status = Some(s);
            }
        }

        terminal.draw(|frame| {
            let query = input.as_str();
            let has_detail = frame.area().height >= 20;
            let top_args = TopPanelArgs {
                query,
                root_label: &root_label,
                backend_label,
                result_count: results.len(),
                watch_enabled,
                mode,
                sort,
                reverse,
                filters: &filters,
                theme,
                status: status.as_str(),
            };
            let controls_height = controls_panel_height(&top_args);
            let show_banner = update_status.is_some();
            let all_chunks = if show_banner {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Length(top_chrome_height(&top_args)),
                        Constraint::Min(6),
                        Constraint::Length(if has_detail { 3 } else { 0 }),
                    ])
                    .split(frame.area())
            } else {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(top_chrome_height(&top_args)),
                        Constraint::Min(6),
                        Constraint::Length(if has_detail { 3 } else { 0 }),
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

            let top_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(controls_height),
                ])
                .split(chunks[0]);

            let status_panel = Paragraph::new(top_status_line(&top_args)).block(
                Block::default()
                    .title("locator")
                    .title_style(
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    )
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.accent)),
            );
            frame.render_widget(status_panel, top_chunks[0]);

            let search_panel = Paragraph::new(search_bar_line(&top_args)).block(
                Block::default()
                    .title("search")
                    .title_style(Style::default().fg(theme.accent))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.accent)),
            );
            frame.render_widget(search_panel, top_chunks[1]);

            let controls_panel = Paragraph::new(top_controls_lines(&top_args)).block(
                Block::default()
                    .title("controls")
                    .title_style(Style::default().fg(theme.muted))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.muted)),
            );
            frame.render_widget(controls_panel, top_chunks[2]);
            if input_mode {
                frame.set_cursor_position(Position {
                    x: top_chunks[1].x + 1 + input.cursor_column() as u16,
                    y: top_chunks[1].y + 1,
                });
            }

            if search_state.should_render_results(query) {
                let rows = results
                    .iter()
                    .enumerate()
                    .map(|(index, result)| result_row(index, result, backend_label, mode, &theme))
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
                                format!("searching {active}...")
                            }
                            _ => format!("results ({})", results.len()),
                        })
                        .title_style(Style::default().fg(theme.ok).add_modifier(Modifier::BOLD))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(theme.muted)),
                )
                .row_highlight_style(Style::default().bg(theme.selected_bg))
                .highlight_symbol(">");
                frame.render_stateful_widget(table, chunks[1], &mut selected);
            } else if !query.is_empty() {
                let hint = Paragraph::new(if should_show_results(query) {
                    match backend_label {
                        "indexed" => "Indexed results update while typing",
                        "hybrid" => {
                            "Indexed results update while typing. Press Enter for live backfill"
                        }
                        _ => "Press Enter to search live filenames",
                    }
                } else {
                    "Type at least 2 letters, then press Enter"
                })
                .style(Style::default().fg(theme.muted))
                .block(
                    Block::default()
                        .title("waiting")
                        .title_style(Style::default().fg(theme.ok))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(theme.muted)),
                );
                frame.render_widget(hint, chunks[1]);
            }

            if has_detail {
                let detail = Paragraph::new(selected_detail(&selected, &results))
                    .style(Style::default().fg(theme.muted))
                    .block(
                        Block::default()
                            .borders(Borders::TOP)
                            .border_style(Style::default().fg(theme.muted)),
                    );
                frame.render_widget(detail, chunks[2]);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc if input_mode => {
                        input_mode = false;
                        indexed_input_exit_deadline = None;
                        status = "normal mode".to_string();
                    }
                    KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('/') if !input_mode => {
                        input_mode = true;
                        indexed_input_exit_deadline = None;
                        status = "search mode".to_string();
                    }
                    KeyCode::Backspace if input_mode && input.backspace() => {
                        search_state.mark_dirty();
                        last_edit = Instant::now();
                        indexed_input_exit_deadline = None;
                        if backend_label == "live" {
                            all_results.clear();
                            results.clear();
                        }
                        normalize_selection(&mut selected, results.len());
                        status = edit_status(backend_label);
                    }
                    KeyCode::Char(ch) => {
                        match ch {
                            'r' if !input_mode || key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if let Some(path) = selected_path(&selected, &results) {
                                    reveal_in_finder(Path::new(path))?;
                                    status = format!("revealed {path}");
                                }
                            }
                            'y' if !input_mode || key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if let Some(path) = selected_path(&selected, &results) {
                                    copy_path(Path::new(path))?;
                                    status = format!("copied {path}");
                                }
                            }
                            'o' if !input_mode => {
                                if let Some(path) = selected_path(&selected, &results) {
                                    open_file(Path::new(path))?;
                                    status = format!("opened {path}");
                                }
                            }
                            'j' if !input_mode => move_selection(&mut selected, results.len(), 1),
                            'k' if !input_mode => move_selection(&mut selected, results.len(), -1),
                            'g' if !input_mode => select_first(&mut selected, results.len()),
                            'G' if !input_mode => select_last(&mut selected, results.len()),
                            'm' if !input_mode => {
                                mode = mode.next();
                                results = apply_local_result_options(
                                    &all_results,
                                    &tui_search_options(input.as_str())
                                        .with_mode(mode)
                                        .with_sort(sort)
                                        .with_reverse(reverse)
                                        .with_filters(filters.clone()),
                                );
                                normalize_selection(&mut selected, results.len());
                                search_state.mark_dirty();
                                last_edit = Instant::now();
                                status = format!("mode: {}", mode.label());
                            }
                            'f' if !input_mode => {
                                filters = cycle_kind_filter(filters);
                                results = apply_local_result_options(
                                    &all_results,
                                    &tui_search_options(input.as_str())
                                        .with_mode(mode)
                                        .with_sort(sort)
                                        .with_reverse(reverse)
                                        .with_filters(filters.clone()),
                                );
                                normalize_selection(&mut selected, results.len());
                                search_state.mark_dirty();
                                last_edit = Instant::now();
                                status = "type filter changed".to_string();
                            }
                            's' if !input_mode => {
                                sort = sort.next();
                                results = apply_local_result_options(
                                    &all_results,
                                    &tui_search_options(input.as_str())
                                        .with_mode(mode)
                                        .with_sort(sort)
                                        .with_reverse(reverse)
                                        .with_filters(filters.clone()),
                                );
                                normalize_selection(&mut selected, results.len());
                                status = format!("sort: {}", sort.label());
                            }
                            'S' if !input_mode => {
                                reverse = toggle_sort_order(reverse);
                                results = apply_local_result_options(
                                    &all_results,
                                    &tui_search_options(input.as_str())
                                        .with_mode(mode)
                                        .with_sort(sort)
                                        .with_reverse(reverse)
                                        .with_filters(filters.clone()),
                                );
                                normalize_selection(&mut selected, results.len());
                                status = format!("sort order: {}", sort_label(sort, reverse));
                            }
                            't' if !input_mode => {
                                theme = theme.cycle();
                                if let Err(error) = theme.persist() {
                                    status = error.to_string();
                                } else {
                                    status = format!("theme: {}", theme.name.label());
                                }
                            }
                            'w' if !input_mode => {
                                watch_enabled = !watch_enabled;
                                status = if watch_enabled {
                                    "watch visible: refresh indicators enabled".to_string()
                                } else {
                                    "watch off".to_string()
                                };
                            }
                            '?' if !input_mode => {
                                status = "/ search, Enter confirm or open, m mode, f type, s sort, t theme".to_string();
                            }
                            _ if input_mode => {
                                input.insert(ch);
                                search_state.mark_dirty();
                                last_edit = Instant::now();
                                indexed_input_exit_deadline = None;
                                if backend_label == "live" {
                                    all_results.clear();
                                    results.clear();
                                }
                                normalize_selection(&mut selected, results.len());
                                status = edit_status(backend_label);
                            }
                            _ => {}
                        }
                    }
                    KeyCode::Left if input_mode => input.move_left(),
                    KeyCode::Right if input_mode => input.move_right(),
                    KeyCode::Down => move_selection(&mut selected, results.len(), 1),
                    KeyCode::Up => move_selection(&mut selected, results.len(), -1),
                    KeyCode::PageDown => move_selection(&mut selected, results.len(), 10),
                    KeyCode::PageUp => move_selection(&mut selected, results.len(), -10),
                    KeyCode::Enter => {
                        let query = input.as_str();
                        let live_backfill =
                            backend_label != "indexed" && !search_state.live_complete_for(query);
                        match enter_action(
                            input_mode,
                            search_state.should_submit(query),
                            live_backfill,
                            selected_path(&selected, &results).is_some(),
                            should_show_results(query),
                        ) {
                            EnterAction::SubmitSearch => {
                                let options = tui_search_options(query)
                                    .with_mode(mode)
                                    .with_sort(sort)
                                    .with_reverse(reverse)
                                    .with_filters(filters.clone());
                                search_worker.submit(SearchRequest {
                                    options: options.clone(),
                                    live_backfill,
                                })?;
                                input_mode = input_mode_after_submit(true);
                                indexed_input_exit_deadline = None;
                                search_state.mark_submitted(options, live_backfill);
                                loading_query = Some(query.to_string());
                                if backend_label == "live" || live_backfill {
                                    all_results.clear();
                                    results.clear();
                                }
                                normalize_selection(&mut selected, results.len());
                                status = if live_backfill {
                                    format!("searching live filenames for {query}")
                                } else {
                                    format!("searching {backend_label} filenames for {query}")
                                };
                            }
                            EnterAction::ConfirmSearch => {
                                input_mode = false;
                                indexed_input_exit_deadline = None;
                                status = "search confirmed".to_string();
                            }
                            EnterAction::OpenSelection => {
                                if let Some(path) = selected_path(&selected, &results) {
                                    open_file(Path::new(path))?;
                                    status = format!("opened {path}");
                                }
                            }
                            EnterAction::Noop => {
                                status = "Type at least 2 letters before searching.".to_string();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

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

fn search_hybrid(db: &Database, root: &Path, options: &SearchOptions) -> Result<Vec<SearchResult>> {
    let indexed = search_for_tui(db, options)?;
    let live = search_live_with_options(root, options)?;
    Ok(merge_results(indexed, live, options.limit))
}

struct SearchWorker {
    tx: Sender<SearchRequest>,
    rx: Receiver<SearchResponse>,
}

impl SearchWorker {
    fn spawn(search_backend: SearchBackend) -> Result<Self> {
        let (query_tx, query_rx) = mpsc::channel::<SearchRequest>();
        let (result_tx, result_rx) = mpsc::channel::<SearchResponse>();

        thread::spawn(move || match search_backend {
            SearchBackend::Indexed { db_path, .. } => {
                while let Ok(mut request) = query_rx.recv() {
                    while let Ok(newer_request) = query_rx.try_recv() {
                        request = newer_request;
                    }
                    let results = open_search_database(&db_path)
                        .and_then(|db| search_for_tui(&db, &request.options));
                    if result_tx
                        .send(SearchResponse {
                            options: request.options,
                            results,
                            complete: true,
                            live_backfill: false,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            SearchBackend::Live { root } => {
                while let Ok(mut request) = query_rx.recv() {
                    loop {
                        while let Ok(newer_request) = query_rx.try_recv() {
                            request = newer_request;
                        }
                        let response_options = request.options.clone();
                        let mut next_query = None;
                        let mut latest = Vec::new();
                        let status = search_live_streaming_with_options(
                            &root,
                            &response_options,
                            || {
                                while let Ok(newer_request) = query_rx.try_recv() {
                                    next_query = Some(newer_request);
                                }
                                next_query.is_some()
                            },
                            |_| {},
                            |partial| {
                                latest = partial.to_vec();
                                let _ = result_tx.send(SearchResponse {
                                    options: response_options.clone(),
                                    results: Ok(latest.clone()),
                                    complete: false,
                                    live_backfill: true,
                                });
                            },
                        );

                        if matches!(status, Ok(LiveSearchStatus::Cancelled)) {
                            if let Some(newer_request) = next_query.take() {
                                request = newer_request;
                                continue;
                            }
                        }

                        let results = status.map(|_| latest);
                        if result_tx
                            .send(SearchResponse {
                                options: response_options,
                                results,
                                complete: true,
                                live_backfill: true,
                            })
                            .is_err()
                        {
                            break;
                        }
                        break;
                    }
                }
            }
            SearchBackend::Hybrid { db_path, root } => {
                while let Ok(mut request) = query_rx.recv() {
                    while let Ok(newer_request) = query_rx.try_recv() {
                        request = newer_request;
                    }
                    let response_options = request.options.clone();
                    let indexed = open_search_database(&db_path)
                        .and_then(|db| search_for_tui(&db, &response_options));
                    if request.live_backfill {
                        if let Ok(results) = &indexed {
                            let _ = result_tx.send(SearchResponse {
                                options: response_options.clone(),
                                results: Ok(results.clone()),
                                complete: false,
                                live_backfill: true,
                            });
                        }
                    }

                    let results = if request.live_backfill {
                        indexed.and_then(|_| {
                            open_search_database(&db_path)
                                .and_then(|db| search_hybrid(&db, &root, &response_options))
                        })
                    } else {
                        indexed
                    };
                    if result_tx
                        .send(SearchResponse {
                            options: response_options,
                            results,
                            complete: true,
                            live_backfill: request.live_backfill,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            tx: query_tx,
            rx: result_rx,
        })
    }

    fn submit(&mut self, request: SearchRequest) -> Result<()> {
        self.tx.send(request).context("send search request")
    }

    fn try_recv(&mut self) -> Option<SearchResponse> {
        let mut latest = None;
        while let Ok(response) = self.rx.try_recv() {
            latest = Some(response);
        }
        latest
    }
}

#[derive(Debug, Clone)]
struct SearchRequest {
    options: SearchOptions,
    live_backfill: bool,
}

struct SearchResponse {
    options: SearchOptions,
    results: Result<Vec<SearchResult>>,
    complete: bool,
    live_backfill: bool,
}

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

fn initial_input_mode() -> bool {
    false
}

fn input_mode_after_submit(submitted: bool) -> bool {
    !submitted
}

fn indexed_response_exit_deadline(
    response_complete: bool,
    live_backfill: bool,
    backend_label: &str,
    now: Instant,
) -> Option<Instant> {
    if response_complete && !live_backfill && matches!(backend_label, "indexed" | "hybrid") {
        Some(now + INDEXED_INPUT_GRACE)
    } else {
        None
    }
}

fn input_mode_after_indexed_grace(
    input_mode: bool,
    exit_deadline: Option<Instant>,
    now: Instant,
) -> bool {
    input_mode && exit_deadline.is_none_or(|deadline| now < deadline)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterAction {
    SubmitSearch,
    ConfirmSearch,
    OpenSelection,
    Noop,
}

fn enter_action(
    input_mode: bool,
    should_submit: bool,
    live_backfill: bool,
    has_selection: bool,
    query_ready: bool,
) -> EnterAction {
    if should_submit || live_backfill {
        EnterAction::SubmitSearch
    } else if input_mode && query_ready {
        EnterAction::ConfirmSearch
    } else if has_selection {
        EnterAction::OpenSelection
    } else {
        EnterAction::Noop
    }
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
        backend_label != "live"
            && self.should_submit(query)
            && elapsed >= Duration::from_millis(150)
    }

    fn mark_submitted(&mut self, options: SearchOptions, live_backfill: bool) {
        if live_backfill {
            self.last_live_query = None;
        }
        self.last_submitted = Some(options);
        self.dirty = false;
    }

    fn accepts_response(&self, current_query: &str, response_options: &SearchOptions) -> bool {
        !self.dirty
            && current_query == response_options.query
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

fn result_row<'a>(
    index: usize,
    result: &'a SearchResult,
    source: &'static str,
    mode: QueryMode,
    theme: &Theme,
) -> Row<'a> {
    Row::new([
        Cell::from(if index == 0 { "*" } else { "" }),
        Cell::from(result.name.clone()).style(Style::default().fg(theme.text)),
        Cell::from(result.kind.clone()).style(Style::default().fg(theme.accent)),
        Cell::from(format_size(result.size_bytes)).style(Style::default().fg(theme.warn)),
        Cell::from(format_date(result.modified_at)).style(Style::default().fg(theme.ok)),
        Cell::from(source).style(Style::default().fg(source_color(source, theme))),
        Cell::from(mode.label()).style(Style::default().fg(theme.muted)),
        Cell::from(result.path.clone()).style(Style::default().fg(theme.muted)),
    ])
}

struct TopPanelArgs<'a> {
    query: &'a str,
    root_label: &'a str,
    backend_label: &'static str,
    result_count: usize,
    watch_enabled: bool,
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
            format!("{} results", args.result_count),
            Style::default().fg(args.theme.ok),
        ),
        Span::raw("  "),
        Span::styled(
            if args.watch_enabled {
                "watch on"
            } else {
                "watch off"
            },
            Style::default().fg(if args.watch_enabled {
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

fn top_controls_lines(args: &TopPanelArgs<'_>) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("nav ", Style::default().fg(args.theme.muted)),
            control_segment("/", "search", &args.theme),
            Span::raw("  "),
            control_segment("Esc", "normal", &args.theme),
            Span::raw("  "),
            control_segment("j/k", "move", &args.theme),
            Span::raw("  "),
            control_segment("Enter", "confirm/open", &args.theme),
        ]),
        Line::from(vec![
            Span::styled("file ", Style::default().fg(args.theme.muted)),
            control_segment("o", "open", &args.theme),
            Span::raw("  "),
            control_segment("r", "reveal", &args.theme),
            Span::raw("  "),
            control_segment("y", "copy", &args.theme),
        ]),
        Line::from(vec![
            Span::styled("filters ", Style::default().fg(args.theme.muted)),
            control_segment("m", args.mode.label(), &args.theme),
            Span::raw("  "),
            control_segment("f", &filter_label(args.filters), &args.theme),
            Span::raw("  "),
            Span::styled(
                format!(
                    "ext:{} size:{} date:{}",
                    ext_filter_label(args.filters),
                    size_filter_label(args.filters),
                    date_filter_label(args.filters)
                ),
                Style::default().fg(args.theme.muted),
            ),
        ]),
        Line::from(vec![
            Span::styled("sort ", Style::default().fg(args.theme.muted)),
            control_segment("s", &sort_label(args.sort, args.reverse), &args.theme),
            Span::raw("  "),
            control_segment("S", sort_order_label(args.reverse), &args.theme),
            Span::raw("  "),
            control_segment("t", args.theme.name.label(), &args.theme),
            Span::raw("  "),
            control_segment("?", "help", &args.theme),
        ]),
    ]
}

fn controls_panel_height(args: &TopPanelArgs<'_>) -> u16 {
    top_controls_lines(args).len() as u16 + 2
}

fn top_chrome_height(args: &TopPanelArgs<'_>) -> u16 {
    3 + 3 + controls_panel_height(args)
}

fn control_segment(key: &str, label: &str, theme: &Theme) -> Span<'static> {
    Span::styled(
        format!("[{key} {label}]"),
        Style::default().fg(theme.accent),
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

fn move_selection(state: &mut TableState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }

    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, (len - 1) as isize);
    state.select(Some(next as usize));
}

fn select_first(state: &mut TableState, len: usize) {
    if len > 0 {
        state.select(Some(0));
    }
}

fn select_last(state: &mut TableState, len: usize) {
    if len > 0 {
        state.select(Some(len - 1));
    }
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

fn selected_detail(state: &TableState, results: &[SearchResult]) -> String {
    state
        .selected()
        .and_then(|index| results.get(index))
        .map(|result| {
            format!(
                "{}  created {}  modified {}  {} bytes  {}",
                result.kind,
                format_date(result.created_at),
                format_date(result.modified_at),
                result.size_bytes,
                result.path
            )
        })
        .unwrap_or_else(|| "No selection".to_string())
}

fn apply_local_result_options(
    results: &[SearchResult],
    options: &SearchOptions,
) -> Vec<SearchResult> {
    let mut visible = results
        .iter()
        .filter(|result| local_filter_matches(result, &options.filters))
        .filter(|result| {
            candidate_matches(
                options.mode,
                &options.query,
                [result.name.as_str(), result.path.as_str()],
            )
            .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    sort_results(&mut visible, options);
    visible.truncate(options.limit);
    visible
}

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
    use tempfile::tempdir;

    use crate::db::{local_db_path_for_root, Database, FileRecord, SearchResult};
    use crate::query::{FileKind, SearchFilters, SearchOptions, SortField};
    use crate::tui::theme::Theme;
    use crate::tui::{
        apply_local_result_options, controls_panel_height, enter_action, format_result_summary,
        indexed_response_exit_deadline, initial_input_mode, input_mode_after_indexed_grace,
        input_mode_after_submit, search_backend_for_directory, search_bar_line, search_hybrid,
        should_show_results, sort_label, toggle_sort_order, top_chrome_height, top_controls_lines,
        top_status_line, tui_search_options, EnterAction, SearchBackend, SearchInput,
        SearchRequest, SearchState, SearchWorker, TopPanelArgs, INDEXED_INPUT_GRACE,
        TUI_RESULT_LIMIT,
    };
    use ratatui::text::Line;
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
            mode: crate::query::QueryMode::Contains,
            sort: SortField::Relevance,
            reverse: false,
            filters: &SearchFilters::new(),
            theme,
            status: "ready",
        };
        let status = line_text(&top_status_line(&args));
        let search = line_text(&search_bar_line(&args));
        let controls = top_controls_lines(&args)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(status.contains("root /tmp"));
        assert!(status.contains("ready"));
        assert_eq!(search, "archive");
        assert!(!search.contains("[/ search]"));
        assert!(!search.contains("[m contains]"));
        assert!(controls.contains("[/ search]"));
        assert!(controls.contains("file [o open]"));
        assert!(controls.contains("filters [m contains]"));
        assert!(controls.contains("sort [s relevance asc]"));
        assert!(controls.contains("[S asc]"));
    }

    #[test]
    fn controls_panel_height_fits_all_control_rows() {
        let theme = Theme::from_name(crate::tui::theme::ThemeName::Default);
        let args = TopPanelArgs {
            query: "archive",
            root_label: "/tmp",
            backend_label: "indexed",
            result_count: 50,
            watch_enabled: false,
            mode: crate::query::QueryMode::Contains,
            sort: SortField::Relevance,
            reverse: false,
            filters: &SearchFilters::new(),
            theme,
            status: "ready",
        };

        assert_eq!(
            controls_panel_height(&args),
            top_controls_lines(&args).len() as u16 + 2
        );
        assert_eq!(
            top_chrome_height(&args),
            3 + 3 + controls_panel_height(&args)
        );
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
    fn tui_starts_in_normal_mode_so_shortcuts_work() {
        assert!(!initial_input_mode());
    }

    #[test]
    fn submitting_search_exits_input_mode() {
        assert!(!input_mode_after_submit(true));
        assert!(input_mode_after_submit(false));
    }

    #[test]
    fn indexed_response_completion_keeps_input_mode_until_grace_expires() {
        let now = Instant::now();
        let deadline = indexed_response_exit_deadline(true, false, "indexed", now);
        let expected_grace = Duration::from_millis(1500);

        assert_eq!(INDEXED_INPUT_GRACE, expected_grace);
        assert_eq!(deadline, Some(now + expected_grace));
        assert!(input_mode_after_indexed_grace(
            true,
            deadline,
            now + expected_grace - Duration::from_millis(1)
        ));
        assert!(!input_mode_after_indexed_grace(
            true,
            deadline,
            now + expected_grace
        ));
        assert!(!input_mode_after_indexed_grace(false, deadline, now));
    }

    #[test]
    fn indexed_response_grace_starts_only_for_complete_indexed_results() {
        let now = Instant::now();

        assert!(indexed_response_exit_deadline(true, false, "indexed", now).is_some());
        assert!(indexed_response_exit_deadline(true, false, "hybrid", now).is_some());
        assert!(indexed_response_exit_deadline(false, false, "indexed", now).is_none());
        assert!(indexed_response_exit_deadline(true, true, "hybrid", now).is_none());
        assert!(indexed_response_exit_deadline(true, false, "live", now).is_none());
    }

    #[test]
    fn enter_in_input_mode_confirms_search_instead_of_opening_selection() {
        assert_eq!(
            enter_action(true, false, false, true, true),
            EnterAction::ConfirmSearch
        );
        assert_eq!(
            enter_action(false, false, false, true, true),
            EnterAction::OpenSelection
        );
    }

    #[test]
    fn dirty_filter_change_keeps_current_results_visible() {
        let mut state = SearchState::default();
        state.mark_submitted(SearchOptions::new("archive"), false);
        state.mark_dirty();

        assert!(state.should_render_results("archive"));
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
                live_backfill: false,
            })
            .expect("request sends");

        let response = (0..20)
            .find_map(|_| {
                thread::sleep(Duration::from_millis(10));
                worker.try_recv()
            })
            .expect("worker returns open error");

        assert_eq!(response.options.query, "archive");
        assert!(response.results.is_err());
        assert!(response.complete);
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
}

//! Interactive settings editor for `lctr config`. Lists each setting with its
//! current value, the available choices, and a description of what it does;
//! changes preview live (the theme setting recolours the editor itself) and are
//! written to `config.toml` on save.

use std::io;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::{key_choices, key_description, key_label, Config, KEYS};
use crate::tui::theme::{Theme, ThemeName};
use crate::tui::TerminalGuard;

/// Launch the settings TUI. Loads config, edits in place, saves on request.
pub fn run() -> Result<()> {
    let _guard = TerminalGuard::enter_default_cursor().context("enter terminal")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    run_loop(&mut terminal)
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut config = Config::load();
    let mut selected = 0usize;
    let mut dirty = false;
    let mut status =
        String::from("\u{2191}\u{2193} move   \u{2190}\u{2192} change   s save   q quit");

    loop {
        // Theme previews live as the user changes the `theme` setting.
        let theme = Theme::from_name(ThemeName::parse(&config.theme));
        terminal.draw(|frame| draw(frame, &config, selected, dirty, &status, &theme))?;

        if !event::poll(std::time::Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if dirty {
                    status =
                        "unsaved changes \u{2014} press s to save, or q again to discard".into();
                    dirty = false; // next q quits
                } else {
                    break;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                selected = (selected + 1) % KEYS.len();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                selected = (selected + KEYS.len() - 1) % KEYS.len();
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter => {
                cycle(&mut config, KEYS[selected], 1);
                dirty = true;
                status = "changed \u{2014} press s to save".into();
            }
            KeyCode::Char('h') | KeyCode::Left => {
                cycle(&mut config, KEYS[selected], -1);
                dirty = true;
                status = "changed \u{2014} press s to save".into();
            }
            KeyCode::Char('s') => match config.save() {
                Ok(()) => {
                    dirty = false;
                    status = format!("saved to {}", Config::path()?.display());
                }
                Err(error) => status = format!("save failed: {error}"),
            },
            _ => {}
        }
    }
    Ok(())
}

/// Advance the value of `key` by `dir` (+1 / -1) through its allowed choices.
fn cycle(config: &mut Config, key: &str, dir: i32) {
    let choices = key_choices(key);
    if choices.is_empty() {
        return;
    }
    let current = config.get(key).unwrap_or_default();
    let index = choices.iter().position(|&c| c == current).unwrap_or(0);
    let len = choices.len() as i32;
    let next = (((index as i32 + dir) % len + len) % len) as usize;
    let _ = config.set(key, choices[next]);
}

fn draw(
    frame: &mut Frame,
    config: &Config,
    selected: usize,
    dirty: bool,
    status: &str,
    theme: &Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let title = format!("lctr settings{}", if dirty { "  *unsaved" } else { "" });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.accent)),
        ),
        chunks[0],
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(chunks[1]);

    render_list(frame, body[0], config, selected, theme);
    render_detail(frame, body[1], config, selected, theme);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status.to_string(),
            Style::default().fg(theme.muted),
        ))),
        chunks[2],
    );
}

fn render_list(frame: &mut Frame, area: Rect, config: &Config, selected: usize, theme: &Theme) {
    let rows = KEYS.iter().enumerate().map(|(index, &key)| {
        let value = config.get(key).unwrap_or_default();
        let is_sel = index == selected;
        let marker = if is_sel { "\u{25b8} " } else { "  " };
        let key_style = if is_sel {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.accent)),
            Span::styled(format!("{:<14}", key_label(key)), key_style),
            Span::styled(value, Style::default().fg(theme.ok)),
        ])
    });
    frame.render_widget(
        Paragraph::new(rows.collect::<Vec<_>>()).block(
            Block::default()
                .title("settings")
                .title_style(Style::default().fg(theme.muted))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.muted)),
        ),
        area,
    );
}

fn render_detail(frame: &mut Frame, area: Rect, config: &Config, selected: usize, theme: &Theme) {
    let key = KEYS[selected];
    let current = config.get(key).unwrap_or_default();

    let mut lines = vec![
        Line::from(Span::styled(
            key_label(key).to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            key_description(key).to_string(),
            Style::default().fg(theme.text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "choices  (\u{2190}/\u{2192} to change)",
            Style::default().fg(theme.muted),
        )),
    ];

    for choice in key_choices(key) {
        let active = choice == current;
        let marker = if active { "\u{25cf} " } else { "\u{25cb} " };
        let style = if active {
            Style::default().fg(theme.ok).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {marker}"), style),
            Span::styled(choice.to_string(), style),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title("about")
                .title_style(Style::default().fg(theme.muted))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.muted)),
        ),
        area,
    );
}

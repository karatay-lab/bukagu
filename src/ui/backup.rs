//! Minimal TUI for a single v3 backup run.
//!
//! A `tokio::task::spawn_blocking` worker (honoring the project's tokio decision)
//! fetches the recipient and runs [`crate::backup::run_backup`], streaming
//! [`BackupEvent`]s over a `std::sync::mpsc` channel. The synchronous render loop
//! drains the channel each tick and redraws a small status panel (Fetching key →
//! Archiving → Pruning → Done/Error). It is uninterruptible while writing; once
//! finished, any key returns to the home.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::backup::key::HttpKeyProvider;
use crate::backup::{self, BackupEvent, BackupReport};
use crate::credentials::Credentials;
use crate::theme;
use crate::ui::widgets;

/// How the backup screen ended — handed back to the home loop.
pub enum BackupOutcome {
    /// The backup finished; carries the report for the summary + store stamp.
    Done(BackupReport),
    /// The backup failed; carries a flattened, user-facing message.
    Failed(String),
}

/// Worker → render-loop messages.
enum Msg {
    Event(BackupEvent),
    Done(BackupReport),
    Error(String),
}

/// Render-loop tick / channel-drain cadence.
const TICK: Duration = Duration::from_millis(100);

/// Braille spinner frames shown while a backup runs.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Run the backup screen for a resolved source + backup dir. Owns the terminal
/// for its lifetime and always restores it before returning.
pub async fn run(source: PathBuf, backup_dir: PathBuf, retention: usize) -> Result<BackupOutcome> {
    let (tx, rx) = mpsc::channel::<Msg>();

    let worker = tokio::task::spawn_blocking(move || {
        // The home path always fetches from the live API (the offline `--recipient`
        // override is CLI-only).
        let provider = match Credentials::load() {
            Ok(creds) => HttpKeyProvider::new(creds),
            Err(e) => {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
                return;
            }
        };
        let mut on_event = |event: BackupEvent| {
            let _ = tx.send(Msg::Event(event));
        };
        match backup::run_backup(
            &source,
            &backup_dir,
            &provider,
            retention,
            false,
            &mut on_event,
        ) {
            Ok(report) => {
                let _ = tx.send(Msg::Done(report));
            }
            Err(e) => {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        }
    });

    let mut terminal = ratatui::init();
    let outcome = render_loop(&mut terminal, &rx);
    ratatui::restore();
    let _ = worker.await; // worker is finished once it has sent its terminal message
    outcome
}

/// The backup screen's state.
struct State {
    status: String,
    bytes: u64,
    tick: usize,
    finished: Option<BackupOutcome>,
}

fn render_loop(terminal: &mut DefaultTerminal, rx: &Receiver<Msg>) -> Result<BackupOutcome> {
    let mut state = State {
        status: "Starting…".to_string(),
        bytes: 0,
        tick: 0,
        finished: None,
    };

    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Event(BackupEvent::FetchingKey) => {
                    state.status = "Fetching the encryption key…".into();
                }
                Msg::Event(BackupEvent::Archiving) => {
                    state.status = "Archiving and encrypting…".into();
                }
                Msg::Event(BackupEvent::Progress { bytes }) => state.bytes = bytes,
                Msg::Event(BackupEvent::Pruning) => state.status = "Pruning old backups…".into(),
                Msg::Done(report) => state.finished = Some(BackupOutcome::Done(report)),
                Msg::Error(e) => state.finished = Some(BackupOutcome::Failed(e)),
            }
        }

        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Ignore input while writing — never interrupt a half-written archive.
            // Once finished, any key returns to the home.
            if state.finished.is_some() {
                break;
            }
        }

        state.tick = state.tick.wrapping_add(1);
    }

    Ok(state
        .finished
        .unwrap_or_else(|| BackupOutcome::Failed("the backup worker stopped unexpectedly".into())))
}

fn draw(frame: &mut Frame, state: &State) {
    let area = centered(frame.area(), 64, 9);
    let block = Block::bordered()
        .title(Span::styled(
            " backup ",
            Style::default()
                .fg(theme::HEADER)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme::GOLD));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let lines = match &state.finished {
        None => {
            let spin = SPINNER[state.tick % SPINNER.len()];
            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("{spin}  {}", state.status),
                    Style::default().fg(theme::AMBER),
                )),
            ];
            if state.bytes > 0 {
                lines.push(Line::from(Span::styled(
                    format!("    {} written", widgets::human_bytes(state.bytes)),
                    Style::default().fg(theme::COPY),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  the source is never modified",
                Style::default().fg(theme::TEXT_DIM),
            )));
            lines
        }
        Some(BackupOutcome::Done(report)) => vec![
            Line::from(""),
            Line::from(Span::styled(
                "✓ Backup complete",
                Style::default()
                    .fg(theme::GOLD)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    "  {} file(s) · {} encrypted",
                    report.files,
                    widgets::human_bytes(report.bytes)
                ),
                Style::default().fg(theme::COPY),
            )),
            Line::from(Span::styled(
                format!("  {}", report.archive_path.display()),
                Style::default().fg(theme::TEXT_DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  press any key to return",
                Style::default().fg(theme::TEXT_DIM),
            )),
        ],
        Some(BackupOutcome::Failed(msg)) => vec![
            Line::from(""),
            Line::from(Span::styled(
                "✗ Backup failed",
                Style::default()
                    .fg(theme::ERROR)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  {msg}"),
                Style::default().fg(theme::TEXT),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  press any key to return",
                Style::default().fg(theme::TEXT_DIM),
            )),
        ],
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// A `width`×`height` rectangle centered within `area` (clamped to fit).
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(state: &State) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|f| draw(f, state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn running_screen_shows_status_and_bytes() {
        let state = State {
            status: "Archiving and encrypting…".into(),
            bytes: 2048,
            tick: 0,
            finished: None,
        };
        let out = render(&state);
        assert!(out.contains("backup"), "panel title drawn");
        assert!(out.contains("Archiving"), "current status shown");
        assert!(out.contains("written"), "byte counter shown");
    }

    #[test]
    fn failed_screen_shows_the_error() {
        let state = State {
            status: String::new(),
            bytes: 0,
            tick: 0,
            finished: Some(BackupOutcome::Failed("no API token".into())),
        };
        let out = render(&state);
        assert!(out.contains("failed"), "failure is announced");
        assert!(out.contains("no API token"), "the error message is shown");
    }
}

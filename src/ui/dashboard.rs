//! The sync dashboard for returning runs (Step 5).
//!
//! A background worker — a `tokio::task::spawn_blocking` task, honoring the
//! project's tokio concurrency decision — runs scan → diff for every destination
//! and streams [`Message`]s over a channel. Once the user confirms (or `--yes`),
//! a second worker applies the plans. The render loop polls the terminal and
//! drains the channel each tick, redrawing the [`App`] state machine.
//!
//! The worker uses a `std::sync::mpsc` channel because the consumer is the
//! synchronous render loop, which polls it non-blockingly via `try_recv` — the
//! natural fit for a TUI tick loop. The sync engine itself is fully synchronous.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::app::{App, DestPlan, Phase, RunOptions};
use crate::core::{Stats, SyncAction, apply, diff, scan};
use crate::message::Message;
use crate::store::Config;
use crate::theme;
use crate::ui::widgets;

/// How the dashboard ended — drives the post-restore stdout summary and exit code.
pub enum Outcome {
    /// A real sync wrote to the destinations; carries the rolled-up totals.
    Applied(Stats),
    /// A `--dry-run` preview finished; nothing was written.
    Preview(Stats),
    /// Every destination already matched the source.
    UpToDate,
    /// The user quit before applying.
    Cancelled,
}

/// How long the render loop waits for input each tick before redrawing. Keeps the
/// channel draining smoothly while the worker streams progress.
const TICK: Duration = Duration::from_millis(100);

/// Run the sync dashboard for a loaded [`Config`]. Owns the terminal for its
/// lifetime and always restores it before returning.
pub async fn run(config: Config, opts: RunOptions) -> Result<Outcome> {
    let (tx, rx) = mpsc::channel::<Message>();

    // Scan + diff every destination off the UI thread.
    let scan_cfg = config.clone();
    let scan_tx = tx.clone();
    tokio::task::spawn_blocking(move || scan_all(&scan_cfg, opts.delete, &scan_tx));

    let mut app = App::new(config, opts);
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app, &rx, &tx);
    ratatui::restore();
    result
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &Receiver<Message>,
    tx: &Sender<Message>,
) -> Result<Outcome> {
    let mut apply_started = false;

    loop {
        terminal.draw(|frame| draw(frame, app))?;

        // Drain everything the worker has sent so far.
        while let Ok(msg) = rx.try_recv() {
            app.on_message(msg);
        }

        // Kick off the apply worker exactly once, when we enter Applying (either
        // by confirming the Review screen or via `--yes`).
        if app.phase == Phase::Applying && !apply_started {
            apply_started = true;
            let source = app.config.source.clone();
            let plans = app.plans.clone();
            let dry_run = app.opts.dry_run;
            let apply_tx = tx.clone();
            tokio::task::spawn_blocking(move || apply_all(&source, &plans, dry_run, &apply_tx));
        }

        // Non-blocking input poll so the channel keeps draining while idle.
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(app, key.code);
        }

        if app.should_quit {
            // Dismissing the error screen surfaces the failure as an error exit.
            if let Some(err) = app.error.take() {
                return Err(anyhow!(err));
            }
            return Ok(outcome_of(app));
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode) {
    match app.phase {
        Phase::Scanning => {
            if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                app.quit();
            }
        }
        Phase::Review => match code {
            KeyCode::Up | KeyCode::Char('k') => app.scroll_up(),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_down(),
            KeyCode::PageUp => app.page_up(),
            KeyCode::PageDown => app.page_down(),
            KeyCode::Enter | KeyCode::Char('y') => app.confirm(),
            KeyCode::Char('q') | KeyCode::Esc => app.quit(),
            _ => {}
        },
        // No quitting mid-write — apply is quick and local, and we never want to
        // leave the terminal restored with a copy half-finished.
        Phase::Applying => {}
        Phase::Done | Phase::Error => {
            if matches!(code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter) {
                app.quit();
            }
        }
    }
}

/// Map the finished state to an [`Outcome`] for the post-restore summary.
fn outcome_of(app: &App) -> Outcome {
    match app.stats.clone() {
        // Quit before anything finished.
        None => Outcome::Cancelled,
        Some(_) if app.nothing_to_do() => Outcome::UpToDate,
        Some(stats) if app.opts.dry_run => Outcome::Preview(stats),
        Some(stats) => Outcome::Applied(stats),
    }
}

// --- Background worker -----------------------------------------------------

/// Scan the source once, then guard + scan + diff each destination, streaming a
/// [`Message::Scanned`] per root and a [`Message::Planned`] per destination. Any
/// failure is reported as a single [`Message::Error`].
fn scan_all(config: &Config, delete_extras: bool, tx: &Sender<Message>) {
    if let Err(e) = scan_all_inner(config, delete_extras, tx) {
        let _ = tx.send(Message::Error {
            message: format!("{e:#}"),
        });
    }
}

fn scan_all_inner(config: &Config, delete_extras: bool, tx: &Sender<Message>) -> Result<()> {
    let source = &config.source;
    let source_scan =
        scan::scan(source).with_context(|| format!("scanning source {}", source.display()))?;
    let _ = tx.send(Message::Scanned {
        root: source.clone(),
        count: source_scan.len(),
    });

    for dest in &config.destinations {
        // Guardrail FIRST — refuse (and report) any mapping that could reach the
        // read-only source before we read or plan anything.
        apply::guard_destination(source, dest)
            .with_context(|| format!("checking destination {}", dest.display()))?;

        let dest_scan =
            scan::scan(dest).with_context(|| format!("scanning destination {}", dest.display()))?;
        let _ = tx.send(Message::Scanned {
            root: dest.clone(),
            count: dest_scan.len(),
        });

        let plan = diff::diff(source, &source_scan, dest, &dest_scan, delete_extras)
            .with_context(|| format!("planning sync for {}", dest.display()))?;
        let _ = tx.send(Message::Planned {
            destination: dest.clone(),
            plan,
        });
    }
    Ok(())
}

/// Apply each destination's plan, streaming per-action progress, then a single
/// rolled-up [`Message::Done`]. `dry_run` validates and reports but writes nothing.
fn apply_all(source: &Path, plans: &[DestPlan], dry_run: bool, tx: &Sender<Message>) {
    if let Err(e) = apply_all_inner(source, plans, dry_run, tx) {
        let _ = tx.send(Message::Error {
            message: format!("{e:#}"),
        });
    }
}

fn apply_all_inner(
    source: &Path,
    plans: &[DestPlan],
    dry_run: bool,
    tx: &Sender<Message>,
) -> Result<()> {
    let mut total = Stats::default();
    for dp in plans {
        apply::apply_with_progress(source, &dp.destination, &dp.plan, dry_run, |done, t| {
            let _ = tx.send(Message::Applied {
                destination: dp.destination.clone(),
                done,
                total: t,
            });
        })
        .with_context(|| format!("applying to {}", dp.destination.display()))?;
        accumulate(&mut total, &dp.plan.stats);
    }
    let _ = tx.send(Message::Done { stats: total });
    Ok(())
}

fn accumulate(total: &mut Stats, s: &Stats) {
    total.create_dirs += s.create_dirs;
    total.copies += s.copies;
    total.overwrites += s.overwrites;
    total.deletes += s.deletes;
    total.bytes += s.bytes;
}

// --- Rendering -------------------------------------------------------------

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::new().style(Style::default().bg(theme::BG).fg(theme::TEXT)),
        area,
    );

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);

    draw_header(frame, app, header);
    match app.phase {
        Phase::Scanning => draw_scanning(frame, app, body),
        Phase::Review => draw_review(frame, app, body),
        Phase::Applying => draw_applying(frame, app, body),
        Phase::Done => draw_done(frame, app, body),
        Phase::Error => draw_error(frame, app, body),
    }
    draw_footer(frame, app, footer);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            "bukagu",
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  sync dashboard", Style::default().fg(theme::TEXT_DIM)),
    ]);
    let phase = Line::from(Span::styled(
        phase_label(app.phase),
        Style::default()
            .fg(theme::HEADER)
            .add_modifier(Modifier::BOLD),
    ));
    let source = Line::from(vec![
        Span::styled("Source:  ", Style::default().fg(theme::TEXT_DIM)),
        Span::styled(
            app.config.source.display().to_string(),
            Style::default().fg(theme::AMBER),
        ),
    ]);

    let mut flags = vec![
        Span::styled("Destinations: ", Style::default().fg(theme::TEXT_DIM)),
        Span::styled(
            app.expected_plans().to_string(),
            Style::default().fg(theme::TEXT),
        ),
    ];
    if app.opts.dry_run {
        flags.push(Span::styled(
            "   [dry-run]",
            Style::default().fg(theme::GOLD),
        ));
    }
    if app.opts.delete {
        flags.push(Span::styled(
            "   [delete-extras]",
            Style::default().fg(theme::DELETE),
        ));
    }
    if app.opts.yes {
        flags.push(Span::styled(
            "   [auto-yes]",
            Style::default().fg(theme::TEXT_DIM),
        ));
    }

    frame.render_widget(
        Paragraph::new(vec![title, phase, source, Line::from(flags)]),
        area,
    );
}

fn phase_label(p: Phase) -> &'static str {
    match p {
        Phase::Scanning => "Scanning source and destinations…",
        Phase::Review => "Review the planned changes",
        Phase::Applying => "Applying changes…",
        Phase::Done => "Done",
        Phase::Error => "Error",
    }
}

fn panel(title: &str, border: Color) -> Block<'_> {
    Block::bordered()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(border))
}

fn draw_scanning(frame: &mut Frame, app: &App, area: Rect) {
    let [bar, _rest] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    let done = app.plans.len();
    let total = app.expected_plans().max(1);
    let ratio = (done as f64 / total as f64).clamp(0.0, 1.0);
    let gauge = Gauge::default()
        .block(panel("scanning", theme::PANEL_BORDER))
        .gauge_style(Style::default().fg(theme::PERSIMMON).bg(theme::DARK_MAROON))
        .ratio(ratio)
        .label(format!("{done}/{total} destinations planned"));
    frame.render_widget(gauge, bar);
}

fn draw_review(frame: &mut Frame, app: &App, area: Rect) {
    let lines = review_lines(app);
    // Clamp the scroll so we never page past the end of the list.
    let inner_h = area.height.saturating_sub(2).max(1) as usize;
    let max_off = lines.len().saturating_sub(inner_h);
    let offset = app.review_scroll.min(max_off) as u16;

    let title = if app.opts.dry_run {
        "planned changes (preview)"
    } else {
        "planned changes"
    };
    let p = Paragraph::new(lines)
        .block(panel(title, theme::PANEL_BORDER))
        .scroll((offset, 0));
    frame.render_widget(p, area);
}

/// Build the Review body: a destination header followed by its color-coded
/// actions, repeated for every destination.
fn review_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(app.review_line_count());
    for dp in &app.plans {
        let header = format!(
            "{}  ({} change(s))",
            dp.destination.display(),
            dp.plan.actions.len()
        );
        lines.push(Line::from(Span::styled(
            header,
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        )));
        for action in &dp.plan.actions {
            let color = widgets::action_color(action);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {:<7}", action.label()),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    action.rel_path().display().to_string(),
                    Style::default().fg(theme::TEXT),
                ),
                size_suffix(action),
            ]));
        }
    }
    lines
}

/// A dim ` (size)` suffix for actions that write bytes; empty otherwise.
fn size_suffix(action: &SyncAction) -> Span<'static> {
    match action {
        SyncAction::Copy { size, .. } | SyncAction::Overwrite { size, .. } => Span::styled(
            format!("   {}", widgets::human_bytes(*size)),
            Style::default().fg(theme::TEXT_DIM),
        ),
        _ => Span::raw(""),
    }
}

fn draw_applying(frame: &mut Frame, app: &App, area: Rect) {
    let [bar, _rest] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    let done = app.applied_actions();
    let total = app.total_actions();
    let ratio = (done as f64 / total.max(1) as f64).clamp(0.0, 1.0);
    let gauge = Gauge::default()
        .block(panel("applying", theme::PANEL_BORDER))
        .gauge_style(Style::default().fg(theme::ORANGE).bg(theme::DARK_MAROON))
        .ratio(ratio)
        .label(format!("{done}/{total} actions"));
    frame.render_widget(gauge, bar);
}

fn draw_done(frame: &mut Frame, app: &App, area: Rect) {
    let stats = app.stats.clone().unwrap_or_default();
    let title = if app.opts.dry_run {
        "preview"
    } else {
        "summary"
    };

    let mut lines: Vec<Line> = Vec::new();
    if app.nothing_to_do() {
        lines.push(Line::from(Span::styled(
            "Everything is already in sync — nothing to do.",
            Style::default()
                .fg(theme::SUCCESS)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        let head = if app.opts.dry_run {
            "Dry run — these changes would be made (nothing was written):"
        } else {
            "Sync complete. Applied:"
        };
        lines.push(Line::from(Span::styled(
            head,
            Style::default()
                .fg(theme::SUCCESS)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        lines.push(stat_line(
            "directories created",
            stats.create_dirs,
            theme::CREATE_DIR,
        ));
        lines.push(stat_line("files copied", stats.copies, theme::COPY));
        lines.push(stat_line(
            "files updated",
            stats.overwrites,
            theme::OVERWRITE,
        ));
        lines.push(stat_line("files deleted", stats.deletes, theme::DELETE));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("    data:  ", Style::default().fg(theme::TEXT_DIM)),
            Span::styled(
                widgets::human_bytes(stats.bytes),
                Style::default().fg(theme::AMBER),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel(title, theme::PANEL_BORDER)),
        area,
    );
}

fn stat_line(label: &str, n: usize, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {n:>5}  "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(label.to_string(), Style::default().fg(theme::TEXT)),
    ])
}

fn draw_error(frame: &mut Frame, app: &App, area: Rect) {
    let msg = app.error.clone().unwrap_or_else(|| "Unknown error.".into());
    let lines = vec![
        Line::from(Span::styled(
            "The sync could not complete:",
            Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(msg, Style::default().fg(theme::TEXT))),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("error", theme::ERROR))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let keys = match app.phase {
        Phase::Scanning => "[q] cancel",
        Phase::Review => "↑/↓ scroll   PgUp/PgDn page   [Enter/y] apply   [q] cancel",
        Phase::Applying => "applying… please wait",
        Phase::Done => "[Enter]/[q] quit",
        Phase::Error => "[q] quit",
    };
    let key_line = Line::from(Span::styled(keys, Style::default().fg(theme::TEXT_DIM)));
    let status_color = if app.phase == Phase::Error {
        theme::ERROR
    } else {
        theme::ACCENT
    };
    let status_line = Line::from(Span::styled(
        app.status.clone(),
        Style::default().fg(status_color),
    ));
    frame.render_widget(Paragraph::new(vec![key_line, status_line]), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use tempfile::tempdir;

    /// Collect everything currently queued on the receiver.
    fn drain(rx: &Receiver<Message>) -> Vec<Message> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    #[test]
    fn scan_all_streams_scanned_and_planned() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.txt", "hello");

        let config = Config {
            source: src.path().to_path_buf(),
            destinations: vec![dst.path().to_path_buf()],
        };
        let (tx, rx) = mpsc::channel();
        scan_all(&config, false, &tx);
        let msgs = drain(&rx);

        let scanned = msgs
            .iter()
            .filter(|m| matches!(m, Message::Scanned { .. }))
            .count();
        assert_eq!(scanned, 2, "one Scanned for source, one per destination");

        let planned: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                Message::Planned { plan, .. } => Some(plan),
                _ => None,
            })
            .collect();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].stats.copies, 1);
    }

    #[test]
    fn apply_all_writes_then_reports_done() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.txt", "hello");

        // Build the plan via a scan pass.
        let config = Config {
            source: src.path().to_path_buf(),
            destinations: vec![dst.path().to_path_buf()],
        };
        let (tx, rx) = mpsc::channel();
        scan_all(&config, false, &tx);
        let plan = drain(&rx)
            .into_iter()
            .find_map(|m| match m {
                Message::Planned { plan, .. } => Some(plan),
                _ => None,
            })
            .expect("a plan");

        let plans = vec![DestPlan {
            destination: dst.path().to_path_buf(),
            plan,
        }];
        let (tx, rx) = mpsc::channel();
        apply_all(src.path(), &plans, false, &tx);
        let msgs = drain(&rx);

        assert_eq!(
            fs::read_to_string(dst.path().join("a.txt")).unwrap(),
            "hello",
            "destination now mirrors the source"
        );
        match msgs.last() {
            Some(Message::Done { stats }) => assert_eq!(stats.copies, 1),
            other => panic!("expected a final Done, got {other:?}"),
        }
    }

    #[test]
    fn scan_all_reports_error_for_unsafe_destination() {
        let src = tempdir().unwrap();
        let inside = src.path().join("backup");
        fs::create_dir_all(&inside).unwrap();

        // A destination *inside* the source must be refused by the guardrail.
        let config = Config {
            source: src.path().to_path_buf(),
            destinations: vec![inside],
        };
        let (tx, rx) = mpsc::channel();
        scan_all(&config, false, &tx);
        let msgs = drain(&rx);

        assert!(
            msgs.iter().any(
                |m| matches!(m, Message::Error { message } if message.contains("refusing to sync"))
            ),
            "guardrail must reject a destination inside the source"
        );
    }
}

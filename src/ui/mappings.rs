//! The v2 **file-mapping** screen: a two-pane page where you map a `source file →
//! destination file(s)`, review a summary, then sync — writing each source's
//! contents into its targets with a [`crate::core::banner`] on top.
//!
//! Reached from the home screen's "Map files" action ([`crate::ui::browser`] →
//! [`crate::ui::browser::HomeIntent::Mappings`]). It owns the terminal for its
//! lifetime and always restores it before returning a [`MapSession`] back to
//! `main`, which persists the edited mappings (and stamps `last_sync` on a real
//! sync).
//!
//! Layout (the **Map** screen):
//!   * **Sources** (left) — every file under the read-only source. `Space` picks
//!     **one** source (sticky, shown with a distinct background); `Space` again
//!     deselects it.
//!   * **Destinations** (right) — destination files, filtered by an `a`-toggled
//!     view: **available** (not yet mapped) or **assigned** (already mapped). In
//!     the available view `Space` multi-selects targets (distinct color, re-press
//!     to deselect); in the assigned view `Space`/`d` unmaps a target.
//!   * **Info** (below, full width) — an accordion: one row per destination
//!     folder with its size / file / mapped counts; `Enter` expands it to the
//!     mappings written into that folder.
//!
//! `Enter` saves the selected source → selected targets as a mapping (merging into
//! an existing mapping for that source, and refusing a target already used — the
//! duplicate-target rule). `s` opens the **Summary** (per-target create/update/ok +
//! counts, blocking issues) → **Done** (`Last sync: OK · <ts>`). The apply is
//! synchronous: a mapping set is a handful of files, so there's no streaming worker
//! — unlike the v1 folder [`crate::ui::dashboard`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use walkdir::WalkDir;

use crate::app::RunOptions;
use crate::core::mapping::{self, Issue, MappingPlan, TargetState};
use crate::store::{self, Config, Mapping};
use crate::theme;
use crate::ui::widgets::human_bytes;

/// What the mapping screen hands back to `main`.
pub struct MapSession {
    /// The (possibly edited) mappings — persisted to the store either way.
    pub mappings: Vec<Mapping>,
    /// Whether a real (non-dry-run) sync wrote files, so `main` stamps `last_sync`.
    pub synced: bool,
    /// A one-line summary for the home's "Last run" line (empty if no sync ran).
    pub summary: String,
}

/// Which screen the mapping flow is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// The two-pane map page (sources · destinations · info accordion).
    Map,
    /// The computed plan: per-target state, counts, and any blocking issues.
    Summary,
    /// A real or dry-run sync finished.
    Done,
    /// A blocking error (apply failure).
    Error,
}

/// Which pane on the Map screen has focus (drives the cursor + the gold border).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sources,
    Destinations,
    Info,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Pane::Sources => Pane::Destinations,
            Pane::Destinations => Pane::Info,
            Pane::Info => Pane::Sources,
        }
    }

    fn prev(self) -> Self {
        match self {
            Pane::Sources => Pane::Info,
            Pane::Destinations => Pane::Sources,
            Pane::Info => Pane::Destinations,
        }
    }
}

/// Which destination files the right pane shows — toggled with `a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestView {
    /// Files not yet mapped — selectable as targets.
    Available,
    /// Files already mapped — reviewable / unmappable.
    Assigned,
}

/// Which button the "exit the mapping tab?" dialog has focused. Cancel is the
/// default so an accidental Enter keeps you on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitChoice {
    Confirm,
    Cancel,
}

impl ExitChoice {
    fn toggle(self) -> Self {
        match self {
            ExitChoice::Confirm => ExitChoice::Cancel,
            ExitChoice::Cancel => ExitChoice::Confirm,
        }
    }
}

/// One destination file discovered on disk (under a destination root).
struct DestFile {
    /// Index into [`MapApp::dest_roots`] this file lives under.
    root_idx: usize,
    /// Absolute path (the value stored as a mapping target).
    abs: PathBuf,
    /// Path relative to its destination root (for display).
    rel: PathBuf,
    /// File size in bytes (for the info accordion's roll-up).
    size: u64,
}

/// One row in the destinations "available" accordion: a folder header (a
/// destination root, by index) or one of its unmapped files (index into
/// [`MapApp::dest_files`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AvailRow {
    Folder(usize),
    File(usize),
}

/// The mapping screen's full state. Kept terminal-free where it matters so the
/// transitions are unit-testable.
struct MapApp {
    // --- fixed context ---
    source: PathBuf,
    dest_roots: Vec<PathBuf>,
    opts: RunOptions,

    /// Which screen is showing.
    screen: Screen,

    // --- the editable result ---
    mappings: Vec<Mapping>,

    // --- discovered files (scanned once at construction) ---
    /// Files under the source, relative to it.
    source_files: Vec<PathBuf>,
    /// Files under every destination root.
    dest_files: Vec<DestFile>,

    // --- map-page interaction ---
    focus: Pane,
    dest_view: DestView,
    /// Destination roots expanded in the available accordion (`Space` on a folder).
    dest_open: HashSet<usize>,
    /// Cursor row within each pane (clamped on use).
    src_cursor: usize,
    dest_cursor: usize,
    info_cursor: usize,
    /// The chosen source (relative path), shown with a sticky background.
    selected_source: Option<PathBuf>,
    /// The chosen target files (absolute) awaiting a save.
    selected_targets: HashSet<PathBuf>,
    /// Which destination folder's mappings are expanded in the info accordion.
    info_expanded: Option<usize>,

    // --- summary / apply ---
    plan: Option<MappingPlan>,
    issues: Vec<Issue>,
    summary_scroll: usize,
    error_msg: Option<String>,

    // --- result ---
    synced: bool,
    synced_at: Option<String>,
    summary: String,

    // --- chrome ---
    /// The "exit the mapping tab?" dialog and which button it has focused, or
    /// `None` while it's hidden.
    exit_prompt: Option<ExitChoice>,
    status: String,
    status_is_error: bool,
    done: bool,
}

impl MapApp {
    fn new(config: &Config, mappings: Vec<Mapping>, opts: RunOptions) -> Self {
        let source = canonical(&config.source);
        let dest_roots: Vec<PathBuf> = config.destinations.iter().map(|d| canonical(d)).collect();
        let source_files = scan_files(&source)
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        let mut dest_files = Vec::new();
        for (root_idx, root) in dest_roots.iter().enumerate() {
            for (rel, size) in scan_files(root) {
                dest_files.push(DestFile {
                    root_idx,
                    abs: root.join(&rel),
                    rel,
                    size,
                });
            }
        }
        // A single destination folder starts expanded (nothing to "open"); with
        // several, they start collapsed so the user opens the one they want.
        let dest_open = if dest_roots.len() == 1 {
            HashSet::from([0])
        } else {
            HashSet::new()
        };
        Self {
            source,
            dest_roots,
            opts,
            screen: Screen::Map,
            mappings,
            source_files,
            dest_files,
            focus: Pane::Sources,
            dest_view: DestView::Available,
            dest_open,
            src_cursor: 0,
            dest_cursor: 0,
            info_cursor: 0,
            selected_source: None,
            selected_targets: HashSet::new(),
            info_expanded: None,
            plan: None,
            issues: Vec::new(),
            summary_scroll: 0,
            error_msg: None,
            synced: false,
            synced_at: None,
            summary: String::new(),
            exit_prompt: None,
            status:
                "Space picks a source (left) & target files (right). Enter saves · [s] review & sync · [q] back"
                    .into(),
            status_is_error: false,
            done: false,
        }
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
    }

    fn err(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = true;
    }

    // --- derived views ------------------------------------------------------

    /// Every absolute path currently used as a mapping target.
    fn mapped_targets(&self) -> HashSet<PathBuf> {
        self.mappings
            .iter()
            .flat_map(|m| m.targets.iter().cloned())
            .collect()
    }

    /// Indices into [`Self::dest_files`] that aren't mapped yet (the available view).
    fn available_indices(&self) -> Vec<usize> {
        let mapped = self.mapped_targets();
        self.dest_files
            .iter()
            .enumerate()
            .filter(|(_, f)| !mapped.contains(&f.abs))
            .map(|(i, _)| i)
            .collect()
    }

    /// The available accordion's rows: each destination folder, followed (when
    /// expanded) by its unmapped files. Drives the right pane's navigation.
    fn available_rows(&self) -> Vec<AvailRow> {
        let mapped = self.mapped_targets();
        let mut rows = Vec::new();
        for root_idx in 0..self.dest_roots.len() {
            rows.push(AvailRow::Folder(root_idx));
            if self.dest_open.contains(&root_idx) {
                for (i, f) in self.dest_files.iter().enumerate() {
                    if f.root_idx == root_idx && !mapped.contains(&f.abs) {
                        rows.push(AvailRow::File(i));
                    }
                }
            }
        }
        rows
    }

    /// How many of `root_idx`'s files are still unmapped (shown on its folder row).
    fn available_in_root(&self, root_idx: usize) -> usize {
        let mapped = self.mapped_targets();
        self.dest_files
            .iter()
            .filter(|f| f.root_idx == root_idx && !mapped.contains(&f.abs))
            .count()
    }

    /// Every `(source_rel, target)` pair across all mappings (the assigned view),
    /// sorted by target so a folder's files group together.
    fn assigned_rows(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut rows: Vec<(PathBuf, PathBuf)> = self
            .mappings
            .iter()
            .flat_map(|m| m.targets.iter().map(|t| (m.source_rel.clone(), t.clone())))
            .collect();
        rows.sort_by(|a, b| a.1.cmp(&b.1));
        rows
    }

    /// How many rows the destination pane currently shows (folders + files in the
    /// available accordion; one row per mapped file in the assigned view).
    fn dest_len(&self) -> usize {
        match self.dest_view {
            DestView::Available => self.available_rows().len(),
            DestView::Assigned => self.assigned_rows().len(),
        }
    }

    /// Total target count across every mapping.
    fn target_count(&self) -> usize {
        self.mappings.iter().map(|m| m.targets.len()).sum()
    }

    // --- mapping edits ------------------------------------------------------

    /// Add `target` for `source_rel`, merging into an existing mapping for the same
    /// source. Rejects a target whose extension differs from the source's (you map
    /// files of the same type), and a target already used anywhere (the
    /// duplicate-target rule).
    fn add_target(&mut self, source_rel: PathBuf, target: PathBuf) -> Result<(), String> {
        let (se, te) = (ext_of(&source_rel), ext_of(&target));
        if se != te {
            return Err(format!(
                "{} has {}, but the source {} has {} — map files of the same type.",
                target.display(),
                ext_label(&te),
                source_rel.display(),
                ext_label(&se),
            ));
        }
        if self.mappings.iter().any(|m| m.targets.contains(&target)) {
            return Err(format!(
                "{} is already mapped — a destination file can't be a target twice.",
                target.display()
            ));
        }
        match self
            .mappings
            .iter_mut()
            .find(|m| m.source_rel == source_rel)
        {
            Some(m) => m.targets.push(target),
            None => self.mappings.push(Mapping {
                source_rel,
                targets: vec![target],
            }),
        }
        Ok(())
    }

    /// Remove one target wherever it's mapped, dropping the mapping if it becomes
    /// empty. Used by the assigned view to unmap a file.
    fn unmap_target(&mut self, target: &Path) {
        for m in &mut self.mappings {
            m.targets.retain(|t| t != target);
        }
        self.mappings.retain(|m| !m.targets.is_empty());
        self.note(format!("Unmapped {}.", target.display()));
    }

    /// Commit the current source + target selection into the mapping set, then
    /// clear it ready for the next one. The saved targets leave the available view.
    fn save_mapping(&mut self) {
        let Some(source_rel) = self.selected_source.clone() else {
            self.err("Pick a source file first — Space on the left pane.");
            return;
        };
        if self.selected_targets.is_empty() {
            self.err("Pick at least one destination file — Space on the right pane.");
            return;
        }
        let mut targets: Vec<PathBuf> = self.selected_targets.iter().cloned().collect();
        targets.sort();
        let mut added = 0usize;
        let mut skipped = 0usize;
        let mut last_err: Option<String> = None;
        for target in targets {
            match self.add_target(source_rel.clone(), target) {
                Ok(()) => added += 1,
                Err(why) => {
                    skipped += 1;
                    last_err = Some(why);
                }
            }
        }
        // Always clear the chosen targets; keep the source selected on a total
        // failure (e.g. every target's extension mismatched) so they can retry.
        self.selected_targets.clear();
        if added > 0 {
            self.selected_source = None;
        }
        match (added, skipped) {
            (0, _) => self.err(last_err.unwrap_or_else(|| "Nothing to map.".into())),
            (n, 0) => self.note(format!(
                "Mapped {} → {n} file(s). Pick another, or [s] to review & sync.",
                source_rel.display()
            )),
            (n, s) => self.err(format!(
                "Mapped {n}, skipped {s}: {}",
                last_err.unwrap_or_default()
            )),
        }
    }

    // --- map-page navigation ------------------------------------------------

    fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
    }

    fn toggle_dest_view(&mut self) {
        self.dest_view = match self.dest_view {
            DestView::Available => DestView::Assigned,
            DestView::Assigned => DestView::Available,
        };
        self.dest_cursor = 0;
        self.focus = Pane::Destinations;
        match self.dest_view {
            DestView::Available => self.note("Showing destination files not mapped yet."),
            DestView::Assigned => {
                self.note("Showing mapped files — Space/d unmaps one. [a] back to available.")
            }
        }
    }

    fn cursor_up(&mut self) {
        match self.focus {
            Pane::Sources => self.src_cursor = self.src_cursor.saturating_sub(1),
            Pane::Destinations => self.dest_cursor = self.dest_cursor.saturating_sub(1),
            Pane::Info => self.info_cursor = self.info_cursor.saturating_sub(1),
        }
    }

    fn cursor_down(&mut self) {
        match self.focus {
            Pane::Sources => {
                self.src_cursor =
                    (self.src_cursor + 1).min(self.source_files.len().saturating_sub(1));
            }
            Pane::Destinations => {
                self.dest_cursor = (self.dest_cursor + 1).min(self.dest_len().saturating_sub(1));
            }
            Pane::Info => {
                self.info_cursor =
                    (self.info_cursor + 1).min(self.dest_roots.len().saturating_sub(1));
            }
        }
    }

    /// `Space` on the focused pane: pick a source, toggle a target, or (in the
    /// assigned view) unmap one.
    fn select(&mut self) {
        match self.focus {
            Pane::Sources => self.toggle_source(),
            Pane::Destinations => match self.dest_view {
                DestView::Available => self.available_activate(),
                DestView::Assigned => self.unmap_at_cursor(),
            },
            Pane::Info => self.info_enter(),
        }
    }

    /// Pick the highlighted source as the (single) selected source; pressing it
    /// again deselects.
    fn toggle_source(&mut self) {
        let Some(rel) = self.source_files.get(self.src_cursor).cloned() else {
            return;
        };
        if self.selected_source.as_ref() == Some(&rel) {
            self.selected_source = None;
            self.note("Source deselected.");
        } else {
            self.note(format!("Source: {}. Now pick target files.", rel.display()));
            self.selected_source = Some(rel);
        }
    }

    /// `Space` in the available accordion: open/collapse a folder, or toggle the
    /// highlighted file in/out of the pending target set (multi-select).
    fn available_activate(&mut self) {
        match self.available_rows().get(self.dest_cursor).copied() {
            Some(AvailRow::Folder(root_idx)) => {
                if !self.dest_open.remove(&root_idx) {
                    self.dest_open.insert(root_idx);
                }
                self.dest_cursor = self.dest_cursor.min(self.dest_len().saturating_sub(1));
            }
            Some(AvailRow::File(fi)) => {
                let abs = self.dest_files[fi].abs.clone();
                if self.selected_targets.remove(&abs) {
                    self.note("Target deselected.");
                } else {
                    self.selected_targets.insert(abs);
                }
            }
            None => {}
        }
    }

    /// Unmap the file highlighted in the assigned view.
    fn unmap_at_cursor(&mut self) {
        let Some((_, target)) = self.assigned_rows().get(self.dest_cursor).cloned() else {
            return;
        };
        self.unmap_target(&target);
        self.dest_cursor = self.dest_cursor.min(self.dest_len().saturating_sub(1));
    }

    /// `Enter` on the info pane toggles a destination folder's mappings open
    /// (accordion — at most one open at a time).
    fn info_enter(&mut self) {
        if self.info_cursor >= self.dest_roots.len() {
            return;
        }
        self.info_expanded =
            (self.info_expanded != Some(self.info_cursor)).then_some(self.info_cursor);
    }

    /// Re-scan the source and destination folders from disk, so files added (or
    /// removed) while bukagu is open show up. Mappings are kept; selections that no
    /// longer exist on disk are dropped, and every cursor is clamped back in range.
    fn reload(&mut self) {
        self.source_files = scan_files(&self.source)
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        let mut dest_files = Vec::new();
        for (root_idx, root) in self.dest_roots.iter().enumerate() {
            for (rel, size) in scan_files(root) {
                dest_files.push(DestFile {
                    root_idx,
                    abs: root.join(&rel),
                    rel,
                    size,
                });
            }
        }
        self.dest_files = dest_files;

        // Drop selections whose files have since vanished.
        if let Some(src) = self.selected_source.clone()
            && !self.source_files.contains(&src)
        {
            self.selected_source = None;
        }
        let on_disk: HashSet<PathBuf> = self.dest_files.iter().map(|f| f.abs.clone()).collect();
        self.selected_targets.retain(|t| on_disk.contains(t));

        // Keep cursors / the open set within bounds of the fresh listing.
        self.dest_open.retain(|i| *i < self.dest_roots.len());
        self.src_cursor = self
            .src_cursor
            .min(self.source_files.len().saturating_sub(1));
        self.dest_cursor = self.dest_cursor.min(self.dest_len().saturating_sub(1));
        self.info_cursor = self
            .info_cursor
            .min(self.dest_roots.len().saturating_sub(1));
        self.note(format!(
            "Reloaded — {} source file(s), {} destination file(s).",
            self.source_files.len(),
            self.dest_files.len()
        ));
    }

    // --- summary / apply ----------------------------------------------------

    /// Compute the plan + issues and show the Summary (or apply straight away under
    /// `--yes`). Refuses with a note when there are no mappings.
    fn enter_summary(&mut self) {
        if self.mappings.is_empty() {
            self.err("No mappings yet — pick a source and target files, then Enter.");
            return;
        }
        self.issues = mapping::validate(&self.source, &self.dest_roots, &self.mappings);
        match mapping::plan(&self.source, &self.mappings) {
            Ok(plan) => {
                self.summary_scroll = 0;
                let auto = self.opts.yes && !self.has_blocking() && !plan.is_noop();
                self.plan = Some(plan);
                self.screen = Screen::Summary;
                if auto {
                    self.apply();
                }
            }
            Err(e) => {
                self.error_msg = Some(format!("{e:#}"));
                self.screen = Screen::Error;
            }
        }
    }

    fn has_blocking(&self) -> bool {
        self.issues.iter().any(Issue::is_blocking)
    }

    /// Write every will-write target. Synchronous — a mapping set is small and
    /// local. Honors `--dry-run` (validates + plans, writes nothing).
    fn apply(&mut self) {
        if self.has_blocking() {
            self.err("Resolve the blocking issues before syncing.");
            return;
        }
        let Some(plan) = self.plan.clone() else {
            return;
        };
        if plan.is_noop() {
            self.note("Everything is already in sync — nothing to write.");
            return;
        }

        let roots = self.dest_roots.clone();
        for tp in plan.targets.iter().filter(|t| t.will_write()) {
            if let Err(e) = mapping::apply_target(&self.source, &roots, tp, self.opts.dry_run) {
                self.error_msg = Some(format!("{e:#}"));
                self.screen = Screen::Error;
                return;
            }
        }

        if self.opts.dry_run {
            self.summary = format!(
                "Dry run — {} mapping change(s) previewed, nothing written.",
                plan.changes()
            );
        } else {
            let ts = store::now_rfc3339();
            self.synced = true;
            self.synced_at = Some(ts.clone());
            self.summary = format!(
                "Mapping sync OK — {} written, {} up to date ({}). Synced at {ts}.",
                plan.changes(),
                plan.stats.up_to_date,
                human_bytes(plan.stats.bytes),
            );
        }
        self.screen = Screen::Done;
    }

    fn summary_scroll_down(&mut self) {
        self.summary_scroll = self.summary_scroll.saturating_add(1);
    }

    fn summary_scroll_up(&mut self) {
        self.summary_scroll = self.summary_scroll.saturating_sub(1);
    }

    // --- keys ---------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let code = key.code;
        // The "exit the mapping tab?" dialog is modal: while it's up, keys drive
        // only it.
        if self.exit_prompt.is_some() {
            self.on_key_exit_prompt(code);
            return;
        }
        match self.screen {
            Screen::Map => self.on_key_map(code),
            Screen::Summary => self.on_key_summary(code),
            Screen::Done => self.on_key_done(code),
            Screen::Error => self.on_key_error(code),
        }
    }

    /// Open the "exit the mapping tab?" confirmation, focused on Cancel so an
    /// accidental Enter keeps you on the page.
    fn prompt_exit(&mut self) {
        self.exit_prompt = Some(ExitChoice::Cancel);
    }

    /// Drive the exit-confirmation dialog: ←/→ (or Tab) move between the buttons,
    /// Enter acts on the focused one, Esc/`n` back out, `y` confirms.
    fn on_key_exit_prompt(&mut self, code: KeyCode) {
        let focused = self.exit_prompt.unwrap_or(ExitChoice::Cancel);
        match code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Tab
            | KeyCode::BackTab => self.exit_prompt = Some(focused.toggle()),
            KeyCode::Enter => {
                self.exit_prompt = None;
                if focused == ExitChoice::Confirm {
                    self.done = true;
                }
            }
            KeyCode::Char('y') => {
                self.exit_prompt = None;
                self.done = true;
            }
            KeyCode::Char('n') | KeyCode::Esc => self.exit_prompt = None,
            _ => {}
        }
    }

    fn on_key_map(&mut self, code: KeyCode) {
        match code {
            KeyCode::Tab => self.focus_next(),
            KeyCode::BackTab => self.focus_prev(),
            KeyCode::Up | KeyCode::Char('k') => self.cursor_up(),
            KeyCode::Down | KeyCode::Char('j') => self.cursor_down(),
            // Left/Right are a convenience for hopping between the two file panes.
            KeyCode::Left | KeyCode::Char('h') => self.focus = Pane::Sources,
            KeyCode::Right | KeyCode::Char('l') => self.focus = Pane::Destinations,
            KeyCode::Char(' ') => self.select(),
            KeyCode::Char('a') => self.toggle_dest_view(),
            KeyCode::Char('d') | KeyCode::Delete
                if self.focus == Pane::Destinations && self.dest_view == DestView::Assigned =>
            {
                self.unmap_at_cursor()
            }
            KeyCode::Enter => {
                if self.focus == Pane::Info {
                    self.info_enter();
                } else {
                    self.save_mapping();
                }
            }
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('s') | KeyCode::Char('c') => self.enter_summary(),
            KeyCode::Char('q') | KeyCode::Esc => self.prompt_exit(),
            _ => {}
        }
    }

    fn on_key_summary(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.summary_scroll_up(),
            KeyCode::Down | KeyCode::Char('j') => self.summary_scroll_down(),
            KeyCode::Enter | KeyCode::Char('y') => self.apply(),
            KeyCode::Esc | KeyCode::Char('q') => {
                self.screen = Screen::Map;
                self.note("Back to the map page.");
            }
            _ => {}
        }
    }

    fn on_key_done(&mut self, code: KeyCode) {
        if matches!(code, KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc) {
            // Return to the page so they can map more; a second [q] leaves the screen.
            self.screen = Screen::Map;
        }
    }

    fn on_key_error(&mut self, code: KeyCode) {
        if matches!(code, KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc) {
            self.error_msg = None;
            self.screen = Screen::Map;
        }
    }

    // --- display helpers ----------------------------------------------------

    /// Best-effort `folder/rel` label for an absolute target path.
    fn target_label(&self, target: &Path) -> String {
        for root in &self.dest_roots {
            if let Ok(rel) = target.strip_prefix(root) {
                return format!("{}/{}", root_name(root), rel.display());
            }
        }
        target.display().to_string()
    }
}

/// Files under `root` (real files only — symlinks skipped), each as a path relative
/// to `root` with its size, sorted case-sensitively by relative path.
fn scan_files(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out: Vec<(PathBuf, u64)> = Vec::new();
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if entry.path_is_symlink() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            out.push((rel.to_path_buf(), meta.len()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The display name of a folder (its last component), falling back to the path.
fn root_name(root: &Path) -> String {
    root.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// A path's extension, lower-cased ("" when it has none) — for the same-type rule.
fn ext_of(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Human label for an extension in messages: `.py`, or `no extension`.
fn ext_label(ext: &str) -> String {
    if ext.is_empty() {
        "no extension".to_string()
    } else {
        format!(".{ext}")
    }
}

/// Best-effort canonicalization (falls back to the path as-is).
fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Run the mapping screen and return the edited mappings + whether a sync ran.
/// The terminal is always restored before returning.
pub fn run(config: &Config, mappings: Vec<Mapping>, opts: RunOptions) -> Result<MapSession> {
    let mut app = MapApp::new(config, mappings, opts);
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    result?;
    Ok(MapSession {
        mappings: app.mappings,
        synced: app.synced,
        summary: app.summary,
    })
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut MapApp) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        // Blocking read: the mapping screen only redraws on input (apply is local).
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
        if app.done {
            return Ok(());
        }
    }
}

// --- Rendering -------------------------------------------------------------

fn draw(frame: &mut Frame, app: &MapApp) {
    let area = frame.area();
    frame.render_widget(
        Block::new().style(Style::default().bg(theme::BG).fg(theme::TEXT)),
        area,
    );
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);

    draw_header(frame, app, header);
    match app.screen {
        Screen::Map => draw_map(frame, app, body),
        Screen::Summary => draw_summary(frame, app, body),
        Screen::Done => draw_done(frame, app, body),
        Screen::Error => draw_error(frame, app, body),
    }
    draw_footer(frame, app, footer);

    // The exit-confirmation dialog sits on top of everything else when open.
    if let Some(choice) = app.exit_prompt {
        draw_exit_dialog(frame, choice, area);
    }
}

fn draw_header(frame: &mut Frame, app: &MapApp, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            "bukagu",
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  file mappings", Style::default().fg(theme::TEXT_DIM)),
    ]);
    let source = Line::from(vec![
        Span::styled("Source  ", Style::default().fg(theme::TEXT_DIM)),
        Span::styled(
            app.source.display().to_string(),
            Style::default().fg(theme::AMBER),
        ),
    ]);
    frame.render_widget(Paragraph::new(vec![title, source]), area);
}

fn panel(title: &str, border: Color) -> Block<'_> {
    Block::bordered()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(border))
}

/// Border style for a Map pane — gold + bold when focused, else the dim panel hue.
fn pane_border(focused: bool) -> Color {
    if focused {
        theme::GOLD
    } else {
        theme::PANEL_BORDER
    }
}

/// The cursor highlight for a list — bright gold reverse when its pane is focused,
/// nothing (a transparent no-op style) otherwise so sticky selections stay legible.
fn cursor_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(theme::SELECTION)
            .fg(theme::BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// The Map screen: sources | destinations on top, the info accordion below.
fn draw_map(frame: &mut Frame, app: &MapApp, area: Rect) {
    let [panes, info] =
        Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(panes);
    draw_sources(frame, app, left);
    draw_destinations(frame, app, right);
    draw_info(frame, app, info);
}

fn draw_sources(frame: &mut Frame, app: &MapApp, area: Rect) {
    let focused = app.focus == Pane::Sources;
    let mapped: HashSet<PathBuf> = app.mappings.iter().map(|m| m.source_rel.clone()).collect();
    let title = format!("sources ({})", app.source_files.len());

    if app.source_files.is_empty() {
        let note = Paragraph::new("(no files under the source folder)")
            .style(
                Style::default()
                    .fg(theme::TEXT_DIM)
                    .add_modifier(Modifier::ITALIC),
            )
            .wrap(Wrap { trim: true })
            .block(panel(&title, pane_border(focused)));
        frame.render_widget(note, area);
        return;
    }

    let items: Vec<ListItem> = app
        .source_files
        .iter()
        .map(|rel| {
            let chosen = app.selected_source.as_ref() == Some(rel);
            let base = if chosen {
                // Sticky "selected source" — a distinct background.
                Style::default()
                    .bg(theme::ROSEWOOD)
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT)
            };
            let mut spans = vec![Span::styled(rel.display().to_string(), base)];
            if mapped.contains(rel) {
                spans.push(Span::styled(
                    "  (mapped)",
                    Style::default().fg(theme::TEXT_DIM),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(panel(&title, pane_border(focused)))
        .highlight_style(cursor_style(focused))
        .highlight_symbol(if focused { "▸ " } else { "  " });
    let mut state = ListState::default();
    if focused {
        state.select(Some(app.src_cursor.min(app.source_files.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_destinations(frame: &mut Frame, app: &MapApp, area: Rect) {
    let focused = app.focus == Pane::Destinations;
    match app.dest_view {
        DestView::Available => draw_dest_available(frame, app, area, focused),
        DestView::Assigned => draw_dest_assigned(frame, app, area, focused),
    }
}

fn draw_dest_available(frame: &mut Frame, app: &MapApp, area: Rect, focused: bool) {
    let title = format!(
        "destinations · available ({})",
        app.available_indices().len()
    );

    if app.dest_files.is_empty() {
        let note =
            Paragraph::new("(no files in the destination folders — add some, then [r] reload)")
                .style(
                    Style::default()
                        .fg(theme::TEXT_DIM)
                        .add_modifier(Modifier::ITALIC),
                )
                .wrap(Wrap { trim: true })
                .block(panel(&title, pane_border(focused)));
        frame.render_widget(note, area);
        return;
    }

    // The accordion: each destination folder, then (when open) its unmapped files.
    let rows = app.available_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            AvailRow::Folder(root_idx) => {
                let open = app.dest_open.contains(&root_idx);
                let caret = if open { "▾" } else { "▸" };
                Line::from(vec![
                    Span::styled(
                        format!("{caret} {}", root_name(&app.dest_roots[root_idx])),
                        Style::default()
                            .fg(theme::GOLD)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("   ({} available)", app.available_in_root(root_idx)),
                        Style::default().fg(theme::TEXT_DIM),
                    ),
                ])
            }
            AvailRow::File(fi) => {
                let f = &app.dest_files[fi];
                let name = f.rel.display().to_string();
                if app.selected_targets.contains(&f.abs) {
                    // Selected target — a distinct color + tick.
                    Line::from(vec![
                        Span::styled(
                            "    ✓ ",
                            Style::default()
                                .fg(theme::GOLD)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            name,
                            Style::default()
                                .fg(theme::GOLD)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                } else {
                    Line::from(Span::styled(
                        format!("      {name}"),
                        Style::default().fg(theme::COPY),
                    ))
                }
            }
        })
        .map(ListItem::new)
        .collect();

    let list = List::new(items)
        .block(panel(&title, pane_border(focused)))
        .highlight_style(cursor_style(focused))
        .highlight_symbol(if focused { "▸ " } else { "  " });
    let mut state = ListState::default();
    if focused && !rows.is_empty() {
        state.select(Some(app.dest_cursor.min(rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_dest_assigned(frame: &mut Frame, app: &MapApp, area: Rect, focused: bool) {
    let rows = app.assigned_rows();
    let title = format!("destinations · assigned ({})", rows.len());

    if rows.is_empty() {
        let note = Paragraph::new("(nothing mapped yet — press [a] to pick target files)")
            .style(
                Style::default()
                    .fg(theme::TEXT_DIM)
                    .add_modifier(Modifier::ITALIC),
            )
            .wrap(Wrap { trim: true })
            .block(panel(&title, pane_border(focused)));
        frame.render_widget(note, area);
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .map(|(source_rel, target)| {
            ListItem::new(Line::from(vec![
                Span::styled(app.target_label(target), Style::default().fg(theme::COPY)),
                Span::styled("  ←  ", Style::default().fg(theme::TEXT_DIM)),
                Span::styled(
                    source_rel.display().to_string(),
                    Style::default().fg(theme::AMBER),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(panel(&title, pane_border(focused)))
        .highlight_style(cursor_style(focused))
        .highlight_symbol(if focused { "▸ " } else { "  " });
    let mut state = ListState::default();
    if focused {
        state.select(Some(app.dest_cursor.min(rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_info(frame: &mut Frame, app: &MapApp, area: Rect) {
    let focused = app.focus == Pane::Info;
    let title = format!(
        "destination folders  ·  {} mapping(s) → {} target(s)",
        app.mappings.len(),
        app.target_count()
    );
    let block = panel(&title, pane_border(focused));

    if app.dest_roots.is_empty() {
        frame.render_widget(
            Paragraph::new("(no destination folders)").block(block),
            area,
        );
        return;
    }

    let mapped = app.mapped_targets();
    let mut items: Vec<ListItem> = Vec::new();
    let mut sel_rows: Vec<usize> = Vec::new();

    for (i, root) in app.dest_roots.iter().enumerate() {
        let open = app.info_expanded == Some(i);
        let caret = if open { "▾" } else { "▸" };
        let files = app.dest_files.iter().filter(|f| f.root_idx == i).count();
        let bytes: u64 = app
            .dest_files
            .iter()
            .filter(|f| f.root_idx == i)
            .map(|f| f.size)
            .sum();
        let mapped_here = app
            .dest_files
            .iter()
            .filter(|f| f.root_idx == i && mapped.contains(&f.abs))
            .count();

        sel_rows.push(items.len());
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("{caret} {}", root.display()),
                Style::default()
                    .fg(theme::GOLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "   {}  ·  {} files  ·  {} mapped",
                    human_bytes(bytes),
                    files,
                    mapped_here
                ),
                Style::default().fg(theme::TEXT_DIM),
            ),
        ])));

        if open {
            let mut any = false;
            for m in &app.mappings {
                for t in &m.targets {
                    if t.strip_prefix(root).is_ok() {
                        any = true;
                        items.push(ListItem::new(Line::from(vec![
                            Span::styled("      ", Style::default()),
                            Span::styled(
                                m.source_rel.display().to_string(),
                                Style::default().fg(theme::AMBER),
                            ),
                            Span::styled("  →  ", Style::default().fg(theme::TEXT_DIM)),
                            Span::styled(app.target_label(t), Style::default().fg(theme::COPY)),
                        ])));
                    }
                }
            }
            if !any {
                items.push(ListItem::new(Line::from(Span::styled(
                    "      (no mappings into this folder yet)",
                    Style::default()
                        .fg(theme::TEXT_DIM)
                        .add_modifier(Modifier::ITALIC),
                ))));
            }
        }
    }

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(theme::SELECTION)
            .fg(theme::BG)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    if focused && !sel_rows.is_empty() {
        let idx = app.info_cursor.min(sel_rows.len() - 1);
        state.select(Some(sel_rows[idx]));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_summary(frame: &mut Frame, app: &MapApp, area: Rect) {
    let lines = summary_lines(app);
    let inner_h = area.height.saturating_sub(2).max(1) as usize;
    let max_off = lines.len().saturating_sub(inner_h);
    let offset = app.summary_scroll.min(max_off) as u16;
    let p = Paragraph::new(lines)
        .block(panel("review the mapping sync", theme::PANEL_BORDER))
        .scroll((offset, 0));
    frame.render_widget(p, area);
}

/// The Summary body: blocking issues (if any), then per-target rows, then counts.
fn summary_lines(app: &MapApp) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let plan = match &app.plan {
        Some(p) => p,
        None => return lines,
    };

    if !app.issues.is_empty() {
        lines.push(Line::from(Span::styled(
            "Issues:",
            Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        )));
        for issue in &app.issues {
            let color = if issue.is_blocking() {
                theme::ERROR
            } else {
                theme::OVERWRITE
            };
            lines.push(Line::from(Span::styled(
                format!("  • {}", issue.message()),
                Style::default().fg(color),
            )));
        }
        lines.push(Line::raw(""));
    }

    for tp in &plan.targets {
        let color = state_color(tp.state);
        let mut spans = vec![
            Span::styled(
                format!("  {:<8}", tp.state.label()),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}  →  ", tp.source_rel.display()),
                Style::default().fg(theme::AMBER),
            ),
            Span::styled(
                tp.target.display().to_string(),
                Style::default().fg(theme::TEXT),
            ),
        ];
        if let Some(note) = tp.banner.note() {
            spans.push(Span::styled(
                format!("   [{note}]"),
                Style::default().fg(theme::TEXT_DIM),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::raw(""));
    let s = &plan.stats;
    lines.push(Line::from(Span::styled(
        format!(
            "{} to create · {} to update · {} up to date · {} bytes",
            s.created,
            s.updated,
            s.up_to_date,
            human_bytes(s.bytes)
        ),
        Style::default()
            .fg(theme::SUCCESS)
            .add_modifier(Modifier::BOLD),
    )));
    if s.source_missing > 0 || s.banner_skipped > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} source(s) missing · {} without a banner",
                s.source_missing, s.banner_skipped
            ),
            Style::default().fg(theme::TEXT_DIM),
        )));
    }
    lines
}

fn state_color(state: TargetState) -> Color {
    match state {
        TargetState::Create => theme::COPY,
        TargetState::Update => theme::OVERWRITE,
        TargetState::UpToDate => theme::SUCCESS,
        TargetState::SourceMissing => theme::ERROR,
    }
}

fn draw_done(frame: &mut Frame, app: &MapApp, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(ts) = &app.synced_at {
        lines.push(Line::from(vec![
            Span::styled("Last sync: ", Style::default().fg(theme::TEXT_DIM)),
            Span::styled(
                "OK",
                Style::default()
                    .fg(theme::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  synced at {ts}"),
                Style::default().fg(theme::TEXT),
            ),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Dry run complete — nothing was written.",
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        app.summary.clone(),
        Style::default().fg(theme::TEXT),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("sync complete", theme::SUCCESS))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_error(frame: &mut Frame, app: &MapApp, area: Rect) {
    let msg = app
        .error_msg
        .clone()
        .unwrap_or_else(|| "Unknown error.".into());
    let lines = vec![
        Line::from(Span::styled(
            "The mapping sync could not complete:",
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

fn draw_footer(frame: &mut Frame, app: &MapApp, area: Rect) {
    let keys = match app.screen {
        Screen::Map => match (app.focus, app.dest_view) {
            (Pane::Destinations, DestView::Assigned) => {
                "Tab focus  ↑/↓ move  Space/d unmap  [a] available  [r] reload  [s] sync  [q] back"
            }
            (Pane::Destinations, DestView::Available) => {
                "Tab focus  ↑/↓ move  Space open folder / select file  Enter save  [a] assigned  [r] reload  [s] sync  [q] back"
            }
            (Pane::Info, _) => {
                "Tab focus  ↑/↓ move  Enter expand  [a] dest view  [r] reload  [s] sync  [q] back"
            }
            (Pane::Sources, _) => {
                "Tab focus  ↑/↓ move  Space pick source  Enter save  [a] dest view  [r] reload  [s] sync  [q] back"
            }
        },
        Screen::Summary => "↑/↓ scroll  [Enter/y] sync  [q] back",
        Screen::Done => "[Enter/q] back to the map page",
        Screen::Error => "[Enter/q] back to the map page",
    };
    let status_color = if app.status_is_error {
        theme::ERROR
    } else {
        theme::ACCENT
    };
    let status = Line::from(Span::styled(
        app.status.clone(),
        Style::default().fg(status_color),
    ));
    let key_line = Line::from(Span::styled(keys, Style::default().fg(theme::TEXT_DIM)));
    frame.render_widget(Paragraph::new(vec![status, key_line]), area);
}

/// A small centered "exit the mapping tab?" confirmation. The focused button is
/// highlighted; the dialog opens focused on Cancel so a stray Enter is harmless.
fn draw_exit_dialog(frame: &mut Frame, choice: ExitChoice, area: Rect) {
    let [v] = Layout::vertical([Constraint::Length(7)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(50)])
        .flex(Flex::Center)
        .areas(v);

    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(Span::styled(
            " exit mapping? ",
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme::GOLD))
        .style(Style::default().bg(theme::BG).fg(theme::TEXT));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [msg_area, buttons_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    frame.render_widget(
        Paragraph::new("Are you sure to exit mapping tab?")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme::TEXT)),
        msg_area,
    );

    let buttons = Line::from(vec![
        exit_button("Confirm", choice == ExitChoice::Confirm),
        Span::raw("      "),
        exit_button("Cancel", choice == ExitChoice::Cancel),
    ]);
    frame.render_widget(
        Paragraph::new(buttons).alignment(Alignment::Center),
        buttons_area,
    );
}

/// One button in the exit dialog: the focused one is reverse-highlighted in gold,
/// the other is dimmed.
fn exit_button(label: &str, focused: bool) -> Span<'static> {
    let style = if focused {
        Style::default()
            .bg(theme::SELECTION)
            .fg(theme::BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT_DIM)
    };
    Span::styled(format!("  {label}  "), style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use std::fs;
    use tempfile::tempdir;

    /// Write `content` to `root/rel`, creating parents; returns the absolute path.
    fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }

    fn app_with(source: &Path, dests: &[&Path]) -> MapApp {
        let config = Config {
            source: source.to_path_buf(),
            destinations: dests.iter().map(|d| d.to_path_buf()).collect(),
        };
        MapApp::new(&config, Vec::new(), RunOptions::default())
    }

    /// Index of the first file row in the available accordion (folders auto-open
    /// for a single destination root).
    fn first_file_row(app: &MapApp) -> usize {
        app.available_rows()
            .iter()
            .position(|r| matches!(r, AvailRow::File(_)))
            .expect("a file row is present")
    }

    #[test]
    fn scans_source_and_destination_files() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        write(src.path(), "sub/b.py", "y = 2\n");
        write(dst.path(), "a.py", "old\n");

        let app = app_with(src.path(), &[dst.path()]);
        assert_eq!(app.source_files.len(), 2, "recursive source scan");
        assert_eq!(app.dest_files.len(), 1, "destination file discovered");
        // Available view shows the one unmapped destination file.
        assert_eq!(app.available_indices().len(), 1);
    }

    #[test]
    fn save_mapping_from_selection_then_target_leaves_available() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        write(dst.path(), "a.py", "old\n");
        let mut app = app_with(src.path(), &[dst.path()]);

        // Pick the source (Space on the left).
        app.src_cursor = 0;
        app.toggle_source();
        assert_eq!(app.selected_source.as_deref(), Some(Path::new("a.py")));

        // Pick the one available target (Space on the file row, right pane).
        app.focus = Pane::Destinations;
        app.dest_cursor = first_file_row(&app);
        app.available_activate();
        assert_eq!(app.selected_targets.len(), 1);

        // Enter saves it.
        app.save_mapping();
        assert_eq!(app.mappings.len(), 1);
        assert_eq!(app.mappings[0].targets.len(), 1);
        assert!(
            app.selected_source.is_none(),
            "selection cleared after save"
        );
        assert!(app.selected_targets.is_empty());

        // The mapped file is no longer available; it now shows in the assigned view.
        assert!(app.available_indices().is_empty());
        assert_eq!(app.assigned_rows().len(), 1);
    }

    #[test]
    fn toggling_a_target_twice_deselects_it() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(dst.path(), "a.py", "old\n");
        let mut app = app_with(src.path(), &[dst.path()]);
        app.focus = Pane::Destinations;
        app.dest_cursor = first_file_row(&app);

        app.available_activate();
        assert_eq!(app.selected_targets.len(), 1);
        app.available_activate();
        assert!(app.selected_targets.is_empty(), "second Space deselects");
    }

    #[test]
    fn space_on_a_folder_opens_and_collapses_it() {
        let src = tempdir().unwrap();
        let d1 = tempdir().unwrap();
        let d2 = tempdir().unwrap();
        write(d1.path(), "a.py", "1\n");
        write(d2.path(), "b.py", "2\n");
        // Two roots → both start collapsed (you open the one you want).
        let mut app = app_with(src.path(), &[d1.path(), d2.path()]);
        assert!(app.dest_open.is_empty(), "multi-root starts collapsed");
        assert_eq!(app.available_rows().len(), 2, "just the two folder rows");

        app.focus = Pane::Destinations;
        app.dest_cursor = 0; // first folder
        app.available_activate(); // Space opens it
        assert!(app.dest_open.contains(&0));
        assert_eq!(app.available_rows().len(), 3, "folder + its file + folder");

        app.available_activate(); // Space again collapses it
        assert!(!app.dest_open.contains(&0));
        assert_eq!(app.available_rows().len(), 2);
    }

    #[test]
    fn add_target_rejects_an_extension_mismatch() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let mut app = app_with(src.path(), &[dst.path()]);
        // Source .ts mapped onto a .txt target → refused.
        let err = app
            .add_target(PathBuf::from("api.ts"), dst.path().join("later.txt"))
            .unwrap_err();
        assert!(err.contains(".ts"), "the message names the source ext");
        assert!(err.contains(".txt"), "and the target ext");
        assert!(app.mappings.is_empty(), "nothing was mapped");

        // Same extension (case-insensitive) is accepted.
        app.add_target(PathBuf::from("api.TS"), dst.path().join("api.ts"))
            .unwrap();
        assert_eq!(app.mappings.len(), 1);
    }

    #[test]
    fn save_mapping_skips_a_mismatch_and_keeps_the_source_selected() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "api.ts", "export const x = 1\n");
        write(dst.path(), "later.txt", "old\n"); // wrong type
        let mut app = app_with(src.path(), &[dst.path()]);

        app.selected_source = Some(PathBuf::from("api.ts"));
        app.selected_targets.insert(dst.path().join("later.txt"));
        app.save_mapping();

        assert!(app.mappings.is_empty(), "the mismatched target was skipped");
        assert!(app.status_is_error, "an error is surfaced");
        assert_eq!(
            app.selected_source.as_deref(),
            Some(Path::new("api.ts")),
            "the source stays selected so the user can pick a matching target"
        );
    }

    #[test]
    fn reload_picks_up_a_newly_added_file() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(dst.path(), "a.py", "1\n");
        let mut app = app_with(src.path(), &[dst.path()]);
        assert_eq!(app.dest_files.len(), 1);

        // A file appears on disk while bukagu is open.
        write(dst.path(), "b.py", "2\n");
        app.reload();
        assert_eq!(app.dest_files.len(), 2, "reload re-reads the folder");
    }

    #[test]
    fn q_prompts_before_exiting_and_confirm_leaves() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let mut app = app_with(src.path(), &[dst.path()]);

        // q opens the confirmation, focused on Cancel — nothing exits yet.
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(app.exit_prompt, Some(ExitChoice::Cancel));
        assert!(!app.done);

        // Esc backs out of the dialog without exiting.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.exit_prompt.is_none());
        assert!(!app.done);

        // q again, move onto Confirm, Enter → exit.
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.exit_prompt, Some(ExitChoice::Confirm));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.done, "confirming exits the mapping tab");
    }

    #[test]
    fn toggle_dest_view_then_unmap_removes_the_mapping() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        let target = write(dst.path(), "a.py", "old\n");
        let mut app = app_with(src.path(), &[dst.path()]);
        app.add_target(PathBuf::from("a.py"), target).unwrap();

        // Switch to the assigned view, then unmap the one row.
        app.toggle_dest_view();
        assert_eq!(app.dest_view, DestView::Assigned);
        assert_eq!(app.assigned_rows().len(), 1);
        app.dest_cursor = 0;
        app.unmap_at_cursor();
        assert!(
            app.mappings.is_empty(),
            "unmapping the last target drops it"
        );
    }

    #[test]
    fn add_target_rejects_a_duplicate_target() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let mut app = app_with(src.path(), &[dst.path()]);
        let t = dst.path().join("a.py");
        app.add_target(PathBuf::from("a.py"), t.clone()).unwrap();
        let err = app.add_target(PathBuf::from("b.py"), t).unwrap_err();
        assert!(err.contains("already mapped"));
        assert_eq!(app.mappings.len(), 1, "the rejected add changed nothing");
    }

    #[test]
    fn enter_summary_builds_a_plan() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        let mut app = app_with(src.path(), &[dst.path()]);
        app.add_target(PathBuf::from("a.py"), dst.path().join("a.py"))
            .unwrap();

        app.enter_summary();
        assert_eq!(app.screen, Screen::Summary);
        assert_eq!(app.plan.as_ref().unwrap().stats.created, 1);
    }

    #[test]
    fn apply_writes_targets_with_a_banner_and_marks_synced() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "print('hi')\n");
        let mut app = app_with(src.path(), &[dst.path()]);
        app.add_target(PathBuf::from("a.py"), dst.path().join("out/a.py"))
            .unwrap();

        app.enter_summary();
        app.apply();
        assert!(app.synced, "a real apply marks the session synced");
        assert_eq!(app.screen, Screen::Done);
        let body = fs::read_to_string(dst.path().join("out/a.py")).unwrap();
        assert!(body.contains(crate::core::banner::BANNER_MARKER));
        assert!(body.contains("print('hi')"));
    }

    #[test]
    fn dry_run_apply_writes_nothing() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        let mut app = MapApp::new(
            &Config {
                source: src.path().to_path_buf(),
                destinations: vec![dst.path().to_path_buf()],
            },
            Vec::new(),
            RunOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        app.add_target(PathBuf::from("a.py"), dst.path().join("a.py"))
            .unwrap();
        app.enter_summary();
        app.apply();
        assert!(!app.synced, "dry run never marks synced");
        assert!(!dst.path().join("a.py").exists());
    }

    #[test]
    fn draw_map_renders_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        write(dst.path(), "a.py", "old\n");
        let mut app = app_with(src.path(), &[dst.path()]);
        app.selected_source = Some(PathBuf::from("a.py"));

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("sources"), "the sources pane is drawn");
        assert!(
            rendered.contains("destinations"),
            "the destinations pane is drawn"
        );
        assert!(rendered.contains("a.py"), "a discovered file is shown");
    }
}

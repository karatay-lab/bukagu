//! The **home** screen: a vertical two-pane view that drives both first-run setup
//! and returning runs (so the actions are always on top, however many times you
//! sync).
//!
//! Layout (top → bottom):
//!   * an **actions** pane — three rows, "Select source folder", "Select
//!     destination folder", and "Sync now"; activating one of the first two opens
//!     a full-screen folder browser, while "Sync now" hands the chosen config back
//!     to the caller to run a sync;
//!   * an **info** pane — an accordion of what's been chosen: the **source**'s
//!     stats (size, file count, mapped count) are always shown at the top (no key
//!     needed); below them a `Destination Folders` header that `Enter` expands
//!     into the destination list, and `Enter` on a destination reveals *its* stats
//!     plus a nested accordion of the files mapped into it, grouped by source
//!     folder; `Enter` on a group expands its `source → target` rows. Only one
//!     destination is open at a time, and within it only one source-folder group
//!     (opening another closes the previous); `Backspace` collapses the open group,
//!     then the destination, then the list.
//!
//! `Tab` switches focus between the two panes (the focused one takes the cursor
//! keys and gets a gold border). `Ctrl+Q` toggles a full-screen overlay listing
//! every shortcut. `c` runs Sync now; `q`/`Esc` leaves the home (edits are saved
//! by the caller).
//!
//! The folder browser is a **modal**: activating an action takes over the whole
//! screen with a directory navigator (arrows move, `→`/`l`/`Enter` open a folder,
//! `←`/`Bksp`/`h` go up, `r` jumps back to the launch folder where bukagu was
//! opened, `Esc` cancels back to the two-pane view). Picking is in place — you
//! never have to open a folder to choose it. **`Space`** is the select key: for the
//! **source** it picks the highlighted folder and returns; for **destinations** it
//! is multi-select — `Space` selects the highlighted folder, `Space` again
//! deselects it, and `Esc` returns to the two-pane view when you're done.
//!
//! [`run_home`] owns the terminal, runs a blocking draw/event loop (the home only
//! redraws on input), and returns the (possibly edited) [`Config`] plus the
//! [`HomeIntent`] the user finished on — `Sync` to run a sync, or `Quit`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use walkdir::WalkDir;

use crate::store::{Config, Mapping};
use crate::theme;
use crate::ui::widgets::human_bytes;

/// Which top-level mode the screen is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The two-pane actions + info view.
    Main,
    /// The full-screen folder browser, picking for the given role.
    Browsing(PickTarget),
}

/// What a folder pick in the browser modal applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    Source,
    Destination,
}

/// Which main-view pane currently has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainFocus {
    Actions,
    Info,
}

impl MainFocus {
    fn next(self) -> Self {
        match self {
            MainFocus::Actions => MainFocus::Info,
            MainFocus::Info => MainFocus::Actions,
        }
    }
}

/// Which button is focused in the "really quit?" confirmation dialog. Cancel is
/// the default so an accidental Enter on the home keeps bukagu open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitChoice {
    Confirm,
    Cancel,
}

impl QuitChoice {
    fn toggle(self) -> Self {
        match self {
            QuitChoice::Confirm => QuitChoice::Cancel,
            QuitChoice::Cancel => QuitChoice::Confirm,
        }
    }
}

/// A selectable row in the info pane's accordion (the source's files above are
/// always shown and never selectable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InfoTarget {
    /// The `Destination Folders` header — `Enter` reveals/hides the list.
    DestHeader,
    /// One destination row (index into `destinations`) — `Enter` expands its
    /// stats and its source-folder groups inline.
    Dest(usize),
    /// A source-folder group inside the currently-expanded destination (index into
    /// that destination's sorted group list) — `Enter` expands its mapped files.
    Folder(usize),
}

/// How the home loop ended.
enum Outcome {
    /// "Sync now" — the user wants to run a sync. The config is read back from the
    /// browser's fields via [`Browser::current_config`].
    Confirmed,
    /// "Map files" — open the v2 file-mapping screen with the current config.
    OpenMappings,
    /// "Backup now" — run a v3 encrypted backup of the source.
    OpenBackup,
    /// `q`/`Esc` — leave the home (any edits are still returned to the caller).
    Cancelled,
}

/// What the user asked for when the home screen returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeIntent {
    /// Run a v1 folder-mirror sync with the returned config.
    Sync,
    /// Open the v2 file-mapping screen with the returned config.
    Mappings,
    /// Run a v3 encrypted backup of the source.
    Backup,
    /// Close the home.
    Quit,
}

/// Home state: the chosen source/destinations, the main-view cursor state, and
/// (while [`Mode::Browsing`]) the folder navigator's position.
struct Browser {
    mode: Mode,

    // --- main view ---
    main_focus: MainFocus,
    /// Highlighted action row (0 = source, 1 = destination, 2 = Sync now,
    /// 3 = Map files, 4 = Backup now).
    actions_sel: usize,
    /// Whether the destination list is revealed under the `Destination Folders`
    /// header in the info pane.
    info_dest_list_open: bool,
    /// Which destination's files are expanded inline, if any (accordion — at most
    /// one open at a time).
    info_dest_expanded: Option<usize>,
    /// Which source-folder group inside the expanded destination is open, if any
    /// (a nested accordion — at most one open at a time, reset when the open
    /// destination changes). Indexes the open destination's group list.
    info_folder_expanded: Option<usize>,
    /// Highlighted selectable info row, indexing [`Browser::info_targets`]
    /// (0 = the `Destination Folders` header).
    info_sel: usize,

    // --- the result ---
    source: Option<PathBuf>,
    destinations: Vec<PathBuf>,
    /// The v2 file mappings, loaded from the store — read-only here, used only to
    /// show how many of each folder's files are mapped in the info pane.
    mappings: Vec<Mapping>,

    // --- folder browser modal ---
    /// The launch directory (where bukagu was opened and `.bukagu/` lives); the
    /// `r` shortcut jumps the browser back here. Never mutated after init.
    root: PathBuf,
    /// The directory whose subfolders are currently listed.
    cwd: PathBuf,
    /// Subdirectories of `cwd`, sorted case-insensitively.
    entries: Vec<PathBuf>,
    /// Selection within `entries`.
    browse_sel: ListState,

    // --- chrome ---
    show_help: bool,
    /// The "really quit?" dialog and which button it has focused, or `None` while
    /// it is hidden. Shown when the user asks to leave the home.
    quit_prompt: Option<QuitChoice>,
    status: String,
    status_is_error: bool,
    /// The result of the most recent sync (shown atop the info pane), if any.
    last_run: Option<String>,
    outcome: Option<Outcome>,
}

impl Browser {
    fn new() -> Self {
        // Start where bukagu was launched (that's also where `.bukagu/` lands).
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let root = cwd.clone();
        let mut b = Self {
            mode: Mode::Main,
            main_focus: MainFocus::Actions,
            actions_sel: 0,
            info_dest_list_open: false,
            info_dest_expanded: None,
            info_folder_expanded: None,
            info_sel: 0,
            source: None,
            destinations: Vec::new(),
            mappings: Vec::new(),
            root,
            cwd,
            entries: Vec::new(),
            browse_sel: ListState::default(),
            show_help: false,
            quit_prompt: None,
            status: "Choose an action above. [s] source · [a] destination · [c] sync now."
                .to_string(),
            status_is_error: false,
            last_run: None,
            outcome: None,
        };
        b.refresh_browse();
        b
    }

    /// Build the home pre-filled from a loaded config (returning runs) and its v2
    /// mappings, with an optional one-line summary of the previous sync shown atop
    /// the info pane.
    fn home(initial: Option<Config>, mappings: Vec<Mapping>, last_run: Option<String>) -> Self {
        let mut b = Self::new();
        if let Some(config) = initial {
            b.source = Some(config.source);
            b.destinations = config.destinations;
        }
        b.mappings = mappings;
        b.last_run = last_run;
        b
    }

    /// How many of the source's files are mapped — i.e. appear as a mapping's
    /// source and still exist on disk (so the count lines up with the file count).
    fn source_mapped(&self) -> usize {
        let Some(src) = self.source.as_deref() else {
            return 0;
        };
        self.mappings
            .iter()
            .filter(|m| src.join(&m.source_rel).is_file())
            .count()
    }

    /// How many files inside destination `dir` are mapping targets. Mirrors the
    /// mapping screen, which attributes each target to the destination root it sits
    /// under.
    fn dest_mapped(&self, dir: &Path) -> usize {
        let root = canonical(dir);
        self.mappings
            .iter()
            .flat_map(|m| &m.targets)
            .filter(|t| t.starts_with(&root))
            .count()
    }

    /// The files mapped into `dir`, grouped by the source file's parent folder for
    /// the info accordion's nested sub-list. Returns `(folder, files)` sorted by
    /// folder, where each file is `(source_file_name, target_rel_within_dir)` sorted
    /// by target. The folder is the source's parent (an empty `PathBuf` for a source
    /// file at the source root). Empty when nothing maps into `dir`.
    fn dest_mapping_groups(&self, dir: &Path) -> Vec<(PathBuf, Vec<(PathBuf, PathBuf)>)> {
        let root = canonical(dir);
        let mut map: BTreeMap<PathBuf, Vec<(PathBuf, PathBuf)>> = BTreeMap::new();
        for m in &self.mappings {
            let folder = m
                .source_rel
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let name = m
                .source_rel
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| m.source_rel.clone());
            for t in &m.targets {
                if let Ok(rel) = t.strip_prefix(&root) {
                    map.entry(folder.clone())
                        .or_default()
                        .push((name.clone(), rel.to_path_buf()));
                }
            }
        }
        let mut groups: Vec<(PathBuf, Vec<(PathBuf, PathBuf)>)> = map.into_iter().collect();
        for (_, files) in &mut groups {
            files.sort_by(|a, b| a.1.cmp(&b.1));
        }
        groups
    }

    /// The source-folder groups of the currently-expanded destination, or empty when
    /// no destination is open. Drives the nested folder accordion's row count.
    fn open_dest_groups(&self) -> Vec<(PathBuf, Vec<(PathBuf, PathBuf)>)> {
        match self
            .info_dest_expanded
            .and_then(|i| self.destinations.get(i))
        {
            Some(d) => self.dest_mapping_groups(d),
            None => Vec::new(),
        }
    }

    /// The config the user has assembled, or `None` until a source and at least one
    /// destination are chosen.
    fn current_config(&self) -> Option<Config> {
        let source = self.source.clone()?;
        if self.destinations.is_empty() {
            return None;
        }
        Some(Config {
            source,
            destinations: self.destinations.clone(),
        })
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = true;
    }

    // --- folder browser modal ----------------------------------------------

    /// Reload the listing for `cwd` and reset the selection to the top.
    fn refresh_browse(&mut self) {
        self.entries = read_subdirs(&self.cwd);
        self.browse_sel
            .select((!self.entries.is_empty()).then_some(0));
    }

    /// The folder a pick acts on: the highlighted subdirectory, or `cwd` itself
    /// when the listing is empty (so a leaf folder you navigated into is still
    /// pickable).
    fn browse_target(&self) -> PathBuf {
        self.browse_sel
            .selected()
            .and_then(|i| self.entries.get(i))
            .cloned()
            .unwrap_or_else(|| self.cwd.clone())
    }

    fn browse_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = self.browse_sel.selected().map_or(0, |i| (i + 1).min(last));
        self.browse_sel.select(Some(next));
    }

    fn browse_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let prev = self
            .browse_sel
            .selected()
            .map_or(0, |i| i.saturating_sub(1));
        self.browse_sel.select(Some(prev));
    }

    /// Descend into the highlighted subdirectory (explore deeper).
    fn browse_descend(&mut self) {
        let Some(i) = self.browse_sel.selected() else {
            self.error("No subfolders here — Enter picks this folder, ← goes up.");
            return;
        };
        if let Some(dir) = self.entries.get(i).cloned() {
            self.cwd = dir;
            self.refresh_browse();
        }
    }

    /// Go up to the parent directory, keeping the child we came from selected.
    fn browse_ascend(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            self.error("Already at the filesystem root.");
            return;
        };
        let from = self.cwd.clone();
        self.cwd = parent;
        self.refresh_browse();
        if let Some(i) = self.entries.iter().position(|p| *p == from) {
            self.browse_sel.select(Some(i));
        }
    }

    /// Jump straight back to the launch folder (where bukagu was opened and
    /// `.bukagu/` lives), wherever the browser has wandered to.
    fn browse_to_root(&mut self) {
        if self.cwd == self.root {
            self.info("Already at the launch folder.");
            return;
        }
        self.cwd = self.root.clone();
        self.refresh_browse();
    }

    /// Act on `Space` in the browser. The **source** is single-pick: choose the
    /// highlighted folder and return to the main view. **Destinations** are
    /// multi-select: toggle the highlighted folder in or out of the list and stay
    /// in the modal (`Esc` returns when done).
    fn browse_select(&mut self) {
        match self.mode {
            Mode::Browsing(PickTarget::Source) => {
                self.set_source(canonical(&self.browse_target()));
                self.return_to_main();
            }
            Mode::Browsing(PickTarget::Destination) => self.toggle_destination(),
            Mode::Main => {}
        }
    }

    /// Add the highlighted folder as a destination, or remove it if it's already
    /// chosen. Source-conflicting folders are disabled (grayed out): pressing Enter
    /// on one just explains calmly and changes nothing.
    fn toggle_destination(&mut self) {
        let pick = canonical(&self.browse_target());
        if let Some(pos) = self.destinations.iter().position(|d| *d == pick) {
            self.destinations.remove(pos);
            let n = self.destinations.len();
            self.info(format!("Removed destination ({n} total). Esc when done."));
        } else if let Some(why) = self.dest_conflict(&pick) {
            // Disabled row — a neutral note, not a red error.
            self.info(conflict_reason(why));
        } else {
            match self.add_destination(pick) {
                Ok(()) => {
                    let n = self.destinations.len();
                    self.info(format!(
                        "Added destination ({n} total). Enter again to deselect, Esc when done."
                    ));
                }
                Err(why) => self.error(why),
            }
        }
    }

    /// Whether `path` (canonicalized) is already in the destination list — used to
    /// mark selected rows in the browser.
    fn is_destination(&self, path: &Path) -> bool {
        self.destinations.contains(&canonical(path))
    }

    fn return_to_main(&mut self) {
        self.mode = Mode::Main;
        // Destinations may have changed in the browser — keep the accordion's
        // indices in range.
        self.clamp_info();
    }

    // --- choosing source / destinations -------------------------------------

    /// Set the source. Any already-added destination that now conflicts with it is
    /// dropped, so "every destination is safe against the source" always holds.
    fn set_source(&mut self, src: PathBuf) {
        let before = self.destinations.len();
        self.destinations
            .retain(|d| source_conflict(d, &src).is_none());
        let pruned = before - self.destinations.len();
        self.source = Some(src);
        if pruned > 0 {
            self.error(format!(
                "Source set — dropped {pruned} destination(s) that overlapped it."
            ));
        } else {
            self.info("Source set. Now add one or more destinations.");
        }
    }

    /// Validate and add a destination. Returns `Err(reason)` for the caller to
    /// surface (so the browser modal can stay open on a bad pick).
    fn add_destination(&mut self, cand: PathBuf) -> Result<(), String> {
        if self.source.is_none() {
            return Err("Choose a source first.".into());
        }
        self.validate_destination(&cand)?;
        self.destinations.push(cand);
        let n = self.destinations.len();
        self.info(format!("Added destination ({n} total)."));
        Ok(())
    }

    /// A destination must not be the source, live inside it, or contain it (any of
    /// those could let a write reach the read-only source), and must not already
    /// be in the list.
    fn validate_destination(&self, cand: &Path) -> Result<(), String> {
        let src = self
            .source
            .as_deref()
            .expect("source is set before a destination is validated");
        if let Some(why) = source_conflict(cand, src) {
            return Err(conflict_reason(why).to_string());
        }
        if self.destinations.iter().any(|d| d == cand) {
            return Err("Already added that destination.".into());
        }
        Ok(())
    }

    /// In the destination picker, why `path` can't be a destination — it's the
    /// source, sits inside it, or contains it. `None` when there's no source yet or
    /// the folder is a safe candidate. Drives the grayed-out "disabled" rows.
    fn dest_conflict(&self, path: &Path) -> Option<Conflict> {
        let src = self.source.as_deref()?;
        source_conflict(&canonical(path), src)
    }

    // --- actions pane -------------------------------------------------------

    /// Rows in the actions pane: source, destination, Sync now, Map files, Backup now.
    const ACTION_COUNT: usize = 5;

    fn actions_up(&mut self) {
        self.actions_sel = self.actions_sel.saturating_sub(1);
    }

    fn actions_down(&mut self) {
        self.actions_sel = (self.actions_sel + 1).min(Self::ACTION_COUNT - 1);
    }

    /// Activate the highlighted action. Rows 0/1 open the folder browser for the
    /// source / a destination (adding a destination requires a source first); the
    /// "Sync now" row hands the config back to run a v1 mirror, and "Map files"
    /// opens the v2 file-mapping screen.
    fn activate_action(&mut self) {
        let target = match self.actions_sel {
            0 => PickTarget::Source,
            1 => PickTarget::Destination,
            2 => {
                self.confirm(); // "Sync now"
                return;
            }
            3 => {
                self.open_mappings(); // "Map files"
                return;
            }
            _ => {
                self.open_backup(); // "Backup now"
                return;
            }
        };
        if target == PickTarget::Destination && self.source.is_none() {
            self.error("Choose a source first — select \"Select source folder\".");
            return;
        }
        // Always begin a browse session at the project root (where `.bukagu`
        // lives), wherever a previous session may have wandered to.
        self.cwd = self.root.clone();
        self.refresh_browse();
        self.mode = Mode::Browsing(target);
        match target {
            PickTarget::Source => {
                self.info("Pick the SOURCE folder — Enter to choose, Esc cancels.")
            }
            PickTarget::Destination => self.info(
                "Pick DESTINATION folders — Enter selects, Enter again deselects, Esc when done.",
            ),
        }
    }

    // --- info pane accordion ------------------------------------------------

    /// The selectable rows, top to bottom: the `Destination Folders` header, then
    /// (when revealed) one row per destination, and under the expanded one a row per
    /// source-folder group. The source's files and the destination stats sit between
    /// these, always shown and never selectable.
    fn info_targets(&self) -> Vec<InfoTarget> {
        let mut targets = vec![InfoTarget::DestHeader];
        if self.info_dest_list_open {
            for i in 0..self.destinations.len() {
                targets.push(InfoTarget::Dest(i));
                if self.info_dest_expanded == Some(i) {
                    let groups = self.dest_mapping_groups(&self.destinations[i]);
                    targets.extend((0..groups.len()).map(InfoTarget::Folder));
                }
            }
        }
        targets
    }

    fn info_len(&self) -> usize {
        self.info_targets().len()
    }

    fn info_down(&mut self) {
        self.info_sel = (self.info_sel + 1).min(self.info_len().saturating_sub(1));
    }

    fn info_up(&mut self) {
        self.info_sel = self.info_sel.saturating_sub(1);
    }

    /// Act on `Enter`: the header reveals/hides the destination list; a destination
    /// row expands its stats + folder groups (closing whichever was open); a folder
    /// row expands its mapped files (closing whichever folder was open).
    fn info_enter(&mut self) {
        match self.info_targets().get(self.info_sel).copied() {
            Some(InfoTarget::DestHeader) => {
                self.info_dest_list_open = !self.info_dest_list_open;
                if !self.info_dest_list_open {
                    self.info_dest_expanded = None;
                    self.info_folder_expanded = None;
                    self.info_sel = 0;
                }
            }
            Some(InfoTarget::Dest(i)) => {
                // Accordion: open this one (re-pressing it closes it again). Opening
                // or switching destinations resets the nested folder accordion.
                self.info_dest_expanded = (self.info_dest_expanded != Some(i)).then_some(i);
                self.info_folder_expanded = None;
            }
            Some(InfoTarget::Folder(fi)) => {
                // Nested accordion: one source folder open at a time.
                self.info_folder_expanded = (self.info_folder_expanded != Some(fi)).then_some(fi);
            }
            None => {}
        }
    }

    /// Collapse one level: the open folder's files first, then the open destination,
    /// then the list.
    fn info_back(&mut self) {
        if self.info_folder_expanded.is_some() {
            self.info_folder_expanded = None;
        } else if self.info_dest_expanded.is_some() {
            self.info_dest_expanded = None;
        } else if self.info_dest_list_open {
            self.info_dest_list_open = false;
            self.info_sel = 0;
        }
    }

    /// Re-seat the accordion after the destination list may have changed (e.g.
    /// after returning from the browser), so no index dangles past the list.
    fn clamp_info(&mut self) {
        if let Some(i) = self.info_dest_expanded
            && i >= self.destinations.len()
        {
            self.info_dest_expanded = None;
        }
        // The open folder is indexed within the open destination's groups; drop it
        // if no destination is open or it now dangles past that group list.
        if self
            .info_folder_expanded
            .is_some_and(|fi| fi >= self.open_dest_groups().len())
        {
            self.info_folder_expanded = None;
        }
        self.info_sel = self.info_sel.min(self.info_len().saturating_sub(1));
    }

    // --- shared chrome ------------------------------------------------------

    fn cycle_main_focus(&mut self) {
        self.main_focus = self.main_focus.next();
    }

    fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// "Sync now": emit the config to sync if we have a source and ≥1 destination.
    fn confirm(&mut self) {
        if self.current_config().is_none() {
            if self.source.is_none() {
                self.error("Choose a source first ([s]).");
            } else {
                self.error("Add at least one destination ([a]) before syncing.");
            }
            return;
        }
        self.outcome = Some(Outcome::Confirmed);
    }

    /// "Map files": open the v2 file-mapping screen. Needs a source and ≥1
    /// destination (the folders mappings are picked from and written into).
    fn open_mappings(&mut self) {
        if self.current_config().is_none() {
            if self.source.is_none() {
                self.error("Choose a source first ([s]) before mapping files.");
            } else {
                self.error("Add at least one destination ([a]) before mapping files.");
            }
            return;
        }
        self.outcome = Some(Outcome::OpenMappings);
    }

    /// "Backup now": run a v3 encrypted backup of the source. Like the other
    /// actions it needs a source and ≥1 destination (bukagu's home is built around
    /// both); source-only backups are available from the `bukagu backup` CLI.
    fn open_backup(&mut self) {
        if self.current_config().is_none() {
            if self.source.is_none() {
                self.error("Choose a source first ([s]) before backing up.");
            } else {
                self.error("Add at least one destination ([a]) before backing up.");
            }
            return;
        }
        self.outcome = Some(Outcome::OpenBackup);
    }

    fn cancel(&mut self) {
        self.outcome = Some(Outcome::Cancelled);
    }

    /// Open the "really quit?" confirmation, focused on Cancel so an accidental
    /// Enter keeps bukagu open.
    fn prompt_quit(&mut self) {
        self.quit_prompt = Some(QuitChoice::Cancel);
    }

    /// Drive the quit-confirmation dialog: ←/→ (or Tab) move between the buttons,
    /// Enter acts on the focused one, and Esc/`n` back out while `y` confirms.
    fn on_key_quit_prompt(&mut self, code: KeyCode) {
        let focused = self.quit_prompt.unwrap_or(QuitChoice::Cancel);
        match code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Tab
            | KeyCode::BackTab => self.quit_prompt = Some(focused.toggle()),
            KeyCode::Enter => {
                self.quit_prompt = None;
                if focused == QuitChoice::Confirm {
                    self.cancel();
                }
            }
            KeyCode::Char('y') => {
                self.quit_prompt = None;
                self.cancel();
            }
            KeyCode::Char('n') | KeyCode::Esc => self.quit_prompt = None,
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // The quit-confirmation dialog is modal: while it's up, keys drive only it.
        if self.quit_prompt.is_some() {
            self.on_key_quit_prompt(key.code);
            return;
        }
        // Ctrl+Q always toggles the shortcuts overlay.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            self.toggle_help();
            return;
        }
        // While the overlay is open, any key just dismisses it.
        if self.show_help {
            self.show_help = false;
            return;
        }
        match self.mode {
            Mode::Browsing(_) => self.on_key_browsing(key.code),
            Mode::Main => self.on_key_main(key.code),
        }
    }

    fn on_key_main(&mut self, code: KeyCode) {
        match code {
            KeyCode::Tab | KeyCode::BackTab => self.cycle_main_focus(),
            // Direct shortcuts to the two actions, whatever the focus.
            KeyCode::Char('s') => {
                self.actions_sel = 0;
                self.activate_action();
            }
            KeyCode::Char('a') => {
                self.actions_sel = 1;
                self.activate_action();
            }
            KeyCode::Char('c') => self.confirm(),
            KeyCode::Char('m') => {
                self.actions_sel = 3;
                self.open_mappings();
            }
            KeyCode::Char('b') => {
                self.actions_sel = 4;
                self.open_backup();
            }
            KeyCode::Char('q') | KeyCode::Esc => self.prompt_quit(),
            _ => match self.main_focus {
                MainFocus::Actions => match code {
                    KeyCode::Up | KeyCode::Char('k') => self.actions_up(),
                    KeyCode::Down | KeyCode::Char('j') => self.actions_down(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.activate_action(),
                    _ => {}
                },
                MainFocus::Info => match code {
                    KeyCode::Up | KeyCode::Char('k') => self.info_up(),
                    KeyCode::Down | KeyCode::Char('j') => self.info_down(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.info_enter(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.info_back(),
                    _ => {}
                },
            },
        }
    }

    fn on_key_browsing(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.browse_up(),
            KeyCode::Down | KeyCode::Char('j') => self.browse_down(),
            // Enter opens the folder (same as → / l) — selection is on Space.
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.browse_descend(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.browse_ascend(),
            // Jump back to the launch folder (where bukagu was opened).
            KeyCode::Char('r') => self.browse_to_root(),
            // Space picks the source / toggles a destination.
            KeyCode::Char(' ') => self.browse_select(),
            // Finish here and return to the two-pane view (not the whole app).
            KeyCode::Char('q') | KeyCode::Esc => self.return_to_main(),
            _ => {}
        }
    }
}

/// How a candidate path conflicts with the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conflict {
    IsSource,
    Inside,
    Contains,
}

/// A user-facing reason a folder can't be a destination, shared by the validator
/// and the disabled-row hint in the browser.
fn conflict_reason(why: Conflict) -> &'static str {
    match why {
        Conflict::IsSource => "That's the source folder — it can't be a destination.",
        Conflict::Inside => "That folder is inside the source — pick one outside it.",
        Conflict::Contains => "That folder contains the source — pick one that doesn't.",
    }
}

/// Whether `cand` clashes with `src` such that a write into `cand` could reach the
/// read-only source. `None` means it's safe.
fn source_conflict(cand: &Path, src: &Path) -> Option<Conflict> {
    if cand == src {
        Some(Conflict::IsSource)
    } else if cand.starts_with(src) {
        Some(Conflict::Inside)
    } else if src.starts_with(cand) {
        Some(Conflict::Contains)
    } else {
        None
    }
}

/// Run the home screen, pre-filled from `initial` (a loaded config on returning
/// runs, or `None` on first run) and its `mappings` (so the info pane can show the
/// mapped-file counts), optionally showing `last_run` atop the info pane. Returns
/// the (possibly edited) config plus whether the user asked to sync or quit. The
/// terminal is always restored before returning.
pub fn run_home(
    initial: Option<Config>,
    mappings: Vec<Mapping>,
    last_run: Option<String>,
) -> Result<(Option<Config>, HomeIntent)> {
    let mut browser = Browser::home(initial, mappings, last_run);
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut browser);
    ratatui::restore();
    let intent = result?;
    Ok((browser.current_config(), intent))
}

fn run_loop(terminal: &mut DefaultTerminal, browser: &mut Browser) -> Result<HomeIntent> {
    loop {
        terminal.draw(|frame| draw(frame, browser))?;
        // Blocking read is fine here: the home only redraws in response to input
        // (the streaming dashboard polls instead).
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            browser.on_key(key);
        }
        if let Some(outcome) = browser.outcome.take() {
            return Ok(match outcome {
                Outcome::Confirmed => HomeIntent::Sync,
                Outcome::OpenMappings => HomeIntent::Mappings,
                Outcome::OpenBackup => HomeIntent::Backup,
                Outcome::Cancelled => HomeIntent::Quit,
            });
        }
    }
}

/// Subdirectories of `dir` (real directories only — symlinks are skipped, to
/// match the scanner's `follow_links(false)`), sorted case-insensitively.
fn read_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort_by_key(|p| {
        p.file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    dirs
}

/// A folder's roll-up totals, as shown in the info pane.
struct FolderStats {
    /// Sum of every (real, non-symlink) file's size, in bytes.
    bytes: u64,
    /// Number of real files (directories aren't counted).
    files: usize,
}

/// Recursively total a folder's real files: their count and combined size. Skips
/// symlinks (and doesn't descend symlinked dirs) to match the scanner, so these
/// numbers line up with what a sync would actually touch. Unreadable entries are
/// silently skipped; a missing/unreadable folder simply totals zero.
fn folder_stats(dir: &Path) -> FolderStats {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in WalkDir::new(dir).min_depth(1).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if entry.path_is_symlink() {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            files += 1;
            bytes += meta.len();
        }
    }
    FolderStats { bytes, files }
}

/// One indented [`Line`] summarising a folder: `size · N files · M mapped` (the
/// `mapped` count is supplied by the caller from the v2 mappings). A `None` path
/// (nothing chosen yet) gets a single italic note instead.
fn folder_stat_lines(path: Option<&Path>, mapped: usize, indent: &str) -> Vec<Line<'static>> {
    let Some(path) = path else {
        return vec![Line::from(Span::styled(
            format!("{indent}(not chosen yet)"),
            Style::default()
                .fg(theme::TEXT_DIM)
                .add_modifier(Modifier::ITALIC),
        ))];
    };
    let stats = folder_stats(path);
    let files_word = if stats.files == 1 { "file" } else { "files" };
    let summary = format!(
        "{}  ·  {} {}  ·  {} mapped",
        human_bytes(stats.bytes),
        stats.files,
        files_word,
        mapped,
    );
    vec![Line::from(Span::styled(
        format!("{indent}{summary}"),
        Style::default().fg(theme::TEXT),
    ))]
}

/// Best-effort canonicalization: fall back to the path as-is if it can't be
/// resolved (the navigated path exists, so this normally succeeds).
fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

// --- Rendering -------------------------------------------------------------

fn draw(frame: &mut Frame, b: &Browser) {
    let area = frame.area();
    // Fill the whole screen with the ember background first.
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

    match b.mode {
        Mode::Main => {
            let subtitle = if b.source.is_none() && b.destinations.is_empty() {
                "first-run setup"
            } else {
                "home"
            };
            draw_header(frame, b, header, subtitle);
            let [actions, info] =
                Layout::vertical([Constraint::Length(6), Constraint::Min(1)]).areas(body);
            draw_actions(frame, b, actions);
            draw_info(frame, b, info);
        }
        Mode::Browsing(target) => {
            let what = match target {
                PickTarget::Source => "select source folder",
                PickTarget::Destination => "select destination folders",
            };
            draw_header(frame, b, header, what);
            draw_modal(frame, b, body);
        }
    }

    draw_footer(frame, b, footer);

    if b.show_help {
        draw_help_overlay(frame, area);
    }
    // The quit dialog sits on top of everything else when open.
    if let Some(choice) = b.quit_prompt {
        draw_quit_dialog(frame, choice, area);
    }
}

fn draw_header(frame: &mut Frame, b: &Browser, area: Rect, subtitle: &str) {
    let title = Line::from(vec![
        Span::styled(
            "bukagu",
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ·  {subtitle}"),
            Style::default().fg(theme::TEXT_DIM),
        ),
    ]);
    // Pinned to the launch folder (where `.bukagu/` lives), not wherever the
    // browser has wandered — that current location lives in the modal title.
    let current = Line::from(vec![
        Span::styled("Root Project  ", Style::default().fg(theme::TEXT_DIM)),
        Span::styled(
            b.root.display().to_string(),
            Style::default().fg(theme::TEXT),
        ),
    ]);
    frame.render_widget(Paragraph::new(vec![title, current]), area);
}

/// Pick the border color for a panel based on whether it currently has focus.
fn border_for(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(theme::GOLD)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::PANEL_BORDER)
    }
}

fn draw_actions(frame: &mut Frame, b: &Browser, area: Rect) {
    let focused = b.main_focus == MainFocus::Actions;
    let block = Block::bordered()
        .title(Span::styled(
            " actions ",
            Style::default()
                .fg(theme::HEADER)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(border_for(focused));

    // Each row shows the action plus a hint of its current value.
    let source_hint = match &b.source {
        Some(s) => Span::styled(
            format!("  → {}", s.display()),
            Style::default().fg(theme::AMBER),
        ),
        None => Span::styled("  (not chosen)", Style::default().fg(theme::TEXT_DIM)),
    };
    let dest_hint = if b.destinations.is_empty() {
        Span::styled("  (none yet)", Style::default().fg(theme::TEXT_DIM))
    } else {
        Span::styled(
            format!("  ({} added)", b.destinations.len()),
            Style::default().fg(theme::COPY),
        )
    };
    let ready = b.source.is_some() && !b.destinations.is_empty();
    let sync_hint = if ready {
        Span::styled(
            "  (mirror source → destinations)",
            Style::default().fg(theme::COPY),
        )
    } else {
        Span::styled(
            "  (choose a source + destination first)",
            Style::default().fg(theme::TEXT_DIM),
        )
    };
    let map_hint = if ready {
        Span::styled(
            "  (map individual files, with a banner)",
            Style::default().fg(theme::COPY),
        )
    } else {
        Span::styled(
            "  (choose a source + destination first)",
            Style::default().fg(theme::TEXT_DIM),
        )
    };
    let backup_hint = if ready {
        Span::styled(
            "  (encrypted backup of the source → ~/bukagu-backups)",
            Style::default().fg(theme::COPY),
        )
    } else {
        Span::styled(
            "  (choose a source + destination first)",
            Style::default().fg(theme::TEXT_DIM),
        )
    };
    let rows = [
        ("Select source folder", source_hint),
        ("Select destination folder", dest_hint),
        ("Sync now", sync_hint),
        ("Map files", map_hint),
        ("Backup now", backup_hint),
    ];
    let items: Vec<ListItem> = rows
        .into_iter()
        .map(|(label, hint)| {
            ListItem::new(Line::from(vec![
                Span::styled(label, Style::default().fg(theme::TEXT)),
                hint,
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(theme::SELECTION)
                .fg(theme::BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    state.select(Some(b.actions_sel));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_info(frame: &mut Frame, b: &Browser, area: Rect) {
    let focused = b.main_focus == MainFocus::Info;
    let dim = Style::default().fg(theme::TEXT_DIM);
    let header = Style::default()
        .fg(theme::GOLD)
        .add_modifier(Modifier::BOLD);

    let block = Block::bordered()
        .title(Span::styled(
            " info ",
            Style::default()
                .fg(theme::HEADER)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(border_for(focused));

    // One ListItem per visual line. `sel_rows[t]` is the item index of the t-th
    // selectable target, so the cursor highlights exactly that row.
    let mut items: Vec<ListItem> = Vec::new();
    let mut sel_rows: Vec<usize> = Vec::new();

    // --- Last sync result, when we've run one this session ---
    if let Some(last) = &b.last_run {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("Last run  ", header),
            Span::styled(last.clone(), Style::default().fg(theme::SUCCESS)),
        ])));
        items.push(ListItem::new(Line::from(""))); // spacer
    }

    // --- Source: header + stats, always shown ---
    match b.source.as_deref() {
        Some(src) => items.push(ListItem::new(Line::from(vec![
            Span::styled("Source  ", header),
            Span::styled(src.display().to_string(), Style::default().fg(theme::AMBER)),
        ]))),
        None => items.push(ListItem::new(Line::from(vec![
            Span::styled("Source  ", header),
            Span::styled("(not chosen yet)", dim.add_modifier(Modifier::ITALIC)),
        ]))),
    }
    let src_lines = folder_stat_lines(b.source.as_deref(), b.source_mapped(), "    ");
    items.extend(src_lines.into_iter().map(ListItem::new));
    items.push(ListItem::new(Line::from(""))); // spacer

    // --- Destination Folders header (selectable target 0) ---
    let caret = if b.info_dest_list_open { "▾" } else { "▸" };
    sel_rows.push(items.len());
    items.push(ListItem::new(Line::from(Span::styled(
        format!("{caret} Destination Folders ({})", b.destinations.len()),
        header,
    ))));

    // --- the destination list, when revealed ---
    if b.info_dest_list_open {
        if b.destinations.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "    (none yet — add some from the actions pane)",
                dim.add_modifier(Modifier::ITALIC),
            ))));
        } else {
            for (i, d) in b.destinations.iter().enumerate() {
                let open = b.info_dest_expanded == Some(i);
                let caret = if open { "▾" } else { "▸" };
                sel_rows.push(items.len());
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(format!("  {caret} "), dim),
                    Span::styled(d.display().to_string(), Style::default().fg(theme::COPY)),
                ])));
                if open {
                    let lines = folder_stat_lines(Some(d), b.dest_mapped(d), "      ");
                    items.extend(lines.into_iter().map(ListItem::new));
                    // Nested accordion: the files mapped into this destination,
                    // grouped by their source folder (one group open at a time).
                    let groups = b.dest_mapping_groups(d);
                    if groups.is_empty() {
                        items.push(ListItem::new(Line::from(Span::styled(
                            "      (no files mapped into this folder yet)",
                            dim.add_modifier(Modifier::ITALIC),
                        ))));
                    } else {
                        for (fi, (folder, files)) in groups.iter().enumerate() {
                            let fopen = b.info_folder_expanded == Some(fi);
                            let fcaret = if fopen { "▾" } else { "▸" };
                            let label = if folder.as_os_str().is_empty() {
                                "(source root)".to_string()
                            } else {
                                format!("{}/", folder.display())
                            };
                            sel_rows.push(items.len());
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled(format!("      {fcaret} "), dim),
                                Span::styled(
                                    label,
                                    Style::default()
                                        .fg(theme::AMBER)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(format!("  ({})", files.len()), dim),
                            ])));
                            if fopen {
                                for (name, target_rel) in files {
                                    items.push(ListItem::new(Line::from(vec![
                                        Span::styled(
                                            format!("          {}", name.display()),
                                            Style::default().fg(theme::AMBER),
                                        ),
                                        Span::styled("  →  ", dim),
                                        Span::styled(
                                            target_rel.display().to_string(),
                                            Style::default().fg(theme::COPY),
                                        ),
                                    ])));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(theme::SELECTION)
            .fg(theme::BG)
            .add_modifier(Modifier::BOLD),
    );

    // Highlight the row backing the selected target (clamped defensively).
    let mut state = ListState::default();
    let idx = b.info_sel.min(sel_rows.len().saturating_sub(1));
    state.select(sel_rows.get(idx).copied());
    frame.render_stateful_widget(list, area, &mut state);
}

/// The full-screen folder browser (active while [`Mode::Browsing`]).
fn draw_modal(frame: &mut Frame, b: &Browser, area: Rect) {
    // Only reached in Browsing mode, so the target is always present.
    let pick_dest = matches!(b.mode, Mode::Browsing(PickTarget::Destination));

    let block = Block::bordered()
        .title(Span::styled(
            format!(" {} ", b.cwd.display()),
            Style::default()
                .fg(theme::HEADER)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        );

    if b.entries.is_empty() {
        let empty = if pick_dest {
            "(no subfolders here — Space selects this folder, r jumps to the launch folder, ← goes up)"
        } else {
            "(no subfolders here — Space picks this folder, r jumps to the launch folder, ← goes up)"
        };
        let note = Paragraph::new(empty)
            .style(
                Style::default()
                    .fg(theme::TEXT_DIM)
                    .add_modifier(Modifier::ITALIC),
            )
            .wrap(Wrap { trim: true })
            .block(block);
        frame.render_widget(note, area);
        return;
    }

    let selected = b.browse_sel.selected();
    let items: Vec<ListItem> = b
        .entries
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            // Folders that can't be a destination (the source, inside it, or
            // containing it) are disabled — grayed out and not selectable.
            let blocked = if pick_dest { b.dest_conflict(p) } else { None };
            let chosen = pick_dest && b.is_destination(p);

            let name_style = if blocked.is_some() {
                Style::default().fg(theme::TEXT_DIM)
            } else {
                Style::default().fg(theme::TEXT)
            };
            let mut spans = vec![
                Span::styled(name, name_style),
                Span::styled("/", Style::default().fg(theme::TEXT_DIM)),
            ];

            if let Some(why) = blocked {
                let tag = match why {
                    Conflict::IsSource => "  ✗ source folder",
                    Conflict::Inside => "  ✗ inside the source",
                    Conflict::Contains => "  ✗ contains the source",
                };
                spans.push(Span::styled(
                    tag,
                    Style::default()
                        .fg(theme::TEXT_DIM)
                        .add_modifier(Modifier::ITALIC),
                ));
            } else if chosen {
                // Mark destinations already in the list, so re-pressing Enter to
                // deselect is discoverable.
                spans.push(Span::styled(
                    "  ✓ selected",
                    Style::default()
                        .fg(theme::COPY)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            if selected == Some(i) {
                let hint = if blocked.is_some() {
                    "   can't be a destination · Enter/→ to open"
                } else if pick_dest {
                    if chosen {
                        "   Space to deselect · Enter/→ to open"
                    } else {
                        "   Space to select · Enter/→ to open"
                    }
                } else {
                    "   Space to choose · Enter/→ to open"
                };
                spans.push(Span::styled(hint, Style::default().fg(theme::TEXT_DIM)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(theme::SELECTION)
                .fg(theme::BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut state = b.browse_sel;
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_footer(frame: &mut Frame, b: &Browser, area: Rect) {
    let keys = match b.mode {
        Mode::Browsing(PickTarget::Source) => {
            "↑/↓ move  →/Enter open  ←/Bksp up  r root  Space pick  Esc cancel  Ctrl+Q keys"
        }
        Mode::Browsing(PickTarget::Destination) => {
            "↑/↓ move  →/Enter open  ←/Bksp up  r root  Space select/deselect  Esc done  Ctrl+Q keys"
        }
        Mode::Main => match b.main_focus {
            MainFocus::Actions => {
                "↑/↓ choose  Enter activate  [s] source  [a] dest  [c] sync  [m] map files  Tab focus  [q] quit"
            }
            MainFocus::Info => {
                "↑/↓ move  Enter expand/collapse  Bksp collapse  [c] sync  [m] map files  Tab focus  [q] quit"
            }
        },
    };
    let status_color = if b.status_is_error {
        theme::ERROR
    } else {
        theme::ACCENT
    };
    let status = Line::from(Span::styled(
        b.status.clone(),
        Style::default().fg(status_color),
    ));
    let key_line = Line::from(Span::styled(keys, Style::default().fg(theme::TEXT_DIM)));
    frame.render_widget(Paragraph::new(vec![status, key_line]), area);
}

/// Full-screen shortcuts overlay, toggled with Ctrl+Q.
fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let [v] = Layout::vertical([Constraint::Percentage(85)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Percentage(72)])
        .flex(Flex::Center)
        .areas(v);

    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(Span::styled(
            " shortcuts ",
            Style::default()
                .fg(theme::GOLD)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme::GOLD))
        .style(Style::default().bg(theme::BG).fg(theme::TEXT));

    let key = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {k:<16}"),
                Style::default()
                    .fg(theme::GOLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(what.to_string(), Style::default().fg(theme::TEXT)),
        ])
    };
    let section = |t: &str| {
        Line::from(Span::styled(
            t.to_string(),
            Style::default()
                .fg(theme::HEADER)
                .add_modifier(Modifier::BOLD),
        ))
    };

    let lines = vec![
        Line::from(""),
        section("  Actions pane"),
        key("↑ / ↓", "choose an action"),
        key("Enter", "open the folder browser / run Sync now"),
        key("s / a", "jump straight to source / destination"),
        key("c", "Sync now (mirror source → destinations)"),
        key(
            "m",
            "Map files (source file → destination file, with a banner)",
        ),
        key("b", "Backup now (encrypted backup of the source)"),
        Line::from(""),
        section("  Folder browser"),
        key("↑ / ↓  j / k", "move the selection"),
        key(
            "→ / l / Enter",
            "open (descend into) the highlighted folder",
        ),
        key("← / Bksp / h", "go up to the parent folder"),
        key("r", "jump back to the launch folder"),
        key("Space (source)", "pick the highlighted folder and return"),
        key(
            "Space (dest.)",
            "select / deselect (Space again) — multi-pick",
        ),
        key("Esc / q", "finish / cancel and go back"),
        Line::from(""),
        section("  Info pane"),
        key("(source)", "its size, file & mapped counts sit up top"),
        key("↑ / ↓", "move between the Destinations header and folders"),
        key(
            "Enter / →",
            "reveal the list / expand a folder, then a source-folder group (one open per level)",
        ),
        key(
            "Backspace / ←",
            "collapse the open group, then the folder, then the list",
        ),
        Line::from(""),
        section("  Anywhere"),
        key("Tab", "switch focus between actions and info"),
        key("Ctrl+Q", "toggle this overlay"),
        key(
            "q / Esc",
            "close bukagu (asks you to confirm; edits are kept)",
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to close.",
            Style::default()
                .fg(theme::TEXT_DIM)
                .add_modifier(Modifier::ITALIC),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// A small centered "really quit?" confirmation. The focused button is
/// highlighted; the dialog opens focused on Cancel so a stray Enter is harmless.
fn draw_quit_dialog(frame: &mut Frame, choice: QuitChoice, area: Rect) {
    let [v] = Layout::vertical([Constraint::Length(7)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(50)])
        .flex(Flex::Center)
        .areas(v);

    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(Span::styled(
            " close bukagu? ",
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
        Paragraph::new("Do you really want to close Bukagu?")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme::TEXT)),
        msg_area,
    );

    let buttons = Line::from(vec![
        quit_button("Confirm", choice == QuitChoice::Confirm),
        Span::raw("      "),
        quit_button("Cancel", choice == QuitChoice::Cancel),
    ]);
    frame.render_widget(
        Paragraph::new(buttons).alignment(Alignment::Center),
        buttons_area,
    );
}

/// One button in the quit dialog: the focused one is reverse-highlighted in gold
/// (matching list selections elsewhere), the other is dimmed.
fn quit_button(label: &str, focused: bool) -> Span<'static> {
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
    use std::fs;
    use tempfile::tempdir;

    /// Build a browser with `source` already chosen, so the destination guardrail
    /// and pruning can be exercised without the TUI.
    fn with_source(source: PathBuf) -> Browser {
        let mut b = Browser::new();
        b.source = Some(source);
        b
    }

    #[test]
    fn read_subdirs_lists_only_dirs_sorted() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("Zeta")).unwrap();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::write(root.join("a-file.txt"), b"x").unwrap();

        let dirs = read_subdirs(root);
        let names: Vec<String> = dirs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["alpha", "Zeta"]);
    }

    #[test]
    fn set_source_records_the_path() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        fs::create_dir(&src).unwrap();

        let mut b = Browser::new();
        b.set_source(src.clone());
        assert_eq!(b.source.as_deref(), Some(src.as_path()));
    }

    #[test]
    fn destination_must_not_be_or_touch_the_source() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        let inside = src.join("sub");
        let parent = tmp.path().to_path_buf(); // contains src
        let sibling = tmp.path().join("backup");
        for p in [&src, &inside, &sibling] {
            fs::create_dir_all(p).unwrap();
        }

        let b = with_source(src.clone());
        assert!(b.validate_destination(&src).is_err(), "source itself");
        assert!(b.validate_destination(&inside).is_err(), "inside source");
        assert!(b.validate_destination(&parent).is_err(), "contains source");
        assert!(b.validate_destination(&sibling).is_ok(), "valid sibling");
    }

    #[test]
    fn source_is_disabled_in_the_destination_picker() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        let inside = src.join("sub");
        let parent = tmp.path().to_path_buf(); // contains src
        let sibling = tmp.path().join("backup");
        for p in [&src, &inside, &sibling] {
            fs::create_dir_all(p).unwrap();
        }

        let mut b = with_source(canonical(&src));
        b.mode = Mode::Browsing(PickTarget::Destination);

        // The source, anything inside it, and anything containing it are blocked.
        assert_eq!(b.dest_conflict(&src), Some(Conflict::IsSource));
        assert_eq!(b.dest_conflict(&inside), Some(Conflict::Inside));
        assert_eq!(b.dest_conflict(&parent), Some(Conflict::Contains));
        assert_eq!(
            b.dest_conflict(&sibling),
            None,
            "a safe sibling is selectable"
        );

        // Pressing Enter on the (disabled) source adds nothing and is NOT a red error.
        b.entries = vec![canonical(&src)];
        b.browse_sel.select(Some(0));
        b.browse_select();
        assert!(
            b.destinations.is_empty(),
            "source can't be added as a destination"
        );
        assert!(
            !b.status_is_error,
            "a disabled row gives a calm note, not an error"
        );
    }

    #[test]
    fn draw_modal_grays_out_the_disabled_source() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        let sib = tmp.path().join("backup");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&sib).unwrap();

        let mut b = with_source(canonical(&src));
        b.mode = Mode::Browsing(PickTarget::Destination);
        b.entries = vec![canonical(&src), canonical(&sib)];
        b.browse_sel.select(Some(1)); // highlight the selectable sibling

        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        terminal.draw(|f| draw(f, &b)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(
            rendered.contains("✗ source folder"),
            "the source row is tagged as a disabled destination"
        );
    }

    #[test]
    fn choosing_an_existing_destination_as_the_new_source_drops_it() {
        let tmp = tempdir().unwrap();
        let s0 = tmp.path().join("s0");
        let a = tmp.path().join("a");
        let bdir = tmp.path().join("b");
        for p in [&s0, &a, &bdir] {
            fs::create_dir_all(p).unwrap();
        }

        let mut b = with_source(canonical(&s0));
        b.destinations = vec![canonical(&a), canonical(&bdir)];

        // Re-point the source at `a`, which is already a destination — it must drop
        // out of the destination list immediately.
        b.set_source(canonical(&a));
        assert_eq!(b.source.as_deref(), Some(canonical(&a).as_path()));
        assert_eq!(b.destinations, vec![canonical(&bdir)], "`a` removed");
        assert!(b.status_is_error, "the pruning is reported as a warning");
    }

    #[test]
    fn add_destination_requires_a_source_first() {
        let tmp = tempdir().unwrap();
        let dst = tmp.path().join("backup");
        fs::create_dir(&dst).unwrap();

        let mut b = Browser::new();
        assert!(b.add_destination(dst).is_err(), "no source yet");
        assert!(b.destinations.is_empty());
    }

    #[test]
    fn duplicate_destination_is_rejected() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        let dst = tmp.path().join("backup");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        let mut b = with_source(src);
        assert!(b.add_destination(dst.clone()).is_ok());
        assert!(b.add_destination(dst).is_err(), "already added");
        assert_eq!(b.destinations.len(), 1);
    }

    #[test]
    fn choosing_source_prunes_conflicting_destinations() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let outside = root.join("outside");
        let parent = root.join("parent");
        let inside_a = parent.join("a");
        let inside_b = parent.join("b");
        for p in [&outside, &inside_a, &inside_b] {
            fs::create_dir_all(p).unwrap();
        }

        let mut b = with_source(canonical(&outside));
        b.destinations = vec![
            canonical(&outside),
            canonical(&inside_a),
            canonical(&inside_b),
        ];
        // Make `parent` the new source — both destinations inside it must drop.
        b.set_source(canonical(&parent));

        assert_eq!(b.source.as_deref(), Some(canonical(&parent).as_path()));
        assert_eq!(b.destinations, vec![canonical(&outside)]);
        assert!(b.status_is_error, "pruning is reported as a warning");
    }

    #[test]
    fn activate_destination_action_requires_source() {
        let mut b = Browser::new();
        b.actions_sel = 1; // "Select destination folder"
        b.activate_action();
        assert_eq!(b.mode, Mode::Main, "no source → modal does not open");
        assert!(b.status_is_error);

        b.source = Some(PathBuf::from("/somewhere"));
        b.activate_action();
        assert_eq!(b.mode, Mode::Browsing(PickTarget::Destination));
    }

    #[test]
    fn activate_source_action_opens_browser() {
        let mut b = Browser::new();
        b.actions_sel = 0;
        b.activate_action();
        assert_eq!(b.mode, Mode::Browsing(PickTarget::Source));
    }

    #[test]
    fn info_accordion_reveals_list_and_expands_one_at_a_time() {
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("a");
        let bdir = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&bdir).unwrap();

        let mut b = with_source(tmp.path().join("src"));
        b.destinations = vec![a, bdir];

        // Start collapsed: the only selectable row is the Destinations header.
        assert!(!b.info_dest_list_open);
        assert_eq!(b.info_len(), 1);

        // Enter on the header reveals the destination list.
        b.info_enter();
        assert!(b.info_dest_list_open);
        assert_eq!(b.info_len(), 3, "header + 2 destinations");

        // Expand the first destination.
        b.info_down();
        assert_eq!(b.info_sel, 1);
        b.info_enter();
        assert_eq!(b.info_dest_expanded, Some(0));

        // Expanding the second closes the first (accordion).
        b.info_down();
        b.info_enter();
        assert_eq!(b.info_dest_expanded, Some(1));

        // Enter again on the open one collapses it.
        b.info_enter();
        assert_eq!(b.info_dest_expanded, None);

        // Backspace collapses the list and re-seats the cursor on the header.
        b.info_back();
        assert!(!b.info_dest_list_open);
        assert_eq!(b.info_sel, 0);
    }

    #[test]
    fn info_accordion_nests_source_folders_one_open_at_a_time() {
        let mut b = with_source(PathBuf::from("/src"));
        b.destinations = vec![PathBuf::from("/d0"), PathBuf::from("/d1")];
        // d0 has two source-folder groups (docs, zods); d1 has one (zods).
        b.mappings = vec![
            Mapping {
                source_rel: PathBuf::from("zods/api.ts"),
                targets: vec![PathBuf::from("/d0/api.ts"), PathBuf::from("/d1/x.ts")],
            },
            Mapping {
                source_rel: PathBuf::from("docs/licence.txt"),
                targets: vec![PathBuf::from("/d0/later.txt")],
            },
        ];

        b.info_enter(); // reveal the destination list
        b.info_down(); // onto d0
        b.info_enter(); // expand d0
        assert_eq!(b.info_dest_expanded, Some(0));

        // Targets now: header, Dest(0), Folder(0)=docs, Folder(1)=zods, Dest(1).
        let targets = b.info_targets();
        assert!(matches!(targets.get(2), Some(InfoTarget::Folder(0))));
        assert!(matches!(targets.get(3), Some(InfoTarget::Folder(1))));
        assert!(matches!(targets.get(4), Some(InfoTarget::Dest(1))));

        // Open the first folder group, then the second — the second closes the first.
        b.info_sel = 2;
        b.info_enter();
        assert_eq!(b.info_folder_expanded, Some(0));
        b.info_sel = 3;
        b.info_enter();
        assert_eq!(b.info_folder_expanded, Some(1));

        // Switching to another destination resets the open folder group.
        b.info_sel = 4;
        b.info_enter();
        assert_eq!(b.info_dest_expanded, Some(1));
        assert_eq!(b.info_folder_expanded, None);

        // With d1 open, targets are: header, Dest(0), Dest(1), Folder(0). Open d1's
        // one folder group, then check Backspace collapses folder → destination →
        // list, in that order.
        b.info_sel = 3; // d1's one source-folder group
        b.info_enter();
        assert_eq!(b.info_folder_expanded, Some(0));
        b.info_back();
        assert_eq!(b.info_folder_expanded, None);
        assert_eq!(b.info_dest_expanded, Some(1));
        b.info_back();
        assert_eq!(b.info_dest_expanded, None);
    }

    #[test]
    fn draw_info_shows_source_and_expanded_destination_stats() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        // Source: 1 file; destination: 2 files — so the two stat lines differ and
        // we can tell both rendered.
        fs::write(src.join("hello.txt"), b"hi").unwrap();
        fs::write(dst.join("inside.txt"), b"yo").unwrap();
        fs::write(dst.join("extra.txt"), b"!!").unwrap();

        let mut b = with_source(canonical(&src));
        b.destinations = vec![canonical(&dst)];
        // Map the one source file onto one of the destination's files, so both the
        // source and the destination report a non-zero mapped count.
        b.mappings = vec![Mapping {
            source_rel: PathBuf::from("hello.txt"),
            targets: vec![canonical(&dst).join("inside.txt")],
        }];
        b.main_focus = MainFocus::Info;
        b.info_enter(); // reveal the destination list
        b.info_down(); // move onto the destination
        b.info_enter(); // expand its stats + source-folder groups
        b.info_down(); // move onto the (source root) group
        b.info_enter(); // expand the group to reveal its mapped files

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &b)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(rendered.contains("Source"), "source header always shown");
        assert!(
            rendered.contains("1 file"),
            "source stats (1 file) shown up front"
        );
        assert!(
            rendered.contains("Destination Folders (1)"),
            "destinations header with count"
        );
        assert!(
            rendered.contains("2 files"),
            "expanded destination shows its own stats inline"
        );
        assert!(
            rendered.contains("1 mapped"),
            "the real mapped count is shown for the mapped source and destination"
        );
        assert!(
            !rendered.contains("0 mapped"),
            "the mapped count reflects the mappings, not the old placeholder 0"
        );
        // The expanded destination groups its mapped files by source folder; the
        // root-level source file lands in the "(source root)" group, and expanding
        // that group lists the file as source → target.
        assert!(
            rendered.contains("(source root)"),
            "mapped files are grouped under a source-folder accordion"
        );
        assert!(
            rendered.contains("hello.txt") && rendered.contains("inside.txt"),
            "the expanded group lists its mapped files (source → target)"
        );
    }

    #[test]
    fn draw_home_shows_sync_now_action_and_last_run() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        let config = Config {
            source: canonical(&src),
            destinations: vec![canonical(&dst)],
        };
        let b = Browser::home(
            Some(config),
            Vec::new(),
            Some("Everything is already in sync — nothing to do.".into()),
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &b)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(
            rendered.contains("Sync now"),
            "the Sync now action is shown"
        );
        assert!(
            rendered.contains("Last run"),
            "the previous sync result is shown atop the info pane"
        );
        assert!(
            rendered.contains("already in sync"),
            "the last-run summary text is shown"
        );
    }

    #[test]
    fn clamp_info_drops_a_stale_expanded_destination() {
        let mut b = Browser::new();
        b.destinations = vec![PathBuf::from("/d0"), PathBuf::from("/d1")];
        b.info_dest_list_open = true;
        b.info_dest_expanded = Some(1);
        b.info_sel = 2;

        // The list shrank (e.g. a destination was pruned) — clamp must not dangle.
        b.destinations.truncate(1);
        b.clamp_info();
        assert_eq!(b.info_dest_expanded, None, "index 1 no longer exists");
        assert_eq!(b.info_sel, 1, "cursor pulled back into range");
    }

    #[test]
    fn tab_cycles_main_focus() {
        let mut b = Browser::new();
        assert_eq!(b.main_focus, MainFocus::Actions);
        b.cycle_main_focus();
        assert_eq!(b.main_focus, MainFocus::Info);
        b.cycle_main_focus();
        assert_eq!(b.main_focus, MainFocus::Actions);
    }

    #[test]
    fn ctrl_q_toggles_help() {
        let mut b = Browser::new();
        assert!(!b.show_help);
        b.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(b.show_help);
        // Any key dismisses the overlay rather than acting.
        b.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(!b.show_help);
    }

    #[test]
    fn confirm_needs_source_and_a_destination() {
        let mut b = Browser::new();
        b.confirm();
        assert!(b.outcome.is_none(), "nothing chosen");

        b.source = Some(PathBuf::from("/src"));
        b.confirm();
        assert!(b.outcome.is_none(), "no destination yet");

        b.destinations.push(PathBuf::from("/dst"));
        b.confirm();
        assert!(
            matches!(b.outcome, Some(Outcome::Confirmed)),
            "source + destination → Sync now"
        );
    }

    #[test]
    fn sync_now_action_confirms_when_ready() {
        let mut b = Browser::new();
        // The third action row is "Sync now".
        b.actions_sel = 2;

        b.activate_action(); // no source/dest yet → refuses, stays put
        assert!(b.outcome.is_none());
        assert!(b.status_is_error);

        b.source = Some(PathBuf::from("/src"));
        b.destinations.push(PathBuf::from("/dst"));
        b.activate_action(); // now ready → confirms a sync
        assert!(matches!(b.outcome, Some(Outcome::Confirmed)));
    }

    #[test]
    fn map_files_action_opens_mappings_when_ready() {
        let mut b = Browser::new();
        // The fourth action row is "Map files".
        b.actions_sel = 3;

        b.activate_action(); // no source/dest yet → refuses, stays put
        assert!(b.outcome.is_none());
        assert!(b.status_is_error);

        b.source = Some(PathBuf::from("/src"));
        b.destinations.push(PathBuf::from("/dst"));
        b.activate_action(); // now ready → opens the mapping screen
        assert!(matches!(b.outcome, Some(Outcome::OpenMappings)));
    }

    #[test]
    fn backup_now_action_opens_backup_when_ready() {
        let mut b = Browser::new();
        // The fifth action row is "Backup now".
        b.actions_sel = 4;

        b.activate_action(); // no source/dest yet → refuses, stays put
        assert!(b.outcome.is_none());
        assert!(b.status_is_error);

        b.source = Some(PathBuf::from("/src"));
        b.destinations.push(PathBuf::from("/dst"));
        b.activate_action(); // now ready → asks to run a backup
        assert!(matches!(b.outcome, Some(Outcome::OpenBackup)));
    }

    #[test]
    fn home_prefills_from_a_loaded_config_and_reads_it_back() {
        let config = Config {
            source: PathBuf::from("/src"),
            destinations: vec![PathBuf::from("/d0"), PathBuf::from("/d1")],
        };
        let b = Browser::home(
            Some(config.clone()),
            Vec::new(),
            Some("Last run — all good.".into()),
        );
        assert_eq!(b.source.as_deref(), Some(config.source.as_path()));
        assert_eq!(b.destinations, config.destinations);
        assert_eq!(b.last_run.as_deref(), Some("Last run — all good."));
        assert_eq!(b.current_config(), Some(config));

        // A fresh home has nothing to sync yet.
        assert_eq!(Browser::new().current_config(), None);
    }

    #[test]
    fn space_toggles_a_destination_in_then_out() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        let dst = tmp.path().join("backup");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        let mut b = with_source(canonical(&src));
        b.mode = Mode::Browsing(PickTarget::Destination);
        // Point the browser cursor at `dst`.
        b.entries = vec![canonical(&dst)];
        b.browse_sel.select(Some(0));

        b.browse_select(); // first Space selects
        assert_eq!(b.destinations, vec![canonical(&dst)]);
        assert_eq!(
            b.mode,
            Mode::Browsing(PickTarget::Destination),
            "stays open"
        );

        b.browse_select(); // second Space deselects
        assert!(b.destinations.is_empty(), "toggled back off");
        assert_eq!(
            b.mode,
            Mode::Browsing(PickTarget::Destination),
            "still open"
        );
    }

    #[test]
    fn space_selects_and_enter_opens_in_the_modal() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("master");
        let dst = tmp.path().join("backup");
        let nested = dst.join("nested");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&nested).unwrap();

        let mut b = with_source(canonical(&src));
        b.mode = Mode::Browsing(PickTarget::Destination);
        b.cwd = tmp.path().to_path_buf();
        b.entries = vec![canonical(&dst)];
        b.browse_sel.select(Some(0));

        // Space selects the highlighted folder as a destination.
        b.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(b.destinations, vec![canonical(&dst)], "Space selects");

        // Enter now *opens* (descends) instead of selecting — cwd moves into it.
        b.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(b.cwd, canonical(&dst), "Enter opens the folder");
        assert_eq!(
            b.destinations,
            vec![canonical(&dst)],
            "Enter didn't toggle the selection"
        );
    }

    #[test]
    fn esc_on_main_opens_quit_confirmation_focused_on_cancel() {
        let mut b = Browser::new();
        b.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            b.quit_prompt,
            Some(QuitChoice::Cancel),
            "Esc asks first, focused on Cancel so a stray Enter is harmless"
        );
        assert!(b.outcome.is_none(), "the app has not quit yet");
    }

    #[test]
    fn confirming_the_quit_dialog_leaves_the_home() {
        let mut b = Browser::new();
        b.prompt_quit();
        // Move focus onto Confirm, then activate it.
        b.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(b.quit_prompt, Some(QuitChoice::Confirm));
        b.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(b.quit_prompt.is_none(), "the dialog closes");
        assert!(
            matches!(b.outcome, Some(Outcome::Cancelled)),
            "confirming quits the home"
        );
    }

    #[test]
    fn cancelling_the_quit_dialog_keeps_bukagu_open() {
        let mut b = Browser::new();
        b.prompt_quit();
        // Cancel is focused by default — Enter just dismisses the dialog.
        b.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(b.quit_prompt.is_none(), "the dialog is dismissed");
        assert!(b.outcome.is_none(), "still on the home");
    }

    #[test]
    fn esc_dismisses_the_quit_dialog_without_quitting() {
        let mut b = Browser::new();
        b.prompt_quit();
        b.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            b.quit_prompt.is_none(),
            "a second Esc backs out of the dialog"
        );
        assert!(b.outcome.is_none(), "and does not quit");
    }

    #[test]
    fn quit_dialog_renders_prompt_and_buttons() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut b = Browser::new();
        b.prompt_quit();

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &b)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(rendered.contains("Do you really want to close Bukagu?"));
        assert!(rendered.contains("Confirm"));
        assert!(rendered.contains("Cancel"));
    }

    #[test]
    fn jump_to_root_returns_to_the_launch_folder() {
        let mut b = Browser::new();
        let root = b.root.clone();
        // Wander up to the parent, then jump straight back to the launch folder.
        b.cwd = root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.clone());
        b.refresh_browse();
        b.browse_to_root();
        assert_eq!(b.cwd, root);
    }
}

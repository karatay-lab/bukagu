//! Dashboard state and the sync state machine (Step 5).
//!
//! This is the dashboard's brain, deliberately **terminal-free**: the render loop
//! in [`crate::ui::dashboard`] feeds worker [`Message`]s into [`App::on_message`]
//! and reads back the current [`Phase`] to decide what to draw. Keeping all the
//! transitions here means the whole flow is unit-testable without a TUI.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::core::{Plan, Stats};
use crate::message::Message;
use crate::store::Config;

/// Where the dashboard currently is. First-run onboarding is handled separately
/// by the directory browser, so it is not a phase here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Walking the source and destinations, building one plan per destination.
    Scanning,
    /// Browsing the color-coded diff, awaiting confirmation.
    Review,
    /// Writing the planned changes into the destinations.
    Applying,
    /// Finished — showing the summary.
    Done,
    /// A worker error — showing the error screen.
    Error,
}

/// Run-time options taken straight from the CLI flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Preview only — validate and plan, but write nothing.
    pub dry_run: bool,
    /// Skip the interactive Review and apply immediately.
    pub yes: bool,
    /// Delete destination entries that no longer exist in the source.
    pub delete: bool,
}

/// One destination's computed plan, kept for the Review list and the apply phase.
#[derive(Debug, Clone)]
pub struct DestPlan {
    pub destination: PathBuf,
    pub plan: Plan,
}

/// Top-level dashboard state shared by every screen.
#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub opts: RunOptions,
    pub phase: Phase,
    /// Plans gathered during scanning — one per destination, in arrival order.
    pub plans: Vec<DestPlan>,
    /// Apply progress per destination: `(actions done, total actions)`.
    pub apply_progress: HashMap<PathBuf, (usize, usize)>,
    /// Rolled-up totals, set once the sync (or preview) finishes.
    pub stats: Option<Stats>,
    /// First visible row in the Review list.
    pub review_scroll: usize,
    /// One-line footer status.
    pub status: String,
    /// Set on a worker error; the loop surfaces it once the user dismisses the screen.
    pub error: Option<String>,
    pub should_quit: bool,
}

impl App {
    /// Start a returning run at [`Phase::Scanning`].
    pub fn new(config: Config, opts: RunOptions) -> Self {
        Self {
            config,
            opts,
            phase: Phase::Scanning,
            plans: Vec::new(),
            apply_progress: HashMap::new(),
            stats: None,
            review_scroll: 0,
            status: "Scanning…".into(),
            error: None,
            should_quit: false,
        }
    }

    /// How many destinations we expect a plan for.
    pub fn expected_plans(&self) -> usize {
        self.config.destinations.len()
    }

    /// Total actions across every destination's plan.
    pub fn total_actions(&self) -> usize {
        self.plans.iter().map(|d| d.plan.actions.len()).sum()
    }

    /// Total actions already applied across destinations.
    pub fn applied_actions(&self) -> usize {
        self.apply_progress.values().map(|(done, _)| *done).sum()
    }

    /// True once scanning finished with no changes to make anywhere.
    pub fn nothing_to_do(&self) -> bool {
        self.plans.iter().all(|d| d.plan.is_empty())
    }

    /// Number of rows the Review list renders: one header per destination plus
    /// each of its actions.
    pub fn review_line_count(&self) -> usize {
        self.plans.iter().map(|d| 1 + d.plan.actions.len()).sum()
    }

    /// Fold a worker message into the state, advancing the phase as needed.
    pub fn on_message(&mut self, msg: Message) {
        match msg {
            Message::Scanned { root, count } => {
                self.status = format!("Scanned {} ({count} entries)…", root.display());
            }
            Message::Planned { destination, plan } => {
                self.plans.push(DestPlan { destination, plan });
                if self.plans.len() >= self.expected_plans() {
                    self.finish_scanning();
                }
            }
            Message::Applied {
                destination,
                done,
                total,
            } => {
                self.apply_progress.insert(destination, (done, total));
            }
            Message::Done { stats } => {
                self.stats = Some(stats);
                self.phase = Phase::Done;
                self.status = if self.opts.dry_run {
                    "Dry run complete — nothing was written.".into()
                } else {
                    "Sync complete.".into()
                };
            }
            Message::Error { message } => {
                self.status = message.clone();
                self.error = Some(message);
                self.phase = Phase::Error;
            }
        }
    }

    /// Called once every destination has a plan. Decides whether to stop (nothing
    /// to do), wait for confirmation (Review), or apply immediately (`--yes`).
    fn finish_scanning(&mut self) {
        if self.nothing_to_do() {
            self.stats = Some(Stats::default());
            self.phase = Phase::Done;
            self.status = "Already up to date — nothing to sync.".into();
        } else if self.opts.yes {
            self.phase = Phase::Applying;
            self.status = "Applying…".into();
        } else {
            self.phase = Phase::Review;
            self.status = self.review_hint();
        }
    }

    fn review_hint(&self) -> String {
        let verb = if self.opts.dry_run {
            "preview"
        } else {
            "apply"
        };
        format!(
            "{} change(s) across {} destination(s).  [Enter/y] {verb}   [q] cancel",
            self.total_actions(),
            self.expected_plans()
        )
    }

    /// Confirm the Review screen → start applying. No-op outside Review.
    pub fn confirm(&mut self) {
        if self.phase == Phase::Review {
            self.phase = Phase::Applying;
            self.status = if self.opts.dry_run {
                "Previewing…".into()
            } else {
                "Applying…".into()
            };
        }
    }

    pub fn scroll_down(&mut self) {
        let max = self.review_line_count().saturating_sub(1);
        self.review_scroll = (self.review_scroll + 1).min(max);
    }

    pub fn scroll_up(&mut self) {
        self.review_scroll = self.review_scroll.saturating_sub(1);
    }

    pub fn page_down(&mut self) {
        let max = self.review_line_count().saturating_sub(1);
        self.review_scroll = (self.review_scroll + 10).min(max);
    }

    pub fn page_up(&mut self) {
        self.review_scroll = self.review_scroll.saturating_sub(10);
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SyncAction;
    use std::path::PathBuf;

    /// A config mirroring `/src` into `n` throwaway destinations.
    fn config(n: usize) -> Config {
        Config {
            source: PathBuf::from("/src"),
            destinations: (0..n).map(|i| PathBuf::from(format!("/d{i}"))).collect(),
        }
    }

    /// A plan with a single Copy action (so it is non-empty).
    fn nonempty_plan() -> Plan {
        let actions = vec![SyncAction::Copy {
            rel_path: PathBuf::from("a.txt"),
            size: 5,
        }];
        let stats = Stats {
            copies: 1,
            bytes: 5,
            ..Default::default()
        };
        Plan { actions, stats }
    }

    fn plan_for(app: &mut App, dest: &str, plan: Plan) {
        app.on_message(Message::Planned {
            destination: PathBuf::from(dest),
            plan,
        });
    }

    #[test]
    fn all_destinations_planned_moves_to_review() {
        let mut app = App::new(config(2), RunOptions::default());
        plan_for(&mut app, "/d0", nonempty_plan());
        assert_eq!(app.phase, Phase::Scanning, "still waiting on /d1");
        plan_for(&mut app, "/d1", nonempty_plan());
        assert_eq!(app.phase, Phase::Review);
        assert_eq!(app.total_actions(), 2);
    }

    #[test]
    fn yes_skips_review_and_goes_straight_to_applying() {
        let opts = RunOptions {
            yes: true,
            ..Default::default()
        };
        let mut app = App::new(config(1), opts);
        plan_for(&mut app, "/d0", nonempty_plan());
        assert_eq!(app.phase, Phase::Applying);
    }

    #[test]
    fn empty_plans_finish_as_done_with_nothing_to_do() {
        let mut app = App::new(config(1), RunOptions::default());
        plan_for(&mut app, "/d0", Plan::default());
        assert_eq!(app.phase, Phase::Done);
        assert!(app.nothing_to_do());
    }

    #[test]
    fn confirm_moves_review_to_applying() {
        let mut app = App::new(config(1), RunOptions::default());
        plan_for(&mut app, "/d0", nonempty_plan());
        assert_eq!(app.phase, Phase::Review);
        app.confirm();
        assert_eq!(app.phase, Phase::Applying);
    }

    #[test]
    fn done_message_records_stats() {
        let mut app = App::new(config(1), RunOptions::default());
        plan_for(&mut app, "/d0", nonempty_plan());
        app.confirm();
        app.on_message(Message::Done {
            stats: Stats {
                copies: 1,
                bytes: 5,
                ..Default::default()
            },
        });
        assert_eq!(app.phase, Phase::Done);
        assert_eq!(app.stats.unwrap().copies, 1);
    }

    #[test]
    fn worker_error_shows_error_phase() {
        let mut app = App::new(config(1), RunOptions::default());
        app.on_message(Message::Error {
            message: "boom".into(),
        });
        assert_eq!(app.phase, Phase::Error);
        assert_eq!(app.error.as_deref(), Some("boom"));
    }

    #[test]
    fn apply_progress_sums_across_destinations() {
        let mut app = App::new(config(2), RunOptions::default());
        plan_for(&mut app, "/d0", nonempty_plan());
        plan_for(&mut app, "/d1", nonempty_plan());
        app.on_message(Message::Applied {
            destination: PathBuf::from("/d0"),
            done: 1,
            total: 1,
        });
        app.on_message(Message::Applied {
            destination: PathBuf::from("/d1"),
            done: 1,
            total: 1,
        });
        assert_eq!(app.applied_actions(), 2);
        assert_eq!(app.total_actions(), 2);
    }
}

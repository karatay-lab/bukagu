//! Messages streamed from the background sync worker to the UI loop.
//!
//! The worker runs scan → diff → apply (per destination) off the UI thread and
//! sends these over a channel so the dashboard stays responsive (Step 5).

use std::path::PathBuf;

use crate::core::{Plan, Stats};

#[derive(Debug)]
pub enum Message {
    /// Scanning progress: entries discovered so far under `root`.
    Scanned { root: PathBuf, count: usize },
    /// A destination's plan is ready for review.
    Planned { destination: PathBuf, plan: Plan },
    /// Apply progress for `destination`: `done` of `total` actions complete.
    Applied {
        destination: PathBuf,
        done: usize,
        total: usize,
    },
    /// All destinations finished; carries the rolled-up totals.
    Done { stats: Stats },
    /// A fatal error from the worker; the UI shows the Error screen.
    Error { message: String },
}

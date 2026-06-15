//! Top-level TUI rendering, built on the bukagu [`crate::theme`].
//!
//! `browser` is the first-run directory picker (Step 4); `dashboard` is the
//! returning-run sync screen (Step 5); `widgets` holds the small reusable pieces
//! they share.

pub mod backup;
pub mod browser;
pub mod dashboard;
pub mod mappings;
pub mod widgets;

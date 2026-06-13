//! Execute a [`Plan`] against one destination.
//!
//! GUARDRAIL: writes happen ONLY inside `destination_root`. Before doing anything
//! we assert the destination neither equals nor contains nor is contained by the
//! source, so bukagu can never modify or delete a file in the read-only source.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Plan, SyncAction};

/// Apply `plan` to `destination_root`, copying files from `source_root`,
/// invoking `progress(done, total)` after each action so the dashboard can
/// advance its gauge. When `dry_run` is true, the guardrail is still validated
/// but nothing is written.
pub fn apply_with_progress<F>(
    source_root: &Path,
    destination_root: &Path,
    plan: &Plan,
    dry_run: bool,
    mut progress: F,
) -> Result<()>
where
    F: FnMut(usize, usize),
{
    let (src, dst) = guard_destination(source_root, destination_root)?;

    let total = plan.actions.len();
    for (i, action) in plan.actions.iter().enumerate() {
        let target = dst.join(action.rel_path());
        // Defense in depth: a relative path must never escape the destination root.
        assert!(
            target.starts_with(&dst),
            "action escaped destination: {}",
            target.display()
        );

        if !dry_run {
            run(action, &src, &target)?;
        }
        progress(i + 1, total);
    }

    Ok(())
}

/// Perform a single action's filesystem write inside the destination.
fn run(action: &SyncAction, src: &Path, target: &Path) -> Result<()> {
    match action {
        SyncAction::CreateDir { .. } => {
            fs::create_dir_all(target).with_context(|| format!("creating dir {}", target.display()))
        }
        SyncAction::Copy { rel_path, .. } | SyncAction::Overwrite { rel_path, .. } => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent {}", parent.display()))?;
            }
            let from = src.join(rel_path);
            fs::copy(&from, target)
                .with_context(|| format!("copying {} -> {}", from.display(), target.display()))?;
            Ok(())
        }
        SyncAction::Delete { is_dir, .. } => {
            if !target.exists() {
                return Ok(());
            }
            if *is_dir {
                fs::remove_dir_all(target)
                    .with_context(|| format!("removing dir {}", target.display()))
            } else {
                fs::remove_file(target)
                    .with_context(|| format!("removing file {}", target.display()))
            }
        }
    }
}

/// GUARDRAIL — refuse any layout where writing into `destination_root` could
/// reach the read-only `source_root`. Canonicalizes both paths (resolving `.`,
/// `..`, and symlinks) and rejects a destination that equals, sits inside, or
/// contains the source. Returns the canonical `(source, destination)` on success.
///
/// Called before every apply, and again by the dashboard worker before it even
/// scans a destination — so a bad mapping fails fast and loud, long before any
/// write. This is the enforced backstop for "bukagu never modifies the source".
pub fn guard_destination(
    source_root: &Path,
    destination_root: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let src = canonical(source_root)?;
    let dst = canonical(destination_root)?;

    if dst == src {
        bail!(
            "refusing to sync: destination equals the source ({})",
            src.display()
        );
    }
    if dst.starts_with(&src) {
        bail!(
            "refusing to sync: destination {} is inside the source {}",
            dst.display(),
            src.display()
        );
    }
    if src.starts_with(&dst) {
        bail!(
            "refusing to sync: source {} is inside the destination {}",
            src.display(),
            dst.display()
        );
    }

    Ok((src, dst))
}

/// Resolve a path to its canonical form so the guardrail comparison is robust to
/// `.`, `..`, and symlinks. Both source and destination are expected to exist.
fn canonical(p: &Path) -> Result<PathBuf> {
    p.canonicalize()
        .with_context(|| format!("resolving path {} (does it exist?)", p.display()))
}

//! Compute a [`Plan`] from the source scan versus one destination scan.
//!
//! Run once per destination. Comparison rule:
//! - in source, not in destination → `Copy` (file) / `CreateDir` (dir)
//! - in both, sizes differ → `Overwrite` (no hashing needed)
//! - in both, sizes equal → hash both, `Overwrite` only if hashes differ
//! - in destination, not in source → `Delete` only when `delete_extras` is set
//!
//! Where a path is a file on one side and a directory on the other, the
//! destination entry is removed first, then created/copied to match the source.
//!
//! Actions are returned in apply-safe order: deletions (deepest first), then
//! directory creations (shallowest first), then file copies/overwrites.

use std::path::Path;

use anyhow::Result;

use super::hash::hash_file;
use super::{Plan, Scan, Stats, SyncAction};

/// Diff `source` against one `destination`, producing an apply-ordered [`Plan`].
///
/// The root paths are needed to open files for hashing; `delete_extras` controls
/// whether extras-in-destination become `Delete` actions.
pub fn diff(
    source_root: &Path,
    source: &Scan,
    destination_root: &Path,
    destination: &Scan,
    delete_extras: bool,
) -> Result<Plan> {
    let mut actions: Vec<SyncAction> = Vec::new();

    // Source drives copies, directory creations, and overwrites.
    for (rel, src) in source {
        let dst = destination.get(rel);

        if src.is_dir {
            match dst {
                Some(d) if d.is_dir => {} // already a directory — nothing to do
                Some(_) => {
                    // A file sits where the source has a directory: remove it, then mkdir.
                    actions.push(SyncAction::Delete {
                        rel_path: rel.clone(),
                        is_dir: false,
                    });
                    actions.push(SyncAction::CreateDir {
                        rel_path: rel.clone(),
                    });
                }
                None => actions.push(SyncAction::CreateDir {
                    rel_path: rel.clone(),
                }),
            }
            continue;
        }

        // `src` is a file.
        match dst {
            None => actions.push(SyncAction::Copy {
                rel_path: rel.clone(),
                size: src.size,
            }),
            Some(d) if d.is_dir => {
                // A directory sits where the source has a file: remove it, then copy.
                actions.push(SyncAction::Delete {
                    rel_path: rel.clone(),
                    is_dir: true,
                });
                actions.push(SyncAction::Overwrite {
                    rel_path: rel.clone(),
                    size: src.size,
                });
            }
            Some(d) => {
                let differs = if src.size != d.size {
                    true
                } else {
                    // Same size — settle it with a content hash of both files.
                    hash_file(&source_root.join(rel))? != hash_file(&destination_root.join(rel))?
                };
                if differs {
                    actions.push(SyncAction::Overwrite {
                        rel_path: rel.clone(),
                        size: src.size,
                    });
                }
            }
        }
    }

    // Extras present in the destination but not the source.
    if delete_extras {
        for (rel, d) in destination {
            if !source.contains_key(rel) {
                actions.push(SyncAction::Delete {
                    rel_path: rel.clone(),
                    is_dir: d.is_dir,
                });
            }
        }
    }

    sort_actions(&mut actions);
    let stats = stats_of(&actions);
    Ok(Plan { actions, stats })
}

/// Apply-safe phase for an action: deletes, then dir creation, then file writes.
fn phase(a: &SyncAction) -> u8 {
    match a {
        SyncAction::Delete { .. } => 0,
        SyncAction::CreateDir { .. } => 1,
        SyncAction::Copy { .. } | SyncAction::Overwrite { .. } => 2,
    }
}

/// Order actions so each is safe to apply in sequence: deletions run deepest
/// first (children before parents), creations and copies run shallowest first
/// (parents before children). Ties break on path for deterministic output.
fn sort_actions(actions: &mut [SyncAction]) {
    actions.sort_by(|a, b| {
        let (pa, pb) = (phase(a), phase(b));
        if pa != pb {
            return pa.cmp(&pb);
        }
        let (da, db) = (
            a.rel_path().components().count(),
            b.rel_path().components().count(),
        );
        let by_depth = if pa == 0 { db.cmp(&da) } else { da.cmp(&db) };
        by_depth.then_with(|| a.rel_path().cmp(b.rel_path()))
    });
}

/// Roll up action counts and total bytes to be written.
fn stats_of(actions: &[SyncAction]) -> Stats {
    let mut s = Stats::default();
    for a in actions {
        match a {
            SyncAction::CreateDir { .. } => s.create_dirs += 1,
            SyncAction::Copy { size, .. } => {
                s.copies += 1;
                s.bytes += size;
            }
            SyncAction::Overwrite { size, .. } => {
                s.overwrites += 1;
                s.bytes += size;
            }
            SyncAction::Delete { .. } => s.deletes += 1,
        }
    }
    s
}

//! The UI-independent sync engine: scan → hash → diff → apply.
//!
//! Everything here is pure logic over the filesystem and is unit-testable without
//! a terminal. The TUI (Steps 4–5) drives it from a background worker, but a plain
//! CLI or a future watch mode could reuse the exact same functions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub mod apply;
pub mod banner;
pub mod diff;
pub mod hash;
pub mod mapping;
pub mod scan;

/// One entry discovered while scanning a folder tree. Within a [`Scan`] it is
/// keyed by `rel_path`, the path relative to the scan root, so source and
/// destination entries line up by location rather than absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the scan root (the map key mirrors this).
    pub rel_path: PathBuf,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Last modification time, as reported by the filesystem.
    pub mtime: SystemTime,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

/// A scanned folder tree, keyed by path relative to the scan root.
pub type Scan = HashMap<PathBuf, FileEntry>;

/// A single change to bring one destination in line with the source.
///
/// Every variant carries the destination-relative path it acts on; nothing here
/// ever references the source as a write target — the source is read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Create a directory that exists in the source but not the destination.
    CreateDir { rel_path: PathBuf },
    /// Copy a file present in the source but missing from the destination.
    Copy { rel_path: PathBuf, size: u64 },
    /// Replace a destination file whose contents differ from the source.
    Overwrite { rel_path: PathBuf, size: u64 },
    /// Remove a destination entry that no longer exists in the source
    /// (only produced when deletion is enabled).
    Delete { rel_path: PathBuf, is_dir: bool },
}

impl SyncAction {
    /// The destination-relative path this action operates on.
    pub fn rel_path(&self) -> &Path {
        match self {
            SyncAction::CreateDir { rel_path }
            | SyncAction::Copy { rel_path, .. }
            | SyncAction::Overwrite { rel_path, .. }
            | SyncAction::Delete { rel_path, .. } => rel_path,
        }
    }

    /// A short, stable label for display (e.g. in the Review list).
    pub fn label(&self) -> &'static str {
        match self {
            SyncAction::CreateDir { .. } => "mkdir",
            SyncAction::Copy { .. } => "copy",
            SyncAction::Overwrite { .. } => "update",
            SyncAction::Delete { .. } => "delete",
        }
    }
}

/// Aggregate counts for a [`Plan`], shown in the summary screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub create_dirs: usize,
    pub copies: usize,
    pub overwrites: usize,
    pub deletes: usize,
    /// Total bytes to be written (copies + overwrites).
    pub bytes: u64,
}

/// The full set of actions to mirror the source into one destination, plus a
/// rolled-up summary. Actions are ordered so directories precede their files.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub actions: Vec<SyncAction>,
    pub stats: Stats,
}

impl Plan {
    /// True when the destination already matches the source (nothing to do).
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Write `content` to `root/rel`, creating parent directories as needed.
    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    /// Plan a sync from `src` into `dst`.
    fn plan(src: &Path, dst: &Path, delete_extras: bool) -> Plan {
        let s = scan::scan(src).unwrap();
        let d = scan::scan(dst).unwrap();
        diff::diff(src, &s, dst, &d, delete_extras).unwrap()
    }

    #[test]
    fn new_file_is_copied() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.txt", "hello");

        let p = plan(src.path(), dst.path(), false);
        assert_eq!(p.stats.copies, 1);
        assert!(matches!(p.actions[0], SyncAction::Copy { .. }));
    }

    #[test]
    fn changed_file_is_overwritten() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.txt", "new longer content");
        write(dst.path(), "a.txt", "old");

        let p = plan(src.path(), dst.path(), false);
        assert_eq!(p.stats.overwrites, 1);
        assert!(matches!(p.actions[0], SyncAction::Overwrite { .. }));
    }

    #[test]
    fn identical_file_is_noop() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.txt", "same");
        write(dst.path(), "a.txt", "same");

        assert!(plan(src.path(), dst.path(), false).is_empty());
    }

    #[test]
    fn same_size_different_content_overwrites() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.txt", "aaaaa"); // 5 bytes
        write(dst.path(), "a.txt", "bbbbb"); // 5 bytes, same size

        let p = plan(src.path(), dst.path(), false);
        assert_eq!(
            p.stats.overwrites, 1,
            "size matched, hash must catch the diff"
        );
    }

    #[test]
    fn extra_in_dest_deleted_only_with_flag() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "keep.txt", "x");
        write(dst.path(), "keep.txt", "x");
        write(dst.path(), "extra.txt", "remove me");

        // Without the flag, the extra is left untouched.
        assert!(plan(src.path(), dst.path(), false).is_empty());

        // With the flag, it becomes a Delete.
        let p = plan(src.path(), dst.path(), true);
        assert_eq!(p.stats.deletes, 1);
        assert!(matches!(p.actions[0], SyncAction::Delete { .. }));
    }

    #[test]
    fn apply_makes_dest_match_source() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "top.txt", "top");
        write(src.path(), "nested/deep/file.txt", "deep content");

        let p = plan(src.path(), dst.path(), false);
        apply::apply_with_progress(src.path(), dst.path(), &p, false, |_, _| {}).unwrap();

        assert_eq!(
            fs::read_to_string(dst.path().join("top.txt")).unwrap(),
            "top"
        );
        assert_eq!(
            fs::read_to_string(dst.path().join("nested/deep/file.txt")).unwrap(),
            "deep content"
        );

        // Idempotent: a second plan finds nothing to do.
        assert!(plan(src.path(), dst.path(), false).is_empty());
    }

    #[test]
    fn apply_refuses_when_dest_inside_source() {
        let src = tempdir().unwrap();
        let inside = src.path().join("sub");
        fs::create_dir_all(&inside).unwrap();

        let err =
            apply::apply_with_progress(src.path(), &inside, &Plan::default(), false, |_, _| {})
                .unwrap_err();
        assert!(err.to_string().contains("refusing to sync"));
    }
}

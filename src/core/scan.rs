//! Walk a folder tree into a [`Scan`], keyed by path relative to the root.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::{FileEntry, Scan};

/// Recursively scan `root`, returning every entry keyed by its path relative to
/// `root`. The root itself is not included.
///
/// **Symlinks are skipped entirely** (v1 decision): a symlink — to a file or a
/// directory — is neither recorded, copied, hashed, nor deleted, and symlinked
/// directories are not descended into. So a mirror never follows a link out of
/// the source tree and never recreates one in a destination. This matches the
/// onboarding browser, which also lists only real subdirectories.
pub fn scan(root: &Path) -> Result<Scan> {
    let mut map = Scan::new();

    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;

        // Skip symlinks (and don't descend symlinked dirs) — see the doc comment.
        if entry.path_is_symlink() {
            continue;
        }

        let path = entry.path();

        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("relativizing {}", path.display()))?
            .to_path_buf();

        let meta = entry
            .metadata()
            .with_context(|| format!("reading metadata for {}", path.display()))?;

        let is_dir = meta.is_dir();
        let entry = FileEntry {
            rel_path: rel.clone(),
            size: if is_dir { 0 } else { meta.len() },
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            is_dir,
        };
        map.insert(rel, entry);
    }

    Ok(map)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn symlinks_are_skipped() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        std::fs::write(root.join("real.txt"), b"hi").unwrap();
        std::fs::create_dir(root.join("realdir")).unwrap();
        std::fs::write(root.join("realdir/inner.txt"), b"deep").unwrap();
        // A symlinked file and a symlinked directory — both must be ignored.
        symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
        symlink(root.join("realdir"), root.join("linkdir")).unwrap();

        let scan = scan(root).unwrap();

        assert!(scan.contains_key(Path::new("real.txt")));
        assert!(scan.contains_key(Path::new("realdir")));
        assert!(scan.contains_key(Path::new("realdir/inner.txt")));
        assert!(!scan.contains_key(Path::new("link.txt")), "symlinked file");
        assert!(!scan.contains_key(Path::new("linkdir")), "symlinked dir");
        // The symlinked dir must not be descended into either.
        assert!(!scan.contains_key(Path::new("linkdir/inner.txt")));
    }
}

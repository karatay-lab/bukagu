//! v3 encrypted source backups.
//!
//! A backup `tar`s the read-only source, gzip-compresses it, and `age`-encrypts
//! the stream to a **public recipient key** fetched from the user's web API,
//! writing one timestamped archive under `~/bukagu-backups/<project>/`. Because
//! the recipient is a public key, the machine running bukagu can encrypt but can
//! **never decrypt** its own backups — only the private identity (kept by the
//! user on their website) can. See `~/.claude/plans/ancient-gliding-thunder.md`.
//!
//! [`crypto`] (age) and [`archive`] (tar+gzip) are pure, offline primitives;
//! [`key`] supplies the recipient; this module orchestrates them: guard → fetch
//! key → stream-encrypt to a temp file → atomic rename → prune old archives.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

use crate::core::mapping::resolved_abs;
use crate::store::BackupSettings;

pub mod archive;
pub mod crypto;
pub mod key;
pub mod restore;

/// Filename suffix for an encrypted backup archive (`<timestamp>.tar.gz.age`).
/// The in-progress temp file is `.<timestamp>.tar.gz.age.partial`. Defined once
/// so the writer, the retention scan, and `restore` all agree on the format.
const ARCHIVE_EXT: &str = ".tar.gz.age";

/// Progress milestones emitted during a backup, for the CLI line / TUI gauge.
#[derive(Debug, Clone)]
pub enum BackupEvent {
    /// About to request the recipient key (network on the live path).
    FetchingKey,
    /// Started streaming the source into the encrypted archive.
    Archiving,
    /// `bytes` of encrypted output written so far (reported periodically).
    Progress { bytes: u64 },
    /// Applying the retention policy (deleting older archives).
    Pruning,
}

/// Outcome of a backup run, for the summary line and to stamp the store.
#[derive(Debug, Clone)]
pub struct BackupReport {
    /// The archive that was (or, on a dry run, would be) written.
    pub archive_path: PathBuf,
    /// Regular files included.
    pub files: usize,
    /// Encrypted archive size in bytes (dry run: estimated source size).
    pub bytes: u64,
    /// How many older archives the retention policy pruned.
    pub pruned: usize,
    /// True if this was a `--dry-run` (nothing written).
    pub dry_run: bool,
}

/// `~/bukagu-backups` — the default root holding every project's backups.
pub fn default_backup_root() -> Result<PathBuf> {
    let home =
        dirs::home_dir().context("could not determine your home directory (is $HOME set?)")?;
    Ok(home.join("bukagu-backups"))
}

/// The per-project backup folder: `{settings.root or ~/bukagu-backups}/<project>`,
/// where `<project>` is a filesystem-safe form of the project root's folder name.
///
/// This keys backups by the launch folder's *name* only, so two different repos
/// that happen to share a leaf name (e.g. `~/a/app` and `~/b/app`) map to the same
/// `<root>/app` and would share a retention pool. Point `settings.root` at distinct
/// folders if you back up same-named projects.
pub fn resolve_backup_dir(settings: &BackupSettings, project_root: &Path) -> Result<PathBuf> {
    let root = match &settings.root {
        Some(r) => r.clone(),
        None => default_backup_root()?,
    };
    Ok(root.join(project_name(project_root)))
}

/// A filesystem-safe folder name derived from a project root's directory name.
fn project_name(project_root: &Path) -> String {
    let raw = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.'); // never "", ".", or ".."
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Back up `source` into `backup_dir`, encrypting to the recipient `provider`
/// supplies. Writes one `<timestamp>.tar.gz.age`, then prunes to `retention`
/// newest archives (`0` keeps all). On `dry_run`, validates and reports but
/// writes nothing. `progress` receives [`BackupEvent`]s as the run proceeds.
pub fn run_backup(
    source: &Path,
    backup_dir: &Path,
    provider: &dyn key::KeyProvider,
    retention: usize,
    dry_run: bool,
    progress: &mut dyn FnMut(BackupEvent),
) -> Result<BackupReport> {
    // GUARDRAIL — before creating any directory, prove the backup target can't
    // touch the read-only source. Never write into the source.
    guard_backup_dir(source, backup_dir)?;

    progress(BackupEvent::FetchingKey);
    let recipient = provider
        .recipient()
        .context("fetching the backup encryption key")?;

    let stamp = crate::store::now_compact();
    let final_path = backup_dir.join(format!("{stamp}{ARCHIVE_EXT}"));

    if dry_run {
        let (files, bytes) = source_stats(source)?;
        return Ok(BackupReport {
            archive_path: final_path,
            files,
            bytes,
            pruned: 0,
            dry_run: true,
        });
    }

    fs::create_dir_all(backup_dir)
        .with_context(|| format!("creating backup folder {}", backup_dir.display()))?;

    // Write to a hidden temp file, then atomically rename — a crash never leaves
    // a half-written archive under a real name.
    let temp_path = backup_dir.join(format!(".{stamp}{ARCHIVE_EXT}.partial"));
    progress(BackupEvent::Archiving);
    let files = match write_encrypted_archive(source, &temp_path, &recipient, progress) {
        Ok(files) => files,
        Err(e) => {
            let _ = fs::remove_file(&temp_path); // best-effort cleanup
            return Err(e);
        }
    };
    fs::rename(&temp_path, &final_path)
        .with_context(|| format!("finalizing {}", final_path.display()))?;
    let bytes = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);

    progress(BackupEvent::Pruning);
    let pruned = prune(backup_dir, retention)?;

    Ok(BackupReport {
        archive_path: final_path,
        files,
        bytes,
        pruned,
        dry_run: false,
    })
}

/// Stream `source` → tar → gzip → age(`recipient`) → `temp_path`, fsync, and
/// return the number of files archived.
fn write_encrypted_archive(
    source: &Path,
    temp_path: &Path,
    recipient: &age::x25519::Recipient,
    progress: &mut dyn FnMut(BackupEvent),
) -> Result<usize> {
    let file =
        File::create(temp_path).with_context(|| format!("creating {}", temp_path.display()))?;
    let counter = CountingWriter::new(file, progress);
    let encryptor = crypto::encrypt_to(recipient, counter)?;
    let (files, encryptor) = archive::pack(source, encryptor)?;
    // Flush the final age chunk (mandatory) and recover the file handle.
    let counter = encryptor.finish().context("finishing encryption")?;
    let file = counter.into_inner();
    file.sync_all().context("flushing the backup to disk")?;
    Ok(files)
}

/// GUARDRAIL — refuse any layout where the backup folder could reach the
/// read-only source. Uses [`resolved_abs`] so it holds even though `backup_dir`
/// may not exist yet (we check *before* creating it).
fn guard_backup_dir(source: &Path, backup_dir: &Path) -> Result<()> {
    let src = resolved_abs(source);
    let dst = resolved_abs(backup_dir);
    if dst == src {
        bail!(
            "refusing to back up: the backup folder equals the source ({})",
            src.display()
        );
    }
    if dst.starts_with(&src) {
        bail!(
            "refusing to back up: the backup folder {} is inside the source {}",
            dst.display(),
            src.display()
        );
    }
    if src.starts_with(&dst) {
        bail!(
            "refusing to back up: the source {} is inside the backup folder {}",
            src.display(),
            dst.display()
        );
    }
    Ok(())
}

/// Keep the `retention` newest `*.tar.gz.age` archives in `backup_dir`, deleting
/// older ones. `retention == 0` keeps everything. Returns how many were pruned.
fn prune(backup_dir: &Path, retention: usize) -> Result<usize> {
    if retention == 0 {
        return Ok(0);
    }
    let mut archives: Vec<PathBuf> = fs::read_dir(backup_dir)
        .with_context(|| format!("listing {}", backup_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_archive(p))
        .collect();
    if archives.len() <= retention {
        return Ok(0);
    }
    archives.sort(); // timestamped names sort oldest-first
    let to_remove = archives.len() - retention;
    let mut pruned = 0;
    for path in archives.into_iter().take(to_remove) {
        fs::remove_file(&path).with_context(|| format!("pruning old backup {}", path.display()))?;
        pruned += 1;
    }
    Ok(pruned)
}

/// Whether `p` is one of our archive files (and not a temp `.partial`).
fn is_archive(p: &Path) -> bool {
    p.is_file()
        && p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.') && n.ends_with(ARCHIVE_EXT))
}

/// Count regular files and total bytes under `source` (symlinks skipped), for the
/// dry-run preview.
fn source_stats(source: &Path) -> Result<(usize, u64)> {
    let mut files = 0;
    let mut bytes = 0u64;
    for entry in WalkDir::new(source).min_depth(1).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", source.display()))?;
        if entry.path_is_symlink() {
            continue;
        }
        let meta = entry
            .metadata()
            .with_context(|| format!("reading metadata for {}", entry.path().display()))?;
        if meta.is_file() {
            files += 1;
            bytes += meta.len();
        }
    }
    Ok((files, bytes))
}

/// Report progress every this many bytes of encrypted output.
const PROGRESS_STEP: u64 = 256 * 1024;

/// A `Write` wrapper that counts bytes and emits [`BackupEvent::Progress`]
/// roughly every [`PROGRESS_STEP`] bytes.
struct CountingWriter<'a, W> {
    inner: W,
    written: u64,
    reported: u64,
    progress: &'a mut dyn FnMut(BackupEvent),
}

impl<'a, W: Write> CountingWriter<'a, W> {
    fn new(inner: W, progress: &'a mut dyn FnMut(BackupEvent)) -> Self {
        Self {
            inner,
            written: 0,
            reported: 0,
            progress,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        if self.written - self.reported >= PROGRESS_STEP {
            self.reported = self.written;
            (self.progress)(BackupEvent::Progress {
                bytes: self.written,
            });
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::key::StaticKeyProvider;
    use age::x25519::Identity;
    use std::fs;
    use tempfile::tempdir;

    fn noop(_: BackupEvent) {}

    /// End-to-end: a backup written with the public recipient must decrypt and
    /// unpack back to the exact source tree using the matching private identity.
    #[test]
    fn backup_then_restore_roundtrips() {
        let src = tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        fs::create_dir_all(src.path().join("nested")).unwrap();
        fs::write(src.path().join("nested/b.bin"), b"\x00\x01\x02beta").unwrap();

        let backups = tempdir().unwrap();
        let id = Identity::generate();
        let provider = StaticKeyProvider::new(id.to_public());

        let report =
            run_backup(src.path(), backups.path(), &provider, 10, false, &mut noop).unwrap();
        assert_eq!(report.files, 2);
        assert!(report.archive_path.exists());
        assert!(
            report
                .archive_path
                .to_string_lossy()
                .ends_with(".tar.gz.age")
        );

        // Restore: decrypt with the private identity, then unpack.
        let out = tempdir().unwrap();
        let file = fs::File::open(&report.archive_path).unwrap();
        let reader = crypto::decrypt_with(&id, file).unwrap();
        archive::unpack(reader, out.path()).unwrap();

        assert_eq!(fs::read(out.path().join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            fs::read(out.path().join("nested/b.bin")).unwrap(),
            b"\x00\x01\x02beta"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let src = tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        let backups = tempdir().unwrap();

        let id = Identity::generate();
        let report = run_backup(
            src.path(),
            backups.path(),
            &StaticKeyProvider::new(id.to_public()),
            10,
            true,
            &mut noop,
        )
        .unwrap();

        assert!(report.dry_run);
        assert_eq!(report.files, 1);
        assert_eq!(
            fs::read_dir(backups.path()).unwrap().count(),
            0,
            "dry run writes no archive"
        );
    }

    #[test]
    fn refuses_backup_dir_inside_source() {
        let src = tempdir().unwrap();
        let inside = src.path().join("backups");
        let id = Identity::generate();
        let err = run_backup(
            src.path(),
            &inside,
            &StaticKeyProvider::new(id.to_public()),
            10,
            false,
            &mut noop,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to back up"));
        // And nothing was created inside the source.
        assert!(
            !inside.exists(),
            "guard runs before any directory is created"
        );
    }

    #[test]
    fn retention_prunes_oldest_archives() {
        let backups = tempdir().unwrap();
        // Names sort chronologically; create five, keep two.
        for stamp in ["20260101T000000Z", "20260102T000000Z", "20260103T000000Z"] {
            fs::write(backups.path().join(format!("{stamp}.tar.gz.age")), b"x").unwrap();
        }
        // A non-archive file and a temp partial must be left alone.
        fs::write(backups.path().join("notes.txt"), b"keep").unwrap();
        fs::write(
            backups.path().join(".20260104T000000Z.tar.gz.age.partial"),
            b"tmp",
        )
        .unwrap();

        let pruned = prune(backups.path(), 2).unwrap();
        assert_eq!(pruned, 1, "one oldest archive pruned");
        assert!(!backups.path().join("20260101T000000Z.tar.gz.age").exists());
        assert!(backups.path().join("20260103T000000Z.tar.gz.age").exists());
        assert!(
            backups.path().join("notes.txt").exists(),
            "non-archive untouched"
        );
        assert!(
            backups
                .path()
                .join(".20260104T000000Z.tar.gz.age.partial")
                .exists(),
            "temp partial untouched"
        );
    }

    #[test]
    fn project_name_is_sanitized() {
        assert_eq!(project_name(Path::new("/home/me/my-repo")), "my-repo");
        assert_eq!(project_name(Path::new("/a/weird name!/")), "weird_name_");
        assert_eq!(project_name(Path::new("/")), "project");
    }
}

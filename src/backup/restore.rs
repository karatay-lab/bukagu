//! Restore a backup: decrypt an archive with the private age identity, then
//! unpack it. CLI-only in v3 (`bukagu restore`).
//!
//! The identity — the secret only the user holds, obtained from their website —
//! is what makes restore possible; the machine that *made* the backup cannot
//! decrypt it. Restore never writes into the read-only source.

use std::fs;
use std::path::{Path, PathBuf};

use age::x25519::Identity;
use anyhow::{Context, Result, anyhow, bail};

use crate::core::mapping::resolved_abs;

/// Parse an age identity from a `--identity` value: either the literal
/// `AGE-SECRET-KEY-1…`, or `@<path>` to read it from an age key file (its first
/// `AGE-SECRET-KEY-` line is used; `#` comment lines are ignored).
pub fn parse_identity(arg: &str) -> Result<Identity> {
    let text = if let Some(path) = arg.strip_prefix('@') {
        fs::read_to_string(path).with_context(|| format!("reading identity file {path}"))?
    } else {
        arg.to_string()
    };
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.to_ascii_uppercase().starts_with("AGE-SECRET-KEY-"))
        .or_else(|| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
        })
        .unwrap_or("");
    line.parse::<Identity>()
        .map_err(|e| anyhow!("not a valid age identity (expected AGE-SECRET-KEY-1…): {e}"))
}

/// The newest `*.tar.gz.age` archive in `backup_dir`, or `None` if there are none.
pub fn newest_archive(backup_dir: &Path) -> Result<Option<PathBuf>> {
    let mut archives: Vec<PathBuf> = fs::read_dir(backup_dir)
        .with_context(|| format!("listing {}", backup_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| super::is_archive(p))
        .collect();
    archives.sort(); // timestamped names sort oldest → newest
    Ok(archives.pop())
}

/// Decrypt and unpack `archive_path` into `target` using the private `identity`.
///
/// GUARDRAIL: refuses any `target` that equals, sits inside, or contains the
/// read-only `source` (when known) — even with `force`. Without `force`, also
/// refuses an existing non-empty `target` rather than mixing into it.
pub fn restore(
    identity: &Identity,
    archive_path: &Path,
    target: &Path,
    source: Option<&Path>,
    force: bool,
) -> Result<()> {
    if let Some(source) = source {
        let src = resolved_abs(source);
        let dst = resolved_abs(target);
        if dst == src || dst.starts_with(&src) || src.starts_with(&dst) {
            bail!(
                "refusing to restore into {} — it overlaps the read-only source {}",
                target.display(),
                source.display()
            );
        }
    }

    if !force && dir_has_entries(target) {
        bail!(
            "{} already exists and is not empty — pass --force to restore into it",
            target.display()
        );
    }

    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;

    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening {}", archive_path.display()))?;
    let reader = super::crypto::decrypt_with(identity, file)?;
    super::archive::unpack(reader, target)?;
    Ok(())
}

/// Whether `dir` exists and contains at least one entry.
fn dir_has_entries(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::key::StaticKeyProvider;
    use crate::backup::run_backup;
    use age::secrecy::ExposeSecret;
    use tempfile::tempdir;

    /// Make a backup of `src` into `backups` encrypted to `id`'s recipient.
    fn make_backup(src: &Path, backups: &Path, id: &Identity) -> PathBuf {
        run_backup(
            src,
            backups,
            &StaticKeyProvider::new(id.to_public()),
            10,
            false,
            &mut |_| {},
        )
        .unwrap()
        .archive_path
    }

    #[test]
    fn restore_roundtrips_a_backup() {
        let src = tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        fs::create_dir_all(src.path().join("d")).unwrap();
        fs::write(src.path().join("d/b.txt"), b"beta").unwrap();

        let backups = tempdir().unwrap();
        let id = Identity::generate();
        let archive = make_backup(src.path(), backups.path(), &id);

        let out = tempdir().unwrap();
        let target = out.path().join("restored");
        restore(&id, &archive, &target, Some(src.path()), false).unwrap();

        assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(target.join("d/b.txt")).unwrap(), b"beta");
    }

    #[test]
    fn restore_refuses_to_clobber_the_source() {
        let src = tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        let backups = tempdir().unwrap();
        let id = Identity::generate();
        let archive = make_backup(src.path(), backups.path(), &id);

        // The source itself, and a subdir of it — both refused, even with --force.
        let err = restore(&id, &archive, src.path(), Some(src.path()), true).unwrap_err();
        assert!(err.to_string().contains("overlaps the read-only source"));
        let inside = src.path().join("sub");
        let err = restore(&id, &archive, &inside, Some(src.path()), true).unwrap_err();
        assert!(err.to_string().contains("overlaps the read-only source"));
    }

    #[test]
    fn restore_refuses_nonempty_target_without_force() {
        let src = tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        let backups = tempdir().unwrap();
        let id = Identity::generate();
        let archive = make_backup(src.path(), backups.path(), &id);

        let out = tempdir().unwrap();
        fs::write(out.path().join("existing.txt"), b"keep").unwrap();
        let err = restore(&id, &archive, out.path(), None, false).unwrap_err();
        assert!(err.to_string().contains("not empty"));

        restore(&id, &archive, out.path(), None, true).unwrap(); // --force proceeds
        assert_eq!(fs::read(out.path().join("a.txt")).unwrap(), b"alpha");
    }

    #[test]
    fn parse_identity_roundtrips_literal_and_file() {
        let id = Identity::generate();
        let secret = id.to_string();
        let secret = secret.expose_secret(); // AGE-SECRET-KEY-1…

        let parsed = parse_identity(secret).unwrap();
        assert_eq!(parsed.to_public().to_string(), id.to_public().to_string());

        // `@file` with comment lines, like a real age key file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("key.txt");
        fs::write(
            &path,
            format!(
                "# created: now\n# public key: {}\n{secret}\n",
                id.to_public()
            ),
        )
        .unwrap();
        let parsed = parse_identity(&format!("@{}", path.display())).unwrap();
        assert_eq!(parsed.to_public().to_string(), id.to_public().to_string());
    }

    #[test]
    fn parse_identity_rejects_junk() {
        assert!(parse_identity("not-a-key").is_err());
        assert!(parse_identity("").is_err());
    }
}

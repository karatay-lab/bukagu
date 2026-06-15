//! Pack the source tree into a gzip-compressed tar stream, and unpack it back.
//!
//! The symlink policy matches the sync engine ([`crate::core::scan`]): a symlink
//! — to a file or a directory — is skipped, so a backup never follows a link out
//! of the source and a restore never recreates one. Real directories *are*
//! archived, so empty directories survive a round-trip.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use walkdir::WalkDir;

/// Walk `source` and write a gzip-compressed tar of its contents into `writer`.
/// Archive paths are relative to `source`. Symlinks are skipped. Returns the
/// number of regular files (not directories) archived **and the inner writer**
/// with the gzip stream finished — so a caller wrapping an `age` encryptor can
/// still call `.finish()` on it to flush the final encrypted chunk.
pub fn pack<W: Write>(source: &Path, writer: W) -> Result<(usize, W)> {
    let gz = GzEncoder::new(writer, Compression::default());
    let mut tar = tar::Builder::new(gz);
    // We never hand a symlink to `append_*` (we skip them below), but be explicit.
    tar.follow_symlinks(false);

    let mut files = 0usize;
    for entry in WalkDir::new(source).min_depth(1).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", source.display()))?;

        // Skip symlinks (and don't descend symlinked dirs) — see the module doc.
        if entry.path_is_symlink() {
            continue;
        }

        let path = entry.path();
        let rel = path
            .strip_prefix(source)
            .with_context(|| format!("relativizing {}", path.display()))?;
        let meta = entry
            .metadata()
            .with_context(|| format!("reading metadata for {}", path.display()))?;

        if meta.is_dir() {
            tar.append_dir(rel, path)
                .with_context(|| format!("archiving dir {}", path.display()))?;
        } else {
            let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            tar.append_file(rel, &mut f)
                .with_context(|| format!("archiving file {}", path.display()))?;
            files += 1;
        }
    }

    // Finish the tar then the gzip layer, recovering the inner writer so the
    // caller can finish whatever it wraps (e.g. the age encryptor).
    let gz = tar.into_inner().context("finishing the tar stream")?;
    let writer = gz.finish().context("finishing gzip compression")?;
    Ok((files, writer))
}

/// Read a gzip-compressed tar from `reader` and unpack it under `dest`,
/// recreating the archived tree (preserving file permissions).
pub fn unpack<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let gz = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .with_context(|| format!("unpacking into {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn pack_then_unpack_preserves_tree() {
        let src = tempdir().unwrap();
        fs::write(src.path().join("top.txt"), b"top").unwrap();
        fs::create_dir_all(src.path().join("nested/deep")).unwrap();
        fs::write(src.path().join("nested/deep/file.bin"), b"\x00\x01\x02deep").unwrap();
        fs::create_dir(src.path().join("empty")).unwrap();

        let (n, buf) = pack(src.path(), Vec::new()).unwrap();
        assert_eq!(n, 2, "two regular files archived");

        let out = tempdir().unwrap();
        unpack(&buf[..], out.path()).unwrap();

        assert_eq!(fs::read(out.path().join("top.txt")).unwrap(), b"top");
        assert_eq!(
            fs::read(out.path().join("nested/deep/file.bin")).unwrap(),
            b"\x00\x01\x02deep"
        );
        assert!(
            out.path().join("empty").is_dir(),
            "an empty directory survives the round-trip"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped() {
        use std::os::unix::fs::symlink;

        let src = tempdir().unwrap();
        fs::write(src.path().join("real.txt"), b"hi").unwrap();
        fs::create_dir(src.path().join("realdir")).unwrap();
        symlink(src.path().join("real.txt"), src.path().join("link.txt")).unwrap();
        symlink(src.path().join("realdir"), src.path().join("linkdir")).unwrap();

        let (n, buf) = pack(src.path(), Vec::new()).unwrap();
        assert_eq!(n, 1, "only the one real file is archived");

        let out = tempdir().unwrap();
        unpack(&buf[..], out.path()).unwrap();
        assert!(out.path().join("real.txt").exists());
        assert!(
            !out.path().join("link.txt").exists(),
            "symlinked file skipped"
        );
        assert!(
            !out.path().join("linkdir").exists(),
            "symlinked dir skipped"
        );
    }
}

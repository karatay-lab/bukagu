//! Content hashing with blake3.
//!
//! Only called from [`super::diff`] when two files share the same size, so the
//! cheap size check rules out most work before any bytes are hashed. Because
//! hashing is CPU-bound, the async worker runs it via `spawn_blocking` (Step 5).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

/// Read buffer size for streaming a file through the hasher.
const CHUNK: usize = 64 * 1024;

/// Hash the full contents of the file at `path` with blake3.
pub fn hash_file(path: &Path) -> Result<blake3::Hash> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; CHUNK];

    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize())
}

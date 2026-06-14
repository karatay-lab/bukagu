//! The on-disk store: `./.bukagu/bukagu-store.json` (relative to the launch cwd).
//!
//! First run finds no store → onboarding writes one. Later runs load it and go
//! straight to the dashboard. The store records which folder is the read-only
//! source and which folders are the destinations it mirrors into.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Folder bukagu creates to hold its state, relative to the launch cwd.
const STORE_DIR: &str = ".bukagu";
/// The store file inside [`STORE_DIR`].
const STORE_FILE: &str = "bukagu-store.json";
/// Bumped if the on-disk schema ever changes. v2 added `mappings`; older v1
/// stores (no `mappings` key) still load — see [`Store::mappings`]'s serde default.
const STORE_VERSION: u32 = 2;

/// The user's sync configuration: one read-only source mirrored into many
/// destinations, matched by relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub source: PathBuf,
    pub destinations: Vec<PathBuf>,
}

/// One explicit v2 file mapping: a single source file (path relative to
/// [`Config::source`]) copied into one or more destination files.
///
/// Targets are stored as absolute paths (each inside one of the destination
/// folders). Every target may be written by **at most one** mapping — the
/// duplicate-target rule the mapping editor and engine both enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// Source file, relative to [`Config::source`].
    pub source_rel: PathBuf,
    /// Absolute destination files this source is written to (with a banner).
    pub targets: Vec<PathBuf>,
}

/// The full store as serialized to JSON.
///
/// Serializes flat: `{ version, source, destinations[], mappings[], created_at,
/// last_sync }`. `mappings` is the v2 addition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    pub version: u32,
    #[serde(flatten)]
    pub config: Config,
    /// v2 explicit file mappings. Defaults to empty so a v1 store (which has no
    /// `mappings` key) loads unchanged.
    #[serde(default)]
    pub mappings: Vec<Mapping>,
    /// When the store was first written (RFC 3339, UTC).
    pub created_at: String,
    /// When the last successful sync finished, or `None` if never.
    pub last_sync: Option<String>,
}

impl Store {
    /// Build a fresh store from a confirmed onboarding [`Config`].
    pub fn new(config: Config) -> Self {
        Self {
            version: STORE_VERSION,
            config,
            mappings: Vec::new(),
            created_at: now_rfc3339(),
            last_sync: None,
        }
    }

    /// Record that a sync just completed by stamping `last_sync` with now.
    pub fn mark_synced(&mut self) {
        self.last_sync = Some(now_rfc3339());
    }

    /// Load the store from `./.bukagu/`, or `None` if it doesn't exist yet.
    pub fn load() -> Result<Option<Self>> {
        Self::load_from(Path::new("."))
    }

    /// Save the store under `./.bukagu/`, creating the folder if needed.
    pub fn save(&self) -> Result<()> {
        self.save_to(Path::new("."))
    }

    /// Like [`Store::load`] but rooted at `base` (used in tests).
    pub fn load_from(base: &Path) -> Result<Option<Self>> {
        let path = path_in(base);
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path)
            .with_context(|| format!("reading store at {}", path.display()))?;
        let store = serde_json::from_str(&data)
            .with_context(|| format!("parsing store at {}", path.display()))?;
        Ok(Some(store))
    }

    /// Like [`Store::save`] but rooted at `base` (used in tests).
    pub fn save_to(&self, base: &Path) -> Result<()> {
        let dir = dir_in(base);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating store dir {}", dir.display()))?;
        let path = path_in(base);
        let json = serde_json::to_string_pretty(self).context("serializing store")?;
        fs::write(&path, json).with_context(|| format!("writing store to {}", path.display()))?;
        Ok(())
    }
}

fn dir_in(base: &Path) -> PathBuf {
    base.join(STORE_DIR)
}

fn path_in(base: &Path) -> PathBuf {
    dir_in(base).join(STORE_FILE)
}

/// The project root implied by a launch directory: the parent if `cwd` *is* the
/// `.bukagu` store dir, else `cwd` unchanged. Pure (no I/O), so it's unit-testable
/// without touching the process-global working directory.
fn project_root_of(cwd: &Path) -> PathBuf {
    if cwd.file_name() == Some(std::ffi::OsStr::new(STORE_DIR))
        && let Some(parent) = cwd.parent()
    {
        return parent.to_path_buf();
    }
    cwd.to_path_buf()
}

/// Make sure the working directory is the **project root** — the folder that
/// holds `.bukagu/`, not the store dir itself.
///
/// If bukagu was launched from *inside* `.bukagu/` (e.g. `cargo run` while `cd`'d
/// there — cargo still finds the manifest in a parent, but the process runs with
/// cwd `= .bukagu`), step up to its parent. Otherwise everything that anchors to
/// `.` (store load/save) or `current_dir()` (the onboarding browser) would be one
/// level too deep — landing at `.bukagu/.bukagu`. Returns the resolved root.
pub fn normalize_cwd() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let root = project_root_of(&cwd);
    if root != cwd {
        std::env::set_current_dir(&root)
            .with_context(|| format!("moving up to the project root {}", root.display()))?;
    }
    Ok(root)
}

/// Current UTC time formatted as `YYYY-MM-DDTHH:MM:SSZ`, with no extra deps.
pub(crate) fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert days-since-Unix-epoch to a `(year, month, day)` civil date.
///
/// Howard Hinnant's `civil_from_days` algorithm — exact for all dates, no deps.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_is_none_when_absent() {
        let tmp = tempdir().unwrap();
        assert!(Store::load_from(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();

        let cfg = Config {
            source: PathBuf::from("/src"),
            destinations: vec![PathBuf::from("/d1"), PathBuf::from("/d2")],
        };
        Store::new(cfg).save_to(base).unwrap();

        let loaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(loaded.version, STORE_VERSION);
        assert_eq!(loaded.config.source, PathBuf::from("/src"));
        assert_eq!(loaded.config.destinations.len(), 2);
        assert!(loaded.last_sync.is_none());
    }

    #[test]
    fn store_json_is_flat() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        let cfg = Config {
            source: PathBuf::from("/src"),
            destinations: vec![PathBuf::from("/d1")],
        };
        Store::new(cfg).save_to(base).unwrap();

        let raw = fs::read_to_string(path_in(base)).unwrap();
        // Flattened: top-level keys, not a nested "config" object.
        assert!(raw.contains("\"source\""));
        assert!(raw.contains("\"destinations\""));
        assert!(!raw.contains("\"config\""));
    }

    #[test]
    fn mappings_roundtrip_and_serialize_flat() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        let mut store = Store::new(Config {
            source: PathBuf::from("/src"),
            destinations: vec![PathBuf::from("/d1")],
        });
        store.mappings = vec![Mapping {
            source_rel: PathBuf::from("a.py"),
            targets: vec![PathBuf::from("/d1/a.py"), PathBuf::from("/d1/copy/a.py")],
        }];
        store.save_to(base).unwrap();

        let raw = fs::read_to_string(path_in(base)).unwrap();
        assert!(raw.contains("\"mappings\""), "mappings serialize flat");
        assert!(raw.contains("\"version\": 2"), "new stores are v2");

        let loaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(loaded.mappings.len(), 1);
        assert_eq!(loaded.mappings[0].targets.len(), 2);
    }

    #[test]
    fn v1_store_without_mappings_loads_with_empty_mappings() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        // A genuine v1 payload: version 1, no `mappings` key at all.
        let v1 = r#"{
            "version": 1,
            "source": "/src",
            "destinations": ["/d1"],
            "created_at": "2026-06-13T00:00:00Z",
            "last_sync": null
        }"#;
        fs::create_dir_all(dir_in(base)).unwrap();
        fs::write(path_in(base), v1).unwrap();

        let loaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(loaded.version, 1, "version field is preserved as read");
        assert!(
            loaded.mappings.is_empty(),
            "a v1 store migrates to an empty mapping list"
        );
    }

    #[test]
    fn timestamp_is_rfc3339_utc() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 20); // YYYY-MM-DDTHH:MM:SSZ
        assert!(t.ends_with('Z'));
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1)); // Unix epoch
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
    }

    #[test]
    fn project_root_steps_out_of_the_store_dir() {
        // Launched from inside `.bukagu/` → root is its parent.
        assert_eq!(
            project_root_of(Path::new("/repos/app/.bukagu")),
            PathBuf::from("/repos/app")
        );
        // Launched from the project root → unchanged.
        assert_eq!(
            project_root_of(Path::new("/repos/app")),
            PathBuf::from("/repos/app")
        );
        // A folder that merely contains `.bukagu` (but isn't it) → unchanged.
        assert_eq!(
            project_root_of(Path::new("/repos/bukagu")),
            PathBuf::from("/repos/bukagu")
        );
    }
}

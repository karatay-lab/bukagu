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
/// Bumped if the on-disk schema ever changes. v2 added `mappings`, v3 added
/// `backup`; older stores (missing those keys) still load — see the serde
/// defaults on [`Store::mappings`] and [`Store::backup`].
const STORE_VERSION: u32 = 3;

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

/// Timestamped archives bukagu keeps per project before pruning the oldest, when
/// [`BackupSettings::retention`] is unset.
pub const DEFAULT_RETENTION: usize = 10;

/// v3 backup configuration: where encrypted source backups are written and how
/// many to retain. Every field is optional, so a v1/v2 store (which has no
/// `backup` key) loads unchanged via the [`Store::backup`] serde default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSettings {
    /// Backup root folder. `None` → `~/bukagu-backups`, resolved at run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// Archives to keep per project. `None` → [`DEFAULT_RETENTION`]; `Some(0)`
    /// means keep every archive (never prune).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<usize>,
    /// When the last successful backup finished (RFC 3339, UTC), or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup: Option<String>,
}

impl BackupSettings {
    /// The retention count to use, falling back to [`DEFAULT_RETENTION`].
    pub fn effective_retention(&self) -> usize {
        self.retention.unwrap_or(DEFAULT_RETENTION)
    }
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
    /// v3 backup settings. Defaults so a v1/v2 store (no `backup` key) loads.
    #[serde(default)]
    pub backup: BackupSettings,
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
            backup: BackupSettings::default(),
            created_at: now_rfc3339(),
            last_sync: None,
        }
    }

    /// Record that a sync just completed by stamping `last_sync` with now.
    pub fn mark_synced(&mut self) {
        self.last_sync = Some(now_rfc3339());
    }

    /// Record that a backup just completed by stamping `backup.last_backup`.
    pub fn mark_backed_up(&mut self) {
        self.backup.last_backup = Some(now_rfc3339());
    }

    /// Load the store from `./.bukagu/`, or `None` if it doesn't exist yet.
    pub fn load() -> Result<Option<Self>> {
        Self::load_from(Path::new("."))
    }

    /// Save the store under `./.bukagu/`, creating the folder if needed.
    pub fn save(&mut self) -> Result<()> {
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
    pub fn save_to(&mut self, base: &Path) -> Result<()> {
        // bukagu always writes the full current schema, so stamp the on-disk
        // version with what we're actually writing. [`Store::load`] preserves the
        // version it read (so an unopened store keeps its number), but the moment
        // we re-save we've upgraded it — keeping the field a truthful record of the
        // schema on disk rather than freezing it at the store's creation version.
        self.version = STORE_VERSION;
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

/// The current UTC time as `(year, month, day, hour, minute, second)`, from the
/// Unix clock with no extra deps. Shared by the two formatters below.
fn now_civil() -> (i64, u32, u32, i64, i64, i64) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Current UTC time formatted as `YYYY-MM-DDTHH:MM:SSZ`, with no extra deps.
pub(crate) fn now_rfc3339() -> String {
    let (y, m, d, hh, mm, ss) = now_civil();
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Current UTC time as a filesystem-safe stamp `YYYYMMDDTHHMMSSZ` — no colons
/// (invalid in Windows paths) or other separators. Names backup archives.
pub(crate) fn now_compact() -> String {
    let (y, m, d, hh, mm, ss) = now_civil();
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
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
        assert!(raw.contains("\"version\": 3"), "new stores are v3");

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
    fn resaving_an_old_store_upgrades_its_version() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        // A genuine v1 payload on disk.
        let v1 = r#"{
            "version": 1,
            "source": "/src",
            "destinations": ["/d1"],
            "created_at": "2026-06-13T00:00:00Z",
            "last_sync": null
        }"#;
        fs::create_dir_all(dir_in(base)).unwrap();
        fs::write(path_in(base), v1).unwrap();

        // Loading preserves the on-disk version…
        let mut loaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(loaded.version, 1, "load preserves the version as read");

        // …but the next save upgrades it to the current schema version.
        loaded.save_to(base).unwrap();
        let reloaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(
            reloaded.version, STORE_VERSION,
            "re-saving stamps the current version"
        );
    }

    #[test]
    fn backup_settings_default_then_roundtrip() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        let mut store = Store::new(Config {
            source: PathBuf::from("/src"),
            destinations: vec![PathBuf::from("/d1")],
        });
        // A fresh store has empty backup settings (resolved to defaults at run time).
        assert_eq!(store.backup, BackupSettings::default());
        assert!(store.backup.last_backup.is_none());

        store.backup.retention = Some(3);
        store.backup.last_backup = Some(now_rfc3339());
        store.save_to(base).unwrap();

        let raw = fs::read_to_string(path_in(base)).unwrap();
        assert!(raw.contains("\"backup\""), "backup settings serialize flat");

        let loaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(loaded.backup.retention, Some(3));
        assert!(loaded.backup.last_backup.is_some());
        assert!(loaded.backup.root.is_none());
    }

    #[test]
    fn v2_store_without_backup_loads_with_default_backup() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        // A genuine v2 payload: version 2, mappings present, but no `backup` key.
        let v2 = r#"{
            "version": 2,
            "source": "/src",
            "destinations": ["/d1"],
            "mappings": [{ "source_rel": "a.py", "targets": ["/d1/a.py"] }],
            "created_at": "2026-06-14T00:00:00Z",
            "last_sync": null
        }"#;
        fs::create_dir_all(dir_in(base)).unwrap();
        fs::write(path_in(base), v2).unwrap();

        let loaded = Store::load_from(base).unwrap().expect("store present");
        assert_eq!(loaded.version, 2, "version field is preserved as read");
        assert_eq!(loaded.mappings.len(), 1, "v2 mappings still load");
        assert_eq!(
            loaded.backup,
            BackupSettings::default(),
            "a v2 store migrates to default backup settings"
        );
    }

    #[test]
    fn timestamp_is_rfc3339_utc() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 20); // YYYY-MM-DDTHH:MM:SSZ
        assert!(t.ends_with('Z'));
    }

    #[test]
    fn now_compact_is_filesystem_safe() {
        let t = now_compact();
        assert_eq!(t.len(), 16); // YYYYMMDDTHHMMSSZ
        assert!(t.ends_with('Z'));
        assert!(t.contains('T'));
        assert!(!t.contains(':'), "colons are invalid in Windows paths");
    }

    #[test]
    fn effective_retention_falls_back_to_default() {
        assert_eq!(
            BackupSettings::default().effective_retention(),
            DEFAULT_RETENTION
        );
        assert_eq!(
            BackupSettings {
                retention: Some(3),
                ..Default::default()
            }
            .effective_retention(),
            3
        );
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

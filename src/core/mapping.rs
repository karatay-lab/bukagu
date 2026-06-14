//! The v2 explicit-mapping engine: validate → plan → apply, per `source → target`.
//!
//! Unlike the v1 folder mirror ([`super::diff`]), v2 syncs a user-chosen set of
//! [`Mapping`]s — one source file written into one or more destination files, each
//! with a [`super::banner`] on top. Everything here is pure logic over the
//! filesystem and unit-testable without a terminal.
//!
//! GUARDRAIL: [`apply_target`] resolves paths and refuses to write into the
//! read-only source (or outside every destination folder) — the enforced backstop
//! for "bukagu only ever modifies destinations".

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::banner;
use crate::store::Mapping;

/// Whether a target got a banner, or why it didn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerMode {
    /// A banner was (or would be) prepended.
    Applied,
    /// The extension has no known comment syntax — copied verbatim.
    SkippedUnknownExt,
    /// The source isn't UTF-8 text (binary) — copied verbatim.
    SkippedBinary,
}

impl BannerMode {
    /// A short tag for the summary screen.
    pub fn note(self) -> Option<&'static str> {
        match self {
            BannerMode::Applied => None,
            BannerMode::SkippedUnknownExt => Some("no banner (unknown type)"),
            BannerMode::SkippedBinary => Some("no banner (binary)"),
        }
    }
}

/// What syncing a single target would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    /// The target doesn't exist yet — it will be written.
    Create,
    /// The target exists but differs from what we'd write — it will be replaced.
    Update,
    /// The target already matches — nothing to do.
    UpToDate,
    /// The source file is gone — this target is skipped (and flagged).
    SourceMissing,
}

impl TargetState {
    /// A short, stable label for the summary list.
    pub fn label(self) -> &'static str {
        match self {
            TargetState::Create => "create",
            TargetState::Update => "update",
            TargetState::UpToDate => "ok",
            TargetState::SourceMissing => "missing",
        }
    }
}

/// One planned write: a source file mapped onto a single destination target.
#[derive(Debug, Clone)]
pub struct TargetPlan {
    pub source_rel: PathBuf,
    pub source_abs: PathBuf,
    pub target: PathBuf,
    pub state: TargetState,
    pub banner: BannerMode,
    /// Bytes that would be written (0 for up-to-date / missing-source rows).
    pub bytes: u64,
}

impl TargetPlan {
    /// Whether this target actually gets written during apply.
    pub fn will_write(&self) -> bool {
        matches!(self.state, TargetState::Create | TargetState::Update)
    }
}

/// Rolled-up counts for a [`MappingPlan`], shown in the summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappingStats {
    pub created: usize,
    pub updated: usize,
    pub up_to_date: usize,
    pub source_missing: usize,
    /// Targets written without a banner (unknown type or binary).
    pub banner_skipped: usize,
    /// Total bytes that would be written (created + updated).
    pub bytes: u64,
}

/// Every target's plan plus a rolled-up summary.
#[derive(Debug, Clone, Default)]
pub struct MappingPlan {
    pub targets: Vec<TargetPlan>,
    pub stats: MappingStats,
}

impl MappingPlan {
    /// Targets that would actually be written (created + updated).
    pub fn changes(&self) -> usize {
        self.stats.created + self.stats.updated
    }

    /// True when nothing would be written.
    pub fn is_noop(&self) -> bool {
        self.changes() == 0
    }
}

/// A problem with the current set of mappings, surfaced before a sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// A destination target is written by more than one mapping.
    DuplicateTarget(PathBuf),
    /// A target resolves inside the read-only source.
    TargetInSource(PathBuf),
    /// A target sits outside every registered destination folder.
    TargetOutsideDestinations(PathBuf),
    /// A mapping's source file no longer exists.
    SourceMissing(PathBuf),
}

impl Issue {
    /// Blocking issues stop a sync entirely; a missing source only skips its own
    /// target, so it's non-blocking.
    pub fn is_blocking(&self) -> bool {
        !matches!(self, Issue::SourceMissing(_))
    }

    /// A user-facing one-line description.
    pub fn message(&self) -> String {
        match self {
            Issue::DuplicateTarget(t) => {
                format!("{} is mapped from more than one source", t.display())
            }
            Issue::TargetInSource(t) => {
                format!(
                    "{} is inside the source (bukagu never writes there)",
                    t.display()
                )
            }
            Issue::TargetOutsideDestinations(t) => {
                format!("{} is outside every destination folder", t.display())
            }
            Issue::SourceMissing(s) => format!("source file {} is missing", s.display()),
        }
    }
}

/// Validate the mappings against the source and destination roots. Returns every
/// issue found (possibly empty). Callers refuse to sync while any *blocking* issue
/// remains; [`Issue::SourceMissing`] is reported but only skips its own target.
pub fn validate(source: &Path, dest_roots: &[PathBuf], mappings: &[Mapping]) -> Vec<Issue> {
    let mut issues = Vec::new();

    // The core rule: a target may be written by at most one mapping. Report each
    // duplicated target once.
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut reported: HashSet<&Path> = HashSet::new();
    for target in mappings.iter().flat_map(|m| &m.targets) {
        if !seen.insert(target) && reported.insert(target) {
            issues.push(Issue::DuplicateTarget(target.clone()));
        }
    }

    for m in mappings {
        if !source.join(&m.source_rel).is_file() {
            issues.push(Issue::SourceMissing(m.source_rel.clone()));
        }
        for target in &m.targets {
            let resolved = resolved_abs(target);
            if resolved.starts_with(resolved_abs(source)) {
                issues.push(Issue::TargetInSource(target.clone()));
            } else if !dest_roots
                .iter()
                .any(|d| resolved.starts_with(resolved_abs(d)))
            {
                issues.push(Issue::TargetOutsideDestinations(target.clone()));
            }
        }
    }

    issues
}

/// Plan every target: compute its [`TargetState`] and banner mode by rendering the
/// bytes we'd write and comparing them with what's on disk.
pub fn plan(source: &Path, mappings: &[Mapping]) -> Result<MappingPlan> {
    let mut targets = Vec::new();
    let mut stats = MappingStats::default();

    for m in mappings {
        let source_abs = source.join(&m.source_rel);
        for target in &m.targets {
            let tp = plan_one(&m.source_rel, &source_abs, target)?;
            match tp.state {
                TargetState::Create => {
                    stats.created += 1;
                    stats.bytes += tp.bytes;
                }
                TargetState::Update => {
                    stats.updated += 1;
                    stats.bytes += tp.bytes;
                }
                TargetState::UpToDate => stats.up_to_date += 1,
                TargetState::SourceMissing => stats.source_missing += 1,
            }
            if tp.state != TargetState::SourceMissing && tp.banner != BannerMode::Applied {
                stats.banner_skipped += 1;
            }
            targets.push(tp);
        }
    }

    Ok(MappingPlan { targets, stats })
}

fn plan_one(source_rel: &Path, source_abs: &Path, target: &Path) -> Result<TargetPlan> {
    if !source_abs.is_file() {
        return Ok(TargetPlan {
            source_rel: source_rel.to_path_buf(),
            source_abs: source_abs.to_path_buf(),
            target: target.to_path_buf(),
            state: TargetState::SourceMissing,
            banner: banner_mode_for_ext(target),
            bytes: 0,
        });
    }

    let (rendered, banner) = render_target(source_abs, target)?;
    let state = match fs::read(target) {
        Ok(existing) if existing == rendered => TargetState::UpToDate,
        // Exists-but-differs, or unreadable/absent → write it.
        Ok(_) => TargetState::Update,
        Err(_) => TargetState::Create,
    };

    Ok(TargetPlan {
        source_rel: source_rel.to_path_buf(),
        source_abs: source_abs.to_path_buf(),
        target: target.to_path_buf(),
        state,
        banner,
        bytes: rendered.len() as u64,
    })
}

/// Build the exact bytes to write to `target`: the source's contents plus a
/// banner. Files whose extension has no comment syntax, or whose source isn't
/// UTF-8 text, are returned verbatim with the matching [`BannerMode`].
pub fn render_target(source_abs: &Path, target: &Path) -> Result<(Vec<u8>, BannerMode)> {
    let bytes = fs::read(source_abs)
        .with_context(|| format!("reading source file {}", source_abs.display()))?;

    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    let Some(style) = banner::comment_style(ext) else {
        return Ok((bytes, BannerMode::SkippedUnknownExt));
    };

    match std::str::from_utf8(&bytes) {
        Ok(text) => Ok((
            banner::with_banner(&style, source_abs, text).into_bytes(),
            BannerMode::Applied,
        )),
        // Binary content in a commentable extension: copy verbatim, don't corrupt it.
        Err(_) => Ok((bytes, BannerMode::SkippedBinary)),
    }
}

/// Banner mode implied by a target's extension alone (no file read) — used to
/// describe a target whose source is missing.
fn banner_mode_for_ext(target: &Path) -> BannerMode {
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    if banner::comment_style(ext).is_some() {
        BannerMode::Applied
    } else {
        BannerMode::SkippedUnknownExt
    }
}

/// Write one target. Re-checks the guardrail first, then (unless `dry_run`)
/// renders the source+banner and writes it, creating parent directories as needed.
/// Up-to-date / missing-source targets should be filtered out by the caller.
pub fn apply_target(
    source: &Path,
    dest_roots: &[PathBuf],
    tp: &TargetPlan,
    dry_run: bool,
) -> Result<()> {
    guard_target(source, dest_roots, &tp.target)?;
    if dry_run {
        return Ok(());
    }

    let (rendered, _) = render_target(&tp.source_abs, &tp.target)?;
    if let Some(parent) = tp.target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent {}", parent.display()))?;
    }
    fs::write(&tp.target, &rendered).with_context(|| format!("writing {}", tp.target.display()))?;
    Ok(())
}

/// GUARDRAIL — refuse to write a target that resolves inside the read-only
/// `source`, or outside every destination root. Resolves the longest existing
/// prefix of each path (so a not-yet-created target is still checked) before the
/// comparison. This is the enforced backstop for "bukagu never modifies the source".
pub fn guard_target(source: &Path, dest_roots: &[PathBuf], target: &Path) -> Result<()> {
    let src = resolved_abs(source);
    let resolved = resolved_abs(target);

    if resolved.starts_with(&src) {
        bail!(
            "refusing to write into the source: {} is inside {}",
            target.display(),
            source.display()
        );
    }
    if !dest_roots
        .iter()
        .any(|d| resolved.starts_with(resolved_abs(d)))
    {
        bail!(
            "refusing to write {}: it is outside every destination folder",
            target.display()
        );
    }
    Ok(())
}

/// Resolve a path's longest existing prefix to its canonical form, then re-attach
/// the remainder. Robust to `.`/`..`/symlinks even when the leaf doesn't exist yet
/// (a target about to be created), so guardrail comparisons can't be fooled.
fn resolved_abs(p: &Path) -> PathBuf {
    for ancestor in p.ancestors() {
        if let Ok(canon) = ancestor.canonicalize() {
            let rest = p.strip_prefix(ancestor).unwrap_or(Path::new(""));
            return canon.join(rest);
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Write `content` to `root/rel`, creating parents as needed; returns the path.
    fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }

    fn mapping(source_rel: &str, targets: Vec<PathBuf>) -> Mapping {
        Mapping {
            source_rel: PathBuf::from(source_rel),
            targets,
        }
    }

    #[test]
    fn create_then_uptodate_then_update() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "print('hi')\n");
        let target = dst.path().join("a.py");
        let maps = [mapping("a.py", vec![target.clone()])];

        // First plan: the target doesn't exist → Create.
        let p = plan(src.path(), &maps).unwrap();
        assert_eq!(p.stats.created, 1);
        assert_eq!(p.targets[0].state, TargetState::Create);

        // Apply, then re-plan: now UpToDate.
        apply_target(
            src.path(),
            &[dst.path().to_path_buf()],
            &p.targets[0],
            false,
        )
        .unwrap();
        let body = fs::read_to_string(&target).unwrap();
        assert!(body.contains(banner::BANNER_MARKER), "banner written");
        assert!(body.contains("print('hi')"), "source body written");

        let p2 = plan(src.path(), &maps).unwrap();
        assert_eq!(p2.stats.up_to_date, 1);
        assert!(p2.is_noop());

        // Change the source → Update.
        write(src.path(), "a.py", "print('changed')\n");
        let p3 = plan(src.path(), &maps).unwrap();
        assert_eq!(p3.stats.updated, 1);
        assert_eq!(p3.targets[0].state, TargetState::Update);
    }

    #[test]
    fn re_apply_does_not_stack_banner() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.js", "const x = 1;\n");
        let target = dst.path().join("a.js");
        let tp = plan(src.path(), &[mapping("a.js", vec![target.clone()])])
            .unwrap()
            .targets
            .remove(0);

        let roots = [dst.path().to_path_buf()];
        apply_target(src.path(), &roots, &tp, false).unwrap();
        // Re-plan + re-apply (simulating a second sync) must not add a second banner.
        let tp2 = plan(src.path(), &[mapping("a.js", vec![target.clone()])])
            .unwrap()
            .targets
            .remove(0);
        apply_target(src.path(), &roots, &tp2, false).unwrap();

        let body = fs::read_to_string(&target).unwrap();
        assert_eq!(body.matches(banner::BANNER_MARKER).count(), 1);
    }

    #[test]
    fn unknown_extension_copies_without_banner() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "data.bin", "raw bytes\n");
        let target = dst.path().join("data.bin");
        let p = plan(src.path(), &[mapping("data.bin", vec![target.clone()])]).unwrap();

        assert_eq!(p.targets[0].banner, BannerMode::SkippedUnknownExt);
        assert_eq!(p.stats.banner_skipped, 1);

        apply_target(
            src.path(),
            &[dst.path().to_path_buf()],
            &p.targets[0],
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "raw bytes\n",
            "verbatim"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        let target = dst.path().join("a.py");
        let p = plan(src.path(), &[mapping("a.py", vec![target.clone()])]).unwrap();

        apply_target(src.path(), &[dst.path().to_path_buf()], &p.targets[0], true).unwrap();
        assert!(!target.exists(), "dry-run leaves the target unwritten");
    }

    #[test]
    fn duplicate_target_is_flagged_and_blocks() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "1\n");
        write(src.path(), "b.py", "2\n");
        let shared = dst.path().join("shared.py");
        let maps = [
            mapping("a.py", vec![shared.clone()]),
            mapping("b.py", vec![shared.clone()]),
        ];

        let issues = validate(src.path(), &[dst.path().to_path_buf()], &maps);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Issue::DuplicateTarget(_)))
        );
        assert!(issues.iter().any(Issue::is_blocking));
    }

    #[test]
    fn target_inside_source_is_refused() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "1\n");
        // A target path that points back inside the read-only source.
        let bad = src.path().join("leak.py");
        let maps = [mapping("a.py", vec![bad.clone()])];

        let issues = validate(src.path(), &[dst.path().to_path_buf()], &maps);
        assert!(issues.iter().any(|i| matches!(i, Issue::TargetInSource(_))));

        // And the guardrail in apply refuses it outright.
        let tp = plan(src.path(), &maps).unwrap().targets.remove(0);
        let err = apply_target(src.path(), &[dst.path().to_path_buf()], &tp, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to write into the source")
        );
    }

    #[test]
    fn missing_source_is_non_blocking_and_skipped() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let target = dst.path().join("gone.py");
        let maps = [mapping("gone.py", vec![target])];

        let issues = validate(src.path(), &[dst.path().to_path_buf()], &maps);
        assert!(issues.iter().any(|i| matches!(i, Issue::SourceMissing(_))));
        assert!(
            !issues.iter().any(Issue::is_blocking),
            "missing source doesn't block"
        );

        let p = plan(src.path(), &maps).unwrap();
        assert_eq!(p.stats.source_missing, 1);
        assert!(!p.targets[0].will_write());
    }

    #[test]
    fn one_source_fans_out_to_many_targets() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write(src.path(), "a.py", "x = 1\n");
        let t1 = dst.path().join("one/a.py");
        let t2 = dst.path().join("two/a.py");
        let maps = [mapping("a.py", vec![t1.clone(), t2.clone()])];

        assert!(validate(src.path(), &[dst.path().to_path_buf()], &maps).is_empty());
        let p = plan(src.path(), &maps).unwrap();
        assert_eq!(p.stats.created, 2);

        let roots = [dst.path().to_path_buf()];
        for tp in &p.targets {
            apply_target(src.path(), &roots, tp, false).unwrap();
        }
        assert!(t1.is_file() && t2.is_file(), "both targets written");
    }
}

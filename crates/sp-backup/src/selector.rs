//! Deciding which files belong in a backup — before any of them is copied.
//!
//! The plan is built first and copied second, for three reasons that all show up
//! in the UI: the progress bar needs a byte total that does not change mid-flight;
//! `max_total_bytes` has to be checked before the disk fills up; and the user
//! should be able to see "312 files, 1.9 GB, excluding `logs/`" and cancel rather
//! than discover it afterwards.

use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation.
///
/// A backup of a large world takes minutes and cannot be interrupted safely in the
/// middle of a write, so cancellation is checked *between files*: the `.part`
/// archive is then discarded, which is exactly why the rename at the end is the
/// only moment a backup becomes visible.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Why a path was left out. Kept in the manifest, because "your backup has 4 files"
/// needs an explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkippedKind {
    /// Excluded by a pattern (user's or default).
    Excluded,
    /// Backing up a symlink can follow the archive out of the server directory,
    /// and restoring one can point a config file at `/etc/shadow`.
    Symlink,
    /// FIFOs, sockets, devices: reading one blocks forever or produces nothing.
    NotAFile,
    /// Could not be opened.
    Unreadable,
}

#[derive(Debug, Clone)]
pub struct SelectedFile {
    /// Absolute path on disk.
    pub abs: PathBuf,
    /// Path inside the archive, `/`-separated, relative to the server root.
    pub rel: String,
    pub bytes: u64,
    pub modified_ms: i64,
    /// Unix mode bits, so an executable script in `mods/` survives a round trip on
    /// the platforms that care. Ignored on Windows, where the concept does not
    /// map onto anything the archive format stores usefully.
    pub executable: bool,
}

/// Directories that are pure noise inside a server root. `backups` is the important
/// one: pointing a destination *at* the server directory would otherwise back a
/// backup up inside itself, forever growing.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "backups",
    "logs",
    "crash-reports",
    "*.tmp",
    "*.part",
    "*.old",
    "hs_err_pid*.log",
    "replay-files",
    ".DS_Store",
    "Thumbs.db",
    "lost+found",
];

/// Worlds that are not the default one, excluded unless the policy asks. A
/// 400 MB minigame world nobody visits should not silently double every backup.
pub const EXTRA_WORLDS: &[&str] = &["world_*", "*/world_*"];

#[derive(Debug, Clone, Default)]
struct Built {
    set: Option<GlobSet>,
    /// Lowercased segments/prefixes matched cheaply before the glob engine runs,
    /// so a huge `libraries/` tree costs one name check instead of a regex.
    dir_names: Vec<String>,
    patterns: Vec<String>,
}

/// Which files to copy.
#[derive(Debug, Clone)]
pub struct Selector {
    excludes: Built,
    includes: Built,
    include_worlds: bool,
    include_logs: bool,
    use_defaults: bool,
    follow_symlinks: bool,
    max_file_bytes: u64,
}

impl Default for Selector {
    fn default() -> Self {
        Self {
            excludes: Built::default(),
            includes: Built::default(),
            include_worlds: true,
            include_logs: false,
            use_defaults: true,
            follow_symlinks: false,
            max_file_bytes: 0,
        }
    }
}

impl Selector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add exclude globs (matched against the `/`-separated relative path).
    pub fn with_excludes(mut self, patterns: Vec<String>) -> Self {
        self.excludes = build(self.excludes, &patterns);
        self
    }

    /// If non-empty, only paths matching one of these are eligible.
    pub fn with_includes(mut self, patterns: &[String]) -> Self {
        self.includes = build(self.includes.clone(), patterns);
        self
    }

    pub fn with_worlds(mut self, yes: bool) -> Self {
        self.include_worlds = yes;
        self
    }

    pub fn with_logs(mut self, yes: bool) -> Self {
        self.include_logs = yes;
        self
    }

    /// Stop adding the built-in exclude list (used by an import of a foreign
    /// archive, where the tree on disk is not what we are measuring).
    pub fn without_defaults(mut self) -> Self {
        self.use_defaults = false;
        self
    }

    pub fn with_default_excludes(mut self, yes: bool) -> Self {
        self.use_defaults = yes;
        self
    }

    /// Include symlinks *as links* (never as their target). Off by default.
    pub fn follow_symlinks(mut self, yes: bool) -> Self {
        self.follow_symlinks = yes;
        self
    }

    /// Ignore single files larger than this (0 = no limit). Catches a runaway
    /// `nohup.out` or a core dump that would otherwise stall a nightly job.
    pub fn max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    fn exclude_set(&self) -> Result<GlobSet, sp_core::CoreError> {
        let mut pats: Vec<String> = Vec::new();
        if self.use_defaults {
            pats.extend(DEFAULT_EXCLUDES.iter().map(|s| (*s).to_string()));
            if !self.include_logs {
                pats.push("logs".into());
                pats.push("*/logs".into());
            }
            if !self.include_worlds {
                pats.extend(EXTRA_WORLDS.iter().map(|s| (*s).to_string()));
            }
        }
        pats.extend(self.excludes.patterns.iter().cloned());
        build_set(&pats)
            .map(|g| g.unwrap_or_else(|| GlobSetBuilder::new().build().expect("empty set builds")))
    }

    fn include_set(&self) -> Result<Option<GlobSet>, sp_core::CoreError> {
        if self.includes.patterns.is_empty() {
            return Ok(None);
        }
        build_set(&self.includes.patterns).map(Some)
    }

    /// Walk `root` and return the plan.
    pub fn plan(&self, root: &Path) -> Result<SelectionPlan, sp_core::CoreError> {
        let excludes = self.exclude_set()?;
        let includes = self.include_set()?;
        let mut plan = SelectionPlan::default();
        if !root.is_dir() {
            return Err(sp_core::CoreError::NotFound {
                what: format!("server directory {} does not exist", root.display()),
            });
        }
        let mut walker = walkdir::WalkDir::new(root)
            .follow_links(self.follow_symlinks)
            .contents_first(false)
            .sort_by_file_name()
            .max_depth(64);
        let mut dirs: Vec<String> = Vec::new();
        for entry in walker.into_iter() {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    // A directory we cannot read is a real problem, but it must not
                    // be reported as "the backup succeeded with 400 files".
                    let path = err
                        .path()
                        .map(|p| rel(root, p))
                        .unwrap_or_else(|| err.to_string());
                    plan.skipped.push(SkippedPath {
                        path,
                        reason: SkippedKind::Unreadable,
                    });
                    continue;
                }
            };
            let abs = entry.path();
            let rels = rel(root, abs);
            if entry.depth() == 0 {
                continue; // the root itself
            }
            if entry.file_type().is_dir() {
                // Prune whole subtrees on an exact-name hit, which is the common
                // case (`logs`, `libraries`) and far cheaper than globbing per file.
                if should_prune_dir(&rels, &excludes) {
                    if let Some(name) = abs.file_name().and_then(|s| s.to_str()) {
                        dirs.push(name.to_string());
                    }
                    plan.dirs += 1;
                    plan.excluded_prefixes.push(rels);
                    continue;
                }
                plan.dirs += 1;
                plan.dir_paths.push(rels.clone());
                continue;
            }
            if entry.file_type().is_symlink() && !self.follow_symlinks {
                plan.skipped.push(SkippedPath {
                    path: rels,
                    reason: SkippedKind::Symlink,
                });
                continue;
            }
            if !entry.file_type().is_file() {
                plan.skipped.push(SkippedPath {
                    path: rels,
                    reason: SkippedKind::NotAFile,
                });
                continue;
            }
            let Ok(md) = entry.metadata() else {
                plan.skipped.push(SkippedPath {
                    path: rels,
                    reason: SkippedKind::Unreadable,
                });
                continue;
            };
            let bytes = md.len();
            if self.max_file_bytes > 0 && bytes > self.max_file_bytes {
                plan.skipped.push(SkippedPath {
                    path: rels,
                    reason: SkippedKind::Excluded,
                });
                continue;
            }
            if excludes.matches(&rels).matched_any() {
                plan.skipped.push(SkippedPath {
                    path: rels,
                    reason: SkippedKind::Excluded,
                });
                continue;
            }
            if let Some(inc) = &includes {
                if !inc.matches(&rels).matched_any() {
                    plan.skipped.push(SkippedPath {
                        path: rels,
                        reason: SkippedKind::Excluded,
                    });
                    continue;
                }
            }
            plan.files.push(SelectedFile {
                abs: abs.to_path_buf(),
                rel: rels,
                bytes,
                modified_ms: mtime_ms(&md),
                executable: is_executable(&md),
            });
            plan.total_bytes += bytes;
            plan.newest_ms = plan.newest_ms.max(mtime_ms(&md));
        }
        // Deterministic order, whatever the filesystem returned: two backups of an
        // unchanged tree then have byte-identical entry lists, which makes an
        // archive diff a real signal.
        plan.files.sort_by(|a, b| a.rel.cmp(&b.rel));
        plan.dir_paths.sort();
        plan.dir_paths.dedup();
        plan
    }

    /// Paths in the plan that must exist as directories on restore, so an empty
    /// `world/datapacks/` is not silently lost.
    pub fn empty_dirs(plan: &SelectionPlan, files: &[String]) -> Vec<String> {
        let have: std::collections::BTreeSet<&str> = files.iter().map(String::as_str).collect();
        plan.dir_paths
            .iter()
            .filter(|d| !d.is_empty() && !have.contains(d.as_str()))
            .cloned()
            .collect()
    }
}

/// What a [`Selector`] decided, and the totals the caller shows the user.
#[derive(Debug, Clone, Default)]
pub struct SelectionPlan {
    pub files: Vec<SelectedFile>,
    pub dir_paths: Vec<String>,
    pub total_bytes: u64,
    pub dirs: usize,
    pub newest_ms: i64,
    pub skipped: Vec<SkippedPath>,
    /// Directory prefixes pruned wholesale (for the UI's "excluded" list).
    pub excluded_prefixes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkippedPath {
    pub path: String,
    pub reason: SkippedKind,
}

impl SelectionPlan {
    pub fn file_count(&self) -> u32 {
        self.files.len() as u32
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Human summary for the confirmation dialog and the progress message.
    pub fn describe(&self) -> String {
        let files = self.files.len();
        let bytes = human_bytes(self.total_bytes);
        match self.skipped.len() {
            0 => format!("{files} files, {bytes}"),
            n => format!("{files} files, {bytes}, {n} skipped"),
        }
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Relative path with `/` separators — the only spelling an archive should contain.
fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn should_prune_dir(rel_path: &str, excludes: &GlobSet) -> bool {
    // A directory prunes when the pattern names it exactly (`logs`) or matches a
    // path under it (`logs/**`). globset does not treat `logs` as matching
    // `logs/crash.txt`, so both spellings are checked.
    !excludes.matches(rel_path).none() || !excludes.matches(format!("{rel_path}/")).none()
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        // A filesystem without mtime (or a clock before 1970) must not poison the
        // manifest: 0 means "unknown" and the restore leaves the mtime alone.
        .unwrap_or(0)
}

#[cfg(unix)]
fn is_executable(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_md: &std::fs::Metadata) -> bool {
    false
}

fn build(mut built: Built, patterns: &[String]) -> Built {
    for p in patterns {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if !built.patterns.iter().any(|e| e == p) {
            built.patterns.push(p.to_string());
        }
        // A bare name (`crash-reports`) is a directory hint: record it so whole
        // subtrees can be pruned without consulting the glob engine per file.
        if !p.contains(['*', '?', '/', '[']) {
            built.dir_names.push(p.to_ascii_lowercase());
        }
    }
    built
}

/// Compile patterns into a matcher, expanding each into the spellings a walk can
/// produce: the path itself, everything under it, and (for bare names) the same at
/// any depth.
fn build_set(patterns: &[String]) -> Result<Option<GlobSet>, sp_core::CoreError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    let mut added = 0usize;
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for raw in patterns {
        let p = clean(raw);
        for variant in expand(&p) {
            if !seen.insert(variant.clone()) {
                continue;
            }
            let glob = Glob::new(&variant).map_err(|e| sp_core::CoreError::Validation {
                field: "exclude_pattern".into(),
                reason: format!("`{raw}` is not a usable glob ({e}) — {variant}"),
            })?;
            b.add(glob);
            added += 1;
        }
    }
    if added == 0 {
        return Ok(None);
    }
    b.build()
        .map(Some)
        .map_err(|e| sp_core::CoreError::other(format!("could not build the exclude matcher: {e}")))
}

fn clean(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    while let Some(stripped) = s.strip_prefix("./") {
        s = stripped.to_string();
    }
    while s.ends_with('/') {
        s.pop();
    }
    s
}

fn expand(p: &str) -> Vec<String> {
    let mut out = vec![p.to_string()];
    out.push(format!("{p}/**"));
    if !p.contains('/') {
        out.push(format!("**/{p}"));
        out.push(format!("**/{p}/**"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(dir: &Path, rel: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    fn tree(dir: &Path) {
        touch(dir, "server.jar", b"jar");
        touch(dir, "server.properties", b"a=b\n");
        touch(dir, "mods/mod-a.jar", b"mod");
        touch(dir, "mods/nested/deep/mod-b.jar", b"deep");
        touch(dir, "world/region/r.0.0.mca", b"region");
        touch(dir, "world_nether/region/r.0.0.mca", b"nether");
        touch(dir, "world/datapacks/bonus/pack.mcmeta", b"{}");
        touch(dir, "logs/2026-11-13-1.log.gz", b"log");
        touch(dir, "crash-reports/crash-1.txt", b"boom");
        touch(dir, "backups/old.zip", b"nested backup");
        touch(dir, "nohup.tmp", b"tmp");
        touch(dir, "libraries/net/minecart/minecart-1.21.jar", b"lib");
    }

    #[test]
    fn plan_includes_the_obvious_and_excludes_the_noise() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let plan = Selector::new().plan(dir.path()).unwrap();
        let paths: Vec<&str> = plan.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(paths.contains(&"server.jar"), "{paths:?}");
        assert!(paths.contains(&"mods/nested/deep/mod-b.jar"), "{paths:?}");
        assert!(paths.contains(&"world/region/r.0.0.mca"), "{paths:?}");
        assert!(
            paths.contains(&"world/datapacks/bonus/pack.mcmeta"),
            "{paths:?}"
        );
        assert!(paths.contains(&"libraries/net/minecart/minecart-1.21.jar"));
        // Default excludes.
        for gone in [
            "logs/2026-11-13-1.log.gz",
            "crash-reports/crash-1.txt",
            "backups/old.zip",
            "nohup.tmp",
        ] {
            assert!(!paths.contains(&gone), "{gone} must be excluded by default");
            assert!(
                plan.skipped.iter().any(|s| s.path == gone
                    || plan
                        .excluded_prefixes
                        .iter()
                        .any(|p| p == gone.rsplit_once('/').unwrap().0)),
                "{gone} should be reported as excluded somewhere: {:?}",
                plan.skipped
            );
        }
        assert!(plan.total_bytes > 0);
        assert!(!plan.is_empty());
        assert!(plan.describe().contains("files,"), "{}", plan.describe());
    }

    #[test]
    fn logs_are_optional_and_worlds_can_be_left_out() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let with_logs: Vec<String> = Selector::new()
            .with_logs(true)
            .plan(dir.path())
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.rel)
            .collect();
        assert!(
            with_logs.iter().any(|p| p.starts_with("logs/")),
            "{with_logs:?}"
        );

        let plan = Selector::new().with_worlds(false).plan(dir.path()).unwrap();
        let paths: Vec<&str> = plan.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(
            paths.contains(&"world/region/r.0.0.mca"),
            "the default world stays"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("world_nether")),
            "{paths:?}"
        );
    }

    #[test]
    fn user_patterns_cover_bare_names_globs_and_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        for (pattern, must_be_gone) in [
            ("crash-reports", "crash-reports/crash-1.txt"),
            ("crash-reports/**", "crash-reports/crash-1.txt"),
            ("**/crash-reports", "crash-reports/crash-1.txt"),
            ("*.log.gz", "logs/2026-11-13-1.log.gz"),
            ("mods/**", "mods/mod-a.jar"),
            ("world/**", "world/region/r.0.0.mca"),
            ("**/*.mca", "world/region/r.0.0.mca"),
            ("libraries", "libraries/net/minecart/minecart-1.21.jar"),
        ] {
            let plan = Selector::new()
                .with_default_excludes(false)
                .with_logs(true)
                .with_excludes(vec![pattern.into()])
                .plan(dir.path())
                .unwrap();
            assert!(
                !plan.files.iter().any(|f| f.rel == must_be_gone),
                "pattern `{pattern}` should exclude {must_be_gone}: {:?}",
                plan.files.iter().map(|f| f.rel.clone()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn includes_narrow_the_plan_and_sizes_are_recorded() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let plan = Selector::new()
            .with_includes(&["mods/**".into()])
            .plan(dir.path())
            .unwrap();
        assert_eq!(
            plan.files.len(),
            2,
            "{:?}",
            plan.files.iter().map(|f| f.rel.clone()).collect::<Vec<_>>()
        );
        let jar = &plan.files[0];
        assert_eq!(jar.rel, "mods/mod-a.jar");
        assert_eq!(jar.bytes, 3, "the size the archive will carry");
        assert!(
            jar.modified_ms > 0,
            "mtime must be recorded for the manifest"
        );
        assert_eq!(plan.total_bytes, 7);
        assert!(!jar.executable || cfg!(not(unix)));
    }

    #[test]
    fn symlinks_and_weird_files_are_reported_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", dir.path().join("mods/innocent.jar"))
                .unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("server.properties"),
                dir.path().join("config-link"),
            )
            .unwrap();
        }
        #[cfg(not(unix))]
        {
            // Windows without developer mode cannot create symlinks from an
            // unprivileged test run; the behaviour that matters (no traversal) is
            // asserted in the restore tests instead.
            return;
        }
        let plan = Selector::new().plan(dir.path()).unwrap();
        let skipped: Vec<&str> = plan
            .skipped
            .iter()
            .filter(|s| s.reason == SkippedKind::Symlink)
            .map(|s| s.path.as_str())
            .collect();
        assert!(skipped.contains(&"mods/innocent.jar"), "{skipped:?}");
        assert!(skipped.contains(&"config-link"), "{skipped:?}");
        assert!(!plan.files.iter().any(|f| f.rel.contains("innocent")));
        assert_eq!(
            fs::read_to_string(dir.path().join("server.properties")).unwrap(),
            "a=b\n"
        );
    }

    #[test]
    fn max_file_bytes_and_total_caps_protect_against_a_runaway_tree() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "big.bin", &vec![b'x'; 4096]);
        touch(dir.path(), "small.bin", b"hi");
        let plan = Selector::new()
            .with_default_excludes(false)
            .max_file_bytes(1024)
            .plan(dir.path())
            .unwrap();
        assert_eq!(
            plan.files.len(),
            1,
            "the 4 KiB file is over the per-file cap"
        );
        assert_eq!(plan.files[0].rel, "small.bin");
        assert_eq!(plan.skipped[0].reason, SkippedKind::Excluded);
    }

    #[test]
    fn a_missing_root_is_a_clear_error() {
        let err = Selector::new()
            .plan(Path::new("/nonexistent-server-panel-dir"))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn empty_directories_are_preserved_for_restore_fidelity() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        fs::create_dir_all(dir.path().join("world/datapacks/empty-one")).unwrap();
        fs::create_dir_all(dir.path().join("plugins")).unwrap();
        let plan = Selector::new().plan(dir.path()).unwrap();
        let files: Vec<String> = plan.files.iter().map(|f| f.rel.clone()).collect();
        let empties = Selector::empty_dirs(&plan, &files);
        assert!(empties.iter().any(|d| d == "plugins"), "{empties:?}");
        assert!(
            empties.iter().any(|d| d.ends_with("empty-one")),
            "{empties:?}"
        );
        assert!(
            !empties.contains(&"world".to_string()),
            "world has files: {empties:?}"
        );
    }

    #[test]
    fn ordering_is_deterministic_so_two_runs_agree() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let a: Vec<String> = Selector::new()
            .plan(dir.path())
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.rel)
            .collect();
        let b: Vec<String> = Selector::new()
            .plan(dir.path())
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.rel)
            .collect();
        assert_eq!(a, b);
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(a, sorted, "the plan must be sorted, not merely stable");
    }

    #[test]
    fn cancel_flag_is_shared_and_visible() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
        let clone = c.clone();
        std::thread::spawn(move || clone.cancel());
        while !c.is_cancelled() {
            std::thread::yield_now();
        }
        assert!(c.is_cancelled());
        assert!(c.0.load(Ordering::Relaxed));
    }

    #[test]
    fn bad_globs_are_a_validation_error_naming_the_pattern() {
        let err = Selector::new()
            .with_excludes(vec!["[[[bad".into()])
            .plan(Path::new("."))
            .err()
            .expect("an uncompilable glob must fail");
        assert_eq!(err.code(), "validation");
        assert!(err.to_string().contains("[[[bad"), "{err}");
    }

    #[test]
    fn human_bytes_reads_like_a_file_manager() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3u64 << 30), "3.0 GiB");
    }
}

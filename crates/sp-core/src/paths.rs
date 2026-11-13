//! Data-directory layout and the file-sandbox that guards it.
//!
//! Two responsibilities:
//! * [`paths`] — where things live per-OS, always *outside* the install
//!   directory so an update or uninstall can never touch server data.
//! * [`Sandbox`] — the single choke point every file operation goes through.
//!   It rejects `..`, absolute paths, NUL bytes, and symlink escapes.

use crate::error::CoreError;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// directories
// ---------------------------------------------------------------------------
//
// Deviation note: the `directories` crate is intentionally *not* used here. The
// rules below are ~25 lines, auditable, and identical to what that crate does,
// which matters because getting these wrong is how a panel ends up deleting a
// user's worlds. `SERVERPANEL_HOME` overrides everything (portable installs,
// CI, and "my data lives on D:" setups).

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

fn home_dir() -> Option<PathBuf> {
    env_path(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .or_else(|| std::env::var_os("HOMEDRIVE").zip(std::env::var_os("HOMEPATH")).map(|(d, p)| PathBuf::from(d).join(p)))
}

/// Windows: `%{FOLDERID_RoamingAppData}\ServerPanel`
/// macOS:   `~/Library/Application Support/ServerPanel`
/// Linux:   `$XDG_DATA_HOME/serverpanel` → `~/.local/share/serverpanel`
pub fn data_root() -> PathBuf {
    if let Some(p) = env_path("SERVERPANEL_HOME") {
        return p;
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = env_path("APPDATA") {
            return appdata.join("ServerPanel");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home.join("Library").join("Application Support").join("ServerPanel");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = env_path("XDG_DATA_HOME") {
            return xdg.join("serverpanel");
        }
        if let Some(home) = home_dir() {
            return home.join(".local").join("share").join("serverpanel");
        }
    }
    // Defensive last resort: a service account with no profile must still boot.
    std::env::temp_dir().join("serverpanel")
}

/// `config.toml` lives beside the data so a reset never loses credentials:
/// `SERVERPANEL_CONFIG_DIR` → `SERVERPANEL_HOME` → platform config dir.
pub fn config_dir() -> PathBuf {
    if let Some(p) = env_path("SERVERPANEL_CONFIG_DIR") {
        return p;
    }
    if let Some(p) = env_path("SERVERPANEL_HOME") {
        return p.clone();
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = env_path("APPDATA") {
            return appdata.join("ServerPanel");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home.join("Library").join("Preferences").join("ServerPanel");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = env_path("XDG_CONFIG_HOME") {
            return xdg.join("serverpanel");
        }
        if let Some(home) = home_dir() {
            return home.join(".config").join("serverpanel");
        }
    }
    data_root()
}

fn sub(name: &str) -> PathBuf {
    data_root().join(name)
}

/// One directory per server, named `s{server_id}-{slug}`.
pub fn servers_dir() -> PathBuf {
    sub("servers")
}
pub fn server_dir(id: i64, name: &str) -> PathBuf {
    servers_dir().join(format!("s{id}-{}", sanitize_slug(name)))
}
/// Panel-managed JREs. Isolated so a system Java update can never break a server.
pub fn java_dir() -> PathBuf {
    sub("java")
}
/// Downloads in flight, resumable `.part` files, and the HTTP cache.
pub fn cache_dir() -> PathBuf {
    sub("cache")
}
pub fn backups_dir() -> PathBuf {
    sub("backups")
}
pub fn logs_dir() -> PathBuf {
    sub("logs")
}
pub fn updates_dir() -> PathBuf {
    sub("updates")
}
pub fn staging_dir() -> PathBuf {
    updates_dir().join("staged")
}
pub fn exports_dir() -> PathBuf {
    sub("exports")
}
/// Tunnel binaries etc.
pub fn tools_dir() -> PathBuf {
    sub("tools")
}

/// All directories the panel expects, in creation order.
pub fn required_dirs() -> Vec<PathBuf> {
    vec![
        data_root(),
        servers_dir(),
        java_dir(),
        cache_dir(),
        backups_dir(),
        logs_dir(),
        updates_dir(),
        staging_dir(),
        exports_dir(),
        tools_dir(),
    ]
}

pub fn ensure_dirs() -> Result<(), CoreError> {
    for d in required_dirs() {
        std::fs::create_dir_all(&d)
            .map_err(|e| CoreError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", d.display()))))?;
    }
    Ok(())
}

/// `Survival Spawn!` → `survival-spawn` (used for directory names only; the DB
/// keeps the original name for display).
pub fn sanitize_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if c == '_' || c == '-' || c == ' ' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "server".to_string()
    } else {
        trimmed.chars().take(40).collect()
    }
}

// ---------------------------------------------------------------------------
// sandbox
// ---------------------------------------------------------------------------

/// A canonicalised root that every relative path must resolve inside of.
///
/// Rules, in order:
/// 1. reject empty/absolute/Windows-verbatim roots and any `..` component
/// 2. reject NUL bytes
/// 3. reject backslash escapes (`..\\..\\windows`) on every platform
/// 4. join, then re-canonicalise *and* re-check containment — this is what stops
///    a symlink inside the tree pointing at `/etc`
#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// `root` need not exist yet (a fresh server directory). We canonicalise the
    /// nearest existing ancestor so the check still works pre-creation.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, CoreError> {
        let root = canonicalize_existing(root.as_ref())?;
        if !root.is_absolute() {
            return Err(CoreError::PathEscape(format!(
                "sandbox root must be absolute, got {}",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-supplied relative path to an absolute one inside the sandbox.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, CoreError> {
        let cleaned = Self::normalize_rel(rel)?;
        let candidate = self.root.join(&cleaned);
        // Containment check on the *requested* path first (cheap, no syscalls).
        Self::assert_within(&self.root, &candidate)?;
        // Then re-check after canonicalisation to defeat symlink escapes. If the
        // target does not exist yet (new file), canonicalise the parent.
        let checked = match canonicalize_existing(&candidate) {
            Ok(p) => p,
            Err(e) if candidate.exists() => return Err(e),
            Err(_) => {
                let parent = candidate
                    .parent()
                    .map(canonicalize_existing)
                    .transpose()?
                    .unwrap_or_else(|| self.root.clone());
                let file = candidate.file_name().ok_or_else(|| {
                    CoreError::PathEscape("missing file name".into())
                })?;
                let rebuilt = parent.join(file);
                Self::assert_within(&self.root, &rebuilt)?;
                return Ok(rebuilt);
            }
        };
        if checked.starts_with(&self.root) {
            Ok(checked)
        } else {
            Err(CoreError::PathEscape(rel.to_string()))
        }
    }

    /// Like [`Sandbox::resolve`] but for writing: also refuses to follow a
    /// symlink that already sits at the destination.
    pub fn resolve_for_write(&self, rel: &str) -> Result<PathBuf, CoreError> {
        let path = self.resolve(rel)?;
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if md.file_type().is_symlink() {
                return Err(CoreError::PathEscape(format!(
                    "{} is a symlink; refusing to write through it",
                    path.display()
                )));
            }
        }
        Ok(path)
    }

    /// Turn an absolute path back into a `/`-separated relative one for the UI.
    pub fn relativize(&self, abs: &Path) -> Result<String, CoreError> {
        let abs = canonicalize_existing(abs)?;
        let rest = abs
            .strip_prefix(&self.root)
            .map_err(|_| CoreError::PathEscape(abs.display().to_string()))?;
        Ok(if rest.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rest.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
    }

    fn normalize_rel(rel: &str) -> Result<PathBuf, CoreError> {
        if rel.contains('\0') {
            return Err(CoreError::PathEscape("NUL byte in path".into()));
        }
        let unified = rel.replace('\\', "/");
        let trimmed = unified.trim_start_matches('/');
        if trimmed.is_empty() || trimmed == "." {
            return Ok(PathBuf::new());
        }
        let mut out = PathBuf::new();
        for part in trimmed.split('/') {
            match part {
                "" | "." => continue,
                ".." => return Err(CoreError::PathEscape(rel.to_string())),
                seg => {
                    // Windows drive/UNC smuggling: `C:`, `C:|`, `\\server\share`
                    if seg.contains(':') {
                        return Err(CoreError::PathEscape(format!("drive in path: {rel}")));
                    }
                    out.push(seg);
                }
            }
        }
        // Re-validate with serde-independent component scan.
        for c in out.components() {
            if !matches!(c, Component::Normal(_) | Component::CurDir) {
                return Err(CoreError::PathEscape(rel.to_string()));
            }
        }
        Ok(out)
    }

    fn assert_within(root: &Path, candidate: &Path) -> Result<(), CoreError> {
        if candidate.starts_with(root) {
            Ok(())
        } else {
            Err(CoreError::PathEscape(format!(
                "{} is outside {}",
                candidate.display(),
                root.display()
            )))
        }
    }
}

/// `canonicalize` that walks up to the nearest existing ancestor when the leaf
/// does not exist, because `std::fs::canonicalize` requires existence.
pub fn canonicalize_existing(path: &Path) -> Result<PathBuf, CoreError> {
    let mut cursor = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = cursor.canonicalize() {
            let mut out = c;
            for seg in tail.into_iter().rev() {
                out.push(seg);
            }
            return Ok(out);
        }
        let file_name = match cursor.file_name() {
            Some(f) => f.to_os_string(),
            None => {
                return Err(CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("cannot canonicalize {}", path.display()),
                )))
            }
        };
        tail.push(file_name);
        let parent = cursor
            .parent()
            .ok_or_else(|| CoreError::not_found(path.display().to_string()))?
            .to_path_buf();
        cursor = parent;
    }
}

/// Reject names that Windows or POSIX treat as special (used for server/file names).
pub fn is_safe_file_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() || n.len() > 255 || n == "." || n == ".." {
        return false;
    }
    if n.chars().any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || (c as u32) < 0x20) {
        return false;
    }
    if n.ends_with('.') || n.ends_with(' ') {
        return false;
    }
    // Windows reserved device names, case-insensitive, with or without extension.
    let stem = n.split('.').next().unwrap_or("").to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7"
            | "COM8" | "COM9" | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8"
            | "LPT9"
    )
}

/// Capabilities-only check: does the target look like a text file we can edit?
pub fn is_text_file(path: &Path) -> bool {
    const TEXT_EXT: &[&str] = &[
        "txt", "json", "yaml", "yml", "toml", "properties", "cfg", "conf", "ini", "md", "log",
        "mcfunction", "lang", "snbt", "nbt", "csv", "tsv", "js", "lua", "py", "xml", "html",
        "css", "mcleaky", "eula", "geo", "biome",
    ];
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => TEXT_EXT.contains(&e.to_ascii_lowercase().as_str()),
        None => matches!(
            path.file_name().and_then(|f| f.to_str()),
            Some("eula.txt") | Some("Dockerfile") | Some(".gitignore") | Some("permissions.yml")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (tempfile::TempDir, Sandbox) {
        let dir = tempfile::tempdir().unwrap();
        // macOS/Linux tmpdirs are symlinks (/var → /private/var); canonicalize so
        // the expectations below match.
        let root = dir.path().canonicalize().unwrap();
        let sb = Sandbox::new(&root).unwrap();
        (dir, sb)
    }

    #[test]
    fn plain_paths_resolve() {
        let (_t, sb) = sandbox();
        let p = sb.resolve("server.properties").unwrap();
        assert_eq!(p, sb.root().join("server.properties"));
        assert_eq!(p, sb.resolve("./server.properties").unwrap());
        assert_eq!(p, sb.resolve("/server.properties").unwrap());
        assert_eq!(sb.root().to_path_buf(), sb.resolve("").unwrap());
        assert_eq!(sb.root().to_path_buf(), sb.resolve(".").unwrap());
    }

    #[test]
    fn nested_paths_work_and_normalize() {
        let (_t, sb) = sandbox();
        let p = sb.resolve("world/region////r.0.0.mca").unwrap();
        assert!(p.starts_with(sb.root()));
        assert!(p.to_string_lossy().ends_with("world/region/r.0.0.mca"));
        // A trailing slash / dot-segments inside the path are fine.
        assert_eq!(p, sb.resolve("world/region/./r.0.0.mca").unwrap());
    }

    #[test]
    fn traversal_is_rejected() {
        let (_t, sb) = sandbox();
        for bad in [
            "..",
            "../evil",
            "a/../../b",
            "..\\..\\windows\\system32",
            "/../../etc/passwd",
            "world/../../../etc/passwd",
            "a/b/../../../c",
        ] {
            let err = sb.resolve(bad).unwrap_err();
            assert!(
                matches!(err, CoreError::PathEscape(_)),
                "expected PathEscape for {bad}, got {err:?}"
            );
        }
    }

    #[test]
    fn absolute_and_drive_paths_are_rejected_or_neutralised() {
        let (_t, sb) = sandbox();
        // `C:\Windows` must never escape, on any platform.
        let err = sb.resolve("C:\\Windows\\win.ini").unwrap_err();
        assert!(matches!(err, CoreError::PathEscape(_)), "{err:?}");
        assert!(sb.resolve("\\\\evil\\share\\x").is_ok(), "UNC prefix stripped to a normal name");
    }

    #[test]
    fn nul_bytes_rejected() {
        let (_t, sb) = sandbox();
        assert!(sb.resolve("bad\0name").is_err());
    }

    #[test]
    fn symlink_escape_is_blocked() {
        let (_t, sb) = sandbox();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let secret = outside.path().join("secret.txt");
            std::fs::write(&secret, b"top").unwrap();
            let link = sb.root().join("link.txt");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            // Reading *through* the link resolves outside the root → blocked.
            let err = sb.resolve("link.txt").unwrap_err();
            assert!(matches!(err, CoreError::PathEscape(_)), "{err:?}");
            // A symlinked directory, reached via a nested path, is also blocked.
            let dirlink = sb.root().join("dirlink");
            std::os::unix::fs::symlink(outside.path(), &dirlink).unwrap();
            assert!(sb.resolve("dirlink/secret.txt").is_err());
        }
    }

    #[test]
    fn write_guard_refuses_existing_symlink() {
        let (_t, sb) = sandbox();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("payload.jar");
            std::fs::write(&target, b"x").unwrap();
            let link = sb.root().join("mods").join("evil.jar");
            std::fs::create_dir_all(sb.root().join("mods")).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            // resolve() may or may not allow reading; resolve_for_write must not.
            let write_err = sb.resolve_for_write(sb.relativize(&link).unwrap_or_else(|_| "mods/evil.jar".into()));
            assert!(write_err.is_err(), "must refuse writing through a symlink");
        }
    }

    #[test]
    fn relativize_roundtrips() {
        let (_t, sb) = sandbox();
        let abs = sb.root().join("plugins").join("x.jar");
        std::fs::create_dir_all(sb.root().join("plugins")).unwrap();
        std::fs::write(&abs, b"y").unwrap();
        assert_eq!(sb.relativize(&abs).unwrap(), "plugins/x.jar");
        assert_eq!(sb.relativize(sb.root()).unwrap(), ".");
        assert!(sb.relativize(Path::new("/")).is_err());
    }

    #[test]
    fn missing_leaf_is_allowed_for_new_files() {
        let (_t, sb) = sandbox();
        let p = sb.resolve("plugins/new/file.yml").unwrap();
        assert_eq!(p, sb.root().join("plugins/new/file.yml"));
    }

    #[test]
    fn slug_sanitising() {
        assert_eq!(sanitize_slug("Survival Spawn!"), "survival-spawn");
        assert_eq!(sanitize_slug("  "), "server");
        assert_eq!(sanitize_slug("测试"), "server", "non-ascii drops out");
        assert_eq!(sanitize_slug("A".repeat(200)).len(), 40);
        assert_eq!(sanitize_slug("my__server--x"), "my-server-x");
    }

    #[test]
    fn windows_reserved_names_rejected() {
        for n in ["nul", "CON.txt", "com1", "LPT9", "a.", "a ", "", ".", "..", "a/b", "a\\b"] {
            assert!(!is_safe_file_name(n), "{n} must be rejected");
        }
        for n in ["server.jar", "plugins", "a-b_c.jar", "eula.txt", "世界"] {
            assert!(is_safe_file_name(n), "{n} must be allowed");
        }
    }

    #[test]
    fn text_detection() {
        assert!(is_text_file(Path::new("a/spigot.yml")));
        assert!(is_text_file(Path::new("server.properties")));
        assert!(is_text_file(Path::new("data/world.dat")));
        assert!(!is_text_file(Path::new("mods/sodium.jar")));
    }

    #[test]
    fn canonicalize_existing_walks_up() {
        let t = tempfile::tempdir().unwrap();
        let deep = t.path().join("a/b/c.txt");
        let c = canonicalize_existing(&deep).unwrap();
        assert!(c.ends_with("a/b/c.txt"), "{c:?}");
    }
}

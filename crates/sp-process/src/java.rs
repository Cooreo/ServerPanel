//! Java runtime discovery and installation.
//!
//! Strategy:
//! 1. **Detect** JREs already on the machine (`PATH`, `JAVA_HOME`, the standard
//!    install directories for each OS) by running `java -version` — that is the
//!    only trustworthy way to learn the *major* version, and it is cheap (one exec
//!    per candidate, done once at startup).
//! 2. **Install** what is missing into `<data>/java/<major>` from Eclipse Temurin
//!    (Adoptium), verified against the SHA-256 the API publishes alongside the
//!    archive. Panel-managed runtimes are isolated, so a system Java update can
//!    never break a working server.
//! 3. **Select** per server: an explicit pin wins, then a managed runtime for the
//!    required major, then any system runtime of that major, then the newest
//!    managed runtime above it. Never *below* it — that is the `-classpath`
//!    `UnsupportedClassVersionError` people call "corrupted install" about.
//!
//! Upstream endpoints used (documented, not invented):
//! * `GET https://api.adoptium.net/v3/assets/feature_releases/{major}` with
//!   `release_type=ga&image_type=jre&vendor=eclipse&os=…&architecture=…`
//! * Mojang's per-version manifest provides the required major (`javaVersion`),
//!   supplied by the caller so this module stays free of Minecraft knowledge.

use serde::Deserialize;
use sp_core::progress::ProgressReporter;
use std::time::Duration;
use sp_core::model::{now_ms, JavaRuntime, JavaSource};
use sp_core::verify::Hashes;
use sp_core::CoreError;
use sp_net::download::{self, DownloadJob};
use sp_net::HttpClient;
use std::path::{Path, PathBuf};

/// Where a discovered JRE came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaCandidate {
    pub path: PathBuf,
    pub home: PathBuf,
    pub major: u16,
    pub version: String,
    pub vendor: String,
    pub source: JavaSource,
}

#[derive(Debug, Clone)]
pub struct JavaManager {
    root: PathBuf,
    client: HttpClient,
    auto_install: bool,
}

impl JavaManager {
    /// `root` is normally `<data>/java`.
    pub fn new(root: impl Into<PathBuf>, client: HttpClient, auto_install: bool) -> Self {
        Self {
            root: root.into(),
            client,
            auto_install,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Managed install directory for a major, e.g. `<data>/java/21`.
    pub fn managed_path(&self, major: u16) -> PathBuf {
        self.root.join(major.to_string())
    }

    /// The `java` executable inside a JAVA_HOME.
    pub fn executable_for(home: &Path) -> PathBuf {
        if cfg!(windows) {
            home.join("bin").join("java.exe")
        } else {
            home.join("bin").join("java")
        }
    }

    /// Scan `PATH`, `JAVA_HOME`, and the platform's usual install locations.
    ///
    /// `SERVERPANEL_JAVA_PATHS` (path-list) overrides the candidate set entirely —
    /// used by tests and by people with non-standard installs.
    pub async fn detect(&self) -> Vec<JavaCandidate> {
        let mut homes: Vec<(PathBuf, JavaSource)> = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();

        let mut push = |home: PathBuf, source: JavaSource| {
            let exe = Self::executable_for(&home);
            if exe.exists() && !seen.contains(&home) {
                seen.push(home.clone());
                homes.push((home, source));
            }
        };

        if let Ok(extra) = std::env::var("SERVERPANEL_JAVA_PATHS") {
            for p in std::env::split_paths(&extra) {
                if p.exists() {
                    push(p, JavaSource::Manual);
                }
            }
        }

        // Panel-managed copies first — they win ties.
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    push(e.path(), JavaSource::Managed);
                }
            }
        }

        if let Ok(jh) = std::env::var("JAVA_HOME") {
            let p = PathBuf::from(jh);
            push(p.clone(), JavaSource::System);
            // Some distros set JAVA_HOME to the *jre* subdir.
            if let Some(parent) = p.parent() {
                push(parent.to_path_buf(), JavaSource::System);
            }
        }

        for dir in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
            // `…/bin/java` implies a home of `…/bin/..`
            let exe = if cfg!(windows) { dir.join("java.exe") } else { dir.join("java") };
            if exe.exists() {
                if let Some(home) = exe.parent().and_then(|p| p.parent()) {
                    push(home.to_path_buf(), JavaSource::System);
                }
            }
        }

        for dir in Self::platform_search_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                if !path.is_dir() {
                    continue;
                }
                let name = path.file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                if !looks_like_java_dir(&name) {
                    continue;
                }
                push(path.clone(), JavaSource::System);
                // macOS bundles nest the home inside Contents/Home.
                let nested = path.join("Contents").join("Home");
                if nested.is_dir() {
                    push(nested, JavaSource::System);
                }
            }
        }

        // Probe in parallel: ~100 ms each, and startup must not serialise a dozen
        // `java -version` invocations.
        let probes = homes
            .into_iter()
            .map(|(home, source)| async move {
                let exe = Self::executable_for(&home);
                probe(&exe, home, source).await
            });
        let mut out: Vec<JavaCandidate> = futures::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .collect();
        out.sort_by(|a, b| {
            b.major
                .cmp(&a.major)
                .then_with(|| source_rank(&a.source).cmp(&source_rank(&b.source)))
                .then_with(|| a.home.cmp(&b.home))
        });
        out.dedup_by(|a, b| a.home == b.home);
        out
    }

    fn platform_search_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        #[cfg(windows)]
        {
            for env in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
                if let Ok(v) = std::env::var(env) {
                    let base = PathBuf::from(v);
                    for vendor in [
                        "Eclipse Adoptium",
                        "Java",
                        "Microsoft\\jdk-21.0.5.11-hotspot",
                        "BellSoft",
                        "Zulu",
                        "Amazon Corretto",
                        "Temurin",
                    ] {
                        dirs.push(base.join(vendor));
                    }
                    dirs.push(base.join("Programs").join("Eclipse Adoptium"));
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            dirs.push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
            dirs.push(PathBuf::from("/System/Library/Java/JavaVirtualMachines"));
            dirs.push(PathBuf::from("/opt/homebrew/opt"));
            dirs.push(PathBuf::from("/usr/local/opt"));
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join("Library/Java/JavaVirtualMachines"));
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            for d in [
                "/usr/lib/jvm",
                "/usr/java",
                "/opt/java",
                "/usr/local/opt",
                "/snap",
                "/opt/homebrew/opt",
            ] {
                dirs.push(PathBuf::from(d));
            }
        }
        dirs
    }

    /// Download + extract a Temurin JRE for `major` into the managed dir.
    pub async fn install(&self, major: u16, progress: &ProgressReporter) -> Result<JavaRuntime, CoreError> {
        if !self.auto_install {
            return Err(CoreError::JavaMissing(
                "auto-install disabled".into(),
                major,
            ));
        }
        let target = self.managed_path(major);
        if Self::executable_for(&target).exists() {
            progress.message(format!("Java {major} is already installed"));
            if let Some(c) = probe(&Self::executable_for(&target), target.clone(), JavaSource::Managed).await {
                return Ok(into_runtime(&c));
            }
        }
        let info = self.resolve_release(major).await?;
        let archive = self
            .root
            .join(format!(".tmp-{major}-{}", file_name(&info.package.link)));
        std::fs::create_dir_all(&self.root)?;
        progress.set_phase(sp_core::model::ProgressPhase::JavaInstall);
        progress.message(format!("Downloading Temurin {major} ({})", info.version.semver));
        let expected = Hashes {
            sha1: None,
            sha256: Some(info.package.checksum.clone()),
            sha512: None,
            bytes: 0,
        };
        let job = DownloadJob::new(&info.package.link, &archive, format!("Temurin {major}"))
            .with_hashes(expected)
            .size_hint(info.package.size);
        let downloaded = download::download(&self.client, &job, Some(progress)).await?;

        progress.message("Extracting");
        let staging = self.root.join(format!(".staging-{major}"));
        let _ = std::fs::remove_dir_all(&staging);
        extract_java_archive(&archive, &staging, downloaded.hashes.bytes)?;
        // The archive has a single top-level dir (`jdk-21.0.5+11`).
        let inner_root = single_child_dir(&staging).unwrap_or_else(|| staging.clone());
        if target.exists() {
            std::fs::remove_dir_all(&target).ok();
        }
        std::fs::create_dir_all(target.parent().unwrap_or(&self.root))?;
        rename_dir(&inner_root, &target)?;
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&archive);

        let exe = Self::executable_for(&target);
        if !exe.exists() {
            return Err(CoreError::Process(format!(
                "extracted Java {major} but {} is missing",
                exe.display()
            )));
        }
        let candidate = probe(&exe, target.clone(), JavaSource::Managed)
            .await
            .ok_or_else(|| CoreError::Process(format!("extracted Java {major} does not run")))?;
        progress.complete(format!("Java {major} installed"));
        Ok(into_runtime(&candidate))
    }

    async fn resolve_release(&self, major: u16) -> Result<AdoptiumRelease, CoreError> {
        let url = format!(
            "https://api.adoptium.net/v3/assets/feature_releases/{major}?release_type=ga&image_type=jre&vendor=eclipse&os={}&architecture={}&page=0&page_size=5&sort_method=DATE&sort_order=DESC",
            adoptium_os(),
            adoptium_arch()
        );
        let releases: Vec<AdoptiumRelease> = self.client.get_json(&url).await?;
        releases
            .into_iter()
            .filter(|r| r.binaries.iter().any(|b| b.package.link.ends_with(".tar.gz") || b.package.link.ends_with(".zip")))
            .find(|r| r.binaries.iter().any(|b| b.image_type.as_deref() == Some("jre")))
            .or_else(|| releases.into_iter().next())
            .ok_or_else(|| CoreError::not_found(format!("no Temurin JRE {major} for {}", adoptium_os())))
    }

    /// Choose the runtime for a server. `required` comes from Mojang's manifest
    /// when available, else from [`sp_core::required_java_major_for_mc`].
    pub fn select(candidates: &[JavaCandidate], required: u16, pinned: Option<u16>) -> Option<JavaCandidate> {
        let want = pinned.unwrap_or(required);
        let matches_major: Vec<&JavaCandidate> = candidates.iter().filter(|c| c.major == want).collect();
        if let Some(c) = matches_major
            .iter()
            .max_by_key(|c| (source_rank(&c.source), version_key(&c.version)))
        {
            return Some((*c).clone());
        }
        // Older than required is never acceptable; newer usually is.
        candidates
            .iter()
            .filter(|c| c.major > want)
            .min_by_key(|c| (c.major, source_rank(&c.source)))
            .cloned()
    }

    /// Convenience for the wizard: pick, and install if the pick is missing.
    pub async fn ensure(
        &self,
        mc_version: &str,
        manifest_major: Option<u16>,
        pinned: Option<u16>,
        progress: &ProgressReporter,
    ) -> Result<JavaRuntime, CoreError> {
        let required = manifest_major.unwrap_or_else(|| sp_core::model::required_java_major_for_mc(mc_version));
        let candidates = self.detect().await;
        if let Some(c) = Self::select(&candidates, required, pinned) {
            return Ok(into_runtime(&c));
        }
        let managed = Self::executable_for(&self.managed_path(required));
        if managed.exists() {
            // detect() may have raced an install; probe directly.
            if let Some(c) = probe(&managed, self.managed_path(required), JavaSource::Managed).await {
                return Ok(into_runtime(&c));
            }
        }
        if !self.auto_install {
            return Err(CoreError::JavaMissing(mc_version.to_string(), required));
        }
        tracing::info!(major = required, mc = mc_version, "installing a managed JRE");
        self.install(required, progress).await
    }
}

/// `java -version` is the ground truth. It writes to **stderr**, so we read both.
async fn probe(exe: &Path, home: PathBuf, source: JavaSource) -> Option<JavaCandidate> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(exe)
            .arg("-version")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let (major, version, vendor) = parse_java_version(&text)?;
    Some(JavaCandidate {
        path: exe.to_path_buf(),
        home,
        major,
        version,
        vendor,
        source,
    })
}

/// Parse `openjdk version "21.0.5" 2024-10-15 LTS` / `java version "1.8.0_381"`.
pub fn parse_java_version(text: &str) -> Option<(u16, String, String)> {
    let line = text
        .lines()
        .find(|l| l.contains("version"))
        .or_else(|| text.lines().next())?;
    let quoted = line
        .split('"')
        .nth(1)
        .or_else(|| line.split_whitespace().find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit())))?;
    let version = quoted.to_string();
    let major = major_from_version(quoted)?;
    let vendor = if line.to_ascii_lowercase().contains("openjdk") {
        "OpenJDK".to_string()
    } else {
        "Oracle".to_string()
    };
    // Temurin/GraalVM/Zulu/Corretto identify themselves on line 2 or 3.
    let vendor = text
        .lines()
        .skip(1)
        .find_map(|l| {
            let l = l.trim();
            for marker in ["Temurin", "Adoptium", "GraalVM", "Corretto", "Zulu", "Microsoft", "Amazon", "Blue Oak", "Liberica"] {
                if l.to_ascii_lowercase().contains(&marker.to_ascii_lowercase()) {
                    return Some(marker.to_string());
                }
            }
            None
        })
        .unwrap_or(vendor);
    Some((major, version, vendor))
}

/// `"1.8.0_381"` → 8, `"21.0.5"` → 21, `"17"` → 17.
pub fn major_from_version(v: &str) -> Option<u16> {
    let v = v.trim().trim_matches('"');
    let first = v.split(|c: char| !c.is_ascii_digit()).next().unwrap_or(v);
    let head: u16 = first.parse().ok()?;
    if head == 1 {
        // The old `1.x` scheme: the minor is the major.
        let minor = v.split('.').nth(1).unwrap_or("0");
        return minor.parse::<u16>().ok();
    }
    Some(head)
}

fn source_rank(s: &JavaSource) -> u8 {
    match s {
        JavaSource::Managed => 0,
        JavaSource::Manual => 1,
        JavaSource::System => 2,
    }
}

fn version_key(v: &str) -> Vec<u32> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(4)
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

fn looks_like_java_dir(name: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "jdk", "jre", "java", "temurin", "corretto", "zulu", "openjdk", "liberica", "graal",
    ];
    MARKERS.iter().any(|m| name.contains(m))
}

fn file_name(url: &str) -> String {
    url.rsplit('/').next().unwrap_or("archive").to_string()
}

fn single_child_dir(root: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(root).ok()?;
    let first = entries.next()?.ok()?.path();
    if entries.next().is_some() || !first.is_dir() {
        return None;
    }
    Some(first)
}

fn rename_dir(from: &Path, to: &Path) -> Result<(), CoreError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Cross-device renames are possible (`%APPDATA%` on another volume); fall
    // back to a copy so an install never fails on that alone.
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::debug!(error = %e, "rename failed, copying instead");
            copy_tree(from, to)
        }
    }
}

pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), CoreError> {
    std::fs::create_dir_all(to)?;
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(|e| CoreError::other(e.to_string()))?;
        let rel = entry
            .path()
            .strip_prefix(from)
            .map_err(|_| CoreError::other("copy_tree: path escaped"))?;
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest)?;
        } else if entry.file_type().is_file() {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(entry.path(), &dest)?;
            // Preserve the executable bit or the extracted `bin/java` will not run.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if entry.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) {
                    let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
    }
    Ok(())
}

/// Extract a Temurin `.zip` (Windows) or `.tar.gz` (elsewhere) into `dest`.
pub fn extract_java_archive(archive: &Path, dest: &Path, _size_hint: u64) -> Result<(), CoreError> {
    std::fs::create_dir_all(dest)?;
    let name = archive
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".zip") {
        let file = std::fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| CoreError::other(format!("zip: {e}")))?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| CoreError::other(format!("zip entry: {e}")))?;
            let path = match entry.enclosed_name() {
                Some(p) => dest.join(p),
                // Zip-slip guard: `enclosed_name` rejects `..` and absolute paths.
                None => return Err(CoreError::PathEscape(format!("zip entry {}", entry.name()))),
            };
            if entry.is_dir() {
                std::fs::create_dir_all(&path)?;
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&path)?;
            std::io::copy(&mut entry, &mut out)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
        Ok(())
    } else {
        let file = std::fs::File::open(archive)?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut ar = tar::Archive::new(gz);
        ar.unpack(dest).map_err(|e| CoreError::other(format!("tar: {e}")))?;
        Ok(())
    }
}

fn into_runtime(c: &JavaCandidate) -> JavaRuntime {
    JavaRuntime {
        id: sp_core::ids::JavaId::new(0), // assigned by the repository
        major: c.major,
        version: c.version.clone(),
        vendor: c.vendor.clone(),
        path: c.path.clone(),
        source: c.source,
        arch: std::env::consts::ARCH.to_string(),
        installed_at_ms: if c.source == JavaSource::Managed { Some(now_ms()) } else { None },
        size_bytes: None,
    }
}

fn adoptium_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    }
}

fn adoptium_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        "x86" => "x32",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        other => {
            tracing::warn!(arch = other, "unknown architecture for Adoptium, asking for x64");
            "x64"
        }
    }
}

// -- Adoptium API shapes ----------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct AdoptiumRelease {
    #[serde(rename = "release_name")]
    pub release_name: String,
    pub version: AdoptiumVersion,
    pub binaries: Vec<AdoptiumBinary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdoptiumVersion {
    pub semver: String,
    pub major: u16,
    #[serde(default)]
    pub build: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdoptiumBinary {
    pub image_type: Option<String>,
    pub architecture: Option<String>,
    pub os: Option<String>,
    pub package: AdoptiumPackage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdoptiumPackage {
    pub link: String,
    pub checksum: String,
    #[serde(default)]
    pub size: Option<u64>,
}

impl AdoptiumRelease {
    /// The archive for this platform, with its download URL and checksum.
    pub fn package(&self) -> Result<AdoptiumPackage, CoreError> {
        self.binaries
            .iter()
            .find(|b| {
                b.os.as_deref() == Some(adoptium_os())
                    && b.architecture.as_deref() == Some(adoptium_arch())
            })
            .or_else(|| self.binaries.first())
            .map(|b| b.package.clone())
            .ok_or_else(|| CoreError::not_found("no binary for this platform in the Adoptium response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_all_eras() {
        assert_eq!(
            parse_java_version(r#"openjdk version "21.0.5" 2024-10-15 LTS
OpenJDK Runtime Environment Temurin-21.0.5+11 (build 21.0.5+11)"#)
                .map(|(m, v, vendor)| (m, v, vendor)),
            Some((21, "21.0.5".into(), "Temurin".into()))
        );
        assert_eq!(
            parse_java_version(r#"openjdk version "1.8.0_382"
OpenJDK Runtime Environment (Zulu 8.72.0.17-CA) (build 1.8.0_382-b05)"#)
                .map(|(m, _, v)| (m, v)),
            Some((8, "Zulu".into()))
        );
        assert_eq!(
            parse_java_version(r#"java version "17.0.9" 2023-10-17 LTS
Java(TM) SE Runtime Environment (build 17.0.9+11-LTS-201)"#)
                .map(|(m, _, _)| m),
            Some(17)
        );
        assert_eq!(
            parse_java_version(r#"openjdk version "17.0.12" 2024-07-16
Microsoft OpenJDK 17 - Microsoft build"#).map(|(_, _, v)| v),
            Some("Microsoft".into())
        );
        assert_eq!(parse_java_version("garbage"), None);
        assert_eq!(parse_java_version(""), None);
    }

    #[test]
    fn major_from_version_matrix() {
        for (v, want) in [
            ("1.8.0_381", 8u16),
            ("1.7.0_80", 7),
            ("9.0.4", 9),
            ("11", 11),
            ("17.0.9", 17),
            ("21.0.5", 21),
            ("22.0.1", 22),
            ("23", 23),
            ("24", 24),
        ] {
            assert_eq!(major_from_version(v), Some(want), "{v}");
        }
        assert_eq!(major_from_version("nope"), None);
    }

    #[test]
    fn selection_prefers_managed_then_upgrades_only() {
        let c = |major: u16, source: JavaSource, version: &str, home: &str| JavaCandidate {
            path: PathBuf::from(home).join("bin/java"),
            home: PathBuf::from(home),
            major,
            version: version.into(),
            vendor: "x".into(),
            source,
        };
        let pool = vec![
            c(17, JavaSource::System, "17.0.9", "/a"),
            c(21, JavaSource::Managed, "21.0.5", "/b"),
            c(21, JavaSource::System, "21.0.1", "/c"),
            c(8, JavaSource::System, "1.8.0", "/d"),
        ];
        // MC 1.21 wants 21 → the managed one, not the system one.
        assert_eq!(JavaManager::select(&pool, 21, None).unwrap().home, PathBuf::from("/b"));
        // MC 1.17 wants 16 → nothing at 16, so the lowest *newer* wins (17).
        assert_eq!(JavaManager::select(&pool, 16, None).unwrap().major, 17);
        // Pinned to 8 → the only 8.
        assert_eq!(JavaManager::select(&pool, 21, Some(8)).unwrap().home, PathBuf::from("/d"));
        // Same-major tie broken by version key.
        assert_eq!(
            JavaManager::select(&[c(21, JavaSource::System, "21.0.1", "/x"), c(21, JavaSource::System, "21.0.9", "/y")], 21, None)
                .unwrap()
                .home,
            PathBuf::from("/y")
        );
        // Nothing at all for a 1.16 server that needs 8 and has none.
        let no8 = vec![c(17, JavaSource::System, "17", "/z")];
        assert_eq!(JavaManager::select(&no8, 8, None).unwrap().major, 17, "newer is allowed");
        assert!(JavaManager::select(&[], 21, None).is_none());
    }

    #[test]
    fn dir_heuristics_and_paths() {
        assert!(looks_like_java_dir("jdk-21.0.5+11"));
        assert!(looks_like_java_dir("temurin-17"));
        assert!(looks_like_java_dir("zulu8.72"));
        assert!(!looks_like_java_dir("node_modules"));
        assert_eq!(file_name("https://x/y/OpenJDK.zip"), "OpenJDK.zip");
        let tmp = tempfile::tempdir().unwrap();
        let mgr = JavaManager::new(tmp.path().join("java"), HttpClient::new(), false);
        assert!(mgr.managed_path(21).ends_with("java/21"));
        assert!(JavaManager::executable_for(Path::new("/opt/jdk")).starts_with("/opt/jdk"));
        assert!(!JavaManager::executable_for(Path::new("/opt/jdk")).to_string_lossy().contains("//"));
    }

    #[test]
    fn auto_install_disabled_is_an_explicit_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = JavaManager::new(tmp.path().join("java"), HttpClient::new(), false);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let progress = ProgressReporter::silent("j", None, sp_core::model::ProgressPhase::JavaInstall);
        let err = rt.block_on(mgr.install(21, &progress)).unwrap_err();
        assert!(matches!(err, CoreError::JavaMissing(..)), "{err:?}");
        assert_eq!(err.code(), "java_missing");
        assert_eq!(err.status(), 422);
    }

    #[test]
    fn copy_tree_preserves_content() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("a/b")).unwrap();
        std::fs::write(src.path().join("a/b/c.txt"), b"hello").unwrap();
        std::fs::write(src.path().join("top.txt"), b"x").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let target = dst.path().join("out");
        copy_tree(src.path(), &target).unwrap();
        assert_eq!(std::fs::read(target.join("a/b/c.txt")).unwrap(), b"hello");
        assert!(target.join("top.txt").exists());
    }

    #[test]
    fn zip_extraction_rejects_traversal() {
        // Build a malicious zip in memory and assert it cannot write outside dest.
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("evil.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("../../escaped.txt", opts).unwrap();
            use std::io::Write;
            w.write_all(b"nope").unwrap();
            w.start_file("ok.txt", opts).unwrap();
            w.write_all(b"fine").unwrap();
            w.finish().unwrap();
        }
        let out = dir.path().join("out");
        // Either the zip crate rejects the entry outright, or it sanitises the name;
        // what must never happen is a file appearing outside `out`.
        match extract_java_archive(&zip_path, &out, 0) {
            Ok(()) => {}
            Err(CoreError::PathEscape(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        assert!(!dir.path().join("escaped.txt").exists(), "zip slip must be blocked");
        assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../escaped.txt").exists());
        // The benign entry is still extracted (inside the destination).
        if out.exists() {
            if let Ok(entries) = std::fs::read_dir(&out) {
                for e in entries.flatten() {
                    assert!(e.path().starts_with(&out), "{:?} escaped the destination", e.path());
                }
            }
        }
    }

    #[test]
    fn tar_gz_extraction_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let tar_path = dir.path().join("jdk.tar.gz");
        {
            use flate2::write::GzEncoder;
            use flate2::Compression;
            let mut builder = tar::Builder::new(GzEncoder::new(
                std::fs::File::create(&tar_path).unwrap(),
                Compression::default(),
            ));
            let payload = b"java payload".to_vec();
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, "jdk-21/bin/java", &payload[..]).unwrap();
            builder.finish().unwrap();
        }
        let out = dir.path().join("out");
        extract_java_archive(&tar_path, &out, 0).unwrap();
        assert_eq!(std::fs::read(out.join("jdk-21/bin/java")).unwrap(), b"java payload");
        assert_eq!(single_child_dir(&out), Some(out.join("jdk-21")));
    }

    #[test]
    fn adoptium_package_picks_the_platform() {
        let json = serde_json::json!({
            "release_name": "jdk-21.0.5+11",
            "version": { "semver": "21.0.5+11", "major": 21, "build": 11 },
            "binaries": [
                { "image_type": "jre", "architecture": "x64", "os": "linux",
                  "package": { "link": "https://g/jre-linux.tar.gz", "checksum": "aa", "size": 1 } },
                { "image_type": "jre", "architecture": "x64", "os": "windows",
                  "package": { "link": "https://g/jre-win.zip", "checksum": "bb", "size": 2 } }
            ]
        });
        let rel: AdoptiumRelease = serde_json::from_value(json).unwrap();
        assert_eq!(rel.version.major, 21);
        let pkg = rel.package().unwrap();
        assert!(pkg.link.contains(adoptium_os()), "{:?}", pkg.link);
        assert_eq!(adoptium_arch(), "x64");
        let empty: AdoptiumRelease = serde_json::from_value(serde_json::json!({
            "release_name": "x", "version": { "semver": "1", "major": 1, "build": 0 }, "binaries": []
        }))
        .unwrap();
        assert!(empty.package().is_err());
    }

    #[tokio::test]
    async fn detection_does_not_panic_on_any_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = JavaManager::new(tmp.path().join("java"), HttpClient::new(), true);
        let found = mgr.detect().await;
        // Whatever exists, results must be sorted newest-first and deduped by home.
        for w in found.windows(2) {
            assert!(w[0].major >= w[1].major, "{:?}", found.iter().map(|f| f.major).collect::<Vec<_>>());
            assert_ne!(w[0].home, w[1].home);
        }
        assert!(JavaManager::platform_search_dirs().iter().all(|d| !d.as_os_str().is_empty()));
    }

    #[tokio::test]
    async fn ensure_uses_the_manifest_major_over_the_heuristic() {
        let tmp = tempfile::tempdir().unwrap();
        // A fake "managed" install that really is this test binary's java, if any.
        let mgr = JavaManager::new(tmp.path().join("java"), HttpClient::new(), false);
        let progress = ProgressReporter::silent("j", None, sp_core::model::ProgressPhase::Resolve);
        let err = mgr.ensure("1.16.5", Some(8), None, &progress).await.unwrap_err();
        // On a machine with no Java 8 at all, we must ask for 8 (from the manifest),
        // not 17 (which the MC-version heuristic would suggest for unknowns).
        match err {
            CoreError::JavaMissing(mc, major) => {
                assert_eq!(mc, "1.16.5");
                assert_eq!(major, 8);
            }
            other => panic!("expected java_missing, got {other:?}"),
        }
    }
}

//! Spigot/CraftBukkit (built locally with BuildTools), Geyser (GitHub releases)
//! and BungeeCord (upload only).
//!
//! Spigot and CraftBukkit are **not distributed as jars**: their project policy is
//! that each server operator builds them from source with BuildTools, which is also
//! why no panel can offer a download URL for them. So this provider does the honest
//! thing instead of pretending: it lists the Minecraft versions BuildTools can
//! compile (derived from Mojang's manifest — the same live source of truth as the
//! rest of the panel) and returns an *installer plan* that runs BuildTools in the
//! server directory.
//!
//! That has consequences the UI must surface, and they are encoded here rather than
//! left to chance:
//! * a JDK (`javac`) and `git` must exist on the host — checked before downloading,
//!   with an actionable error if not;
//! * the build takes 5–20 minutes and prints a lot; it is streamed into the server's
//!   console ring so the wizard can watch it live;
//! * exit code 0 plus `spigot-<rev>.jar` appearing is the success test — BuildTools
//!   has been observed to exit 0 without producing a jar when its git checkout
//!   fails, so the artifact check is mandatory.

use crate::{
    CatalogEntry, DownloadFile, FileRole, InstallPlan, InstallerRun, LaunchMode, Provider,
    RegistryConfig, VersionInfo, VersionSelection,
};
use async_trait::async_trait;
use serde::Deserialize;
use sp_core::model::{now_ms, ReleaseChannel, ServerKind};
use sp_core::CoreError;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

pub const BUILDTOOLS_BASE: &str = "https://hub.spigotmc.org/jenkins";
pub const GITHUB_API: &str = "https://api.github.com";
pub const GITHUB: &str = "https://github.com";

/// Everything BuildTools needs that the panel cannot ship.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildPrereqs {
    pub jdk: Option<String>,
    pub git: Option<String>,
}

impl BuildPrereqs {
    pub fn missing(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.jdk.is_none() {
            v.push("a JDK (javac)");
        }
        if self.git.is_none() {
            v.push("git");
        }
        v
    }

    pub fn is_ready(&self) -> bool {
        self.missing().is_empty()
    }

    /// Detect without asking the user to try an install first.
    pub fn detect() -> Self {
        Self {
            jdk: probe_version(&["javac", "-version"], |out| {
                out.lines().next().unwrap_or("").to_string()
            }),
            git: probe_version(&["git", "--version"], |out| out.trim().to_string()),
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "jdk: {}; git: {}",
            self.jdk.as_deref().unwrap_or("not found"),
            self.git.as_deref().unwrap_or("not found")
        )
    }
}

fn probe_version<F>(args: &[&str], parse: F) -> Option<String>
where
    F: FnOnce(String) -> String,
{
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(parse(text))
}

/// Spigot or CraftBukkit — same flow, different artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildTarget {
    Spigot,
    CraftBukkit,
}

impl BuildTarget {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Spigot => "spigot",
            Self::CraftBukkit => "craftbukkit",
        }
    }

    pub fn kind(self) -> ServerKind {
        match self {
            Self::Spigot => ServerKind::Spigot,
            Self::CraftBukkit => ServerKind::CraftBukkit,
        }
    }

    /// What BuildTools names the jar it leaves in the working directory.
    pub fn produced_jar(self, mc: &str) -> String {
        match self {
            Self::Spigot => format!("spigot-{mc}.jar"),
            Self::CraftBukkit => format!("craftbukkit-{mc}.jar"),
        }
    }

    pub fn args(self, mc: &str) -> Vec<String> {
        let mut a = vec!["--rev".to_string(), mc.to_string()];
        if self == Self::CraftBukkit {
            // Skip the Spigot compile: roughly halves the wait.
            a.push("--only-verify".to_string());
            a.push("false".to_string());
            a.push("--tools".to_string());
            a.push(".".to_string());
        }
        a
    }
}

#[derive(Debug, Clone)]
pub struct SpigotProvider {
    target: BuildTarget,
    client: sp_net::HttpClient,
    /// Where the version list comes from. Injected so tests are deterministic and
    /// the panel keeps a single source of truth for "which MC versions exist".
    min_minor: u32,
}

impl SpigotProvider {
    pub fn new(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Self::build(cfg, BuildTarget::Spigot)
    }

    pub fn craftbukkit(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Self::build(cfg, BuildTarget::CraftBukkit)
    }

    fn build(cfg: &RegistryConfig, target: BuildTarget) -> Result<Self, CoreError> {
        Ok(Self {
            target,
            client: cfg.client(target.slug(), BUILDTOOLS_BASE)?,
            // BuildTools supports 1.8+; older releases need a versioned tool that
            // the panel deliberately does not attempt.
            min_minor: 8,
        })
    }

    pub fn buildtools_url(&self) -> String {
        format!(
            "{BUILDTOOLS_BASE}/job/BuildTools/lastSuccessfulBuild/artifact/target/BuildTools.jar"
        )
    }

    /// The `--rev` value to use. `latest` means "whatever BuildTools calls latest".
    fn rev_for(&self, sel: &VersionSelection) -> String {
        sel.mc_version
            .clone()
            .or_else(|| sel.exact.clone())
            .unwrap_or_else(|| "latest".into())
    }

    /// Keep only MC versions BuildTools can plausibly build.
    pub fn supported<'a>(versions: impl Iterator<Item = &'a str>, min_minor: u32) -> Vec<String> {
        versions
            .filter_map(|v| {
                let mut it = v.split('.');
                let (Some(major), Some(minor)) = (it.next(), it.next()) else {
                    return None;
                };
                if major == "1" && minor.parse::<u32>().ok()? >= min_minor {
                    Some(v.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    fn plan_for(&self, rev: &str, prereqs: &BuildPrereqs) -> Result<InstallPlan, CoreError> {
        let missing = prereqs.missing();
        if !missing.is_empty() {
            return Err(CoreError::InvalidState(format!(
                "building {} {} needs {} on this host — install them, or switch to \
                 Paper/Purpur which publishes prebuilt jars",
                self.target.slug(),
                rev,
                missing.join(" and ")
            )));
        }
        let produced = self.target.produced_jar(rev);
        Ok(InstallPlan {
            provider: self.target.slug(),
            version: VersionInfo {
                id: rev.to_string(),
                display: format!("{} {} (built locally)", self.target.slug(), rev),
                mc_version: rev.to_string(),
                channel: ReleaseChannel::Stable,
                build: None,
                released_at_ms: Some(now_ms()),
                java_major: Some(sp_core::model::required_java_major_for_mc(rev)),
                downloads: vec![DownloadFile::new(
                    FileRole::Installer,
                    self.buildtools_url(),
                    "BuildTools.jar",
                )],
                unstable: false,
                note: Some("compiled on your machine, so there is no upstream checksum".into()),
            },
            files: vec![DownloadFile::new(
                FileRole::Installer,
                self.buildtools_url(),
                "BuildTools.jar",
            )],
            launch: LaunchMode::Jar {
                jar: self.target.jar_name(),
                args: Vec::new(),
            },
            jvm_args: Vec::new(),
            natives_dir: None,
            java_major: sp_core::model::required_java_major_for_mc(rev),
            properties: BTreeMap::new(),
            installer: Some(InstallerRun {
                jar: "BuildTools.jar".into(),
                args: self.target.args(rev),
                ok_exit_codes: vec![0],
                expected_artifact: Some(produced),
            }),
            notes: vec![
                "Compiling takes 5–20 minutes and needs ~1 GB of disk".to_string(),
                "Progress streams into this server's console; the wizard stays open".to_string(),
                format!(
                    "{} will be renamed to {}",
                    self.target.produced_jar(rev),
                    self.target.jar_name()
                ),
            ],
        })
    }
}

impl BuildTarget {
    fn jar_name(self) -> String {
        self.kind().jar_name().to_string()
    }
}

#[async_trait]
impl Provider for SpigotProvider {
    fn kind(&self) -> ServerKind {
        self.target.kind()
    }

    fn name(&self) -> &'static str {
        self.target.slug()
    }

    fn needs_build_tools(&self) -> bool {
        true
    }

    fn install_hint(&self) -> Option<String> {
        Some(format!(
            "{} is built from source on your machine (that is the Spigot project's \
             distribution policy). Paper and Purpur publish the same API with prebuilt \
             jars if you want a one-click install.",
            self.target.slug()
        ))
    }

    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        // BuildTools publishes nothing machine-readable, so the catalogue of what
        // *can* be built is Mojang's release list filtered by BuildTools' support.
        let mojang_cfg = RegistryConfig::default();
        let mojang = crate::mojang::MojangProvider::new(&mojang_cfg)?;
        let manifest = mojang.manifest().await?;
        let mut seen = Vec::new();
        let mut versions = Vec::new();
        for entry in crate::mojang::MojangProvider::parse_manifest(&manifest) {
            if entry.unstable {
                continue;
            }
            if Self::supported([entry.mc_version.as_str()].into_iter(), self.min_minor).is_empty() {
                continue;
            }
            if let Some(mc) = mc_version {
                if entry.mc_version != mc {
                    continue;
                }
            }
            if seen.contains(&entry.mc_version) {
                continue;
            }
            seen.push(entry.mc_version.clone());
            versions.push(VersionInfo {
                note: Some("built locally with BuildTools (5–20 min)".into()),
                ..entry
            });
        }
        Ok(CatalogEntry {
            versions,
            fetched_at_ms: now_ms(),
            stale: false,
        })
    }

    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        let rev = self.rev_for(sel);
        self.plan_for(&rev, &BuildPrereqs::detect())
    }
}

/// Geyser: distributed as a GitHub release asset, so the release API is the
/// catalogue. `api.github.com` is the only endpoint used and it is public for
/// releases (60 req/h unauthenticated, which is far more than the TTL needs).
#[derive(Debug, Clone)]
pub struct GeyserProvider {
    client: sp_net::HttpClient,
    owner: &'static str,
    repo: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl GeyserProvider {
    pub fn new(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Ok(Self {
            client: cfg.client("geyser", GITHUB_API)?,
            owner: "GeyserMC",
            repo: "Geyser",
        })
    }

    pub async fn latest_async(&self) -> Result<GhRelease, CoreError> {
        self.client
            .get_json(&format!(
                "/repos/{}/{}/releases/latest",
                self.owner, self.repo
            ))
            .await
    }

    /// Choose the portable jar from a release's assets.
    ///
    /// Geyser publishes several jars per release; `Geyser-<ver>.jar` (older naming:
    /// `geyser-<ver>.jar`) is the standalone one. Platform-specific artifacts
    /// (`-bukkit`, `-velocity`, `-fabric`, `-spigot`, `-viabackwards`, `-common`,
    /// `-api`, `-bootstrap`) are plugin variants and must not be chosen here.
    pub fn pick_asset(release: &GhRelease) -> Option<&GhAsset> {
        let mut candidates: Vec<&GhAsset> = release
            .assets
            .iter()
            .filter(|a| a.name.to_ascii_lowercase().ends_with(".jar"))
            .filter(|a| {
                let n = a.name.to_ascii_lowercase();
                ![
                    "bukkit",
                    "spigot",
                    "velocity",
                    "fabric",
                    "forge",
                    "neoforge",
                    "nukkit",
                    "sponge",
                    "viabackwards",
                    "viaversions",
                    "common",
                    "api",
                    "bootstrap",
                    "standalone",
                ]
                .iter()
                .any(|skip| n.contains(skip))
            })
            .collect();
        candidates.sort_by_key(|a| {
            // Prefer the shortest, most generic name (the portable jar).
            (a.name.len(), a.name.to_ascii_lowercase())
        });
        candidates.first().copied()
    }
}

#[async_trait]
impl Provider for GeyserProvider {
    fn kind(&self) -> ServerKind {
        ServerKind::Geyser
    }

    fn name(&self) -> &'static str {
        "geyser"
    }

    fn install_hint(&self) -> Option<String> {
        Some(
            "Geyser usually runs as a *plugin* in your Java server; a standalone \
             Geyser server is for hosts that cannot install plugins"
                .to_string(),
        )
    }

    async fn versions(&self, _mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        let releases = self
            .client
            .get_json::<Vec<GhRelease>>(&format!(
                "/repos/{}/{}/releases?per_page=15",
                self.owner, self.repo
            ))
            .await?;
        let versions = releases
            .into_iter()
            .filter_map(|r| {
                let asset = Self::pick_asset(&r)?;
                Some(VersionInfo {
                    id: r.tag_name.clone(),
                    display: r.tag_name.clone(),
                    mc_version: String::new(),
                    channel: if r.prerelease {
                        ReleaseChannel::Beta
                    } else {
                        ReleaseChannel::Stable
                    },
                    build: None,
                    released_at_ms: r.published_at.as_deref().and_then(crate::mojang::parse_iso),
                    java_major: Some(17),
                    downloads: vec![DownloadFile::new(
                        FileRole::ServerJar,
                        asset.browser_download_url.clone(),
                        ServerKind::Geyser.jar_name(),
                    )
                    .with_sha256(None, asset.size)],
                    unstable: r.prerelease,
                    note: r
                        .body
                        .as_deref()
                        .and_then(|b| b.lines().find(|l| l.trim().starts_with('-')))
                        .map(|l| l.trim().to_string()),
                })
            })
            .collect();
        Ok(CatalogEntry {
            versions,
            fetched_at_ms: now_ms(),
            stale: false,
        })
    }

    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        let release = match &sel.exact {
            Some(tag) => {
                self.client
                    .get_json::<GhRelease>(&format!(
                        "/repos/{}/{}/releases/tags/{tag}",
                        self.owner, self.repo
                    ))
                    .await?
            }
            None => self.latest_async().await?,
        };
        let asset = Self::pick_asset(&release).ok_or_else(|| CoreError::Upstream {
            service: "geyser",
            status: 200,
            body: format!(
                "release {} has no standalone jar among {} assets",
                release.tag_name,
                release.assets.len()
            ),
        })?;
        Ok(InstallPlan {
            provider: "geyser",
            version: VersionInfo {
                id: release.tag_name.clone(),
                display: format!("Geyser {}", release.tag_name),
                mc_version: String::new(),
                channel: if release.prerelease {
                    ReleaseChannel::Beta
                } else {
                    ReleaseChannel::Stable
                },
                build: None,
                released_at_ms: release
                    .published_at
                    .as_deref()
                    .and_then(crate::mojang::parse_iso),
                java_major: Some(17),
                downloads: vec![],
                unstable: release.prerelease,
                note: Some("Bedrock↔Java bridge; no Minecraft version pin".into()),
            },
            files: vec![DownloadFile::new(
                FileRole::ServerJar,
                asset.browser_download_url.clone(),
                ServerKind::Geyser.jar_name(),
            )
            .with_sha256(None, asset.size)],
            launch: LaunchMode::Jar {
                jar: ServerKind::Geyser.jar_name().to_string(),
                args: Vec::new(),
            },
            jvm_args: Vec::new(),
            natives_dir: None,
            java_major: 17,
            properties: BTreeMap::from([("bedrock-port".to_string(), "19132".to_string())]),
            installer: None,
            notes: vec![
                "Geyser needs UDP 19132 open; the Java server it bridges to should have \
                 `force-gamemode=false`"
                    .to_string(),
                "GitHub publishes no checksums for release assets, so this jar is \
                 downloaded unverified (over TLS, from github.com)"
                    .to_string(),
            ],
        })
    }
}

/// A kind with no automatable source. This is a product answer, not a missing
/// feature: the UI offers "drop the jar in" with the upstream download page named,
/// and `plan()` explains why rather than returning an empty list.
#[derive(Debug, Clone)]
pub struct UploadOnlyProvider {
    kind: ServerKind,
    docs_url: &'static str,
    #[allow(dead_code)]
    client: sp_net::HttpClient,
}

impl UploadOnlyProvider {
    pub fn bungeecord(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Ok(Self {
            kind: ServerKind::BungeeCord,
            docs_url: "https://www.spigotmc.org/wiki/bungeecord/",
            client: cfg.client("bungeecord", GITHUB)?,
        })
    }

    pub fn jar_file_name(&self) -> &'static str {
        self.kind.jar_name()
    }

    pub fn docs_url(&self) -> &'static str {
        self.docs_url
    }

    /// Where an uploaded jar must land for the launcher to find it.
    pub fn expected_path(&self, server_dir: &Path) -> std::path::PathBuf {
        server_dir.join(self.jar_file_name())
    }
}

#[async_trait]
impl Provider for UploadOnlyProvider {
    fn kind(&self) -> ServerKind {
        self.kind
    }

    fn name(&self) -> &'static str {
        "manual"
    }

    fn install_hint(&self) -> Option<String> {
        Some(format!(
            "{} publishes jars behind an account login, so the panel cannot fetch one. \
             Download it yourself and use “Upload a server jar” — the file must be named \
             {} inside the server directory. See {}",
            self.kind.label(),
            self.jar_file_name(),
            self.docs_url
        ))
    }

    async fn versions(&self, _mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        Ok(CatalogEntry {
            versions: Vec::new(),
            fetched_at_ms: now_ms(),
            stale: false,
        })
    }

    async fn plan(&self, _sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        Err(CoreError::InvalidState(format!(
            "{} cannot be installed automatically: upload {} to the server directory \
             and press “Re-detect”",
            self.kind.label(),
            self.jar_file_name()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh_release(assets: &[(&str, u64)]) -> GhRelease {
        GhRelease {
            tag_name: "v2.6.1".into(),
            prerelease: false,
            published_at: Some("2025-02-01T10:00:00Z".into()),
            body: Some("### Changelog\n- Fix form fix\n".into()),
            assets: assets
                .iter()
                .map(|(n, s)| GhAsset {
                    name: n.to_string(),
                    browser_download_url: format!("https://gh.download/{n}"),
                    size: Some(*s),
                    content_type: Some("application/java-archive".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn build_prereqs_report_what_is_missing() {
        let none = BuildPrereqs::default();
        assert!(!none.is_ready());
        assert_eq!(none.missing(), vec!["a JDK (javac)", "git"]);
        let partial = BuildPrereqs {
            jdk: Some("javac 21".into()),
            git: None,
        };
        assert_eq!(partial.missing(), vec!["git"]);
        let ok = BuildPrereqs {
            jdk: Some("javac 21".into()),
            git: Some("git version 2.43".into()),
        };
        assert!(ok.is_ready());
        assert!(ok.summary().contains("javac 21"));
        // Real detection must not panic on a machine with neither tool installed.
        let _ = BuildPrereqs::detect();
    }

    #[test]
    fn only_versions_buildtools_understand_are_offered() {
        let v = SpigotProvider::supported(
            [
                "1.7.10", "1.8.9", "1.12.2", "1.16.5", "1.21.4", "25w03a", "b1.8.1",
            ]
            .into_iter(),
            8,
        );
        assert_eq!(v, vec!["1.8.9", "1.12.2", "1.16.5", "1.21.4"]);
        assert!(SpigotProvider::supported(["1.2.5"].into_iter(), 8).is_empty());
    }

    #[test]
    fn craftbukkit_and_spigot_produce_their_own_jar_names() {
        assert_eq!(
            BuildTarget::Spigot.produced_jar("1.21.4"),
            "spigot-1.21.4.jar"
        );
        assert_eq!(
            BuildTarget::CraftBukkit.produced_jar("1.20.6"),
            "craftbukkit-1.20.6.jar"
        );
        assert_eq!(BuildTarget::Spigot.args("1.21.4"), vec!["--rev", "1.21.4"]);
        assert!(BuildTarget::CraftBukkit
            .args("1.21.4")
            .contains(&"--tools".to_string()));
        assert_eq!(BuildTarget::Spigot.jar_name(), "server.jar");
    }

    #[test]
    fn missing_jdk_is_an_actionable_error_not_a_download() {
        let cfg = RegistryConfig::default();
        let p = SpigotProvider::new(&cfg).unwrap();
        let err = p.plan_for("1.21.4", &BuildPrereqs::default()).unwrap_err();
        assert_eq!(err.code(), "invalid_state");
        let msg = err.to_string();
        assert!(msg.contains("JDK") && msg.contains("git"), "{msg}");
        assert!(
            msg.contains("Paper"),
            "the error must point at the easier path"
        );
        let ready = BuildPrereqs {
            jdk: Some("javac 21".into()),
            git: Some("git 2.43".into()),
        };
        let plan = p.plan_for("1.21.4", &ready).unwrap();
        assert_eq!(plan.provider, "spigot");
        assert_eq!(plan.files[0].dest, "BuildTools.jar");
        assert_eq!(plan.files[0].url, p.buildtools_url());
        assert!(
            plan.files[0].unverified,
            "Jenkins artifacts have no checksum"
        );
        let inst = plan.installer.unwrap();
        assert_eq!(inst.args, vec!["--rev".to_string(), "1.21.4".to_string()]);
        assert_eq!(inst.ok_exit_codes, vec![0]);
        assert_eq!(inst.expected_artifact.as_deref(), Some("spigot-1.21.4.jar"));
        assert!(plan
            .notes
            .iter()
            .any(|n| n.contains("renamed to server.jar")));
        assert_eq!(plan.java_major, 21);
    }

    #[test]
    fn geyser_asset_choice_avoids_plugin_variants() {
        let r = gh_release(&[
            ("geyser-2.6.1-bukkit.jar", 1),
            ("geyser-2.6.1-velocity.jar", 1),
            ("Geyser-Spigot.jar", 1),
            ("geyser-2.6.1.jar", 9_000_000),
            ("geyser-2.6.1-viabackwards.jar", 1),
            ("source.zip", 1),
        ]);
        let chosen = GeyserProvider::pick_asset(&r).unwrap();
        assert_eq!(chosen.name, "geyser-2.6.1.jar");
        assert_eq!(chosen.size, Some(9_000_000));
        assert!(GeyserProvider::pick_asset(&gh_release(&[("notes.txt", 1)])).is_none());
        assert!(GeyserProvider::pick_asset(&gh_release(&[])).is_none());
    }

    #[tokio::test]
    async fn geyser_listing_and_plan_use_the_release_api() {
        let r = gh_release(&[("geyser-2.6.1.jar", 42)]);
        let mut routes = BTreeMap::new();
        routes.insert(
            "/repos/GeyserMC/Geyser/releases/latest",
            serde_json::to_string(&r).unwrap(),
        );
        routes.insert(
            "/repos/GeyserMC/Geyser/releases",
            format!("[{}]", serde_json::to_string(&r).unwrap()),
        );
        routes.insert(
            "/repos/GeyserMC/Geyser/releases/tags/v2.6.1",
            serde_json::to_string(&r).unwrap(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("geyser".into(), format!("http://127.0.0.1:{}", mock.port));
        let p = GeyserProvider::new(&cfg).unwrap();
        let listing = p.versions(None).await.unwrap();
        assert_eq!(listing.versions.len(), 1);
        assert_eq!(listing.versions[0].id, "v2.6.1");
        assert_eq!(listing.versions[0].note.as_deref(), Some("- Fix form fix"));
        assert!(listing.versions[0].released_at_ms.is_some());
        for sel in [
            VersionSelection::default(),
            VersionSelection {
                exact: Some("v2.6.1".into()),
                ..Default::default()
            },
        ] {
            let plan = p.plan(&sel).await.unwrap();
            assert_eq!(plan.files[0].dest, "server.jar");
            assert!(plan.files[0].unverified);
            assert_eq!(plan.java_major, 17);
            assert_eq!(
                plan.properties.get("bedrock-port").map(String::as_str),
                Some("19132")
            );
            assert!(plan.notes.iter().any(|n| n.contains("UDP 19132")));
            assert!(plan.notes.iter().any(|n| n.contains("no checksums")));
        }
        assert!(p.install_hint().unwrap().contains("plugin"));
    }

    #[tokio::test]
    async fn upload_only_kinds_answer_honestly() {
        let cfg = RegistryConfig::default();
        let p = UploadOnlyProvider::bungeecord(&cfg).unwrap();
        assert_eq!(p.kind(), ServerKind::BungeeCord);
        assert_eq!(p.jar_file_name(), "proxy.jar");
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(p.expected_path(dir.path()), dir.path().join("proxy.jar"));
        let entry = p.versions(None).await.unwrap();
        assert!(entry.versions.is_empty());
        let hint = p.install_hint().unwrap();
        assert!(hint.contains("spigotmc.org/wiki"), "{hint}");
        assert!(hint.contains("proxy.jar"));
        let err = p.plan(&VersionSelection::default()).await.unwrap_err();
        assert_eq!(err.code(), "invalid_state");
        assert!(err.to_string().contains("Upload a server jar"));
    }
}

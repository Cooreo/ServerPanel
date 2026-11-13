//! Forge and NeoForge.
//!
//! Both are maven repositories, not build services, so the catalogue is the maven
//! metadata itself: `maven-metadata.xml` lists every published version and each jar
//! has a `.sha1` sibling. `promotions_slim.json` (Forge only) additionally names
//! which build is `recommended` per Minecraft version — that is what the UI marks as
//! the safe default, and it is fetched live rather than baked in.
//!
//! The install contract differs by era, and getting this wrong is the #1 cause of
//! "Forge install fails with no message":
//!
//! * **≤ 1.16.5** — running the installer with `--installServer` writes the universal
//!   jar plus libraries, and `java -jar forge-…-universal.jar` then works.
//! * **≥ 1.17** — the installer also writes an *argfile*
//!   (`libraries/net/minecraftforge/forge/<full version>/unix_args.txt`, or
//!   `win_args.txt`) containing `-p <classpath>` and the `-D` properties. `run.sh`
//!   is just `java @argfile`, so `LaunchMode::ArgFile` is exactly what we need; the
//!   panel reads and splices the argfile itself (see `expand_argfile`) because we
//!   never launch through a shell.
//!
//! NeoForge (1.20.2+) follows the ≥ 1.17 shape with its own coordinates.

use crate::{
    CatalogEntry, DownloadFile, FileRole, InstallPlan, InstallerRun, LaunchMode, Provider,
    RegistryConfig, VersionInfo, VersionSelection,
};
use async_trait::async_trait;
use regex::Regex;
use sp_core::model::{now_ms, ReleaseChannel, ServerKind};
use sp_core::CoreError;
use std::collections::BTreeMap;
use std::path::Path;

pub const FORGE_BASE: &str = "https://maven.minecraftforge.net";
pub const NEOFORGE_BASE: &str = "https://maven.neoforged.net/releases";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKind {
    Forge,
    NeoForge,
}

impl ModKind {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
        }
    }

    pub fn base(self) -> &'static str {
        match self {
            Self::Forge => FORGE_BASE,
            Self::NeoForge => NEOFORGE_BASE,
        }
    }

    pub fn kind(self) -> ServerKind {
        match self {
            Self::Forge => ServerKind::Forge,
            Self::NeoForge => ServerKind::NeoForge,
        }
    }

    /// `net/minecraftforge/forge` vs `net/neoforged/neoforge`.
    pub fn maven_dir(self) -> &'static str {
        match self {
            Self::Forge => "net/minecraftforge/forge",
            Self::NeoForge => "net/neoforged/neoforge",
        }
    }

    /// Version ranges where the installer's output is a universal jar rather than
    /// an argfile. 1.17 is the dividing line (NeoForge only exists after it).
    pub fn uses_argfile(mc_version: &str) -> bool {
        let mut it = mc_version.split('.').skip(1);
        let minor: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(21);
        minor >= 17
    }
}

/// One entry of `maven-metadata.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MavenVersion {
    /// Raw maven version, e.g. `1.20.4-47.3.14` or `21.4.118-beta`.
    pub version: String,
}

impl MavenVersion {
    /// Forge: the MC part is the prefix. NeoForge: the major *is* the MC minor.
    pub fn mc_version(&self, kind: ModKind) -> String {
        match kind {
            ModKind::Forge => self
                .version
                .split('-')
                .next()
                .unwrap_or(&self.version)
                .to_string(),
            ModKind::NeoForge => {
                let head: u32 = self
                    .version
                    .split('.')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(21);
                if head >= 20 {
                    // NeoForge versions are `<mc minor>.<patch>.<build>`: 21.4 → 1.21.4.
                    format!("1.{head}")
                } else {
                    format!("1.{head}")
                }
            }
        }
    }

    /// Build number = last numeric component (47.3.**14** → 14 for Forge ordering;
    /// NeoForge orders by the whole string).
    pub fn sort_key(&self) -> Vec<u64> {
        let tail = self.version.rsplit('-').next().unwrap_or(&self.version);
        tail.split('.')
            .map(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()))
            .filter_map(|p| p.parse::<u64>().ok())
            .collect()
    }

    pub fn is_prerelease(&self) -> bool {
        self.version.contains("beta")
            || self.version.contains("rc")
            || self.version.contains("snapshot")
            || self.version.contains("alpha")
    }
}

/// Parse `maven-metadata.xml`. We only need `<version>` text, and the file is
/// machine-generated, so a targeted regex beats an XML dependency.
pub fn parse_maven_metadata(xml: &str) -> Vec<MavenVersion> {
    let re = Regex::new(r"<version>\s*([^<\s]+)\s*</version>").expect("valid pattern");
    let mut out: Vec<MavenVersion> = re
        .captures_iter(xml)
        .filter_map(|c| {
            c.get(1).map(|m| MavenVersion {
                version: m.as_str().to_string(),
            })
        })
        .collect();
    out.retain(|v| !v.version.ends_with("-universal.jar"));
    out.sort_by(|a, b| {
        b.sort_key()
            .cmp(&a.sort_key())
            .then_with(|| b.version.cmp(&a.version))
    });
    out
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ForgePromotions {
    /// `"1.20.4": "-47.3.14"` — note the leading dash, which callers must strip.
    pub promotions: BTreeMap<String, String>,
    #[serde(default)]
    pub home: Option<String>,
}

impl ForgePromotions {
    /// `recommended` build for a Minecraft version, if upstream promoted one.
    pub fn recommended(&self, mc: &str) -> Option<String> {
        self.promotions
            .get(&format!("{mc}-recommended"))
            .or_else(|| self.promotions.get(mc))
            .map(|s| s.trim_start_matches('-').to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn latest(&self, mc: &str) -> Option<String> {
        self.promotions
            .get(&format!("{mc}-latest"))
            .map(|s| s.trim_start_matches('-').to_string())
    }
}

#[derive(Debug, Clone)]
pub struct ForgeProvider {
    kind: ModKind,
    client: sp_net::HttpClient,
    promotions: bool,
}

impl ForgeProvider {
    pub fn forge(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Self::new(cfg, ModKind::Forge)
    }
    pub fn neoforge(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Self::new(cfg, ModKind::NeoForge)
    }

    fn new(cfg: &RegistryConfig, kind: ModKind) -> Result<Self, CoreError> {
        Ok(Self {
            kind,
            client: cfg.client(kind.slug(), kind.base())?,
            promotions: kind == ModKind::Forge,
        })
    }

    fn metadata_url(&self) -> String {
        format!("/{}/maven-metadata.xml", self.kind.maven_dir())
    }

    fn artifact_url(&self, version: &str, suffix: &str) -> String {
        format!(
            "{}/{}/{}/{}-{}{}.jar",
            self.kind.base().trim_end_matches('/'),
            self.kind.maven_dir(),
            version,
            self.kind.slug(),
            version,
            suffix
        )
    }

    pub async fn all_versions(&self) -> Result<Vec<MavenVersion>, CoreError> {
        let xml = self.client.get_text(&self.metadata_url()).await?;
        Ok(parse_maven_metadata(&xml))
    }

    pub async fn promotions(&self) -> Option<ForgePromotions> {
        if !self.promotions {
            return None;
        }
        // promotions_slim.json lives on the *files* host, not the maven repo host.
        let client = sp_net::HttpClientBuilder::new()
            .service("forge-promotions")
            .user_agent("ServerPanel")
            .build_client()
            .ok()?;
        client
            .get_json(
                "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json",
            )
            .await
            .ok()
    }

    /// `.sha1` next to the artifact, which both maven repos publish.
    pub async fn sha1_for(&self, url: &str) -> Option<String> {
        self.client
            .get_text(&format!("{url}.sha1"))
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| (32..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()))
    }

    fn info(&self, v: &MavenVersion, promoted: bool) -> VersionInfo {
        let mc = v.mc_version(self.kind);
        VersionInfo {
            id: v.version.clone(),
            display: format!("{} {}", self.kind.slug(), v.version),
            mc_version: mc,
            channel: if promoted {
                ReleaseChannel::Stable
            } else if v.is_prerelease() {
                ReleaseChannel::Nightly
            } else {
                ReleaseChannel::Beta
            },
            build: None,
            released_at_ms: None,
            java_major: None,
            downloads: vec![DownloadFile::new(
                FileRole::Installer,
                self.artifact_url(&v.version, "-installer"),
                format!("{}-installer.jar", self.kind.slug()),
            )],
            unstable: v.is_prerelease(),
            note: if promoted {
                Some("marked recommended by Forge".into())
            } else {
                None
            },
        }
    }

    /// The installer headless invocation. Both ecosystems accept `--installServer`;
    /// Forge additionally needs the target dir as its argument.
    pub fn installer_args(mc_version: &str, full_version: &str) -> Vec<String> {
        let _ = mc_version;
        let _ = full_version;
        vec!["--installServer".to_string()]
    }
}

#[async_trait]
impl Provider for ForgeProvider {
    fn kind(&self) -> ServerKind {
        self.kind.kind()
    }

    fn name(&self) -> &'static str {
        self.kind.slug()
    }

    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        let all = self.all_versions().await?;
        let promoted = self
            .promotions()
            .await
            .and_then(|p| mc_version.as_ref().and_then(|mc| p.recommended(mc)));
        let mut versions: Vec<VersionInfo> = all
            .iter()
            .filter(|v| {
                mc_version
                    .map(|mc| v.mc_version(self.kind) == mc)
                    .unwrap_or(true)
            })
            .filter(|v| !v.is_prerelease())
            .take(60)
            .map(|v| self.info(v, promoted.as_deref() == Some(v.version.as_str())))
            .collect();
        if versions.is_empty() {
            // A version with only prereleases is still installable; hiding it
            // entirely would be worse than labelling it.
            versions = all
                .iter()
                .filter(|v| {
                    mc_version
                        .map(|mc| v.mc_version(self.kind) == mc)
                        .unwrap_or(true)
                })
                .take(20)
                .map(|v| self.info(v, false))
                .collect();
        }
        Ok(CatalogEntry {
            versions,
            fetched_at_ms: now_ms(),
            stale: false,
        })
    }

    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        let all = self.all_versions().await?;
        let chosen = match &sel.exact {
            Some(exact) => all.into_iter().find(|v| &v.version == exact),
            None => {
                let promoted = self
                    .promotions()
                    .await
                    .and_then(|p| sel.mc_version.as_deref().and_then(|mc| p.recommended(mc)));
                all.into_iter()
                    .filter(|v| {
                        sel.mc_version
                            .as_deref()
                            .map(|mc| v.mc_version(self.kind) == mc)
                            .unwrap_or(true)
                    })
                    .filter(|v| promoted.as_ref().map(|p| &v.version == p).unwrap_or(true))
                    .next()
                    .or_else(|| {
                        // Nothing promoted in range: newest non-prerelease matching MC.
                        let mut fallback: Vec<_> = self
                            .all_versions()
                            .await?
                            .into_iter()
                            .filter(|v| {
                                sel.mc_version
                                    .as_deref()
                                    .map(|mc| v.mc_version(self.kind) == mc)
                                    .unwrap_or(true)
                            })
                            .filter(|v| !v.is_prerelease())
                            .collect();
                        if fallback.is_empty() {
                            fallback = self
                                .all_versions()
                                .await?
                                .into_iter()
                                .filter(|v| {
                                    sel.mc_version
                                        .as_deref()
                                        .map(|mc| v.mc_version(self.kind) == mc)
                                        .unwrap_or(true)
                                })
                                .collect();
                        }
                        fallback.into_iter().next()
                    })
            }
        }
        .ok_or_else(|| {
            CoreError::not_found(format!(
                "no {} version for {}",
                self.kind.slug(),
                sel.mc_version.as_deref().unwrap_or("any Minecraft version")
            ))
        })?;

        let mc = chosen.mc_version(self.kind);
        let installer_url = self.artifact_url(&chosen.version, "-installer");
        let universal_url = self.artifact_url(&chosen.version, "-universal");
        let installer_sha = self.sha1_for(&installer_url).await;
        let universal_sha = self.sha1_for(&universal_url).await;

        let use_argfile = ModKind::uses_argfile(&mc);
        let argfile = argfile_for_expectation(self.kind, &chosen.version);
        // The universal jar is what pre-1.17 launches directly; on ≥1.17 it is
        // still fetched because `run.sh`'s argfile references it by path.
        let mut files = vec![DownloadFile::new(
            FileRole::Installer,
            installer_url,
            format!("{}-installer.jar", self.kind.slug()),
        )
        .with_sha1(installer_sha, None)];
        if !use_argfile {
            files.push(
                DownloadFile::new(
                    FileRole::ServerJar,
                    universal_url,
                    format!("{}-universal.jar", self.kind.slug()),
                )
                .with_sha1(universal_sha.clone(), None),
            );
        }
        let launch = if use_argfile {
            LaunchMode::ArgFile {
                argfile,
                args: Vec::new(),
            }
        } else {
            LaunchMode::Jar {
                jar: format!("{}-universal.jar", self.kind.slug()),
                args: vec!["nogui".to_string()],
            }
        };
        let mut notes = vec![
            "The Forge installer downloads libraries and Minecraft itself — keep the \
             server directory writable and give it a few minutes"
                .to_string(),
        ];
        if use_argfile {
            notes.push(format!(
                "Mods go in `mods/`; launch flags come from {} written by the installer",
                ServerKind::Forge.jar_name()
            ));
        }

        Ok(InstallPlan {
            provider: self.kind.slug(),
            version: VersionInfo {
                mc_version: mc.clone(),
                java_major: Some(sp_core::model::required_java_major_for_mc(&mc)),
                downloads: files.clone(),
                ..self.info(&chosen, false)
            },
            files,
            launch,
            jvm_args: Vec::new(),
            natives_dir: None,
            java_major: sp_core::model::required_java_major_for_mc(&mc),
            properties: BTreeMap::new(),
            installer: Some(InstallerRun {
                jar: format!("{}-installer.jar", self.kind.slug()),
                args: Self::installer_args(&mc, &chosen.version),
                ok_exit_codes: vec![0, 1],
                expected_artifact: Some(if use_argfile {
                    argfile_for_expectation(self.kind, &chosen.version)
                } else {
                    format!("{}-universal.jar", self.kind.slug())
                }),
            }),
            notes,
        })
    }
}

fn argfile_for_expectation(kind: ModKind, version: &str) -> String {
    let dir = match kind {
        ModKind::Forge => "net/minecraftforge/forge",
        ModKind::NeoForge => "net/neoforged/neoforge",
    };
    let suffix = if cfg!(windows) { "win" } else { "unix" };
    format!("libraries/{dir}/{version}/{suffix}_args.txt")
}

/// Expand an argfile into argv. Forge's files are whitespace-separated tokens that
/// may be quoted (Windows paths in quotes because of spaces in `Program Files`).
///
/// Recursion is supported (`@other.txt`) but bounded, and `-cp`/`-p` entries are
/// kept verbatim because the classpath inside is already relative to the server dir.
pub fn expand_argfile(path: &Path, max_depth: u8) -> Result<Vec<String>, CoreError> {
    let mut out = Vec::new();
    expand_into(path, max_depth, &mut out)?;
    Ok(out)
}

fn expand_into(path: &Path, depth: u8, out: &mut Vec<String>) -> Result<(), CoreError> {
    if depth == 0 {
        return Err(CoreError::other(format!(
            "argfile nesting too deep at {}",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)?;
    for token in split_args(&text) {
        if let Some(nested) = token.strip_prefix('@') {
            let candidate = path
                .parent()
                .map(|p| p.join(nested))
                .unwrap_or_else(|| Path::new(nested).to_path_buf());
            expand_into(&candidate, depth - 1, out)?;
        } else {
            out.push(token);
        }
    }
    Ok(())
}

/// Shell-like splitting: whitespace-separated, `"…"` and `'…'` group, `\\` escapes.
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\'
                    && q == '"'
                    && chars.peek().is_some_and(|n| *n == '"' || *n == '\\')
                {
                    cur.push(chars.next().unwrap());
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    has = true;
                }
                '\\' if chars.peek().is_some_and(|n| matches!(n, '"' | '\'' | '\\')) => {
                    cur.push(chars.next().unwrap());
                    has = true;
                }
                c if c.is_whitespace() => {
                    if has {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                }
                c => {
                    cur.push(c);
                    has = true;
                }
            },
        }
    }
    if has {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const METADATA: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>net.minecraftforge</groupId>
  <artifactId>forge</artifactId>
  <versioning>
    <latest>1.20.4-49.1.15</latest>
    <release>1.20.4-49.1.15</release>
    <versions>
      <version>1.20.4-49.0.19</version>
      <version>1.20.4-49.1.15</version>
      <version>1.20.2-48.1.0</version>
      <version>1.16.5-36.2.39</version>
      <version>1.12.2-14.23.5.2860</version>
      <version>1.21.4-54.0.2-beta</version>
    </versions>
    <lastUpdated>20241203113815</lastUpdated>
  </versioning>
</metadata>"#;

    #[test]
    fn maven_metadata_parses_and_orders() {
        let v = parse_maven_metadata(METADATA);
        assert_eq!(v.len(), 6);
        assert_eq!(v[0].version, "1.21.4-54.0.2-beta", "highest build first");
        assert!(v[0].is_prerelease());
        assert_eq!(v[1].version, "1.20.4-49.1.15");
        assert_eq!(v[1].mc_version(ModKind::Forge), "1.20.4");
        assert_eq!(v.last().unwrap().version, "1.12.2-14.23.5.2860");
        assert_eq!(parse_maven_metadata("<metadata/>").len(), 0);
    }

    #[test]
    fn neoforge_version_numbers_map_back_to_minecraft() {
        let v = MavenVersion {
            version: "21.4.118-beta".into(),
        };
        assert_eq!(v.mc_version(ModKind::NeoForge), "1.21");
        let v2 = MavenVersion {
            version: "20.4.80".into(),
        };
        assert_eq!(v2.mc_version(ModKind::NeoForge), "1.20");
        assert!(
            v2.sort_key()
                < MavenVersion {
                    version: "20.4.81".into()
                }
                .sort_key()
        );
    }

    #[test]
    fn promotions_are_read_for_the_recommended_default() {
        let p: ForgePromotions = serde_json::from_str(
            r#"{"home":"https://files.minecraftforge.net/","promotions":{"1.20.4-recommended":"47.3.0","1.20.4-latest":"49.1.15","latest":"-55.0.10","1.8.9-recommended":"-11.15.1.2318-1.8.9"}}"#,
        )
        .unwrap();
        assert_eq!(p.recommended("1.20.4").as_deref(), Some("47.3.0"));
        assert_eq!(p.latest("1.20.4").as_deref(), Some("49.1.15"));
        assert_eq!(
            p.recommended("1.8.9").as_deref(),
            Some("11.15.1.2318-1.8.9"),
            "legacy promotions carry a trailing -mc suffix that must survive"
        );
        assert_eq!(p.recommended("1.99"), None);
    }

    #[test]
    fn era_split_at_1_17() {
        assert!(!ModKind::uses_argfile("1.16.5"));
        assert!(!ModKind::uses_argfile("1.12.2"));
        assert!(ModKind::uses_argfile("1.17.1"));
        assert!(ModKind::uses_argfile("1.21.4"));
        assert!(ModKind::uses_argfile("1.20.4"), "NeoForge-era");
    }

    #[test]
    fn argfile_splitting_matches_the_shell() {
        assert_eq!(split_args("a b c"), vec!["a", "b", "c"]);
        assert_eq!(
            split_args("-cp \"C:\\Program Files\\x.jar\":y.jar -Dfml=1"),
            vec![
                "-cp".to_string(),
                "C:\\Program Files\\x.jar:y.jar".to_string(),
                "-Dfml=1".to_string()
            ]
        );
        assert_eq!(split_args("  spaced   out  "), vec!["spaced", "out"]);
        assert_eq!(split_args("\"quoted empty\""), vec!["quoted empty"]);
        assert_eq!(
            split_args("\"\""),
            vec![""],
            "an explicit empty arg survives"
        );
        assert_eq!(split_args("a\\ b"), vec!["a b"]);
        assert_eq!(split_args(""), Vec::<String>::new());
    }

    #[test]
    fn argfile_expansion_reads_files_and_recurses_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("win_args.txt"),
            "@extra.txt\n-p \"libs/a.jar;libs/b.jar\" -Dminecraft.appel.args=x\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("extra.txt"), "-XX:+UseG1GC").unwrap();
        let args = expand_argfile(&dir.path().join("win_args.txt"), 4).unwrap();
        assert_eq!(
            args,
            vec![
                "-XX:+UseG1GC".to_string(),
                "-p".to_string(),
                "libs/a.jar;libs/b.jar".to_string(),
                "-Dminecraft.appel.args=x".to_string()
            ]
        );
        // A cyclic argfile must not hang.
        std::fs::write(dir.path().join("loop.txt"), "@loop.txt").unwrap();
        assert!(expand_argfile(&dir.path().join("loop.txt"), 4).is_err());
        assert!(expand_argfile(&dir.path().join("missing.txt"), 4).is_err());
    }

    #[test]
    fn artifact_urls_follow_maven_layout() {
        let cfg = RegistryConfig::default();
        let p = ForgeProvider::forge(&cfg).unwrap();
        assert_eq!(
            p.artifact_url("1.20.4-49.1.15", "-installer"),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.4-49.1.15/forge-1.20.4-49.1.15-installer.jar"
        );
        assert_eq!(
            p.metadata_url(),
            "/net/minecraftforge/forge/maven-metadata.xml"
        );
        let n = ForgeProvider::neoforge(&cfg).unwrap();
        assert_eq!(
            n.artifact_url("21.4.118-beta", "-installer"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.4.118-beta/neoforge-21.4.118-beta-installer.jar"
        );
        assert_eq!(
            argfile_for_expectation(ModKind::NeoForge, "21.4.118-beta"),
            "libraries/net/neoforged/neoforge/21.4.118-beta/unix_args.txt"
        );
        assert_eq!(
            argfile_for_expectation(ModKind::Forge, "1.20.4-49.1.15"),
            "libraries/net/minecraftforge/forge/1.20.4-49.1.15/unix_args.txt"
        );
    }

    #[tokio::test]
    async fn plan_wires_installer_and_launch_mode_per_era() {
        let mut routes = BTreeMap::new();
        routes.insert(
            "/net/minecraftforge/forge/maven-metadata.xml",
            METADATA.to_string(),
        );
        routes.insert(
            "/net/minecraftforge/forge/1.20.4-49.1.15/forge-1.20.4-49.1.15-installer.jar.sha1",
            "0123456789abcdef0123456789abcdef01234567".to_string(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("forge".into(), format!("http://127.0.0.1:{}", mock.port));
        let p = ForgeProvider::forge(&cfg).unwrap();

        let listing = p.versions(Some("1.20.4")).await.unwrap();
        assert_eq!(listing.versions.len(), 2);
        assert!(
            listing.versions.iter().all(|v| !v.unstable),
            "beta filtered"
        );

        let plan = p
            .plan(&VersionSelection {
                mc_version: Some("1.20.4".into()),
                exact: Some("1.20.4-49.1.15".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plan.provider, "forge");
        assert_eq!(plan.java_major, 21);
        assert_eq!(
            plan.files.len(),
            1,
            "argfile era: only the installer is fetched"
        );
        assert_eq!(plan.files[0].role, FileRole::Installer);
        assert_eq!(
            plan.files[0].sha1.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert!(!plan.files[0].unverified);
        match &plan.launch {
            LaunchMode::ArgFile { argfile, args } => {
                assert!(argfile.ends_with("unix_args.txt") || argfile.ends_with("win_args.txt"));
                assert!(argfile.contains("1.20.4-49.1.15"));
                assert!(args.is_empty());
            }
            other => panic!("1.20.4 must use the argfile, got {other:?}"),
        }
        let inst = plan.installer.as_ref().unwrap();
        assert_eq!(inst.args, vec!["--installServer".to_string()]);
        assert_eq!(inst.ok_exit_codes, vec![0, 1]);
        assert!(inst
            .expected_artifact
            .as_ref()
            .unwrap()
            .ends_with("_args.txt"));

        // Pre-1.17: the universal jar is fetched and launched with -jar nogui.
        let legacy = p
            .plan(&VersionSelection {
                mc_version: Some("1.16.5".into()),
                exact: Some("1.16.5-36.2.39".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(legacy.files.len(), 2);
        assert_eq!(legacy.files[1].dest, "forge-universal.jar");
        match &legacy.launch {
            LaunchMode::Jar { jar, args } => {
                assert_eq!(jar, "forge-universal.jar");
                assert_eq!(args, &["nogui".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(legacy.java_major, 8, "1.16.5 must run on Java 8");

        // A version nothing can satisfy is a 404 for the UI, not a panic.
        let err = p
            .plan(&VersionSelection {
                mc_version: Some("1.5.2".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }
}

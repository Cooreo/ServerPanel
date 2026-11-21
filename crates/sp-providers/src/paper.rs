//! PaperMC (`api.papermc.io/v2`) and Purpur (`api.purpurmc.org/v2`).
//!
//! Paper's v2 API is what every launcher tool in the ecosystem uses and is stable;
//! a v3 exists but publishes no different data, so we stay on the documented v2 and
//! note it in `docs/providers.md`.
//!
//! Listing shape matters for the wizard: `GET /projects/{slug}` gives the MC
//! versions, and builds only exist per version (`/versions/{mc}/builds`). Fetching
//! "every build of every version" would be ~200 requests, so `versions(None)`
//! returns the *version* list with a note, and `versions(Some(mc))` returns real
//! builds. The UI's second dropdown does exactly that, which is also why the panel
//! never shows a stale build number.
//!
//! Checksums: Paper publishes `checksum` (SHA-256) per download; Purpur publishes
//! `sha256` on the same key path. Both are enforced by `sp_net::download`.

use crate::mojang::MojangProvider;
use crate::{
    CatalogEntry, DownloadFile, FileRole, InstallPlan, LaunchMode, Provider, RegistryConfig,
    VersionInfo, VersionSelection,
};
use async_trait::async_trait;
use serde::Deserialize;
use sp_core::model::{now_ms, ReleaseChannel, ServerKind};
use sp_core::CoreError;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const PAPER_BASE: &str = "https://api.papermc.io";
pub const PURPUR_BASE: &str = "https://api.purpurmc.org";

/// `/v2/projects/{slug}/versions/{mc}/builds/{n}` — one build.
#[derive(Debug, Clone, Deserialize)]
pub struct Build {
    pub project: String,
    pub version: String,
    pub build: u64,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub commits: Vec<Commit>,
    #[serde(default)]
    pub downloads: BTreeMap<String, BuildDownload>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Commit {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildDownload {
    pub name: String,
    pub url: String,
    /// Paper's field; Purpur calls the same thing `sha256` and both are accepted.
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectListing {
    pub name: String,
    #[serde(default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionListing {
    #[serde(default)]
    pub builds: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildListing {
    #[serde(default)]
    pub builds: Vec<Build>,
}

/// Paper / Folia / Velocity / Waterfall — one provider, four projects.
#[derive(Debug, Clone)]
pub struct PaperProvider {
    kind: ServerKind,
    slug: &'static str,
    client: sp_net::HttpClient,
    mojang: Arc<MojangProvider>,
    include_old: bool,
}

impl PaperProvider {
    pub fn paper(cfg: &RegistryConfig, mojang: Arc<MojangProvider>) -> Result<Self, CoreError> {
        Self::new(cfg, mojang, ServerKind::Paper, "paper")
    }
    pub fn folia(cfg: &RegistryConfig, mojang: Arc<MojangProvider>) -> Result<Self, CoreError> {
        Self::new(cfg, mojang, ServerKind::Folia, "folia")
    }
    pub fn velocity(cfg: &RegistryConfig, mojang: Arc<MojangProvider>) -> Result<Self, CoreError> {
        Self::new(cfg, mojang, ServerKind::Velocity, "velocity")
    }
    pub fn waterfall(cfg: &RegistryConfig, mojang: Arc<MojangProvider>) -> Result<Self, CoreError> {
        Self::new(cfg, mojang, ServerKind::Waterfall, "waterfall")
    }

    fn new(
        cfg: &RegistryConfig,
        mojang: Arc<MojangProvider>,
        kind: ServerKind,
        slug: &'static str,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            kind,
            slug,
            client: cfg.client(slug, PAPER_BASE)?,
            mojang,
            include_old: cfg.include_snapshots,
        })
    }

    fn path(&self, tail: &str) -> String {
        format!(
            "/v2/projects/{}/{}",
            self.slug,
            tail.trim_start_matches('/')
        )
    }

    pub async fn mc_versions(&self) -> Result<Vec<String>, CoreError> {
        let listing: ProjectListing = self.client.get_json(&self.path("")).await?;
        Ok(listing.versions)
    }

    pub async fn builds(&self, mc: &str) -> Result<Vec<Build>, CoreError> {
        let listing: BuildListing = self
            .client
            .get_json(&self.path(&format!("versions/{mc}/builds")))
            .await?;
        Ok(listing.builds)
    }

    pub async fn current_build(&self, mc: &str) -> Result<Build, CoreError> {
        self.client
            .get_json(&self.path(&format!("versions/{mc}/builds/current")))
            .await
    }

    /// Convert one build into a UI row (no download list; that is `plan()`'s job).
    pub fn build_to_version(&self, build: &Build) -> VersionInfo {
        let channel = match build.channel.as_deref().unwrap_or("STABLE") {
            "STABLE" => ReleaseChannel::Stable,
            "BETA" => ReleaseChannel::Beta,
            _ => ReleaseChannel::Nightly,
        };
        let unstable = !matches!(channel, ReleaseChannel::Stable);
        let note = if unstable && !self.include_old {
            Some(format!(
                "{} channel build — Paper marks this as not recommended for production",
                build.channel.as_deref().unwrap_or("OLD")
            ))
        } else {
            None
        };
        let downloads = build
            .downloads
            .get("application")
            .map(|d| {
                vec![
                    DownloadFile::new(FileRole::ServerJar, d.url.clone(), self.kind.jar_name())
                        .with_sha256(d.checksum.clone().or_else(|| d.sha256.clone()), None),
                ]
            })
            .unwrap_or_default();
        VersionInfo {
            id: build.build.to_string(),
            display: format!("{} build {}", build.version, build.build),
            mc_version: build.version.clone(),
            channel,
            build: Some(build.build),
            released_at_ms: build.time.as_deref().and_then(crate::mojang::parse_iso),
            java_major: None,
            downloads,
            unstable,
            note,
        }
    }

    async fn plan_build(&self, mc: &str, build: u64) -> Result<Build, CoreError> {
        self.client
            .get_json(&self.path(&format!("versions/{mc}/builds/{build}")))
            .await
    }
}

#[async_trait]
impl Provider for PaperProvider {
    fn kind(&self) -> ServerKind {
        self.kind
    }

    fn name(&self) -> &'static str {
        self.slug
    }

    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        match mc_version {
            None => {
                let versions = self.mc_versions().await?;
                let out = versions
                    .into_iter()
                    .rev()
                    .map(|mc| VersionInfo {
                        id: mc.clone(),
                        display: mc.clone(),
                        mc_version: mc.clone(),
                        channel: ReleaseChannel::Stable,
                        build: None,
                        released_at_ms: None,
                        java_major: self.mojang.java_major_for(&mc).await,
                        downloads: vec![],
                        unstable: false,
                        note: Some("select this version to list builds".into()),
                    })
                    .collect();
                Ok(CatalogEntry {
                    versions: out,
                    fetched_at_ms: now_ms(),
                    stale: false,
                })
            }
            Some(mc) => {
                let mut versions: Vec<VersionInfo> = self
                    .builds(mc)
                    .await?
                    .into_iter()
                    .map(|b| self.build_to_version(&b))
                    .filter(|v| self.include_old || !matches!(v.channel, ReleaseChannel::Nightly))
                    .collect();
                versions.sort_by(|a, b| b.build.unwrap_or(0).cmp(&a.build.unwrap_or(0)));
                Ok(CatalogEntry {
                    versions,
                    fetched_at_ms: now_ms(),
                    stale: false,
                })
            }
        }
    }

    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        let mc = sel
            .mc_version
            .clone()
            .or_else(|| sel.exact.clone())
            .ok_or_else(|| {
                CoreError::validation(
                    "mc_version",
                    format!("{} needs a Minecraft version to resolve a build", self.slug),
                )
            })?;
        let build = match &sel.exact {
            Some(exact) if exact.chars().all(|c| c.is_ascii_digit()) => {
                self.plan_build(&mc, exact.parse::<u64>().unwrap_or(0))
                    .await?
            }
            // `<mc>-<build>` form, as typed in the API or copied from a link.
            Some(exact) => {
                let tail = exact.rsplit('-').next().unwrap_or_default();
                match tail.parse::<u64>() {
                    Ok(b) if b > 0 => self.plan_build(&mc, b).await?,
                    _ => self.current_build(&mc).await?,
                }
            }
            None => self.current_build(&mc).await?,
        };
        let application =
            build
                .downloads
                .get("application")
                .ok_or_else(|| CoreError::Upstream {
                    service: self.slug,
                    status: 200,
                    body: format!("build {} published no application download", build.build),
                })?;
        let info = self.build_to_version(&build);
        let mut notes =
            vec!["Paper replaces the vanilla server: keep `server.properties` as-is".to_string()];
        if let Some(first) = build.commits.first().and_then(|c| c.message.as_ref()) {
            notes.push(format!("Latest change: {}", first.trim()));
        }
        Ok(InstallPlan {
            provider: self.slug,
            version: VersionInfo {
                downloads: info.downloads,
                ..info
            },
            files: vec![DownloadFile::new(
                FileRole::ServerJar,
                application.url.clone(),
                self.kind.jar_name(),
            )
            .with_sha256(
                application
                    .checksum
                    .clone()
                    .or_else(|| application.sha256.clone()),
                None,
            )],
            launch: LaunchMode::Jar {
                jar: self.kind.jar_name().to_string(),
                args: Vec::new(),
            },
            jvm_args: Vec::new(),
            natives_dir: None,
            java_major: self
                .mojang
                .java_major_for(&mc)
                .await
                .unwrap_or_else(|| sp_core::model::required_java_major_for_mc(&mc)),
            properties: BTreeMap::new(),
            installer: None,
            notes,
        })
    }
}

/// Purpur (`api.purpurmc.org/v2`), a Paper fork. Its `latest` endpoint returns one
/// build, so listing builds per version is a single request — which is why Purpur
/// can offer a build list where Paper needs a version list.
#[derive(Debug, Clone)]
pub struct PurpurProvider {
    client: sp_net::HttpClient,
    mojang: Option<Arc<MojangProvider>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurpurVersions {
    #[serde(default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurpurVersionInfo {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub builds: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurpurBuild {
    pub version: String,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub commits: Option<String>,
    pub downloads: PurpurDownloads,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurpurDownloads {
    pub application: PurpurArtifact,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurpurArtifact {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl PurpurProvider {
    pub fn new(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Ok(Self {
            client: cfg.client("purpur", PURPUR_BASE)?,
            mojang: None,
        })
    }

    /// Purpur's build id is `1.21.4-2285-1.21.4`; the middle number is the build.
    pub fn build_number(version: &str) -> Option<u64> {
        version
            .split('-')
            .nth(1)
            .and_then(|s| s.trim_start_matches('v').parse().ok())
    }

    pub fn mc_of(version: &str) -> Option<&str> {
        version.split('-').next().filter(|s| !s.is_empty())
    }
}

#[async_trait]
impl Provider for PurpurProvider {
    fn kind(&self) -> ServerKind {
        ServerKind::Purpur
    }

    fn name(&self) -> &'static str {
        "purpur"
    }

    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        match mc_version {
            None => {
                let listing: PurpurVersions = self.client.get_json("/v2/purpur").await?;
                let out = listing
                    .versions
                    .into_iter()
                    .rev()
                    .map(|mc| VersionInfo {
                        id: mc.clone(),
                        display: mc,
                        mc_version: Default::default(),
                        channel: ReleaseChannel::Stable,
                        build: None,
                        released_at_ms: None,
                        java_major: None,
                        downloads: vec![],
                        unstable: false,
                        note: None,
                    })
                    .collect();
                // mc_version is the join key for the second dropdown.
                let out: Vec<VersionInfo> = out
                    .into_iter()
                    .map(|mut v| {
                        v.mc_version = v.id.clone();
                        v
                    })
                    .collect();
                Ok(CatalogEntry {
                    versions: out,
                    fetched_at_ms: now_ms(),
                    stale: false,
                })
            }
            Some(mc) => {
                let info: PurpurVersionInfo =
                    self.client.get_json(&format!("/v2/purpur/{mc}")).await?;
                let latest: PurpurBuild = self
                    .client
                    .get_json(&format!("/v2/purpur/{mc}/latest"))
                    .await?;
                let mut versions = vec![self.to_version(mc, &latest)];
                // Older builds exist but Purpur only serves `latest` by default;
                // expose the numbering so a user can still pin one explicitly.
                if let Some(max) = info.builds.iter().max() {
                    if let Some(v) = versions.first_mut() {
                        v.build = Some(*max);
                    }
                }
                Ok(CatalogEntry {
                    versions,
                    fetched_at_ms: now_ms(),
                    stale: false,
                })
            }
        }
    }

    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        let mc = sel
            .mc_version
            .as_deref()
            .or(sel.exact.as_deref())
            .ok_or_else(|| {
                CoreError::validation("mc_version", "purpur needs a Minecraft version")
            })?;
        let build: PurpurBuild = self
            .client
            .get_json(&format!("/v2/purpur/{mc}/latest"))
            .await?;
        let version = self.to_version(mc, &build);
        let java_major = match &self.mojang {
            Some(m) => m.java_major_for(mc).await,
            None => None,
        }
        .unwrap_or_else(|| sp_core::model::required_java_major_for_mc(mc));
        Ok(InstallPlan {
            provider: "purpur",
            version,
            files: vec![DownloadFile::new(
                FileRole::ServerJar,
                build.downloads.application.url.clone(),
                ServerKind::Purpur.jar_name(),
            )
            .with_sha256(build.downloads.application.sha256.clone(), None)],
            launch: LaunchMode::Jar {
                jar: ServerKind::Purpur.jar_name().to_string(),
                args: Vec::new(),
            },
            jvm_args: Vec::new(),
            natives_dir: None,
            java_major,
            properties: BTreeMap::new(),
            installer: None,
            notes: vec![
                "Purpur adds per-mob behaviour toggles in purpur.yml — no extra flags needed"
                    .to_string(),
            ],
        })
    }
}

impl PurpurProvider {
    fn to_version(&self, mc: &str, build: &PurpurBuild) -> VersionInfo {
        VersionInfo {
            id: build.version.clone(),
            display: format!("Purpur {}", build.version),
            mc_version: mc.to_string(),
            channel: ReleaseChannel::Stable,
            build: Self::build_number(&build.version),
            released_at_ms: build.time.as_deref().and_then(crate::mojang::parse_iso),
            java_major: None,
            downloads: vec![DownloadFile::new(
                FileRole::ServerJar,
                build.downloads.application.url.clone(),
                ServerKind::Purpur.jar_name(),
            )
            .with_sha256(build.downloads.application.sha256.clone(), None)],
            unstable: false,
            note: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_fixture() -> Build {
        serde_json::from_str(
            r#"{
  "project":"paper","version":"1.21.4","build":418,
  "time":"2024-12-04 01:28:38 +0000","channel":"STABLE",
  "commits":[{"id":"a1b2c3d","time":"2024-12-04 01:00:00 +0000","message":"Update to 1.21.4"}],
  "downloads":{
    "application":{"name":"paper-1.21.4-418.jar","url":"https://p/paper-1.21.4-418.jar","checksum":"cafe"},
    "world-customization":{"name":"world-customization-1.21.4-418.jar","url":"https://p/wc.jar","checksum":"beef"}
  }
}"#,
        )
        .unwrap()
    }

    fn purpur_fixture() -> PurpurBuild {
        serde_json::from_str(
            r#"{
  "version":"1.21.4-2285-1.21.4","timestamp":1733300000000,
  "time":"2024-12-04 10:00:00 +0000",
  "url":"https://api.purpurmc.org/v2/purpur/1.21.4/2285/download",
  "filename":"purpur-1.21.4-2285.jar","commit":"abc123","server":"1.21.4","linux64":"",
  "downloads":{"application":{"name":"purpur-1.21.4-2285.jar","url":"https://p/purpur.jar","sha256":"deadbeef"}}
}"#,
        )
        .unwrap()
    }

    fn paper() -> PaperProvider {
        let cfg = RegistryConfig::default();
        PaperProvider::new(
            &cfg,
            Arc::new(MojangProvider::new(&cfg).unwrap()),
            ServerKind::Paper,
            "paper",
        )
        .unwrap()
    }

    #[test]
    fn paper_build_becomes_a_ui_row_with_a_verified_download() {
        let p = paper();
        let v = p.build_to_version(&build_fixture());
        assert_eq!(v.id, "418");
        assert_eq!(v.display, "1.21.4 build 418");
        assert_eq!(v.channel, ReleaseChannel::Stable);
        assert!(!v.unstable);
        assert_eq!(v.build, Some(418));
        assert!(v.released_at_ms.unwrap() > 1_700_000_000_000);
        assert_eq!(v.downloads.len(), 1, "only the application jar is needed");
        assert_eq!(v.downloads[0].dest, "server.jar");
        assert_eq!(v.downloads[0].sha256.as_deref(), Some("cafe"));
        assert!(!v.downloads[0].unverified);
    }

    #[test]
    fn old_channel_builds_are_flagged_and_note_why() {
        let mut b = build_fixture();
        b.channel = Some("OLD".into());
        let mut p = paper();
        p.include_old = false;
        let v = p.build_to_version(&b);
        assert_eq!(v.channel, ReleaseChannel::Nightly);
        assert!(v.unstable);
        assert!(v.note.unwrap().contains("not recommended"));
    }

    #[test]
    fn purpur_parsing_extracts_the_middle_build_number() {
        let f = purpur_fixture();
        assert_eq!(PurpurProvider::build_number(&f.version), Some(2285));
        assert_eq!(PurpurProvider::mc_of(&f.version), Some("1.21.4"));
        assert_eq!(PurpurProvider::build_number("nonsense"), None);
        let cfg = RegistryConfig::default();
        let prov = PurpurProvider::new(&cfg).unwrap();
        let v = prov.to_version("1.21.4", &f);
        assert_eq!(v.build, Some(2285));
        assert_eq!(v.mc_version, "1.21.4");
        assert_eq!(v.downloads[0].sha256.as_deref(), Some("deadbeef"));
        assert_eq!(v.downloads[0].dest, "server.jar");
    }

    #[tokio::test]
    async fn paper_paths_are_relative_to_the_base_url() {
        let p = paper();
        assert_eq!(p.path(""), "/v2/projects/paper/");
        assert_eq!(
            p.path("versions/1.21.4/builds/current"),
            "/v2/projects/paper/versions/1.21.4/builds/current"
        );
        assert_eq!(
            p.path("/v2/x"),
            "/v2/projects/paper/v2/x",
            "double prefixes are a config bug, not ours to hide"
        );
    }

    #[tokio::test]
    async fn listing_and_plan_against_a_mock_upstream() {
        let b = build_fixture();
        let mut routes = BTreeMap::new();
        routes.insert(
            "/v2/projects/paper",
            r#"{"name":"Paper","version":"2.0.0","url":"/v2","versions":["1.21.2","1.21.4"]}"#
                .to_string(),
        );
        routes.insert(
            "/v2/projects/paper/versions/1.21.4/builds",
            format!(
                r#"{{"project":"paper","version":"1.21.4","builds":[{}]}"#,
                serde_json::to_string(&b).unwrap()
            ),
        );
        routes.insert(
            "/v2/projects/paper/versions/1.21.4/builds/current",
            serde_json::to_string(&b).unwrap(),
        );
        routes.insert(
            "/v2/projects/paper/versions/1.21.4/builds/418",
            serde_json::to_string(&b).unwrap(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("paper".into(), format!("http://127.0.0.1:{}", mock.port));
        let mojang_cfg = cfg.clone();
        let p = PaperProvider::new(
            &cfg,
            Arc::new(MojangProvider::new(&mojang_cfg).unwrap()),
            ServerKind::Paper,
            "paper",
        )
        .unwrap();

        let entry = p.versions(Some("1.21.4")).await.unwrap();
        assert_eq!(entry.versions.len(), 1);
        assert_eq!(entry.versions[0].build, Some(418));

        // The version listing hits `/v2/projects/paper` and nothing else
        // (java_major_for resolves via the mojang base URL → mock 404 → None).
        let versions_entry = p.versions(None).await.unwrap();
        assert_eq!(
            versions_entry.versions.len(),
            2,
            "newest last in the API, reversed for the UI"
        );
        assert_eq!(versions_entry.versions[0].id, "1.21.4");
        assert!(versions_entry.versions[0].note.is_some());

        for sel in [
            VersionSelection {
                mc_version: Some("1.21.4".into()),
                ..Default::default()
            },
            VersionSelection {
                mc_version: Some("1.21.4".into()),
                exact: Some("418".into()),
                ..Default::default()
            },
            VersionSelection {
                mc_version: Some("1.21.4".into()),
                exact: Some("paper-1.21.4-418".into()),
                ..Default::default()
            },
        ] {
            let plan = p.plan(&sel).await.unwrap();
            assert_eq!(plan.provider, "paper");
            assert_eq!(plan.files[0].dest, "server.jar");
            assert_eq!(plan.files[0].sha256.as_deref(), Some("cafe"));
            assert_eq!(plan.version.build, Some(418));
            assert!(plan.notes.iter().any(|n| n.contains("Update to 1.21.4")));
            // No manifest answer from the mock → documented fallback heuristic (21).
            assert_eq!(plan.java_major, 21);
        }

        // Missing mc_version is a 400, not a 500.
        let err = p.plan(&VersionSelection::default()).await.unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[tokio::test]
    async fn purpur_listing_and_plan() {
        let b = purpur_fixture();
        let mut routes = BTreeMap::new();
        routes.insert(
            "/v2/purpur",
            r#"{"project":"purpur","versions":["1.21.3","1.21.4"]}"#.to_string(),
        );
        routes.insert(
            "/v2/purpur/1.21.4",
            r#"{"project":"purpur","version":"1.21.4","builds":[2283,2285]}"#.to_string(),
        );
        routes.insert(
            "/v2/purpur/1.21.4/latest",
            serde_json::to_string(&b).unwrap(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("purpur".into(), format!("http://127.0.0.1:{}", mock.port));
        let p = PurpurProvider::new(&cfg).unwrap();
        let listing = p.versions(None).await.unwrap();
        assert_eq!(listing.versions.len(), 2);
        assert_eq!(listing.versions[0].mc_version, "1.21.4");
        let per_mc = p.versions(Some("1.21.4")).await.unwrap();
        assert_eq!(per_mc.versions.len(), 1);
        assert_eq!(per_mc.versions[0].build, Some(2285));
        let plan = p
            .plan(&VersionSelection {
                mc_version: Some("1.21.4".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plan.files[0].url, "https://p/purpur.jar");
        assert_eq!(plan.java_major, 21);
        assert!(p
            .plan(&VersionSelection::default())
            .await
            .unwrap_err()
            .code()
            .contains("validation"));
    }

    #[tokio::test]
    async fn unreachable_upstream_is_reported_as_such() {
        let routes = BTreeMap::new();
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("paper".into(), format!("http://127.0.0.1:{}", mock.port));
        let p = PaperProvider::new(
            &cfg,
            Arc::new(MojangProvider::new(&cfg).unwrap()),
            ServerKind::Paper,
            "paper",
        )
        .unwrap();
        let err = p.versions(None).await.unwrap_err();
        assert_eq!(
            err.code(),
            "upstream",
            "a 404 is an upstream answer, not a crash"
        );
        match err {
            CoreError::Upstream {
                service, status, ..
            } => {
                assert_eq!(service, "paper");
                assert_eq!(status, 404);
            }
            other => panic!("{other:?}"),
        }
    }
}

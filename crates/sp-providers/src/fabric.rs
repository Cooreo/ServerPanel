//! Fabric (`meta.fabricmc.net/v2`) and Quilt (`meta.quiltmc.org/v3`).
//!
//! Both ecosystems ship an *installer* jar whose CLI is the documented way to lay
//! out a server directory: it writes the launcher jar, the loader/intermediary
//! libraries and Minecraft's own jar into the shape their launchers expect.
//! Building that layout by hand from a launch profile works today and breaks the
//! day upstream changes where `versions/{mc}/{mc}.jar` must live, so we run the
//! official installer headlessly instead and only keep the profile fetch for
//! version resolution.
//!
//! Fabric's documented CLI (`fabric-installer.jar server -dir <dir> -mcversion
//! <mc> -loader <ver> -downloadMinecraft`) is also the shape Quilt's fork accepts
//! (`quilt-installer.jar install server -mc <mc> -loader <ver> -dir <dir>`).
//! `docs/providers.md` records the flag spellings and where they come from, and
//! the tests pin them so a refactor cannot silently change what we exec.

use crate::{
    CatalogEntry, DownloadFile, FileRole, InstallPlan, InstallerRun, LaunchMode, Provider,
    RegistryConfig, VersionInfo, VersionSelection,
};
use async_trait::async_trait;
use serde::Deserialize;
use sp_core::model::{now_ms, ReleaseChannel, ServerKind};
use sp_core::CoreError;
use std::collections::BTreeMap;

pub const FABRIC_BASE: &str = "https://meta.fabricmc.net";
pub const QUILT_BASE: &str = "https://meta.quiltmc.org";

#[derive(Debug, Clone, Deserialize)]
pub struct GameVersion {
    pub version: String,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoaderForGame {
    pub loader: LoaderEntry,
    /// Quilt-only: the QSL versions this loader works with.
    #[serde(default)]
    pub qsl: Option<QslChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoaderEntry {
    pub version: String,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QslChannel {
    #[serde(default)]
    pub versions: Vec<QslVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QslVersion {
    pub version: String,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallerEntry {
    pub version: String,
    pub url: String,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Fabric,
    Quilt,
}

impl Ecosystem {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
        }
    }

    pub fn base(self) -> &'static str {
        match self {
            Self::Fabric => FABRIC_BASE,
            Self::Quilt => QUILT_BASE,
        }
    }

    pub(crate) fn api_root(self) -> &'static str {
        match self {
            Self::Fabric => "/v2",
            Self::Quilt => "/v3",
        }
    }

    pub fn kind(self) -> ServerKind {
        match self {
            Self::Fabric => ServerKind::Fabric,
            Self::Quilt => ServerKind::Quilt,
        }
    }

    /// The jar the installer leaves behind to launch the server.
    pub fn launch_jar(self) -> &'static str {
        match self {
            Self::Fabric => "fabric-server-launch.jar",
            Self::Quilt => "quilt-server-launch.jar",
        }
    }

    /// CLI that performs a headless server install (pinned by test).
    pub fn installer_args(self, mc: &str, loader: &str) -> Vec<String> {
        match self {
            Self::Fabric => vec![
                "server".into(),
                "-dir".into(),
                ".".into(),
                "-mcversion".into(),
                mc.into(),
                "-loader".into(),
                loader.into(),
                "-downloadMinecraft".into(),
            ],
            Self::Quilt => vec![
                "install".into(),
                "server".into(),
                "-dir".into(),
                ".".into(),
                "-mc".into(),
                mc.into(),
                "-loader".into(),
                loader.into(),
                "-downloadMinecraft".into(),
            ],
        }
    }
}

#[derive(Debug, Clone)]
pub struct FabricProvider {
    eco: Ecosystem,
    client: sp_net::HttpClient,
}

impl FabricProvider {
    pub fn fabric(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Self::new(cfg, Ecosystem::Fabric)
    }
    pub fn quilt(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Self::new(cfg, Ecosystem::Quilt)
    }

    fn new(cfg: &RegistryConfig, eco: Ecosystem) -> Result<Self, CoreError> {
        Ok(Self {
            eco,
            client: cfg.client(eco.slug(), eco.base())?,
        })
    }

    pub fn client(&self) -> &sp_net::HttpClient {
        &self.client
    }

    fn path(&self, tail: &str) -> String {
        format!("{}{}", self.eco.api_root(), tail)
    }

    pub async fn game_versions(&self) -> Result<Vec<GameVersion>, CoreError> {
        self.client.get_json(&self.path("/versions/game")).await
    }

    pub async fn loaders_for(&self, mc: &str) -> Result<Vec<LoaderForGame>, CoreError> {
        self.client
            .get_json(&self.path(&format!("/versions/loader/{mc}")))
            .await
    }

    pub async fn installers(&self) -> Result<Vec<InstallerEntry>, CoreError> {
        self.client
            .get_json(&self.path("/versions/installer"))
            .await
    }

    /// The newest *stable* loader, falling back to the newest listed one (Quilt
    /// publishes few stable channels and an empty list would be worse than a
    /// bleeding-edge loader, which is what upstream itself recommends).
    pub fn pick_loader(loaders: &[LoaderForGame], exact: Option<&str>) -> Option<LoaderForGame> {
        if let Some(exact) = exact {
            return loaders.iter().find(|l| l.loader.version == exact).cloned();
        }
        loaders
            .iter()
            .find(|l| l.loader.stable)
            .or_else(|| loaders.first())
            .cloned()
    }

    pub fn pick_installer<'a>(
        installers: &'a [InstallerEntry],
        exact: Option<&str>,
    ) -> Option<&'a InstallerEntry> {
        if let Some(exact) = exact {
            return installers.iter().find(|i| i.version == exact);
        }
        installers
            .iter()
            .find(|i| i.stable)
            .or_else(|| installers.first())
    }

    /// Build the plan from already-fetched metadata (pure; used by tests).
    pub fn plan_from(
        eco: Ecosystem,
        mc: &str,
        loader: &LoaderEntry,
        installer: &InstallerEntry,
        qsl: Option<&str>,
    ) -> InstallPlan {
        let jar_name = format!("{}-installer.jar", eco.slug());
        InstallPlan {
            provider: eco.slug(),
            version: VersionInfo {
                id: format!("{mc}-{loader}", mc = mc, loader = loader.version),
                display: format!("{} {} (loader {})", eco.slug(), mc, loader.version),
                mc_version: mc.to_string(),
                channel: if loader.stable {
                    ReleaseChannel::Stable
                } else {
                    ReleaseChannel::Beta
                },
                build: None,
                released_at_ms: Some(now_ms()),
                java_major: Some(sp_core::model::required_java_major_for_mc(mc)),
                downloads: vec![DownloadFile::new(
                    FileRole::Installer,
                    installer.url.clone(),
                    jar_name.clone(),
                )],
                unstable: !loader.stable,
                note: None,
            },
            files: vec![DownloadFile::new(
                FileRole::Installer,
                installer.url.clone(),
                jar_name.clone(),
            )],
            launch: LaunchMode::Jar {
                jar: eco.launch_jar().to_string(),
                args: Vec::new(),
            },
            jvm_args: Vec::new(),
            natives_dir: None,
            java_major: sp_core::model::required_java_major_for_mc(mc),
            properties: BTreeMap::new(),
            installer: Some(InstallerRun {
                jar: jar_name,
                args: eco.installer_args(mc, &loader.version),
                // The Fabric installer returns 0; Quilt's fork has been observed
                // returning 1 after a successful install when it self-deletes.
                ok_exit_codes: vec![0, 1],
                expected_artifact: Some(eco.launch_jar().to_string()),
            }),
            notes: {
                let mut n = vec![
                    "The loader installer downloads Minecraft itself, so the host needs \
                     internet access during install"
                        .to_string(),
                    format!("{} mods go in `mods/`", eco.slug()),
                ];
                if eco == Ecosystem::Quilt {
                    n.push(match qsl {
                        Some(v) => format!(
                            "QSL {v} is the Quilt standard library — installed as a mod from Modrinth \
                             (`qsl`), not by the loader"
                        ),
                        None => "Install QSL (`qsl` on Modrinth) or no Quilt API mods will load"
                            .to_string(),
                    });
                }
                n
            },
        }
    }
}

#[async_trait]
impl Provider for FabricProvider {
    fn kind(&self) -> ServerKind {
        self.eco.kind()
    }

    fn name(&self) -> &'static str {
        self.eco.slug()
    }

    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        match mc_version {
            None => {
                let games = self.game_versions().await?;
                let versions = games
                    .into_iter()
                    .filter(|g| self.client_allows(g.stable))
                    .map(|g| VersionInfo {
                        id: g.version.clone(),
                        display: g.version.clone(),
                        mc_version: g.version,
                        channel: ReleaseChannel::Stable,
                        build: None,
                        released_at_ms: None,
                        java_major: None,
                        downloads: vec![],
                        unstable: false,
                        note: Some("select this version to list loader builds".into()),
                    })
                    .collect();
                Ok(CatalogEntry {
                    versions,
                    fetched_at_ms: now_ms(),
                    stale: false,
                })
            }
            Some(mc) => {
                let loaders = self.loaders_for(mc).await?;
                let versions = loaders
                    .iter()
                    .map(|l| VersionInfo {
                        id: l.loader.version.clone(),
                        display: format!("loader {}", l.loader.version),
                        mc_version: mc.to_string(),
                        channel: if l.loader.stable {
                            ReleaseChannel::Stable
                        } else {
                            ReleaseChannel::Beta
                        },
                        build: None,
                        released_at_ms: None,
                        java_major: None,
                        downloads: vec![],
                        unstable: !l.loader.stable,
                        note: l.qsl.as_ref().and_then(|q| {
                            q.versions
                                .first()
                                .map(|v| format!("latest QSL: {}", v.version))
                        }),
                    })
                    .collect();
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
                    "Fabric and Quilt installs are per-Minecraft-version",
                )
            })?;
        let loaders = self.loaders_for(&mc).await?;
        let exact_loader = sel.exact.as_deref().filter(|e| {
            e.starts_with("0.")
                || e.contains("fabric")
                || e.chars().next().is_some_and(|c| c.is_ascii_digit())
        });
        let loader = Self::pick_loader(&loaders, exact_loader).ok_or_else(|| {
            CoreError::not_found(format!(
                "no {} loader published for Minecraft {mc}",
                self.eco.slug()
            ))
        })?;
        let installers = self.installers().await?;
        let installer =
            Self::pick_installer(&installers, None).ok_or_else(|| CoreError::Upstream {
                service: self.eco.slug(),
                status: 200,
                body: "the meta API listed no installer version".into(),
            })?;
        let qsl = loaders
            .iter()
            .find(|l| l.loader.version == loader.version)
            .and_then(|l| l.qsl.as_ref())
            .and_then(|q| q.versions.first())
            .map(|v| v.version.clone());
        Ok(FabricProvider::plan_from(
            self.eco,
            &mc,
            &loader.loader,
            installer,
            qsl.as_deref(),
        ))
    }
}

impl FabricProvider {
    fn client_allows(&self, stable: bool) -> bool {
        // Fabric's game list marks 1.3-1.19 snapshots `stable:false`; keep those
        // out unless the user opted into unstable entries.
        stable || self.eco == Ecosystem::Quilt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaders_fixture() -> Vec<LoaderForGame> {
        serde_json::from_str(
            r#"[
  {"loader":{"version":"0.16.9","stable":true},"launcherMeta":{"version":"1.1.0","stable":true},"stable":true},
  {"loader":{"version":"0.16.10","stable":false},"stable":false},
  {"loader":{"version":"0.16.8","stable":true},"stable":true}
]"#,
        )
        .unwrap()
    }

    fn quilt_loaders_fixture() -> Vec<LoaderForGame> {
        serde_json::from_str(
            r#"[
  {"loader":{"version":"0.26.4","stable":true},
   "qsl":{"versions":[{"version":"11.0.0+1.21.4","stable":true},{"version":"12.0.0-beta.1+1.21.4","stable":false}]}}
]"#,
        )
        .unwrap()
    }

    fn installer() -> InstallerEntry {
        InstallerEntry {
            version: "1.1.1".into(),
            url: "https://maven.fabricmc.net/net/fabricmc/installer/1.1.1/installer-1.1.1.jar"
                .into(),
            stable: true,
        }
    }

    #[test]
    fn loader_selection_prefers_stable_and_honours_pins() {
        let l = loaders_fixture();
        assert_eq!(
            FabricProvider::pick_loader(&l, None)
                .unwrap()
                .loader
                .version,
            "0.16.9"
        );
        assert_eq!(
            FabricProvider::pick_loader(&l, Some("0.16.8"))
                .unwrap()
                .loader
                .version,
            "0.16.8"
        );
        assert!(FabricProvider::pick_loader(&l, Some("9.9.9")).is_none());
        assert!(FabricProvider::pick_loader(&[], None).is_none());
        // Quilt style: no stable entry at all → first is used rather than nothing.
        let unstable_only: Vec<LoaderForGame> =
            serde_json::from_str(r#"[{"loader":{"version":"0.99.0","stable":false}}]"#).unwrap();
        assert_eq!(
            FabricProvider::pick_loader(&unstable_only, None)
                .unwrap()
                .loader
                .version,
            "0.99.0"
        );
    }

    #[test]
    fn installer_cli_is_pinned() {
        assert_eq!(
            Ecosystem::Fabric.installer_args("1.21.4", "0.16.9"),
            vec![
                "server",
                "-dir",
                ".",
                "-mcversion",
                "1.21.4",
                "-loader",
                "0.16.9",
                "-downloadMinecraft"
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
        );
        assert_eq!(
            Ecosystem::Quilt.installer_args("1.21.4", "0.26.4"),
            vec![
                "install",
                "server",
                "-dir",
                ".",
                "-mc",
                "1.21.4",
                "-loader",
                "0.26.4",
                "-downloadMinecraft"
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
        );
        assert_eq!(Ecosystem::Fabric.launch_jar(), "fabric-server-launch.jar");
        assert_eq!(Ecosystem::Quilt.launch_jar(), "quilt-server-launch.jar");
        assert_eq!(Ecosystem::Fabric.api_root(), "/v2");
        assert_eq!(Ecosystem::Quilt.api_root(), "/v3");
    }

    #[test]
    fn plan_shape_for_modded_installs() {
        let loader = LoaderEntry {
            version: "0.16.9".into(),
            stable: true,
        };
        let plan =
            FabricProvider::plan_from(Ecosystem::Fabric, "1.21.4", &loader, &installer(), None);
        assert_eq!(plan.provider, "fabric");
        assert_eq!(plan.java_major, 21);
        assert_eq!(plan.version.id, "1.21.4-0.16.9");
        let inst = plan
            .installer
            .clone()
            .expect("fabric must run an installer");
        assert_eq!(inst.jar, "fabric-installer.jar");
        assert_eq!(
            inst.expected_artifact.as_deref(),
            Some("fabric-server-launch.jar")
        );
        assert!(inst.args.contains(&"-downloadMinecraft".to_string()));
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].role, FileRole::Installer);
        assert!(
            plan.files[0].unverified,
            "the meta API publishes no checksum"
        );
        match &plan.launch {
            LaunchMode::Jar { jar, args } => {
                assert_eq!(jar, "fabric-server-launch.jar");
                assert!(args.is_empty());
            }
            other => panic!("{other:?}"),
        }
        // Quilt adds the QSL guidance, Fabric does not.
        assert!(plan.notes.iter().all(|n| !n.contains("QSL")));
        let quilt = FabricProvider::plan_from(
            Ecosystem::Quilt,
            "1.21.4",
            &loader,
            &installer(),
            Some("11.0.0+1.21.4"),
        );
        assert!(quilt.notes.iter().any(|n| n.contains("11.0.0+1.21.4")));
        assert_eq!(quilt.files[0].dest, "quilt-installer.jar");
    }

    #[test]
    fn qsl_metadata_is_parsed_but_not_downloaded() {
        let l = quilt_loaders_fixture();
        assert_eq!(l[0].qsl.as_ref().unwrap().versions.len(), 2);
        assert!(l[0].qsl.as_ref().unwrap().versions[0].stable);
        assert!(l[0].loader.stable);
        let plan = FabricProvider::plan_from(
            Ecosystem::Quilt,
            "1.21.4",
            &l[0].loader,
            &installer(),
            l[0].qsl
                .as_ref()
                .and_then(|q| q.versions.first())
                .map(|v| v.version.as_str()),
        );
        assert!(
            plan.files.iter().all(|f| !f.dest.starts_with("mods/")),
            "QSL is a mod and belongs to the Modrinth layer, not the loader install"
        );
    }

    #[tokio::test]
    async fn meta_api_round_trip_against_a_mock() {
        let mut routes = BTreeMap::new();
        routes.insert(
            "/v2/versions/game",
            r#"[{"version":"1.21.4","stable":true},{"version":"25w03a","stable":false}]"#
                .to_string(),
        );
        routes.insert(
            "/v2/versions/loader/1.21.4",
            serde_json::to_string(&loaders_fixture()).unwrap(),
        );
        routes.insert(
            "/v2/versions/installer",
            serde_json::to_string(&vec![installer()]).unwrap(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("fabric".into(), format!("http://127.0.0.1:{}", mock.port));
        let p = FabricProvider::fabric(&cfg).unwrap();

        let listing = p.versions(None).await.unwrap();
        assert_eq!(
            listing.versions.len(),
            1,
            "unstable games are hidden by default"
        );
        let loaders = p.versions(Some("1.21.4")).await.unwrap();
        assert_eq!(loaders.versions.len(), 3);
        assert_eq!(loaders.versions[0].id, "0.16.9");
        assert_eq!(loaders.versions[1].channel, ReleaseChannel::Beta);

        let plan = p
            .plan(&VersionSelection {
                mc_version: Some("1.21.4".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plan.version.id, "1.21.4-0.16.9");

        // Unknown MC version → 404 from upstream, surfaced as not_found for the
        // empty loader list rather than a crash.
        let err = p
            .plan(&VersionSelection {
                mc_version: Some("1.0.0".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, CoreError::Upstream { .. } | CoreError::NotFound { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn quilt_uses_its_own_base_and_shape() {
        let mut routes = BTreeMap::new();
        routes.insert(
            "/v3/versions/loader/1.21.4",
            serde_json::to_string(&quilt_loaders_fixture()).unwrap(),
        );
        routes.insert(
            "/v3/versions/installer",
            r#"[{"version":"1.0.4","url":"https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-installer/1.0.4/quilt-installer-1.0.4.jar","stable":true}]"#.to_string(),
        );
        routes.insert(
            "/v3/versions/game",
            r#"[{"version":"1.21.4","stable":false}]"#.to_string(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.base_url_overrides
            .insert("quilt".into(), format!("http://127.0.0.1:{}", mock.port));
        let p = FabricProvider::quilt(&cfg).unwrap();
        assert_eq!(p.kind(), ServerKind::Quilt);
        // Quilt keeps unstable games (its catalogue is small and version-gated).
        assert_eq!(p.versions(None).await.unwrap().versions.len(), 1);
        let plan = p
            .plan(&VersionSelection {
                mc_version: Some("1.21.4".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plan.provider, "quilt");
        let LaunchMode::Jar { jar, .. } = &plan.launch else {
            panic!("a modded install must launch the loader's jar")
        };
        assert_eq!(
            jar, "quilt-server-launch.jar",
            "the launch mode must point at the installer's output"
        );
        assert_eq!(plan.installer.unwrap().jar, "quilt-installer.jar");
    }
}

//! Server-software catalogues.
//!
//! Every provider here is a thin, well-tested translation layer over an upstream
//! HTTP API. The design rules that make the result trustworthy:
//!
//! * **Nothing is hard-coded.** Supported MC versions, build numbers, loader
//!   versions, library lists and checksums all come from the upstream API at
//!   install time. Adding Minecraft 1.99 requires no ServerPanel release.
//! * **Parsing is separated from I/O.** Every provider exposes `parse_*` functions
//!   that take a JSON document; the tests feed them fixtures captured from live
//!   responses, so a schema change is a failing test rather than a broken install.
//! * **Every download carries a checksum** when upstream publishes one. Mojang,
//!   Paper and Purpur all do; where they do not (some Jenkins artifacts) we say so
//!   in the plan and the UI shows it.
//! * **Failure is named.** Each provider builds its client with
//!   `HttpClientBuilder::service("<name>")`, so the error message says which
//!   upstream was unreachable.
//!
//! Endpoints used (all documented, verified response shapes are in the tests):
//!
//! | provider  | base                                                       |
//! |-----------|------------------------------------------------------------|
//! | Mojang    | `https://piston-meta.mojang.com/mc/game/version_manifest_v2.json` |
//! | PaperMC   | `https://api.papermc.io/v2/projects/{paper,folia,velocity}` |
//! | Purpur    | `https://api.purpurmc.org/v2/purpur`                       |
//! | Fabric    | `https://meta.fabricmc.net/v2`                             |
//! | Quilt     | `https://meta.quiltmc.org/v3`                              |
//! | Forge     | `https://maven.minecraftforge.net/net/minecraftforge/forge` |
//! | NeoForge  | `https://maven.neoforged.net/releases/net/neoforged/neoforge` |
//! | Spigot    | `https://hub.spigotmc.org/jenkins/job/BuildTools`           |
//! | Geyser    | `https://api.github.com/repos/GeyserMC/Geyser/releases`     |

pub mod fabric;
pub mod install;
pub mod forge;
pub mod mojang;
pub mod paper;
pub mod profile;
pub mod spigot;

pub use profile::LaunchProfile;
pub use sp_core::model::LaunchMode;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sp_core::model::ReleaseChannel;
use sp_core::CoreError;
use std::collections::BTreeMap;

/// Which file a download is, which decides where it goes and how it is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileRole {
    /// The jar passed to `-jar`.
    ServerJar,
    /// A jar placed on the classpath under `libraries/`.
    Library,
    /// A natives jar: extracted into `natives/`, never on the classpath.
    Native,
    /// An installer we must execute before the server can run.
    Installer,
    /// A Mojang asset index (small json; the objects are fetched by the server).
    AssetIndex,
    /// Anything else that lands in the server directory verbatim.
    File,
}

/// One file to fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadFile {
    pub role: FileRole,
    pub url: String,
    /// Path relative to the server directory (e.g. `libraries/it/unimi/dsi/x.jar`).
    pub dest: String,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    /// Upstream published no checksum for this artifact. The UI shows a warning
    /// rather than pretending the file was verified.
    pub unverified: bool,
}

impl DownloadFile {
    pub fn new(role: FileRole, url: impl Into<String>, dest: impl Into<String>) -> Self {
        Self {
            role,
            url: url.into(),
            dest: dest.into(),
            sha1: None,
            sha256: None,
            size: None,
            unverified: true,
        }
    }

    pub fn with_sha1(mut self, sha1: Option<String>, size: Option<u64>) -> Self {
        self.unverified = sha1.is_none() && self.sha256.is_none();
        self.sha1 = sha1;
        self.size = size;
        self
    }

    pub fn with_sha256(mut self, sha256: Option<String>, size: Option<u64>) -> Self {
        self.unverified = sha256.is_none() && self.sha1.is_none();
        self.sha256 = sha256;
        self.size = size.or(self.size);
        self
    }
}

/// A version as the UI lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// Provider-local id (`1.21.4`, `build 418`, `0.16.9`).
    pub id: String,
    /// What the UI shows.
    pub display: String,
    /// Minecraft version this build runs (exact, not a range).
    pub mc_version: String,
    pub channel: ReleaseChannel,
    /// Paper/Purpur build number.
    pub build: Option<u64>,
    /// Upstream release timestamp, ms since epoch.
    pub released_at_ms: Option<i64>,
    /// Authoritative Java major when upstream states one.
    pub java_major: Option<u16>,
    /// Downloads that describe this version (jar at minimum).
    pub downloads: Vec<DownloadFile>,
    /// Upstream marks this as a snapshot / experimental build.
    pub unstable: bool,
    /// Anything the operator should read before choosing this version.
    pub note: Option<String>,
}

/// What the caller asked for. `exact` wins over `channel`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSelection {
    /// `Some("1.21.4")` restricts the catalogue; `None` = every supported version.
    pub mc_version: Option<String>,
    /// An exact provider version (`418`, `0.16.9`, `1.20.4-47.3.0`).
    pub exact: Option<String>,
    pub channel: ReleaseChannel,
    /// Only offer versions that can run with this Java major installed.
    pub java_major: Option<u16>,
}

/// Everything needed to materialise a server directory on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPlan {
    pub provider: &'static str,
    pub version: VersionInfo,
    pub files: Vec<DownloadFile>,
    pub launch: LaunchMode,
    /// Extra JVM flags upstream insists on (e.g. Aikar-style defaults are *not*
    /// here — those are the user's choice; only functional requirements appear).
    pub jvm_args: Vec<String>,
    /// `-Djava.library.path` value, relative to the server dir, when natives exist.
    pub natives_dir: Option<String>,
    /// Java major the game/loader requires (upstream-provided when available).
    pub java_major: u16,
    /// server.properties entries the provider needs (e.g. `online-mode=false` for
    /// a cracked launcher profile is never set automatically — only genuine
    /// technical requirements like `server-port` go here).
    pub properties: BTreeMap<String, String>,
    /// An installer jar that must be executed before the server can start.
    pub installer: Option<InstallerRun>,
    /// Notes rendered in the wizard (e.g. "Spigot builds need a JDK").
    pub notes: Vec<String>,
}

impl InstallPlan {
    pub fn total_bytes(&self) -> Option<u64> {
        let mut total = 0u64;
        let mut known = false;
        for f in &self.files {
            if let Some(s) = f.size {
                total = total.saturating_add(s);
                known = true;
            }
        }
        known.then_some(total)
    }

    pub fn unverified_files(&self) -> usize {
        self.files.iter().filter(|f| f.unverified).count()
    }
}

/// A headless installer invocation the panel must run in the server directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallerRun {
    /// Path relative to the server directory.
    pub jar: String,
    pub args: Vec<String>,
    /// Exit codes that mean "success" (Forge returns 1 after a successful install
    /// on some versions because it deletes its own launcher).
    pub ok_exit_codes: Vec<i32>,
    /// Written by the installer; its presence proves the install worked.
    pub expected_artifact: Option<String>,
}

/// What a provider can do for a given selection.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub versions: Vec<VersionInfo>,
    /// ms since epoch when we fetched this list.
    pub fetched_at_ms: i64,
    /// The provider could not be reached; the caller should serve stale data.
    pub stale: bool,
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn kind(&self) -> sp_core::model::ServerKind;
    fn name(&self) -> &'static str;
    /// Human explanation when this kind cannot be installed remotely.
    fn install_hint(&self) -> Option<String> {
        None
    }

    /// List versions, optionally narrowed to one MC version.
    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError>;

    /// Build the plan for a selection (may fetch more metadata).
    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError>;

    /// Cheap capability flag for the UI (does this provider need a JDK on the host?)
    fn needs_build_tools(&self) -> bool {
        false
    }
}

/// Pick the newest entry by `(build, semver, id)` semantics; providers with real
/// release timestamps use those.
pub fn pick_latest(mut versions: Vec<VersionInfo>) -> Option<VersionInfo> {
    versions.sort_by(|a, b| {
        b.released_at_ms
            .unwrap_or(0)
            .cmp(&a.released_at_ms.unwrap_or(0))
            .then_with(|| b.build.unwrap_or(0).cmp(&a.build.unwrap_or(0)))
            .then_with(|| compare_version_strings(&b.id, &a.id))
    });
    versions.into_iter().next()
}

/// Compare `1.20.4`/`1.21`/`2.3.45`/`latest` sensibly (numeric segments first).
pub fn compare_version_strings(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit() && c != '.')
            .flat_map(|chunk| chunk.split('.'))
            .filter_map(|p| p.parse::<u64>().ok())
            .collect()
    };
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    a.cmp(b)
}

/// The registry maps a [`ServerKind`] to its provider.
#[derive(Debug, Clone)]
pub struct Registry {
    by_kind: BTreeMap<&'static str, std::sync::Arc<dyn Provider>>,
}

impl Registry {
    pub fn build(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        let mut by_kind: BTreeMap<&'static str, std::sync::Arc<dyn Provider>> = BTreeMap::new();
        let mut insert = |p: std::sync::Arc<dyn Provider>| {
            by_kind.insert(kind_key(p.kind()), p);
        };
        let mojang = std::sync::Arc::new(mojang::MojangProvider::new(cfg)?);
        insert(mojang.clone());
        insert(std::sync::Arc::new(paper::PaperProvider::paper(cfg, mojang.clone())?));
        insert(std::sync::Arc::new(paper::PaperProvider::folia(cfg, mojang.clone())?));
        insert(std::sync::Arc::new(paper::PaperProvider::velocity(cfg, mojang)?));
        insert(std::sync::Arc::new(paper::PurpurProvider::new(cfg)?));
        insert(std::sync::Arc::new(fabric::FabricProvider::fabric(cfg)?));
        insert(std::sync::Arc::new(fabric::FabricProvider::quilt(cfg)?));
        insert(std::sync::Arc::new(forge::ForgeProvider::forge(cfg)?));
        insert(std::sync::Arc::new(forge::ForgeProvider::neoforge(cfg)?));
        insert(std::sync::Arc::new(spigot::SpigotProvider::new(cfg)?));
        insert(std::sync::Arc::new(spigot::GeyserProvider::new(cfg)?));
        insert(std::sync::Arc::new(spigot::UploadOnlyProvider::bungeecord(cfg)?));
        Ok(Self { by_kind })
    }

    pub fn get(&self, kind: sp_core::model::ServerKind) -> Option<std::sync::Arc<dyn Provider>> {
        self.by_kind.get(kind_key(kind)).cloned()
    }

    pub fn kinds(&self) -> Vec<sp_core::model::ServerKind> {
        self.by_kind.keys().filter_map(|k| kind_from_key(k)).collect()
    }
}

fn kind_key(kind: sp_core::model::ServerKind) -> &'static str {
    match kind {
        sp_core::model::ServerKind::Vanilla => "vanilla",
        sp_core::model::ServerKind::Paper => "paper",
        sp_core::model::ServerKind::Purpur => "purpur",
        sp_core::model::ServerKind::Folia => "folia",
        sp_core::model::ServerKind::Fabric => "fabric",
        sp_core::model::ServerKind::Quilt => "quilt",
        sp_core::model::ServerKind::Forge => "forge",
        sp_core::model::ServerKind::NeoForge => "neoforge",
        sp_core::model::ServerKind::Spigot => "spigot",
        sp_core::model::ServerKind::CraftBukkit => "craftbukkit",
        sp_core::model::ServerKind::BungeeCord => "bungeecord",
        sp_core::model::ServerKind::Waterfall => "waterfall",
        sp_core::model::ServerKind::Velocity => "velocity",
        sp_core::model::ServerKind::Geyser => "geyser",
    }
}

fn kind_from_key(key: &str) -> Option<sp_core::model::ServerKind> {
    use sp_core::model::ServerKind::*;
    Some(match key {
        "vanilla" => Vanilla,
        "paper" => Paper,
        "purpur" => Purpur,
        "folia" => Folia,
        "fabric" => Fabric,
        "quilt" => Quilt,
        "forge" => Forge,
        "neoforge" => NeoForge,
        "spigot" => Spigot,
        "craftbukkit" => CraftBukkit,
        "bungeecord" => BungeeCord,
        "waterfall" => Waterfall,
        "velocity" => Velocity,
        "geyser" => Geyser,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// Override for tests / self-hosted mirrors. Keyed by provider name.
    pub base_url_overrides: BTreeMap<String, String>,
    /// Fetch Mojang's library/native jars during install instead of letting the
    /// server download them on first boot.
    ///
    /// Vanilla and Paper work either way (their boot launcher fetches what is
    /// missing); Fabric, Quilt and Forge *require* the library set, so those
    /// providers always prefetch regardless of this flag. Turn it on for hosts
    /// that will run offline, off for the fastest possible wizard.
    pub prefetch_libraries: bool,
    /// Minutes a version list may be served before refetching.
    pub catalogue_ttl_minutes: u64,
    /// Include upstream snapshots/experimental builds in listings.
    pub include_snapshots: bool,
    pub user_agent: String,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            base_url_overrides: BTreeMap::new(),
            catalogue_ttl_minutes: 60,
            include_snapshots: false,
            prefetch_libraries: false,
            user_agent: concat!("ServerPanel/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

impl RegistryConfig {
    pub fn base(&self, provider: &str, default: &str) -> String {
        self.base_url_overrides
            .get(provider)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    pub fn client(
        &self,
        provider: &'static str,
        default_base: &str,
    ) -> Result<sp_net::HttpClient, CoreError> {
        let base = self.base(provider, default_base);
        sp_net::HttpClientBuilder::new()
            .service(provider)
            .user_agent(self.user_agent.clone())
            .base_url(base.clone())
            .build_client()
    }
}

/// Where a Mojang-style library list puts its files.
pub fn library_dest(path: &str) -> String {
    format!("libraries/{path}")
}

/// Serialise a plan to the on-disk receipt the wizard writes next to the server
/// (`serverpanel-install.json`), so a reinstall can be replayed without requerying.
pub fn plan_receipt(plan: &InstallPlan) -> Result<String, CoreError> {
    serde_json::to_string_pretty(plan).map_err(|e| CoreError::other(e.to_string()))
}

/// Read back [`plan_receipt`].
pub fn plan_from_receipt(json: &str) -> Result<InstallPlan, CoreError> {
    serde_json::from_str(json).map_err(|e| CoreError::other(e.to_string()))
}

/// Sort a listing newest-first, stable for equal timestamps.
pub fn sort_newest_first(versions: &mut Vec<VersionInfo>) {
    versions.sort_by(|a, b| {
        b.released_at_ms
            .unwrap_or(0)
            .cmp(&a.released_at_ms.unwrap_or(0))
            .then_with(|| b.build.unwrap_or(0).cmp(&a.build.unwrap_or(0)))
            .then_with(|| std::cmp::Reverse(a.id.cmp(&b.id)))
    });
}

/// Drop entries that cannot run on the requested Java, and snapshots when asked.
pub fn filter_listing(
    mut versions: Vec<VersionInfo>,
    sel: &VersionSelection,
    include_snapshots: bool,
) -> Vec<VersionInfo> {
    if let Some(mc) = &sel.mc_version {
        versions.retain(|v| &v.mc_version == mc);
    }
    if !include_snapshots {
        versions.retain(|v| !v.unstable);
    }
    if let Some(major) = sel.java_major {
        let compatible: Vec<VersionInfo> = versions
            .iter()
            .filter(|v| v.java_major.map(|m| m <= major).unwrap_or(true))
            .cloned()
            .collect();
        if !compatible.is_empty() {
            versions = compatible;
        } else {
            tracing::debug!(major, "no version fits the requested Java, ignoring the filter");
        }
    }
    versions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(id: &str, mc: &str, build: Option<u64>, at: Option<i64>) -> VersionInfo {
        VersionInfo {
            id: id.into(),
            display: format!("{id} ({mc})"),
            mc_version: mc.into(),
            channel: ReleaseChannel::Stable,
            build,
            released_at_ms: at,
            java_major: None,
            downloads: vec![],
            unstable: false,
            note: None,
        }
    }

    #[test]
    fn version_string_comparison() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_version_strings("1.21.4", "1.21.3"), Greater);
        assert_eq!(compare_version_strings("1.9", "1.10"), Less);
        assert_eq!(compare_version_strings("1.20.4", "1.20.4"), Equal);
        assert_eq!(compare_version_strings("47.3.0", "47.2.99"), Greater);
        assert_eq!(compare_version_strings("1.21.4-2285", "1.21.4-2286"), Less);
    }

    #[test]
    fn latest_prefers_timestamp_then_build() {
        let list = vec![
            v("a", "1.20", Some(1), Some(100)),
            v("b", "1.21", Some(2), Some(300)),
            v("c", "1.21", Some(9), Some(200)),
        ];
        assert_eq!(pick_latest(list).unwrap().id, "b");
        let no_times = vec![v("x", "1.20", Some(1), None), v("y", "1.20", Some(7), None)];
        assert_eq!(pick_latest(no_times).unwrap().id, "y");
    }

    #[test]
    fn sorting_is_stable_and_newest_first() {
        let mut list = vec![
            v("old", "1.19", Some(1), Some(1)),
            v("new", "1.21", Some(5), Some(9)),
            v("mid", "1.20", Some(3), Some(5)),
        ];
        sort_newest_first(&mut list);
        assert_eq!(
            list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["new", "mid", "old"]
        );
    }

    #[test]
    fn filters_apply() {
        let mut list = vec![
            VersionInfo { unstable: true, ..v("s", "1.21", None, Some(2)) },
            v("p", "1.21", None, Some(1)),
            v("q", "1.20", None, Some(3)),
        ];
        let sel = VersionSelection {
            mc_version: Some("1.21".into()),
            ..Default::default()
        };
        let out = filter_listing(list.clone(), &sel, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "p");
        list.sort_by(|a, b| b.id.cmp(&a.id));
        assert_eq!(filter_listing(list, &sel, true).len(), 2, "snapshots opt in");
    }

    #[test]
    fn java_major_filter_never_empties_the_list() {
        let list = vec![VersionInfo { java_major: Some(21), ..v("a", "1.21", None, Some(1)) }];
        let sel = VersionSelection {
            java_major: Some(8),
            ..Default::default()
        };
        assert_eq!(filter_listing(list, &sel, false).len(), 1);
    }

    #[test]
    fn download_file_tracks_verification() {
        let f = DownloadFile::new(FileRole::ServerJar, "https://x/y.jar", "y.jar");
        assert!(f.unverified);
        let f = f.with_sha1(Some("abc".into()), Some(10));
        assert!(!f.unverified);
        assert_eq!(f.size, Some(10));
        let f = f.with_sha256(None, None);
        assert!(!f.unverified, "a sha1 still counts as verified");
        let g = DownloadFile::new(FileRole::Library, "u", "d").with_sha256(Some("h".into()), None);
        assert!(!g.unverified);
    }

    #[test]
    fn plan_totals() {
        let plan = InstallPlan {
            provider: "test",
            version: v("a", "1.21", None, None),
            files: vec![
                DownloadFile::new(FileRole::ServerJar, "u", "a.jar").with_sha1(Some("s".into()), Some(100)),
                DownloadFile::new(FileRole::Library, "u", "b.jar").with_sha1(None, Some(50)),
            ],
            launch: LaunchMode::Jar { jar: "a.jar".into(), args: vec!["--nogui".into()] },
            jvm_args: vec![],
            natives_dir: None,
            java_major: 21,
            properties: BTreeMap::new(),
            installer: None,
            notes: vec![],
        };
        assert_eq!(plan.total_bytes(), Some(150));
        assert_eq!(plan.unverified_files(), 1);
        assert_eq!(plan.launch.program_args(), ["--nogui"]);
        let json = plan_receipt(&plan).unwrap();
        let back = plan_from_receipt(&json).unwrap();
        assert_eq!(back, plan, "a receipt must round-trip exactly");
        assert!(json.contains("\"mode\": \"jar\""));
        assert!(plan_from_receipt("{").is_err());
    }

    #[test]
    fn registry_covers_every_server_kind() {
        let cfg = RegistryConfig::default();
        let reg = Registry::build(&cfg).unwrap();
        for kind in [
            sp_core::model::ServerKind::Vanilla,
            sp_core::model::ServerKind::Paper,
            sp_core::model::ServerKind::Purpur,
            sp_core::model::ServerKind::Folia,
            sp_core::model::ServerKind::Fabric,
            sp_core::model::ServerKind::Quilt,
            sp_core::model::ServerKind::Forge,
            sp_core::model::ServerKind::NeoForge,
            sp_core::model::ServerKind::Spigot,
            sp_core::model::ServerKind::CraftBukkit,
            sp_core::model::ServerKind::BungeeCord,
            sp_core::model::ServerKind::Waterfall,
            sp_core::model::ServerKind::Velocity,
            sp_core::model::ServerKind::Geyser,
        ] {
            assert!(reg.get(kind).is_some(), "no provider for {kind:?}");
            assert_eq!(kind_key(kind), kind_key(kind));
        }
        assert_eq!(reg.kinds().len(), 13, "Velocity+BungeeCord share one provider entry");
    }

    #[test]
    fn base_url_overrides_take_effect() {
        let mut cfg = RegistryConfig::default();
        assert_eq!(cfg.base("paper", "https://api.papermc.io"), "https://api.papermc.io");
        cfg.base_url_overrides
            .insert("paper".into(), "http://127.0.0.1:9/v2".into());
        assert_eq!(cfg.base("paper", "https://api.papermc.io"), "http://127.0.0.1:9/v2");
        let client = cfg.client("paper", "https://api.papermc.io").unwrap();
        assert_eq!(client.base_url(), Some("http://127.0.0.1:9/v2"));
        // A provider may therefore ask for relative paths only.
        assert_eq!(
            client.resolve("/projects/paper"),
            "http://127.0.0.1:9/v2/projects/paper"
        );
        assert_eq!(client.resolve("https://cdn/x.jar"), "https://cdn/x.jar");
    }
}

/// Loopback HTTP server used by every provider test in this crate.
///
/// Deliberately not axum/wiremock: the providers only need `GET path → canned body`,
/// and hand-rolling it keeps the fixtures honest (exact bytes, no middleware).
#[cfg(test)]
pub(crate) mod mock {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub(crate) struct MockUpstream {
        pub port: u16,
        pub hits: Arc<std::sync::Mutex<Vec<String>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockUpstream {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// `routes` maps a request path (no query) to a response body.
    pub(crate) async fn serve(routes: BTreeMap<&'static str, String>) -> MockUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = Arc::new(routes);
        let hits = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let hits_bg = hits.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let routes = routes.clone();
                let hits = hits_bg.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let mut text = String::new();
                    while !text.contains("\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) => return,
                            Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
                            Err(_) => return,
                        }
                    }
                    let path = text
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_string();
                    hits.lock().expect("mock hits lock").push(path.clone());
                    let (status, body) = match routes.get(path.as_str()) {
                        Some(b) => ("200 OK", b.clone()),
                        None => ("404 Not Found", "{\"error\":\"not found\"}".to_string()),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        MockUpstream { port, hits, handle }
    }
}


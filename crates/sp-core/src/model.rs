//! Domain model. These structs are the API contract; see
//! `docs/architecture.md#type-generation` for how they become TypeScript.

use crate::ids::{BackupId, JavaId, ServerId, TaskId, UserId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Server software / versions
// ---------------------------------------------------------------------------

/// The server implementations ServerPanel can install. Adding a variant means
/// adding a module in `sp-providers` and one line in its registry — see
/// `docs/adding-a-provider.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum ServerKind {
    Vanilla,
    Paper,
    Purpur,
    Folia,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    Spigot,
    CraftBukkit,
    BungeeCord,
    Waterfall,
    Velocity,
    Geyser,
}

impl ServerKind {
    /// The file name the panel standardises each server type's jar to, so the
    /// launcher never has to remember what upstream called the build it fetched.
    /// Providers write to this name; `sp-process` reads it.
    /// Display name for pickers and log lines.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Vanilla => "Vanilla",
            Self::Paper => "Paper",
            Self::Purpur => "Purpur",
            Self::Folia => "Folia",
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
            Self::Spigot => "Spigot",
            Self::CraftBukkit => "CraftBukkit",
            Self::BungeeCord => "BungeeCord",
            Self::Waterfall => "Waterfall",
            Self::Velocity => "Velocity",
            Self::Geyser => "Geyser",
        }
    }

    pub const fn jar_name(&self) -> &'static str {
        match self {
            Self::BungeeCord | Self::Waterfall => "proxy.jar",
            Self::Velocity => "velocity.jar",
            _ => "server.jar",
        }
    }

    pub const ALL: [ServerKind; 14] = [
        ServerKind::Vanilla,
        ServerKind::Paper,
        ServerKind::Purpur,
        ServerKind::Folia,
        ServerKind::Fabric,
        ServerKind::Quilt,
        ServerKind::Forge,
        ServerKind::NeoForge,
        ServerKind::Spigot,
        ServerKind::CraftBukkit,
        ServerKind::BungeeCord,
        ServerKind::Waterfall,
        ServerKind::Velocity,
        ServerKind::Geyser,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Paper => "paper",
            Self::Purpur => "purpur",
            Self::Folia => "folia",
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::Spigot => "spigot",
            Self::CraftBukkit => "craftbukkit",
            Self::BungeeCord => "bungeecord",
            Self::Waterfall => "waterfall",
            Self::Velocity => "velocity",
            Self::Geyser => "geyser",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Vanilla => "Vanilla",
            Self::Paper => "Paper",
            Self::Purpur => "Purpur",
            Self::Folia => "Folia",
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
            Self::Spigot => "Spigot",
            Self::CraftBukkit => "CraftBukkit",
            Self::BungeeCord => "BungeeCord",
            Self::Waterfall => "Waterfall",
            Self::Velocity => "Velocity",
            Self::Geyser => "Geyser",
        }
    }

    /// Proxy software has no world, no `server.properties`, and no online-mode.
    pub fn is_proxy(&self) -> bool {
        matches!(self, Self::BungeeCord | Self::Waterfall | Self::Velocity)
    }

    /// Software that loads Bukkit-style plugins.
    pub fn supports_plugins(&self) -> bool {
        matches!(
            self,
            Self::Paper
                | Self::Purpur
                | Self::Folia
                | Self::Spigot
                | Self::CraftBukkit
                | Self::Vanilla
        ) || self.is_proxy()
    }

    /// Software that loads mods (and therefore has a `mods/` folder).
    pub fn supports_mods(&self) -> bool {
        matches!(self, Self::Fabric | Self::Quilt | Self::Forge | Self::NeoForge)
    }

    /// Which content kinds the Modrinth browser should offer by default.
    pub fn default_content_kinds(&self) -> Vec<ContentKind> {
        if self.is_proxy() {
            return vec![ContentKind::Plugin];
        }
        if self.supports_mods() {
            return vec![ContentKind::Mod, ContentKind::Modpack, ContentKind::Datapack];
        }
        vec![ContentKind::Plugin, ContentKind::Datapack, ContentKind::Mod]
    }

    pub fn from_str_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl std::fmt::Display for ServerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ServerKind {
    type Err = crate::CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str_name(s).ok_or_else(|| crate::CoreError::validation("kind", format!("unknown server kind '{s}'")))
    }
}

/// Minecraft release channel used when listing versions from Mojang's manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum VersionChannel {
    Release,
    Snapshot,
    OldBeta,
    OldRelease,
    /// Anything Mojang reports that we do not classify (never hard-filtered out).
    All,
}

/// A Minecraft version resolved from `version_manifest.json` — never hardcoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GameVersion {
    /// e.g. `1.21.4` or `24w44a`
    pub id: String,
    pub channel: VersionChannel,
    /// `None` for very old versions that predate the manifest's `time` field.
    pub release_time_ms: Option<i64>,
    /// Major Java version Mojang requires for this MC version, when known.
    pub java_version: Option<u16>,
    /// Mojang's per-version manifest URL (client is enough; we derive server).
    pub manifest_url: String,
    /// `true` when the version declares a downloadable server jar.
    pub has_server: bool,
}

/// A concrete build of a server implementation for a Minecraft version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BuildInfo {
    pub kind: ServerKind,
    pub mc_version: String,
    /// Implementation build/version string, e.g. Paper `build 224`.
    pub build: String,
    pub download_url: String,
    /// Lowercase hex digest as published by the provider; `None` only when the
    /// provider publishes none (we then record what we downloaded).
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub java_major: Option<u16>,
    /// Loader name for modded kinds (`fabric`, `forge`, …).
    pub loader: Option<String>,
    pub loader_version: Option<String>,
    /// Extra jars the launcher must place alongside the main jar
    /// (Forge/NeoForge installers, Fabric `intermediary`, Quilt `loader`).
    pub extra_downloads: Vec<DownloadArtifact>,
    /// Set for kinds that need a build step (Spigot BuildTools).
    pub build_recipe: Option<BuildRecipe>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct DownloadArtifact {
    pub url: String,
    /// Relative path inside the server directory.
    pub path: String,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

/// A provider-initiated compile (Spigot's BuildTools).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct BuildRecipe {
    pub tool_jar_url: String,
    pub tool_jar_sha256: Option<String>,
    pub args: Vec<String>,
    pub java_major: u16,
    /// Wall-clock budget shown in the UI ("this can take 10-20 minutes").
    pub expected_minutes: u32,
    pub notes: Option<String>,
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

/// Lifecycle state of a managed server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum ServerStatus {
    /// Directory exists but no server jar is installed yet.
    #[default]
    NotInstalled,
    /// Installed and idle.
    Stopped,
    Starting,
    Running,
    /// `stop` was sent, awaiting process exit.
    Stopping,
    /// Saving the world as part of a scheduled backup.
    Saving,
    /// Exited non-zero and auto-restart is disabled or exhausted.
    Crashed,
    /// Provider is compiling (Spigot BuildTools) or downloading.
    Installing,
    /// Terminal state after an operator killed the process.
    Killed,
}

impl ServerStatus {
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Saving)
    }
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Starting | Self::Stopping | Self::Saving | Self::Installing)
    }
    /// Whether the UI should show the pulsing "online" indicator.
    pub fn is_online(self) -> bool {
        self.is_running()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Saving => "saving",
            Self::Crashed => "crashed",
            Self::Installing => "installing",
            Self::Killed => "killed",
        }
    }
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Auto-restart policy applied when a server exits unexpectedly.
///
/// Deliberately a *flat struct* rather than a serde-tagged enum: one JSON shape
/// for every mode keeps the generated TypeScript simple and makes the settings
/// form a plain two-way binding instead of a discriminated-union dance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RestartPolicy {
    pub mode: RestartMode,
    /// Used by [`RestartMode::OnCrash`]: give up after N consecutive crashes.
    pub max_retries: u32,
    /// Base backoff in seconds; grows linearly with the consecutive crash count.
    pub backoff_secs: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self { mode: RestartMode::OnCrash, max_retries: 3, backoff_secs: 15 }
    }
}

impl RestartPolicy {
    pub const fn on_crash(max_retries: u32, backoff_secs: u64) -> Self {
        Self { mode: RestartMode::OnCrash, max_retries, backoff_secs }
    }
    pub const fn never() -> Self {
        Self { mode: RestartMode::Never, max_retries: 0, backoff_secs: 0 }
    }
    /// Backoff before restart number `consecutive` (1-based).
    pub fn delay_for(&self, consecutive: u32) -> Duration {
        let n = u64::from(consecutive.max(1).min(10));
        Duration::from_secs(self.backoff_secs.max(1) * n)
    }
}

/// When a server should be brought back after an unexpected exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum RestartMode {
    /// Never restart.
    Never,
    /// Restart with backoff, giving up after [`RestartPolicy::max_retries`].
    #[default]
    OnCrash,
    /// Always restart unless the panel stopped the server deliberately.
    Always,
    /// Only start/stop on cron (`scheduled_tasks`), never on crash.
    ScheduledOnly,
}

/// JVM + runtime configuration for one server.
///
/// `#[serde(default)]` on the struct is what lets a partially-filled PATCH be
/// applied as "merge into current", which the settings drawer relies on.
fn default_launch_mode() -> LaunchMode {
    LaunchMode::jar(ServerKind::Vanilla.jar_name())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct ServerRuntimeConfig {
    /// Hard memory limit in MiB (also drives `-Xms`/`-Xmx` when `tune_heap` is set).
    pub memory_mb: u32,
    /// Set `-Xms` to `memory_mb` too — recommended for Minecraft (avoids heap growth stalls).
    pub tune_heap: bool,
    /// Fully replaces the generated flag list when non-empty.
    pub jvm_flags: Vec<String>,
    /// Extra args appended after the jar (`--nogui` is added automatically when needed).
    pub program_args: Vec<String>,
    /// `Some(0)` = auto-select by MC version; `Some(n)` = pin major; `None` = `java_on_path`.
    pub java_major: Option<u16>,
    pub java_on_path: Option<PathBuf>,
    /// Env vars injected into the server process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Seconds to wait for a graceful `stop` before SIGTERM/SIGKILL.
    pub shutdown_timeout_secs: u64,
    /// Force IPv4 stack — fixes `-Djava.net.preferIPv4Stack` style issues on dual-stack hosts.
    pub prefer_ipv4: bool,
    /// Server directory (absolute, canonicalised).
    pub server_dir: PathBuf,
    /// Bind address. `127.0.0.1` by default; LAN requires an explicit opt-in.
    ///
    /// A fresh install on a home machine must not expose an unauthenticated
    /// RCON-free-but-server-query-open port to the whole network, so this is the
    /// loopback address and the UI has to be told to change it.
    pub bind_address: String,
    /// Minecraft server port (0 for proxies, which manage their own).
    pub port: u16,
    /// `-Dcom.sun.management.jmxremote` off by default; when enabled, uses this port.
    pub jmx_port: Option<u16>,
    pub restart_policy: RestartPolicy,
    /// How to point the JVM at the installed software (jar / classpath / argfile).
    #[serde(default = "default_launch_mode")]
    pub launch: LaunchMode,
    /// Run the server with `--world` set to this value (world switching).
    pub world_override: Option<String>,
}

impl Default for ServerRuntimeConfig {
    fn default() -> Self {
        Self {
            memory_mb: 2048,
            tune_heap: true,
            jvm_flags: crate::flags::aikar_flags(),
            program_args: Vec::new(),
            java_major: Some(0),
            java_on_path: None,
            env: BTreeMap::new(),
            shutdown_timeout_secs: 60,
            prefer_ipv4: true,
            server_dir: PathBuf::from("."),
            bind_address: "127.0.0.1".into(),
            port: 25565,
            jmx_port: None,
            restart_policy: RestartPolicy::default(),
            launch: LaunchMode::default(),
            world_override: None,
        }
    }
}

/// A row of the `servers` table plus derived fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Server {
    pub id: ServerId,
    pub name: String,
    pub kind: ServerKind,
    pub mc_version: String,
    /// Implementation build marker (Paper build number, Forge version, …).
    pub build: String,
    pub status: ServerStatus,
    /// Install pipeline state, independent of process lifecycle: the row exists
    /// the moment a wizard is submitted, but the jar may still be downloading.
    pub install_state: InstallState,
    pub config: ServerRuntimeConfig,
    /// `eula.txt` present *and* `eula=true`, plus the logged agreement.
    pub eula_accepted: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Ordering hint for the sidebar.
    pub position: i32,
    /// Ids of servers this one is a clone/template of (lineage only).
    pub template_of: Option<ServerId>,
    /// Optional CPU weight (`nice`-like fairness) exposed to the UI as a percent.
    pub cpu_shares_percent: Option<u32>,
    /// Free-form labels shown as chips in the UI.
    #[serde(default)]
    pub tags: Vec<String>,
    pub last_error: Option<String>,
    /// `server-icon.png` presence, used by the switcher.
    pub has_icon: bool,
}

impl Server {
    pub fn jar_path(&self) -> PathBuf {
        self.config.server_dir.join("server.jar")
    }
}

/// Install-pipeline state for a server row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum InstallState {
    /// Row created, nothing downloaded yet.
    #[default]
    Pending,
    /// Provider is resolving/downloading/extracting (or BuildTools is compiling).
    Running,
    /// Jar + launchers are on disk and verified.
    Ready,
    /// The last attempt failed; `Server::last_error` explains it and the UI offers
    /// a retry.
    Failed,
}

impl InstallState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
    pub fn from_str_name(s: &str) -> Self {
        match s {
            "running" | "installing" => Self::Running,
            "ready" | "complete" | "completed" => Self::Ready,
            "failed" | "error" => Self::Failed,
            _ => Self::Pending,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Failed)
    }
}

/// Response body for `GET /api/servers`: the row plus live process state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ServerWithState {
    pub server: Server,
    pub state: Option<ProcessState>,
    pub stats: Option<ResourceStats>,
    pub installed_size_bytes: Option<u64>,
    pub backup_count: u32,
    pub next_backup_at_ms: Option<i64>,
    /// True when a newer build exists upstream (checked lazily, cached).
    pub update_available: Option<BuildInfo>,
}

/// How a JVM is pointed at its code, decided by the software's provider and stored
/// per server so a restart after a settings change launches the same way.
///
/// Lives in `sp-core` (not `sp-providers`) because both the catalogue layer and the
/// process layer need it and they must not depend on each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum LaunchMode {
    /// `java <flags> -jar <jar> <args>` — the common case.
    Jar {
        /// Path relative to the server directory.
        jar: String,
        args: Vec<String>,
    },
    /// `java <flags> -cp <a:b:c> <mainClass> <args>` — legacy modded launchers.
    Classpath {
        /// Path fragments, joined with the platform path separator.
        entries: Vec<String>,
        main_class: String,
        args: Vec<String>,
    },
    /// `java <flags> @<argfile> <args>` — Forge/NeoForge ≥1.17, whose installer
    /// writes an argfile holding `-cp` plus `-D` properties. The JVM expands `@file`
    /// itself, so no shell is involved.
    ArgFile {
        /// Path relative to the server directory.
        argfile: String,
        args: Vec<String>,
    },
}

impl Default for LaunchMode {
    fn default() -> Self {
        Self::jar("server.jar")
    }
}

impl LaunchMode {
    /// The plain `java -jar server.jar` mode every unmodded server uses.
    pub fn jar(jar: impl Into<String>) -> Self {
        Self::Jar { jar: jar.into(), args: Vec::new() }
    }

    /// Program (not JVM) arguments the provider wants appended.
    pub fn program_args(&self) -> &[String] {
        match self {
            Self::Jar { args, .. } | Self::Classpath { args, .. } | Self::ArgFile { args, .. } => args,
        }
    }

    /// What appears between the JVM flags and the program args.
    pub fn classpath_separator() -> &'static str {
        if cfg!(windows) {
            ";"
        } else {
            ":"
        }
    }
}

/// Live process facts, derived from the process manager (not the DB).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ProcessState {
    pub pid: Option<u32>,
    pub status: ServerStatus,
    pub started_at_ms: Option<i64>,
    /// Whole seconds since the process started (0 when stopped).
    pub uptime_secs: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub consecutive_crashes: u32,
    pub restart_scheduled_at_ms: Option<i64>,
    /// A queued start (server created while at capacity, or dependency order).
    pub pending_start: bool,
    /// `save-off` currently active because a live backup is running.
    pub save_disabled: bool,
    pub last_crash_report: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Resources / monitoring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResourceStats {
    pub sampled_at_ms: i64,
    /// 0.0..=100.0, panel-wide or per-server depending on the endpoint.
    pub cpu_percent: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
    /// Parsed from `TPS:`/MSPT tick output or the spark plugin.
    pub tps: Option<f32>,
    pub mspt: Option<f32>,
    pub players_online: u32,
    pub players_max: u32,
    /// Bytes/sec, from the NIC counters.
    pub net_rx_bps: Option<u64>,
    pub net_tx_bps: Option<u64>,
    pub java_pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct PlayerSample {
    pub t_ms: i64,
    pub online: u32,
    pub max: u32,
}

/// One line of console output. `raw` keeps ANSI escapes so xterm.js can render
/// colours; `plain` is the searchable/filtered form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct LogLine {
    pub seq: u64,
    pub at_ms: i64,
    pub stream: LogStream,
    pub raw: String,
    pub plain: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum LogStream {
    Stdout,
    Stderr,
    /// Panel-generated (install progress, lifecycle notes).
    System,
}

/// A historical log file on disk (including rotated `.log.gz`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct LogFileEntry {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at_ms: i64,
    pub gzipped: bool,
}

/// Parsed `crash-reports/*.txt` summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct CrashReport {
    pub server_id: ServerId,
    pub path: String,
    pub created_at_ms: i64,
    pub headline: String,
    pub caused_by: Vec<String>,
    /// Up to 400 lines; the UI virtualises this.
    pub excerpt: Vec<String>,
    pub aikar_flags_present: bool,
}

// ---------------------------------------------------------------------------
// EULA
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct EulaAgreement {
    pub server_id: ServerId,
    pub accepted_by: Option<UserId>,
    pub accepted_at_ms: i64,
    /// SHA-256 of the EULA text we displayed, so the record is meaningful.
    pub text_sha256: String,
    pub source_url: String,
    pub user_agent: String,
    pub ip: Option<String>,
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct FileEntry {
    pub name: String,
    /// Path relative to the server root, always `/`-separated.
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub modified_at_ms: i64,
    /// MIME type for files, `None` for dirs.
    pub mime: Option<String>,
    pub read_only: bool,
    pub symlink: bool,
}

// ---------------------------------------------------------------------------
// Config / players / world
// ---------------------------------------------------------------------------

/// One parsed `server.properties` key with the metadata needed to render a form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct PropertyEntry {
    pub key: String,
    pub value: String,
    pub default_value: Option<String>,
    pub description: String,
    pub group: PropertyGroup,
    pub kind: PropertyKind,
    pub restart_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum PropertyGroup {
    Gameplay,
    Difficulty,
    World,
    Network,
    Security,
    Queries,
    Chat,
    Advanced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum PropertyKind {
    Bool,
    Int { min: i64, max: i64 },
    String { max_len: usize },
    Enum { options: Vec<String> },
    /// `motd` — rendered with colour-code preview.
    Motd,
    Json,
}

/// Player list entry resolved against Mojang's session/profile services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct PlayerRecord {
    pub uuid: String,
    pub name: String,
    /// Which list(s) this player is on, per server.
    pub whitelisted: bool,
    pub op: bool,
    pub banned: bool,
    /// Populated from `playerdb.co` (documented in `docs/providers.md`).
    pub name_history: Vec<String>,
    pub last_seen_ms: Option<i64>,
}

/// One entry in `banned-players.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct BanEntry {
    pub uuid: String,
    pub name: String,
    pub reason: String,
    pub source: String,
    pub created_ms: i64,
    pub expires_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct WorldSummary {
    pub name: String,
    pub level_name: Option<String>,
    pub seed: Option<String>,
    pub generator: Option<String>,
    pub size_bytes: u64,
    pub modified_at_ms: i64,
    pub is_default: bool,
}

// ---------------------------------------------------------------------------
// Content (Modrinth and local installs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum ContentKind {
    Mod,
    Plugin,
    Datapack,
    Modpack,
    ResourcePack,
    Shader,
    Addon,
}

impl ContentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::Plugin => "plugin",
            Self::Datapack => "datapack",
            Self::Modpack => "modpack",
            Self::ResourcePack => "resourcepack",
            Self::Shader => "shader",
            Self::Addon => "addon",
        }
    }
    /// Directory the file goes into, relative to the server root.
    pub fn install_dir(&self) -> Option<&'static str> {
        match self {
            Self::Mod | Self::Addon => Some("mods"),
            Self::Plugin => Some("plugins"),
            Self::Datapack | Self::ResourcePack => Some("datapacks"),
            Self::Shader => None,
            Self::Modpack => None,
        }
    }
}

/// A file attached to an installed content item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct InstalledContent {
    pub id: i64,
    pub server_id: ServerId,
    pub kind: ContentKind,
    pub project_id: String,
    pub project_slug: String,
    pub title: String,
    pub version_id: String,
    pub version_number: String,
    pub filename: String,
    /// Path relative to the server dir, e.g. `mods/sodium-0.6.jar`.
    pub path: String,
    pub enabled: bool,
    pub mc_versions: Vec<String>,
    pub loaders: Vec<String>,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub installed_at_ms: i64,
    /// Filled in by the update check.
    pub update_to: Option<ContentUpdateTarget>,
    /// Set when the file was added by hand (upload/URL) rather than Modrinth.
    pub source: ContentSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ContentUpdateTarget {
    pub version_id: String,
    pub version_number: String,
    pub filename: String,
    pub url: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum ContentSource {
    Modrinth,
    Upload,
    Url,
    /// Part of a modpack (`mrpack`).
    Modpack,
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct JavaRuntime {
    pub id: JavaId,
    pub major: u16,
    /// Full version string, e.g. `21.0.5+11`.
    pub version: String,
    pub vendor: String,
    pub path: PathBuf,
    pub source: JavaSource,
    pub arch: String,
    pub installed_at_ms: Option<i64>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum JavaSource {
    /// Discovered on `PATH` or in a well-known install location.
    System,
    /// Downloaded into the panel-managed `java/` folder from Adoptium.
    Managed,
    /// Explicitly chosen by the user.
    Manual,
}

/// The Java major version Mojang requires, per MC version.
///
/// This is a *heuristic ladder*, not a version list: it keys off the MC major so
/// new Minecraft releases keep working with zero code changes (the exact value is
/// overridden by `javaVersion.majorVersion` from Mojang's manifest when present —
/// see `sp_providers::mojang::server_manifest`).
pub fn required_java_major_for_mc(mc_version: &str) -> u16 {
    // Mojang's own per-version manifest is authoritative; this ladder is the
    // fallback for old versions and for offline UX while we fetch.
    let Some(minor) = mc_minor(mc_version) else {
        return 21;
    };
    match minor {
        0..=16 => 8,
        17 => 16,
        18..=20 => 17,
        // 1.21.x requires Java 21; anything newer uses whatever we know about
        // from the manifest, defaulting up to 21 rather than down.
        _ => 21,
    }
}

/// Extract the *minor* number from an MC version string.
///
/// * `1.21.4` → `21`
/// * `1.8.9`  → `8`
/// * `24w44a` → `21` — modern snapshot ids encode the year; we map the year to
///   the contemporary minor (`2024 → 1.21`, `2023 → 1.20`, …). This is only ever
///   used for the Java-requirement heuristic below, never to pick a download.
/// * `b1.8.1`, `a1.0.16` → `None` (pre-1.0 versions are all Java 8 anyway).
pub fn mc_minor(mc_version: &str) -> Option<u16> {
    let v = mc_version.trim();
    if let Some(rest) = v.strip_prefix("1.") {
        return rest.split('.').next()?.parse::<u16>().ok();
    }
    // Snapshots look like `24w44a` (year + week).
    let bytes = v.as_bytes();
    if bytes.len() >= 5 && bytes[2] == b'w' && v[..2].bytes().all(|b| b.is_ascii_digit()) {
        let year: u16 = v[..2].parse().ok()?;
        // 2013 (`13w49a`, 1.7.x era) → 10 … 2024 (`24w44a`, 1.21 era) → 21.
        return year.checked_sub(3);
    }
    None
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum BackupFormat {
    Zip,
    TarZstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum BackupKind {
    Manual,
    Scheduled,
    PreRestore,
    Import,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum BackupState {
    Queued,
    Running,
    Completed,
    Failed,
    Restoring,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Backup {
    pub id: BackupId,
    pub server_id: ServerId,
    pub name: String,
    pub kind: BackupKind,
    pub format: BackupFormat,
    pub state: BackupState,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    /// Relative to the backup root, or a remote key for S3/SFTP targets.
    pub object_key: String,
    pub destination_id: String,
    pub created_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub error: Option<String>,
    /// `true` when the backup was taken with `save-off` (safe to restore live).
    pub world_consistent: bool,
    pub included_bytes: u64,
    pub file_count: u32,
    pub restored_count: u32,
    pub last_restored_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct BackupPolicy {
    pub enabled: bool,
    pub cron: String,
    pub format: BackupFormat,
    pub keep_last: u32,
    pub keep_days: u32,
    pub exclude_patterns: Vec<String>,
    /// Run `save-off` / `save-all` / `save-on` around the snapshot.
    pub quiesce: bool,
    pub include_worlds: bool,
    pub include_logs: bool,
    pub destination_id: String,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: "0 4 * * *".into(),
            format: BackupFormat::TarZstd,
            keep_last: 7,
            keep_days: 14,
            exclude_patterns: vec![
                "logs/*.gz".into(),
                "*.tmp".into(),
                "crash-reports".into(),
            ],
            quiesce: true,
            include_worlds: true,
            include_logs: false,
            destination_id: "local".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum TaskAction {
    /// Send a command to the console.
    Command,
    /// `/say` a message with the configured broadcast prefix.
    Broadcast,
    Restart,
    Start,
    Stop,
    Backup,
    /// Check upstream for a newer build / mod updates.
    UpdateCheck,
    /// Delete old backups beyond retention.
    PruneBackups,
    /// Reload the server (`/reload`).
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ScheduledTask {
    pub id: TaskId,
    pub server_id: Option<ServerId>,
    pub name: String,
    pub action: TaskAction,
    /// Cron expression, 5 or 6 fields (see [`crate::cron`]).
    pub cron: String,
    pub payload: String,
    pub enabled: bool,
    pub only_if_running: bool,
    /// Minutes before a restart to warn (0 disables warnings).
    pub warn_before_secs: Vec<u32>,
    pub last_run_at_ms: Option<i64>,
    pub next_run_at_ms: Option<i64>,
    pub last_result: Option<String>,
    pub fail_count: u32,
}

// ---------------------------------------------------------------------------
// Auth / users / audit
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum Role {
    Admin,
    Moderator,
    Viewer,
}

/// Fine-grained capabilities. Roles map to these; per-server grants can add more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum Permission {
    ServerView,
    ServerCreate,
    ServerDelete,
    ServerConfigure,
    ServerControl,
    ConsoleSend,
    FilesRead,
    FilesWrite,
    ContentInstall,
    BackupManage,
    SchedulerManage,
    UserManage,
    PanelSettings,
    PanelUpdate,
    AuditRead,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Moderator => "moderator",
            Self::Viewer => "viewer",
        }
    }
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Self::Admin),
            "moderator" => Some(Self::Moderator),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
    pub fn permissions(&self) -> &'static [Permission] {
        use Permission::*;
        match self {
            Self::Admin => &[
                ServerView, ServerCreate, ServerDelete, ServerConfigure, ServerControl, ConsoleSend,
                FilesRead, FilesWrite, ContentInstall, BackupManage, SchedulerManage, UserManage,
                PanelSettings, PanelUpdate, AuditRead,
            ],
            Self::Moderator => &[
                ServerView,
                ServerCreate,
                ServerConfigure,
                ServerControl,
                ConsoleSend,
                FilesRead,
                FilesWrite,
                ContentInstall,
                BackupManage,
                SchedulerManage,
                AuditRead,
            ],
            Self::Viewer => &[ServerView, FilesRead, AuditRead],
        }
    }
    pub fn permits(&self, p: Permission) -> bool {
        self.permissions().contains(&p)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    pub totp_enabled: bool,
    pub created_at_ms: i64,
    pub last_login_at_ms: Option<i64>,
    pub failed_attempts: u32,
    pub locked_until_ms: Option<i64>,
    /// Extra grants beyond the role (per-server scope in `server_grants`).
    pub extra_permissions: Vec<Permission>,
    pub server_grants: Vec<ServerGrant>,
    pub theme_pref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ServerGrant {
    pub server_id: ServerId,
    pub permissions: Vec<Permission>,
}

impl User {
    pub fn permits(&self, p: Permission) -> bool {
        self.role.permits(p) || self.extra_permissions.contains(&p)
    }
    pub fn permits_on(&self, p: Permission, server: ServerId) -> bool {
        self.permits(p)
            || self
                .server_grants
                .iter()
                .any(|g| g.server_id == server && g.permissions.contains(&p))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct AuditEntry {
    pub id: i64,
    pub at_ms: i64,
    pub user_id: Option<UserId>,
    pub username: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub ip: Option<String>,
    pub outcome: AuditOutcome,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum AuditOutcome {
    Success,
    Failure,
}

// ---------------------------------------------------------------------------
// Progress + events
// ---------------------------------------------------------------------------

/// What a progress event refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum ProgressPhase {
    Resolve,
    Download,
    Verify,
    Extract,
    Build,
    Install,
    Backup,
    Restore,
    JavaInstall,
    SelfUpdate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ProgressEvent {
    /// Stable id so the UI can attach progress to the card that started it.
    pub job_id: String,
    pub server_id: Option<ServerId>,
    pub phase: ProgressPhase,
    /// 0.0..=1.0; `None` = indeterminate spinner.
    pub fraction: Option<f64>,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub rate_bps: Option<f64>,
    pub eta_secs: Option<f64>,
    pub message: String,
    pub at_ms: i64,
    pub finished: bool,
    pub error: Option<String>,
}

impl ProgressEvent {
    pub fn new(job_id: impl Into<String>, phase: ProgressPhase, message: impl Into<String>) -> Self {
        Self {
            job_id: job_id.into(),
            server_id: None,
            phase,
            fraction: None,
            bytes_done: 0,
            bytes_total: None,
            rate_bps: None,
            eta_secs: None,
            message: message.into(),
            at_ms: now_ms(),
            finished: false,
            error: None,
        }
    }

    pub fn with_bytes(mut self, done: u64, total: Option<u64>) -> Self {
        self.bytes_done = done;
        self.bytes_total = total;
        self.fraction = match total {
            Some(t) if t > 0 => Some((done as f64 / t as f64).min(1.0)),
            _ => None,
        };
        self
    }

    pub fn done(mut self) -> Self {
        self.finished = true;
        self.fraction = Some(1.0);
        self
    }

    pub fn fail(mut self, err: &crate::CoreError) -> Self {
        self.finished = true;
        self.error = Some(err.to_string());
        self.fraction = self.fraction.map(|_| 1.0);
        self
    }
}

/// Envelope pushed over `/api/events` (WebSocket).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum PanelEvent {
    Progress(ProgressEvent),
    Log(LogLine),
    Status {
        server_id: ServerId,
        status: ServerStatus,
        reason: Option<String>,
    },
    Stats(ResourceStats),
    PlayerHistory(PlayerSample),
    Notification(NotificationEvent),
    /// Tell the UI to invalidate a TanStack Query key.
    Invalidate { key: Vec<String> },
    Heartbeat { at_ms: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct NotificationEvent {
    pub title: String,
    pub body: String,
    pub level: NotificationLevel,
    pub server_id: Option<ServerId>,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

// ---------------------------------------------------------------------------
// Self-update
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ReleaseInfo {
    pub version: String,
    pub channel: ReleaseChannel,
    pub tag: String,
    pub published_at_ms: i64,
    pub changelog_md: String,
    pub url: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub enum ReleaseChannel {
    Stable,
    Beta,
    Nightly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub target: Option<String>,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Unix milliseconds. Used everywhere instead of `DateTime` so the DB stays a
/// plain INTEGER and the UI formats with `Intl` (one code path, no tz surprises).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Convert to unix ms for the wire (`None` for times before the epoch).
pub fn to_epoch_ms(t: chrono::DateTime<chrono::Utc>) -> i64 {
    t.timestamp_millis()
}

/// Small helper used by tests and the wizard's defaults.
pub fn suggested_heap_mb(total_ram_mb: u64, server_count: usize) -> u32 {
    let per = (total_ram_mb / (server_count.max(1) as u64)).saturating_sub(1024);
    per.clamp(1024, 32_768) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mc_minor_parsing() {
        assert_eq!(mc_minor("1.21.4"), Some(21));
        assert_eq!(mc_minor("1.8.9"), Some(8));
        assert_eq!(mc_minor("24w44a"), Some(21)); // 2024 → 1.21 era
        assert_eq!(mc_minor("13w49a"), Some(10)); // 2013 → 1.7.x era
        assert_eq!(mc_minor("21w03a"), Some(18)); // 2021 → 1.18/1.17 era
        assert_eq!(mc_minor("b1.7.3"), None);
        assert_eq!(mc_minor(""), None);
    }

    #[test]
    fn java_ladder() {
        assert_eq!(required_java_major_for_mc("1.16.5"), 8);
        assert_eq!(required_java_major_for_mc("1.17.1"), 16);
        assert_eq!(required_java_major_for_mc("1.18.2"), 17);
        assert_eq!(required_java_major_for_mc("1.20.4"), 21);
        assert_eq!(required_java_major_for_mc("1.21.4"), 21);
        // Unknown/future must not regress to Java 8.
        assert_eq!(required_java_major_for_mc("1.99"), 21);
    }

    #[test]
    fn kinds_declare_content_dirs() {
        assert_eq!(ContentKind::Mod.install_dir(), Some("mods"));
        assert_eq!(ContentKind::Plugin.install_dir(), Some("plugins"));
        assert_eq!(ContentKind::Modpack.install_dir(), None);
        assert!(ServerKind::Velocity.is_proxy());
        assert!(!ServerKind::Velocity.supports_mods());
        assert!(ServerKind::Fabric.supports_mods());
    }

    #[test]
    fn restart_policy_wire_shape_and_backoff() {
        let p = RestartPolicy::default();
        assert_eq!(p.mode, RestartMode::OnCrash);
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"mode":"on-crash","max_retries":3,"backoff_secs":15}"#);
        let back: RestartPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        // backoff grows linearly, clamps, and never sleeps 0 s
        assert_eq!(p.delay_for(1), Duration::from_secs(15));
        assert_eq!(p.delay_for(3), Duration::from_secs(45));
        assert_eq!(p.delay_for(99), Duration::from_secs(150));
        assert_eq!(RestartPolicy::never().delay_for(0), Duration::from_secs(1));
        assert_eq!(serde_json::to_string(&RestartMode::ScheduledOnly).unwrap(), "\"scheduled-only\"");
    }

    #[test]
    fn status_predicates() {
        assert!(ServerStatus::Running.is_online());
        assert!(ServerStatus::Saving.is_busy());
        assert!(!ServerStatus::Stopped.is_busy());
        assert_eq!(ServerStatus::Crashed.as_str(), "crashed");
    }

    #[test]
    fn role_capability_matrix() {
        assert!(Role::Admin.permits(Permission::UserManage));
        assert!(!Role::Moderator.permits(Permission::UserManage));
        assert!(Role::Moderator.permits(Permission::ConsoleSend));
        assert!(!Role::Viewer.permits(Permission::ConsoleSend));
        assert!(Role::Viewer.permits(Permission::ServerView));
    }

    #[test]
    fn per_server_grant_extends_viewer() {
        let mut u = User {
            id: UserId::new(1),
            username: "u".into(),
            role: Role::Viewer,
            totp_enabled: false,
            created_at_ms: 0,
            last_login_at_ms: None,
            failed_attempts: 0,
            locked_until_ms: None,
            extra_permissions: vec![],
            server_grants: vec![ServerGrant {
                server_id: ServerId::new(9),
                permissions: vec![Permission::ConsoleSend],
            }],
            theme_pref: None,
        };
        assert!(!u.permits(Permission::ConsoleSend));
        assert!(u.permits_on(Permission::ConsoleSend, ServerId::new(9)));
        assert!(!u.permits_on(Permission::ConsoleSend, ServerId::new(10)));
        u.extra_permissions.push(Permission::BackupManage);
        assert!(u.permits(Permission::BackupManage));
    }

    #[test]
    fn progress_fraction_is_clamped() {
        let p = ProgressEvent::new("job-1", ProgressPhase::Download, "downloading")
            .with_bytes(50, Some(100));
        assert_eq!(p.fraction, Some(0.5));
        let p = p.with_bytes(150, Some(100));
        assert_eq!(p.fraction, Some(1.0));
        let p = ProgressEvent::new("j", ProgressPhase::Build, "building").with_bytes(1, None);
        assert_eq!(p.fraction, None);
    }

    #[test]
    fn suggested_heap_reserves_headroom() {
        assert_eq!(suggested_heap_mb(16_384, 1), 15_360);
        // Never propose a heap that starves the OS.
        assert_eq!(suggested_heap_mb(2_048, 4), 1_024);
    }

    #[test]
    fn server_kind_roundtrip() {
        for k in ServerKind::ALL {
            let s = serde_json::to_string(&k).unwrap();
            let de: ServerKind = serde_json::from_str(&s).unwrap();
            assert_eq!(de, k);
            assert_eq!(k.as_str(), ServerKind::from_str_name(k.as_str()).unwrap().as_str());
        }
    }

    #[test]
    fn kebab_case_enums_match_wire_format() {
        assert_eq!(serde_json::to_string(&ServerKind::NeoForge).unwrap(), "\"neoforge\"");
        assert_eq!(serde_json::to_string(&BackupFormat::TarZstd).unwrap(), "\"tar-zstd\"");
        assert_eq!(serde_json::to_string(&VersionChannel::OldBeta).unwrap(), "\"old-beta\"");
    }

    #[test]
    fn enum_with_multi_variant_display() {
        // guards the `matches!(Self::A | Self::B | Self::C, self)` forms
        for k in [ServerKind::BungeeCord, ServerKind::Waterfall, ServerKind::Velocity] {
            assert!(k.is_proxy());
            assert!(k.supports_plugins(), "{k} proxies accept plugins");
            assert!(k.default_content_kinds().contains(&ContentKind::Plugin));
        }
    }
}

/// Regenerates `web/src/lib/api/types.ts` from the Rust types above.
///
/// Run with `cargo test -p sp-core --features codegen`. CI runs it and fails if
/// the committed file differs, so the frontend can never drift from the API.
#[cfg(all(test, feature = "codegen"))]
mod codegen {
    use super::*;
    use ts_rs::TS;

    #[test]
    fn export_bindings() -> ts_rs::Result<()> {
        // Every type the frontend touches must be listed here; the export test
        // walks nested types on its own, but top-level request/response shapes
        // need an explicit call.
        Server::export()?;
        ServerWithState::export()?;
        ServerStatus::export()?;
        ServerKind::export()?;
        ServerRuntimeConfig::export()?;
        RestartPolicy::export()?;
        RestartMode::export()?;
        ProcessState::export()?;
        ResourceStats::export()?;
        GameVersion::export()?;
        BuildInfo::export()?;
        DownloadArtifact::export()?;
        BuildRecipe::export()?;
        VersionChannel::export()?;
        LogLine::export()?;
        LogStream::export()?;
        LogFileEntry::export()?;
        CrashReport::export()?;
        FileEntry::export()?;
        PropertyEntry::export()?;
        PropertyKind::export()?;
        PropertyGroup::export()?;
        PlayerRecord::export()?;
        BanEntry::export()?;
        WorldSummary::export()?;
        InstalledContent::export()?;
        ContentKind::export()?;
        ContentSource::export()?;
        ContentUpdateTarget::export()?;
        JavaRuntime::export()?;
        JavaSource::export()?;
        Backup::export()?;
        BackupPolicy::export()?;
        BackupFormat::export()?;
        BackupKind::export()?;
        BackupState::export()?;
        ScheduledTask::export()?;
        TaskAction::export()?;
        User::export()?;
        Role::export()?;
        Permission::export()?;
        ServerGrant::export()?;
        AuditEntry::export()?;
        AuditOutcome::export()?;
        PanelEvent::export()?;
        ProgressEvent::export()?;
        ProgressPhase::export()?;
        NotificationEvent::export()?;
        NotificationLevel::export()?;
        EulaAgreement::export()?;
        PlayerSample::export()?;
        ReleaseInfo::export()?;
        ReleaseAsset::export()?;
        ReleaseChannel::export()?;
        Ok(())
    }
}

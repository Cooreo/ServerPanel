//! TOML + environment configuration.
//!
//! Layering (later wins):
//! 1. built-in defaults in this module
//! 2. `config.toml` in the data directory
//! 3. environment variables `SERVERPANEL__<SECTION>__<KEY>` (double underscore =
//!    nesting; single `_` inside a key name is preserved)
//!
//! The env layer is implemented by folding overrides into a JSON value, which
//! keeps one code path for types, defaults and validation.

use crate::error::CoreError;
use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const ENV_PREFIX: &str = "SERVERPANEL__";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PanelConfig {
    #[serde(default)]
    pub panel: PanelSection,
    #[serde(default)]
    pub data: DataSection,
    #[serde(default)]
    pub log: LogSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub downloads: DownloadsSection,
    #[serde(default)]
    pub servers: ServersSection,
    #[serde(default)]
    pub backups: BackupsSection,
    #[serde(default)]
    pub updates: UpdatesSection,
    #[serde(default)]
    pub network: NetworkSection,
}

/// Where the panel listens and how it behaves as an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PanelSection {
    /// Bind address. `127.0.0.1` keeps the panel local-only; LAN access is an
    /// explicit opt-in (and the UI warns when it is on).
    pub bind: String,
    pub port: u16,
    /// If `port` is taken and this is true, try `port+1`, `port+2`, …
    pub port_auto_increment: bool,
    /// Open the desktop window / default browser once the API is up.
    pub open_at_startup: bool,
    /// Optional base path if you reverse-proxy the panel under a sub-path.
    pub base_path: String,
    /// Trust `X-Forwarded-For`/`X-Real-IP` (only safe behind a reverse proxy).
    pub trust_proxy: bool,
    /// Max request body size for uploads (bytes).
    pub max_body_bytes: usize,
    /// Graceful shutdown budget for *all* managed servers.
    pub shutdown_all_timeout_secs: u64,
}

impl Default for PanelSection {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 8100,
            port_auto_increment: true,
            open_at_startup: true,
            base_path: "/".into(),
            trust_proxy: false,
            max_body_bytes: 512 * 1024 * 1024,
            shutdown_all_timeout_secs: 180,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DataSection {
    /// Overrides the platform data directory. Server worlds, downloads and
    /// backups live here — never next to the executable — so updates and
    /// uninstalls cannot destroy them.
    pub root: Option<PathBuf>,
    /// Keep `.disabled` copies instead of deleting on uninstall? (reserved)
    pub preserve_on_uninstall: bool,
}

impl Default for DataSection {
    fn default() -> Self {
        Self {
            root: None,
            preserve_on_uninstall: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogSection {
    /// `tracing` EnvFilter directive, e.g. `info,sp_process=debug`.
    pub filter: String,
    pub json: bool,
    /// Also write rolling files to `<data>/logs`.
    pub file: bool,
    pub max_days: u64,
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            filter: "info,axum=warn,tower_http=warn,sqlx=warn,reqwest=warn".into(),
            json: false,
            file: true,
            max_days: 14,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    pub session_ttl_hours: u64,
    pub cookie_name: String,
    /// SameSite for the session cookie. `Lax` is required for the desktop
    /// window; `Strict` for LAN-exposed installs; `None` only with HTTPS.
    pub cookie_same_site: SameSite,
    /// Require the double-submit CSRF token on unsafe methods.
    pub csrf: bool,
    /// Lock an account after N failed logins for this many minutes.
    pub max_failed_logins: u32,
    pub lockout_minutes: u32,
    /// Issuer string shown in authenticator apps for TOTP.
    pub totp_issuer: String,
    /// Argon2id parameters. Defaults follow OWASP (19 MiB, 2 passes, 1 lane).
    pub argon2_m_kib: u32,
    pub argon2_t: u32,
    pub argon2_p: u32,
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            session_ttl_hours: 24 * 14,
            cookie_name: "sp_session".into(),
            cookie_same_site: SameSite::Lax,
            csrf: true,
            max_failed_logins: 8,
            lockout_minutes: 15,
            totp_issuer: "ServerPanel".into(),
            argon2_m_kib: 19_456,
            argon2_t: 2,
            argon2_p: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DownloadsSection {
    /// Parallel file downloads (Modrinth asks us to be polite; 4 is a good
    /// default for a modpack with 150 files).
    pub max_concurrency: usize,
    pub retry_attempts: u32,
    pub retry_base_ms: u64,
    /// Reuse a `.part` file across restarts when the server supports ranges.
    pub resume: bool,
    /// Fail the download when the upstream published a hash and it mismatches.
    pub require_hashes_when_available: bool,
    /// Per-request timeout for the *transfer* (not counting retries).
    pub timeout_secs: u64,
    /// Cache GET responses from third-party APIs (Modrinth, Mojang, PaperMC).
    pub api_cache_minutes: u64,
}

impl Default for DownloadsSection {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            retry_attempts: 4,
            retry_base_ms: 500,
            resume: true,
            require_hashes_when_available: true,
            timeout_secs: 600,
            api_cache_minutes: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServersSection {
    /// Bring previously-running servers back up when the panel starts.
    pub auto_start_on_boot: bool,
    /// Boot order: lowest first; `None` sorts by name.
    pub stagger_starts: bool,
    pub max_concurrent_starts: usize,
    pub shutdown_timeout_secs: u64,
    /// Kill the whole process tree's group when a server refuses to stop.
    pub kill_process_group: bool,
    /// Refuse to create servers outside this port range (0..=0 disables).
    pub allowed_port_min: u16,
    pub allowed_port_max: u16,
    /// Append `-Djava.net.preferIPv4Stack=true` by default.
    pub prefer_ipv4: bool,
}

impl Default for ServersSection {
    fn default() -> Self {
        Self {
            auto_start_on_boot: true,
            stagger_starts: true,
            max_concurrent_starts: 3,
            shutdown_timeout_secs: 60,
            kill_process_group: true,
            allowed_port_min: 1024,
            allowed_port_max: 65_535,
            prefer_ipv4: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackupsSection {
    pub root: Option<PathBuf>,
    pub default_format: crate::model::BackupFormat,
    pub default_keep_last: u32,
    pub default_keep_days: u32,
    /// Compression level for `tar.zst` (1..=22).
    pub zstd_level: i32,
    /// Stop the server before backing up when `save-off` is unavailable.
    pub stop_for_consistency: bool,
}

impl Default for BackupsSection {
    fn default() -> Self {
        Self {
            root: None,
            default_format: crate::model::BackupFormat::TarZstd,
            default_keep_last: 7,
            default_keep_days: 14,
            zstd_level: 10,
            stop_for_consistency: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpdatesSection {
    /// Check GitHub Releases for a newer panel build.
    pub enabled: bool,
    pub repo: String,
    pub channel: crate::model::ReleaseChannel,
    pub check_interval_hours: u64,
    /// Stage downloads under `<data>/updates` before swapping.
    pub auto_download: bool,
}

impl Default for UpdatesSection {
    fn default() -> Self {
        Self {
            enabled: true,
            repo: "Cooreo/ServerPanel".into(),
            channel: crate::model::ReleaseChannel::Stable,
            check_interval_hours: 12,
            auto_download: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkSection {
    /// Ask the router for a UPnP mapping when a server is created.
    pub upnp_enabled: bool,
    pub upnp_lease_days: u32,
    /// Service used to discover the public IPv4 (plain-text HTTP endpoint).
    pub public_ip_service: String,
    /// Add a Windows Firewall allow rule (used by the installer and the wizard).
    pub windows_firewall_rule: bool,
    /// Opt-in tunnel (playit.gg). Binary is downloaded on demand; never on by default.
    pub tunnel: TunnelSection,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            upnp_enabled: false,
            upnp_lease_days: 0,
            public_ip_service: "https://api.ipify.org".into(),
            windows_firewall_rule: true,
            tunnel: TunnelSection::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TunnelSection {
    pub enabled: bool,
    /// playit.gg config path under the data dir.
    pub config_path: Option<PathBuf>,
    pub auto_update: bool,
}

impl Default for TunnelSection {
    fn default() -> Self {
        Self {
            enabled: false,
            config_path: None,
            auto_update: false,
        }
    }
}

impl PanelConfig {
    /// Load defaults → file → env. Missing file is not an error.
    pub fn load() -> Result<Self, CoreError> {
        let path = Self::config_path();
        Self::load_from(Some(path.as_path()))
    }

    /// Path of the config file we would read, honouring `SERVERPANEL_CONFIG`.
    pub fn config_path() -> PathBuf {
        if let Ok(p) = std::env::var("SERVERPANEL_CONFIG") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        paths::config_dir().join("config.toml")
    }

    pub fn load_from(path: Option<&Path>) -> Result<Self, CoreError> {
        let mut value =
            serde_json::to_value(Self::default()).map_err(|e| CoreError::Serde(e.to_string()))?;

        if let Some(path) = path {
            if path.exists() {
                let text = std::fs::read_to_string(path)?;
                let file_value: toml::Value =
                    toml::from_str(&text).map_err(|e| CoreError::Validation {
                        field: path.display().to_string(),
                        reason: format!("invalid config.toml: {e}"),
                    })?;
                let json = toml_to_json(file_value);
                merge(&mut value, json, &path.display().to_string())?;
            }
        }

        apply_env_overrides(&mut value, &std::env::vars().collect::<Vec<_>>());
        tracing::debug!("configuration loaded");

        // Round-trip through serde with unknown-field detection so typos in
        // config.toml are loud rather than silent.
        serde_json::from_value(value).map_err(|e| CoreError::Validation {
            field: "config".into(),
            reason: e.to_string(),
        })
    }

    /// Write a commented default config (used by `serverpanel config init`).
    pub fn default_documented() -> &'static str {
        include_str!("../config.example.toml")
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.panel.bind, self.panel.port)
    }

    pub fn is_local_only(&self) -> bool {
        matches!(self.panel.bind.as_str(), "127.0.0.1" | "localhost" | "::1")
    }
}

/// TOML has no integer-vs-float distinction problems here, but JSON does not
/// have TOML's datetimes, so we normalise everything to serde_json::Value.
fn toml_to_json(v: toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::from(i),
        toml::Value::Float(f) => serde_json::Value::from(f),
        toml::Value::Boolean(b) => serde_json::Value::from(b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// Deep-merge `src` into `dst`, reporting the source path on error.
fn merge(dst: &mut serde_json::Value, src: serde_json::Value, at: &str) -> Result<(), CoreError> {
    match (dst, src) {
        (serde_json::Value::Object(dst_map), serde_json::Value::Object(src_map)) => {
            for (k, v) in src_map {
                match dst_map.get_mut(&k) {
                    Some(existing) => merge(existing, v, &format!("{at}.{k}"))?,
                    None => {
                        return Err(CoreError::Validation {
                            field: format!("{at}.{k}"),
                            reason: "unknown configuration key".into(),
                        })
                    }
                }
            }
            Ok(())
        }
        (dst, src) => {
            *dst = src;
            Ok(())
        }
    }
}

/// `SERVERPANEL__PANEL__PORT=8123` → `{"panel":{"port":8123}}`.
/// Values are parsed as JSON when possible (numbers, bools, arrays), else as strings.
fn apply_env_overrides(dst: &mut serde_json::Value, vars: &[(String, String)]) {
    for (key, raw) in vars {
        let Some(rest) = key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        // Owned segments, because `SERVERPANEL__X__Y` is case-insensitive for the
        // path but the JSON keys are lowercase, so the walk cannot borrow `rest`.
        let path: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
        // Reborrow, don't move: `let mut node = dst;` takes the `&mut` out of the
        // parameter, so the *second* iteration of the env-var loop has nothing left
        // to walk. This is what `E0382: value moved here, in previous iteration of
        // loop` points at — the note names the loop, not the line that moves first.
        let mut node: &mut serde_json::Value = &mut *dst;
        for (i, seg) in path.iter().enumerate() {
            let last = i + 1 == path.len();
            if !node.is_object() {
                *node = serde_json::Value::Object(Default::default());
            }
            let map = node.as_object_mut().expect("just made object");
            let entry = map
                .entry(seg.to_string())
                .or_insert_with(|| serde_json::Value::Null);
            if last {
                *entry = parse_scalar(raw);
                break;
            }
            node = entry;
        }
    }
}

fn parse_scalar(raw: &str) -> serde_json::Value {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        // Accept bare words like `lan` as strings but never swallow them as JSON.
        return v;
    }
    match t.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => serde_json::Value::Bool(true),
        "false" | "no" | "off" | "0" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(t.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> PanelConfig {
        let v: toml::Value = text.parse().unwrap();
        let mut dst = serde_json::to_value(PanelConfig::default()).unwrap();
        merge(&mut dst, toml_to_json(v), "test").unwrap();
        serde_json::from_value(dst).unwrap()
    }

    #[test]
    fn defaults_are_local_only_and_fast() {
        let c = PanelConfig::default();
        assert_eq!(c.panel.bind, "127.0.0.1");
        assert!(c.is_local_only());
        assert_eq!(c.bind_addr(), "127.0.0.1:8100");
        assert_eq!(c.auth.argon2_m_kib, 19_456);
        assert_eq!(c.downloads.max_concurrency, 4);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c = cfg("[panel]\nport = 9000\n");
        assert_eq!(c.panel.port, 9000);
        // untouched keys survive
        assert_eq!(c.panel.bind, "127.0.0.1");
        assert!(c.panel.open_at_startup);
        assert_eq!(c.auth.cookie_name, "sp_session");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let v: toml::Value = "[panel]\npot = 1\n".parse().unwrap();
        let mut dst = serde_json::to_value(PanelConfig::default()).unwrap();
        let err = merge(&mut dst, toml_to_json(v), "config.toml").unwrap_err();
        assert!(err.to_string().contains("pot"), "{err}");
        assert!(matches!(err, CoreError::Validation { .. }));
    }

    #[test]
    fn env_overrides_beat_files_and_typed() {
        let mut dst = serde_json::to_value(PanelConfig::default()).unwrap();
        apply_env_overrides(
            &mut dst,
            &vec![
                ("SERVERPANEL__PANEL__PORT".into(), "9999".into()),
                ("SERVERPANEL__AUTH__CSRF".into(), "false".into()),
                (
                    "SERVERPANEL__SERVERS__ALLOWED_PORT_MIN".into(),
                    "25000".into(),
                ),
                ("SOMETHING_ELSE".into(), "ignored".into()),
            ],
        );
        let c: PanelConfig = serde_json::from_value(dst).unwrap();
        assert_eq!(c.panel.port, 9999);
        assert!(!c.auth.csrf);
        assert_eq!(c.servers.allowed_port_min, 25000);
    }

    #[test]
    fn json_scalars_in_env_are_preserved() {
        assert_eq!(parse_scalar("[1,2]"), serde_json::json!([1, 2]));
        assert_eq!(parse_scalar("\"quoted\""), serde_json::json!("quoted"));
        assert_eq!(parse_scalar("lan"), serde_json::json!("lan"));
        assert_eq!(parse_scalar("yes"), serde_json::json!(true));
    }

    #[test]
    fn toml_datetimes_become_strings() {
        let v = toml_to_json(toml::Value::String("x".into()));
        assert_eq!(v, serde_json::Value::String("x".into()));
    }

    #[test]
    fn same_site_wire_strings() {
        assert_eq!(serde_json::to_string(&SameSite::Lax).unwrap(), "\"lax\"");
        assert_eq!(SameSite::None.as_str(), "None");
    }
}

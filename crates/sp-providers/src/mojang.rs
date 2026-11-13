//! Mojang's official version manifest and launcher profiles.
//!
//! Base: `https://piston-meta.mojang.com`. Endpoints (stable for a decade):
//! * `GET /mc/game/version_manifest_v2.json` → `{latest:{release,snapshot},versions:[…]}`
//! * `GET <versions[].url>` → a [`LaunchProfile`] with `downloads.server`,
//!   `libraries[]`, `assetIndex`, `javaVersion.majorVersion`.
//!
//! Two behaviours worth calling out because they are the difference between "the
//! server starts" and "the server starts *here*":
//!
//! * **Libraries are optional for vanilla.** `server.jar` is a boot launcher: if a
//!   library or native is missing it downloads it into the Mojang repo layout on
//!   first start. `RegistryConfig::prefetch_libraries` decides whether we do that
//!   work up front (offline hosts, faster first boot) or let the server handle it
//!   (fastest wizard). Modded providers always prefetch — their launch profiles are
//!   not self-fetching.
//! * **`mainClass` is advisory for a dedicated server.** Vanilla's server jar
//!   declares its own classpath in its manifest, so we launch with `-jar` and do
//!   *not* build `-cp` from the profile; the profile's `arguments.game` is used only
//!   for what is not substituted (`--username`-style placeholders are dropped).

use crate::{
    profile::{self, AssetIndex, Artifact, LaunchProfile, Library},
    CatalogEntry, DownloadFile, FileRole, InstallPlan, LaunchMode, Provider, RegistryConfig,
    VersionInfo, VersionSelection,
};
use async_trait::async_trait;
use serde::Deserialize;
use sp_core::model::{now_ms, ReleaseChannel, ServerKind};
use sp_core::CoreError;
use std::collections::BTreeMap;

pub const DEFAULT_BASE: &str = "https://piston-meta.mojang.com";

#[derive(Debug, Clone, Deserialize)]
pub struct VersionManifest {
    pub latest: LatestPointers,
    pub versions: Vec<ManifestVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LatestPointers {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestVersion {
    pub id: String,
    pub url: String,
    /// `release` | `snapshot` | `old_beta` | `old_alpha`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "releaseTime")]
    pub release_time: String,
    #[serde(default, rename = "time")]
    pub time: Option<String>,
}

/// A provider for the official `server.jar`.
#[derive(Debug, Clone)]
pub struct MojangProvider {
    client: sp_net::HttpClient,
    prefetch_libraries: bool,
    include_snapshots: bool,
}

impl MojangProvider {
    pub fn new(cfg: &RegistryConfig) -> Result<Self, CoreError> {
        Ok(Self {
            client: cfg.client("mojang", DEFAULT_BASE)?,
            prefetch_libraries: cfg.prefetch_libraries,
            include_snapshots: cfg.include_snapshots,
        })
    }

    /// Exposed so other providers can answer "which Java does this MC version need".
    pub fn client(&self) -> &sp_net::HttpClient {
        &self.client
    }

    pub async fn manifest(&self) -> Result<VersionManifest, CoreError> {
        self.client
            .get_json("/mc/game/version_manifest_v2.json")
            .await
    }

    /// `Some(major)` when Mojang states it; `None` for `latest`/unknown ids so the
    /// caller falls back to [`sp_core::model::required_java_major_for_mc`].
    pub async fn java_major_for(&self, mc_version: &str) -> Option<u16> {
        let id = mc_version.trim();
        if id.is_empty() || id.eq_ignore_ascii_case("latest") {
            return None;
        }
        let manifest = self.manifest().await.ok()?;
        let entry = manifest.versions.iter().find(|v| v.id == id)?;
        let profile: LaunchProfile = self.client.get_json(&entry.url).await.ok()?;
        profile.java_version.map(|j| j.major_version)
    }

    pub async fn profile_for(&self, mc_version: &str) -> Result<LaunchProfile, CoreError> {
        let manifest = self.manifest().await?;
        let entry = manifest
            .versions
            .iter()
            .find(|v| v.id == mc_version)
            .ok_or_else(|| CoreError::not_found(format!("Minecraft version {mc_version}")))?;
        self.client.get_json(&entry.url).await
    }

    /// Turn a manifest listing into UI rows.
    pub fn parse_manifest(manifest: &VersionManifest) -> Vec<VersionInfo> {
        manifest
            .versions
            .iter()
            .map(|v| {
                let unstable = v.kind != "release";
                VersionInfo {
                    id: v.id.clone(),
                    display: v.id.clone(),
                    mc_version: v.id.clone(),
                    channel: match v.kind.as_str() {
                        "snapshot" => ReleaseChannel::Beta,
                        "release" => ReleaseChannel::Stable,
                        _ => ReleaseChannel::Nightly,
                    },
                    build: None,
                    released_at_ms: parse_iso(&v.release_time),
                    java_major: None,
                    downloads: vec![DownloadFile::new(
                        FileRole::ServerJar,
                        format!("manifest:{}", v.id),
                        ServerKind::Vanilla.jar_name(),
                    )],
                    unstable,
                    note: if matches!(v.kind.as_str(), "old_beta" | "old_alpha") {
                        Some("historical build: pre-1.3 server, no online-mode support".into())
                    } else {
                        None
                    },
                }
            })
            .collect()
    }

    /// Build the file list + launch mode from a profile. Pure, so the shape of
    /// Mojang's JSON is covered by tests without touching the network.
    pub fn plan_from_profile(
        profile: &LaunchProfile,
        prefetch_libraries: bool,
    ) -> Result<InstallPlan, CoreError> {
        let server = profile
            .downloads
            .get("server")
            .ok_or_else(|| CoreError::Upstream {
                service: "mojang",
                status: 200,
                body: format!("profile {} has no `server` download", profile.id),
            })?;
        let mut files = vec![DownloadFile::new(
            FileRole::ServerJar,
            server.url.clone(),
            ServerKind::Vanilla.jar_name(),
        )
        .with_sha1(server.sha1.clone(), server.size)];

        let feats = BTreeMap::new();
        if prefetch_libraries {
            files.extend(library_files(&profile.libraries, &feats));
        }

        let mut notes = Vec::new();
        if !prefetch_libraries {
            notes.push(
                "Libraries are fetched by the server itself on first start \
                 (~10 s with internet; enable `prefetch_libraries` for offline hosts)"
                    .to_string(),
            );
        }
        let natives = profile
            .libraries
            .iter()
            .filter(|l| !l.natives.is_empty() && profile::rules_allow(&l.rules, &feats))
            .next()
            .map(|_| "natives".to_string());

        Ok(InstallPlan {
            provider: "mojang",
            version: VersionInfo {
                id: profile.id.clone(),
                display: profile.id.clone(),
                mc_version: profile.id.clone(),
                channel: if profile.type_.as_deref() == Some("snapshot") {
                    ReleaseChannel::Beta
                } else {
                    ReleaseChannel::Stable
                },
                build: None,
                released_at_ms: profile.release_time.as_deref().and_then(parse_iso),
                java_major: profile.java_version.as_ref().map(|j| j.major_version),
                downloads: files.clone(),
                unstable: profile.type_.as_deref() == Some("snapshot"),
                note: None,
            },
            files,
            launch: LaunchMode::Jar {
                jar: ServerKind::Vanilla.jar_name().to_string(),
                args: crate::profile::game_args(profile, &feats),
            },
            jvm_args: profile::jvm_args(profile, &feats),
            natives_dir: natives,
            java_major: profile
                .java_version
                .as_ref()
                .map(|j| j.major_version)
                .unwrap_or_else(|| sp_core::model::required_java_major_for_mc(&profile.id)),
            properties: BTreeMap::new(),
            installer: None,
            notes,
        })
    }

    /// Asset index + object roots, for callers that want them (modpack installers
    /// do; a plain server install does not).
    pub fn asset_index(profile: &LaunchProfile) -> Option<&AssetIndex> {
        profile.asset_index.as_ref()
    }
}

/// Every jar a launch profile needs on disk, with upstream checksums.
///
/// Rules are evaluated for this OS; native classifier jars become `Native` files
/// (extracted at start, never on the classpath).
pub fn library_files(libraries: &[Library], feats: &BTreeMap<String, bool>) -> Vec<DownloadFile> {
    let mut out = Vec::with_capacity(libraries.len());
    for lib in libraries {
        if !profile::rules_allow(&lib.rules, feats) {
            continue;
        }
        if let Some(dl) = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
            out.push(
                DownloadFile::new(FileRole::Library, dl.url.clone(), crate::library_dest(&dl.path))
                    .with_sha1(dl.sha1.clone(), dl.size),
            );
        } else if let (Some(base), Some(path)) = (
            lib.url.as_deref(),
            profile::maven_path(&lib.name),
        ) {
            // Maven-only libraries (Fabric/Quilt profiles use these).
            let url = format!("{}/{}", base.trim_end_matches('/'), path);
            out.push(DownloadFile::new(FileRole::Library, url, crate::library_dest(&path)));
        } else {
            tracing::debug!(library = %lib.name, "library has no downloadable artifact");
            continue;
        }
        let native_key = lib.natives.get(profile::os_name());
        if let (Some(_), Some(classifiers)) = (native_key, lib.downloads.as_ref().map(|d| &d.classifiers)) {
            // The value can carry an arch placeholder: `"natives-windows-x86"`.
            let key = native_key
                .cloned()
                .unwrap_or_default()
                .replace("${arch}", if cfg!(target_pointer_width = "64") { "64" } else { "32" });
            let found = classifiers
                .get(&key)
                .or_else(|| native_key.and_then(|k| classifiers.get(k.as_str())));
            if let Some(art) = found {
                out.push(native_file(art, lib));
            }
        }
    }
    out
}

fn native_file(art: &Artifact, _lib: &Library) -> DownloadFile {
    DownloadFile::new(
        FileRole::Native,
        art.url.clone(),
        format!("natives/{}", art.path.rsplit('/').next().unwrap_or("native.jar")),
    )
    .with_sha1(art.sha1.clone(), art.size)
}

/// Parse `2024-12-03T12:00:00+00:00` (and the space-separated variant Paper uses).
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    use chrono::{DateTime, FixedOffset};
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = DateTime::<FixedOffset>::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z") {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = DateTime::<FixedOffset>::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.naive_local().and_utc().timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    None
}

#[async_trait]
impl Provider for MojangProvider {
    fn kind(&self) -> ServerKind {
        ServerKind::Vanilla
    }

    fn name(&self) -> &'static str {
        "mojang"
    }

    async fn versions(&self, mc_version: Option<&str>) -> Result<CatalogEntry, CoreError> {
        let manifest = self.manifest().await?;
        let mut versions = Self::parse_manifest(&manifest);
        if let Some(mc) = mc_version {
            versions.retain(|v| v.mc_version == mc);
        }
        if !self.include_snapshots {
            versions.retain(|v| !v.unstable);
        }
        crate::sort_newest_first(&mut versions);
        Ok(CatalogEntry {
            versions,
            fetched_at_ms: now_ms(),
            stale: false,
        })
    }

    async fn plan(&self, sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
        let manifest = self.manifest().await?;
        let id = resolve_id(&manifest, sel)?;
        let entry = manifest
            .versions
            .iter()
            .find(|v| v.id == id)
            .ok_or_else(|| CoreError::not_found(format!("Minecraft version {id}")))?;
        let mut profile: LaunchProfile = self.client.get_json(&entry.url).await?;
        // Profiles used by mod loaders can inherit; merge so the plan is complete.
        if let Some(parent_id) = profile.inherits_from.clone() {
            let parent = self.profile_for(&parent_id).await?;
            merge_into(&mut profile, &parent);
        }
        let mut plan = Self::plan_from_profile(&profile, self.prefetch_libraries)?;
        if let Some(exact) = &sel.exact {
            plan.version.note = Some(format!("pinned build {exact}"));
        }
        Ok(plan)
    }
}

fn resolve_id(manifest: &VersionManifest, sel: &VersionSelection) -> Result<String, CoreError> {
    if let Some(exact) = &sel.exact {
        let known = manifest.versions.iter().any(|v| &v.id == exact);
        if !known {
            return Err(CoreError::not_found(format!(
                "Minecraft version {exact} is not in Mojang's manifest"
            )));
        }
        return Ok(exact.clone());
    }
    if let Some(mc) = &sel.mc_version {
        if manifest.versions.iter().any(|v| &v.id == mc) {
            return Ok(mc.clone());
        }
    }
    Ok(match sel.channel {
        ReleaseChannel::Stable => manifest.latest.release.clone(),
        ReleaseChannel::Beta | ReleaseChannel::Nightly => manifest.latest.snapshot.clone(),
    })
}

/// Merge a parent profile into a child the way the launcher does: the child's
/// arrays are prepended to the parent's, its scalars win.
fn merge_into(child: &mut LaunchProfile, parent: &LaunchProfile) {
    let mut libs = parent.libraries.clone();
    libs.extend(child.libraries.iter().cloned());
    child.libraries = libs;
    if child.main_class.is_none() {
        child.main_class = parent.main_class.clone();
    }
    if child.downloads.is_empty() {
        child.downloads = parent.downloads.clone();
    }
    if child.asset_index.is_none() {
        child.asset_index = parent.asset_index.clone();
    }
    if child.assets.is_none() {
        child.assets = parent.assets.clone();
    }
    if child.java_version.is_none() {
        child.java_version = parent.java_version.clone();
    }
    if child.arguments.is_none() {
        child.arguments = parent.arguments.clone();
    }
    child.inherits_from = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_fixture() -> VersionManifest {
        serde_json::from_str(
            r#"{
  "latest": { "isolation": "1.21.4", "release": "1.21.4", "snapshot": "25w03a" },
  "versions": [
    {"id":"25w03a","url":"https://p/25w03a.json","type":"snapshot","releaseTime":"2025-01-15T12:00:00+00:00","time":"2025-01-15T12:00:00+00:00"},
    {"id":"1.21.4","url":"https://p/1.21.4.json","type":"release","releaseTime":"2024-12-03T11:38:23+00:00"},
    {"id":"1.8.9","url":"https://p/1.8.9.json","type":"release","releaseTime":"2015-12-03T09:00:00+00:00"},
    {"id":"b1.8.1","url":"https://p/b.json","type":"old_beta","releaseTime":"2011-09-15T00:00:00+00:00"}
  ]
}"#,
        )
        .unwrap()
    }

    fn profile_fixture() -> LaunchProfile {
        serde_json::from_str(
            r#"{
  "id":"1.21.4","type":"release","releaseTime":"2024-12-03T11:38:23+00:00",
  "mainClass":"net.minecraft.server.Main",
  "javaVersion":{"majorVersion":21},
  "arguments":{"game":["--username","${auth}"],"jvm":["-XX:+UseG1GC","-Djava.library.path=${natives_directory}","-cp"]},
  "downloads":{"server":{"url":"https://p/server.jar","sha1":"11aa","size":51039903}},
  "assetIndex":{"id":"16","url":"https://p/16.json","totalSize":654321},
  "libraries":[
    {"name":"com.mojang:logging:1.2.7","downloads":{"artifact":{"path":"com/mojang/logging/1.2.7/logging-1.2.7.jar","url":"https://p/logging.jar","sha1":"aa","size":9}},"rules":[{"action":"allow"}]},
    {"name":"org.lwjgl:lwjgl:3.3.4","downloads":{"artifact":{"path":"a","url":"https://p/x"},"classifiers":{"natives-windows":{"path":"lw/natives-windows.jar","url":"https://p/nat-win.jar","sha1":"bb","size":7},"natives-linux":{"path":"lw/natives-linux.jar","url":"https://p/nat-linux.jar","sha1":"cc","size":8}}},"natives":{"windows":"natives-windows","linux":"natives-linux"},"rules":[{"action":"allow"}]},
    {"name":"com.mojang:bad-os-only:1","downloads":{"artifact":{"path":"z","url":"https://p/z"}},"rules":[{"action":"allow","os":{"name":"osx"}}]}
  ]
}"#,
        )
        .unwrap()
    }

    #[test]
    fn manifest_listing_marks_channels_and_sorts_newest_first() {
        let m = manifest_fixture();
        assert_eq!(m.latest.release, "1.21.4");
        let mut v = MojangProvider::parse_manifest(&m);
        assert_eq!(v.len(), 4);
        assert!(v.iter().any(|e| e.unstable && e.id == "25w03a"));
        assert_eq!(
            v.iter().find(|e| e.id == "b1.8.1").unwrap().channel,
            ReleaseChannel::Nightly,
            "old_* builds are not beta releases"
        );
        assert!(v.iter().find(|e| e.id == "b1.8.1").unwrap().note.is_some());
        crate::sort_newest_first(&mut v);
        assert_eq!(v[0].id, "25w03a");
        assert_eq!(v[1].id, "1.21.4");
        // Released timestamps are real ms, not zeros.
        assert!(v[1].released_at_ms.unwrap() > 1_700_000_000_000);
    }

    #[test]
    fn plan_uses_jar_mode_and_the_canonical_name() {
        let p = profile_fixture();
        let plan = MojangProvider::plan_from_profile(&p, false).unwrap();
        assert_eq!(plan.provider, "mojang");
        assert_eq!(plan.java_major, 21);
        assert_eq!(plan.version.mc_version, "1.21.4");
        match &plan.launch {
            LaunchMode::Jar { jar, args } => {
                assert_eq!(jar, "server.jar");
                assert_eq!(args, &["--username".to_string(), "${auth}".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(plan.files.len(), 1, "no prefetch means only the jar");
        assert_eq!(plan.files[0].sha1.as_deref(), Some("11aa"));
        assert_eq!(plan.files[0].size, Some(51039903));
        assert!(!plan.files[0].unverified);
        assert_eq!(plan.total_bytes(), Some(51039903));
        assert!(!plan.jvm_args.contains(&"-cp".to_string()));
        assert!(!plan
            .jvm_args
            .iter()
            .any(|a| a.starts_with("-Djava.library.path")));
        assert!(plan.notes.iter().any(|n| n.contains("first start")));
    }

    #[test]
    fn prefetch_adds_libraries_and_natives_for_this_os() {
        let p = profile_fixture();
        let plan = MojangProvider::plan_from_profile(&p, true).unwrap();
        let libs: Vec<&DownloadFile> = plan
            .files
            .iter()
            .filter(|f| f.role == FileRole::Library)
            .collect();
        assert_eq!(libs.len(), 2, "osx-only library must be excluded");
        assert!(libs.iter().any(|f| f.dest.ends_with("logging-1.2.7.jar")));
        let natives: Vec<&DownloadFile> = plan
            .files
            .iter()
            .filter(|f| f.role == FileRole::Native)
            .collect();
        assert_eq!(natives.len(), 1);
        assert!(natives[0]
            .dest
            .ends_with(if profile::os_name() == "windows" { "nat-win.jar" } else { "nat-linux.jar" }));
        assert_eq!(natives[0].sha1.as_deref().unwrap().len(), 2);
        assert_eq!(plan.natives_dir.as_deref(), Some("natives"));
    }

    #[test]
    fn library_files_support_maven_url_only_libraries() {
        let libs: Vec<Library> = serde_json::from_str(
            r#"[{"name":"net.fabricmc:fabric-loader:0.16.9","url":"https://maven.fabricmc.net/"}]"#,
        )
        .unwrap();
        let files = crate::mojang::library_files(&libs, &BTreeMap::new());
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url,
            "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
        );
        assert_eq!(
            files[0].dest,
            "libraries/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
        );
        assert!(files[0].unverified, "no checksum from a bare maven repo");
    }

    #[test]
    fn timestamp_parsing_is_lenient() {
        assert!(parse_iso("2024-12-03T11:38:23+00:00").unwrap() > 0);
        assert!(parse_iso("2024-12-03 11:38:23 +0000").is_some());
        assert!(parse_iso("2024-12-03").is_some());
        assert_eq!(parse_iso("not a date"), None);
        assert_eq!(parse_iso(""), None);
    }

    #[test]
    fn merge_inherits_what_a_child_lacks() {
        let mut child: LaunchProfile = serde_json::from_str(
            r#"{"id":"1.21.4-fabric","inherits_from":"1.21.4","libraries":[{"name":"a:b:1"}]}"#,
        )
        .unwrap();
        let parent = profile_fixture();
        merge_into(&mut child, &parent);
        assert_eq!(child.libraries.len(), 4, "parent libraries come first");
        assert_eq!(child.main_class.as_deref(), Some("net.minecraft.server.Main"));
        assert!(child.downloads.contains_key("server"));
        assert_eq!(child.java_version.as_ref().unwrap().major_version, 21);
        assert_eq!(child.asset_index.as_ref().unwrap().id, "16");
        assert!(child.inherits_from.is_none());
    }

    #[tokio::test]
    async fn selection_resolves_latest_release_snapshot_and_exact() {
        let m = manifest_fixture();
        let sel = |exact: Option<&str>, mc: Option<&str>, ch| VersionSelection {
            exact: exact.map(str::to_string),
            mc_version: mc.map(str::to_string),
            channel: ch,
            java_major: None,
        };
        assert_eq!(resolve_id(&m, &sel(None, None, ReleaseChannel::Stable)).unwrap(), "1.21.4");
        assert_eq!(
            resolve_id(&m, &sel(None, None, ReleaseChannel::Beta)).unwrap(),
            "25w03a"
        );
        assert_eq!(resolve_id(&m, &sel(None, Some("1.8.9"), ReleaseChannel::Stable)).unwrap(), "1.8.9");
        assert_eq!(resolve_id(&m, &sel(Some("1.8.9"), None, ReleaseChannel::Stable)).unwrap(), "1.8.9");
        let err = resolve_id(&m, &sel(Some("9.9.9"), None, ReleaseChannel::Stable)).unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[tokio::test]
    async fn upstream_failures_are_named_not_hidden() {
        // The mock serves 404 for everything the provider does not declare, so this
        // asserts the error mapping (service name, status) rather than any network.
        let mut routes = BTreeMap::new();
        routes.insert(
            "/mc/game/version_manifest_v2.json",
            serde_json::to_string(&manifest_fixture()).unwrap(),
        );
        let mock = crate::mock::serve(routes).await;
        let mut cfg = RegistryConfig::default();
        cfg.include_snapshots = true;
        cfg.base_url_overrides
            .insert("mojang".into(), format!("http://127.0.0.1:{}", mock.port));
        let provider = MojangProvider::new(&cfg).unwrap();

        let entry = provider.versions(None).await.unwrap();
        assert_eq!(entry.versions.len(), 4);
        assert_eq!(entry.versions[0].id, "25w03a", "newest first");
        assert!(!entry.stale);

        // The profile URLs in the fixture point at https://p — unreachable here,
        // which must surface as an upstream-unavailable error naming mojang.
        let err = provider
            .plan(&VersionSelection {
                exact: Some("1.21.4".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, CoreError::UpstreamUnavailable { service, .. } if service == "mojang"),
            "{err:?}"
        );
        assert_eq!(err.code(), "upstream_unavailable");
        let hits = mock.hits.lock().unwrap().len();
        assert!(hits >= 2, "manifest + a profile attempt, got {hits}");
    }
}

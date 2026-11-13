//! Turning "the user clicked Install on three mods" into a list of files that will
//! actually load, and nothing that will not.
//!
//! The pure half of this module (`resolve_from`, `select_version`) does the deciding
//! and is tested exhaustively; the async half only fetches and writes. That split is
//! deliberate: the interesting failures (a client-only mod, a Forge build on Fabric,
//! a missing required library) are pure data problems, and they should be provable
//! without an HTTP server in the loop.

use crate::client::{ModrinthClient, SearchQuery};
use crate::{DependencyType, EnvSupport, ModrinthVersion, Project, ProjectType, VersionType};
use serde::{Deserialize, Serialize};
use sp_core::progress::ProgressReporter;
use sp_core::CoreError;
use sp_net::download::{self, DownloadJob};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How to break ties when several versions are compatible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Prefer {
    /// Newest published — what "latest" means to a user.
    #[default]
    Newest,
    MostDownloads,
    /// The version the author pinned (`featured`), else newest.
    Featured,
}

/// What the user asked for, already narrowed by the server's own facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub mc_version: Option<String>,
    /// Loader(s) the server runs, e.g. `["fabric"]`, or `["paper","spigot","bukkit"]`
    /// for a Bukkit-family server. Empty means "no loader filter" (plugins).
    pub loaders: Vec<String>,
    /// Install beta/alpha builds when a release exists? No by default.
    pub allow_prerelease: bool,
    #[serde(default)]
    pub prefer: Prefer,
    /// Pull `optional` dependencies too. Off by default: an optional dependency is
    /// a feature flag, not a requirement, and installing 6 extra jars because one mod
    /// mentions them is how people end up with a mods folder they cannot audit.
    pub include_optional_dependencies: bool,
    /// Skip the download when the same version is already recorded as installed.
    pub skip_already_installed: bool,
    /// Project ids → version id already installed (from `server_content`).
    #[serde(default)]
    pub installed: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The project is client-side (`server_side: unsupported`) — installing it would
    /// do nothing at best and break world load at worst.
    ClientOnly,
    /// Nothing published for this Minecraft version + loader combination.
    NoCompatibleVersion,
    /// Another selected mod declares this one `incompatible`.
    DeclaredIncompatible,
    /// The exact version is already installed.
    AlreadyInstalled,
    /// The chosen version lists no installable file (source-only release).
    NoArtifact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub project_slug: String,
    pub reason: SkipReason,
    pub detail: Option<String>,
}

impl Skipped {
    /// The sentence the UI shows. Every variant must have one — a skip the user cannot
    /// understand reads as a silent failure.
    pub fn message(&self) -> String {
        let base = match self.reason {
            SkipReason::ClientOnly => format!(
                "{} is client-side only, so it was not installed on the server",
                self.project_slug
            ),
            SkipReason::NoCompatibleVersion => format!(
                "no {} build matches this server's Minecraft version and loader",
                self.project_slug
            ),
            SkipReason::DeclaredIncompatible => format!(
                "{} is declared incompatible by another selected mod",
                self.project_slug
            ),
            SkipReason::AlreadyInstalled => format!("{} is already installed", self.project_slug),
            SkipReason::NoArtifact => format!("{} has no downloadable file", self.project_slug),
        };
        match &self.detail {
            Some(d) if !d.is_empty() => format!("{base} ({d})"),
            _ => base,
        }
    }
}

/// One file to write, with everything the caller needs to record it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedFile {
    pub project_id: String,
    pub project_slug: String,
    pub version_id: String,
    pub version_number: String,
    pub kind: ProjectType,
    pub file_name: String,
    pub url: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub size: u64,
    /// Directory relative to the server root (or the world root for datapacks).
    pub dest_dir: String,
    /// Human-readable "why is this here": the mod the user clicked, or the chain of
    /// required dependencies leading to it.
    pub required_by: Vec<String>,
    pub enabled: bool,
}

impl ResolvedFile {
    pub fn dest_rel(&self) -> String {
        format!("{}/{}", self.dest_dir.trim_end_matches('/'), self.file_name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Resolution {
    pub chosen: Vec<ResolvedFile>,
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<String>,
}

impl Resolution {
    pub fn is_empty(&self) -> bool {
        self.chosen.is_empty()
    }
    pub fn summary(&self) -> String {
        match (self.chosen.len(), self.skipped.len()) {
            (0, 0) => "nothing to do".into(),
            (0, s) => format!("nothing installed, {s} skipped"),
            (c, 0) => format!("{c} file(s) to install"),
            (c, s) => format!("{c} file(s) to install, {s} skipped"),
        }
    }
}

/// Where a modpack/mod install writes. Datapacks need the world; everything else is
/// relative to the server root.
#[derive(Debug, Clone)]
pub struct InstallTarget {
    pub server_dir: PathBuf,
    pub world: Option<String>,
    pub loader: Option<String>,
}

impl InstallTarget {
    pub fn new(server_dir: impl Into<PathBuf>) -> Self {
        Self {
            server_dir: server_dir.into(),
            world: None,
            loader: None,
        }
    }

    pub fn with_world(mut self, world: impl Into<String>) -> Self {
        self.world = Some(world.into());
        self
    }

    /// Resolve a destination relative path to disk, rejecting escapes.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, CoreError> {
        let mut root = self.server_dir.clone();
        if rel.starts_with("world/datapacks/") || rel.starts_with("datapacks/") {
            let world = self.world.clone().unwrap_or_else(|| "world".to_string());
            root.push(world);
        }
        let mut out = root;
        for seg in rel.replace('\\', "/").split('/') {
            match seg {
                "" | "." => continue,
                ".." => return Err(CoreError::PathEscape(rel.to_string())),
                s => out.push(s),
            }
        }
        Ok(out)
    }
}

/// Pick the version to install from a project's published versions.
pub fn select_version(versions: &[ModrinthVersion], sel: &Selection) -> Option<ModrinthVersion> {
    let mut candidates: Vec<ModrinthVersion> = versions
        .iter()
        .filter(|v| {
            sel.mc_version
                .as_deref()
                .map(|mc| v.mc_supports(mc))
                .unwrap_or(true)
        })
        .filter(|v| v.loader_matches(&sel.loaders))
        .filter(|v| sel.allow_prerelease || matches!(v.version_type, VersionType::Release))
        // A version explicitly client-only is out, even if the project is server-side.
        .filter(|v| v.required_envs.server.installable_on_server())
        .filter(|v| v.install_file().is_some())
        .cloned()
        .collect();
    if candidates.is_empty() {
        // Nothing release-quality matched. Rather than installing an alpha silently,
        // let the caller report "no compatible version" — but *do* relax the prerelease
        // filter for servers that asked for betas, which the filter above handles.
        return None;
    }
    match sel.prefer {
        Prefer::MostDownloads => candidates.sort_by_key(|v| std::cmp::Reverse(v.downloads)),
        Prefer::Featured => candidates.sort_by(|a, b| {
            b.featured.cmp(&a.featured).then_with(|| {
                cmp_pub_date(b.date_published.as_deref(), a.date_published.as_deref())
            })
        }),
        Prefer::Newest => candidates.sort_by(|a, b| {
            a.version_type
                .rank()
                .cmp(&b.version_type.rank())
                .then_with(|| {
                    cmp_pub_date(b.date_published.as_deref(), a.date_published.as_deref())
                })
        }),
    }
    candidates.into_iter().next()
}

fn cmp_pub_date(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    // RFC 3339 sorts lexically; missing dates sort last.
    b.unwrap_default().cmp(a.unwrap_default())
}

trait UnwrapDefault {
    fn unwrap_default(&self) -> &str;
}
impl UnwrapDefault for Option<&str> {
    fn unwrap_default(&self) -> &str {
        self.unwrap_or("")
    }
}

/// Pure resolution over already-fetched data.
///
/// `projects` is `(project, its versions)` per requested project; `labels` maps a
/// project id to the slug used in messages.
pub fn resolve_from(projects: &[(Project, Vec<ModrinthVersion>)], sel: &Selection) -> Resolution {
    let mut out = Resolution::default();
    let mut chosen_slugs: BTreeSet<String> = BTreeSet::new();
    let mut by_id: BTreeMap<String, (Project, Vec<ModrinthVersion>)> = BTreeMap::new();

    for (p, vs) in projects {
        by_id.insert(p.id.clone(), (p.clone(), vs.clone()));
    }

    // 1. Incompatible declarations are global: collect them before choosing anything.
    let mut incompatible: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (p, vs) in projects {
        let Some(v) = select_version(vs, sel) else {
            continue;
        };
        for d in &v.dependencies {
            if d.dependency_type == DependencyType::Incompatible {
                if let Some(pid) = &d.project_id {
                    incompatible
                        .entry(pid.clone())
                        .or_default()
                        .push(p.slug.clone());
                }
            }
        }
    }

    // 2. Walk the selection plus the transitive required dependencies.
    let mut queue: Vec<(Project, String)> = projects
        .iter()
        .map(|(p, _)| (p.clone(), p.slug.clone()))
        .collect();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut depth = 0usize;
    while !queue.is_empty() && depth < 5 {
        let mut next = Vec::new();
        for (project, required_by) in queue {
            if !visited.insert(project.id.clone()) {
                continue;
            }
            if project.server_side == EnvSupport::Unsupported {
                out.skipped.push(Skipped {
                    project_slug: project.slug.clone(),
                    reason: SkipReason::ClientOnly,
                    detail: None,
                });
                continue;
            }
            if let Some(who) = incompatible.get(&project.id) {
                out.skipped.push(Skipped {
                    project_slug: project.slug.clone(),
                    reason: SkipReason::DeclaredIncompatible,
                    detail: Some(format!("required by {}", who.join(", "))),
                });
                continue;
            }
            let versions = by_id
                .get(&project.id)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let Some(chosen) = select_version(&versions, sel) else {
                out.skipped.push(Skipped {
                    project_slug: project.slug.clone(),
                    reason: SkipReason::NoCompatibleVersion,
                    detail: Some(format!(
                        "wanted {}{}",
                        sel.mc_version.as_deref().unwrap_or("any version"),
                        if sel.loaders.is_empty() {
                            String::new()
                        } else {
                            format!(" with {}", sel.loaders.join("/"))
                        }
                    )),
                });
                continue;
            };
            let file = match chosen.install_file() {
                Some(f) => f,
                None => {
                    out.skipped.push(Skipped {
                        project_slug: project.slug.clone(),
                        reason: SkipReason::NoArtifact,
                        detail: None,
                    });
                    continue;
                }
            };
            if sel.skip_already_installed
                && sel
                    .installed
                    .get(&project.id)
                    .is_some_and(|v| v == &chosen.id)
            {
                out.skipped.push(Skipped {
                    project_slug: project.slug.clone(),
                    reason: SkipReason::AlreadyInstalled,
                    detail: Some(chosen.version_number.clone()),
                });
                continue;
            }
            let dest_dir = match project.project_type {
                ProjectType::Mod => "mods".to_string(),
                ProjectType::Plugin => "plugins".to_string(),
                ProjectType::Datapack => "world/datapacks".to_string(),
                other => {
                    out.warnings.push(format!(
                        "{} is {:?}, which is not installed into a server directory — skipped",
                        project.slug, other
                    ));
                    continue;
                }
            };
            chosen_slugs.insert(project.slug.clone());
            out.chosen.push(ResolvedFile {
                project_id: project.id.clone(),
                project_slug: project.slug.clone(),
                version_id: chosen.id.clone(),
                version_number: chosen.version_number.clone(),
                kind: project.project_type,
                file_name: file.filename.clone(),
                url: file.url.clone(),
                sha1: file.hashes.sha1.clone(),
                sha512: file.hashes.sha512.clone(),
                size: file.size,
                dest_dir,
                required_by: vec![required_by.clone()],
                enabled: true,
            });

            for dep in &chosen.dependencies {
                let wanted = match dep.dependency_type {
                    DependencyType::Required => true,
                    DependencyType::Optional => sel.include_optional_dependencies,
                    // Embedded code is inside the jar already; incompatible ones were
                    // handled up front.
                    DependencyType::Embedded | DependencyType::Incompatible => false,
                };
                if !wanted {
                    continue;
                }
                let Some(pid) = dep
                    .project_id
                    .clone()
                    .or_else(|| dep.version_id.as_ref().and_then(|_| None))
                else {
                    out.warnings.push(format!(
                        "{} declares a dependency the panel cannot resolve by project id",
                        project.slug
                    ));
                    continue;
                };
                if visited.contains(&pid) {
                    continue;
                }
                match by_id.get(&pid) {
                    Some((dp, _)) => next.push((dp.clone(), project.slug.clone())),
                    None => {
                        // A dependency we have not fetched metadata for cannot be
                        // resolved locally; say so instead of installing a broken set.
                        out.warnings.push(format!(
                            "{} requires another mod (project {pid}) that must be installed too",
                            project.slug
                        ));
                    }
                }
            }
        }
        queue = next;
        depth += 1;
    }
    // Merge `required_by` for files reached from two directions.
    for i in 0..out.chosen.len() {
        let dupes: Vec<usize> = out
            .chosen
            .iter()
            .enumerate()
            .filter(|(j, f)| *j != i && f.project_id == out.chosen[i].project_id)
            .map(|(j, _)| j)
            .collect();
        for j in dupes {
            out.chosen[i]
                .required_by
                .extend(out.chosen[j].required_by.iter().cloned());
        }
    }
    // Group by destination folder so the progress list reads as
    // "mods, then plugins, then datapacks" and the file order is deterministic.
    out.chosen.sort_by(|a, b| {
        a.dest_dir
            .cmp(&b.dest_dir)
            .then_with(|| a.project_slug.cmp(&b.project_slug))
    });
    out
}

/// Fetch the dependency projects we do not have metadata for, then re-resolve.
///
/// This is the async counterpart of `resolve_from`: it fills `by_id` from the API
/// when a mod requires a project the user did not select, which is the common case
/// (a library mod is rarely searched for by name).
pub async fn resolve_with_dependencies(
    client: &ModrinthClient,
    requested: &[String],
    sel: &Selection,
) -> Result<Resolution, CoreError> {
    let mut projects: Vec<(Project, Vec<ModrinthVersion>)> = Vec::new();
    for slug in requested {
        let project = client.project(slug).await?;
        let versions = client.versions_of(&project.id, sel).await?;
        projects.push((project, versions));
    }
    let mut resolution = resolve_from(&projects, sel);

    // Second pass: any warning naming an unresolved project id gets fetched once.
    let unresolved: Vec<String> = resolution
        .warnings
        .iter()
        .filter_map(|w| {
            let idx = w.find("project ")?;
            let tail = &w[idx + "project ".len()..];
            let id: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            (!id.is_empty()).then_some(id)
        })
        .collect();
    let mut extra: Vec<(Project, Vec<ModrinthVersion>)> = Vec::new();
    for id in unresolved.into_iter().collect::<BTreeSet<_>>() {
        match fetch_project(client, &id, sel).await {
            Ok(pair) => extra.push(pair),
            Err(e) => {
                tracing::debug!(project = %id, error = %e, "could not resolve a mod's dependency");
                resolution
                    .warnings
                    .push(format!("could not check dependency {id}: {e}"));
            }
        }
    }
    if !extra.is_empty() {
        projects.extend(extra);
        resolution = resolve_from(&projects, sel);
    }
    Ok(resolution)
}

async fn fetch_project(
    client: &ModrinthClient,
    id: &str,
    sel: &Selection,
) -> Result<(Project, Vec<ModrinthVersion>), CoreError> {
    let project = client.project(id).await?;
    let versions = client.versions_of(&project.id, sel).await?;
    Ok((project, versions))
}

/// Download every resolved file. Files already present with the right SHA-1 are
/// left alone, so re-running an install is a no-op rather than a 200 MB re-download.
pub async fn fetch(
    client: &ModrinthClient,
    resolution: &Resolution,
    target: &InstallTarget,
    progress: &ProgressReporter,
) -> Result<Vec<PathBuf>, CoreError> {
    let http = client.http().clone();
    let mut written = Vec::with_capacity(resolution.chosen.len());
    let total = resolution.chosen.iter().map(|f| f.size).sum::<u64>();
    progress.total(total.max(1));
    for (i, file) in resolution.chosen.iter().enumerate() {
        let dest = target.resolve(&file.dest_rel())?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        progress.message(format!(
            "Installing {} ({}/{})",
            file.project_slug,
            i + 1,
            resolution.chosen.len()
        ));
        let mut job = DownloadJob::new(file.url.clone(), &dest, file.file_name.clone())
            .size_hint((file.size > 0).then_some(file.size))
            .reuse_if_valid(true);
        // Modrinth publishes sha1 (and often sha512); enforce both when present.
        if let Some(s) = &file.sha1 {
            job = job.with_sha1(Some(s.clone()));
        }
        if let Some(s) = &file.sha512 {
            job = job.with_sha512(Some(s.clone()));
        }
        download::download(&http, &job, Some(progress)).await?;
        written.push(dest);
    }
    Ok(written)
}

/// Search narrowed to what can run on this server.
pub async fn search_for_server(
    client: &ModrinthClient,
    query: &str,
    kind: ProjectType,
    mc_version: &str,
    loaders: &[String],
    limit: u8,
) -> Result<crate::SearchPage, CoreError> {
    let mut q = SearchQuery::new(query).typed(kind).for_mc(mc_version);
    // `server_side_only` is a plain field on `SearchQuery`, not a builder method.
    q.server_side_only = true;
    q.limit = Some(limit.min(100));
    for l in loaders {
        q = q.loader(l.clone());
    }
    client.search(&q).await
}

/// An available update for one installed file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAvailable {
    pub project_slug: String,
    pub version_id_current: String,
    pub version_number_current: String,
    pub version_id_latest: String,
    pub version_number_latest: String,
    pub file_name: String,
    pub url: String,
    pub sha1: Option<String>,
    pub size: u64,
    /// The newest version dropped MC/loader support for this server.
    pub breaking: bool,
}

/// Compute updates from the files actually on disk.
///
/// `installed` is `(path, sha1 hex of that file)`. The hash lookup is one POST for
/// the whole folder; projects it does not know are then checked individually, so a
/// Modrinth response-shape change costs requests, not correctness.
pub async fn check_updates(
    client: &ModrinthClient,
    sel: &Selection,
    installed: &[(PathBuf, String, String)],
) -> Result<Vec<UpdateAvailable>, CoreError> {
    if installed.is_empty() {
        return Ok(Vec::new());
    }
    let hashes: Vec<String> = installed.iter().map(|(_, h, _)| h.clone()).collect();
    let known = client
        .versions_for_hashes(&hashes)
        .await
        .unwrap_or_default();
    let mut by_project: BTreeMap<String, ModrinthVersion> = BTreeMap::new();
    for v in known {
        // Keep the newest matching version per project that we learned about.
        match by_project.get(&v.project_id) {
            Some(cur)
                if cmp_pub_date(Some(&cur.version_number), Some(&v.version_number))
                    != std::cmp::Ordering::Less => {}
            _ => {
                by_project.insert(v.project_id.clone(), v);
            }
        }
    }
    let mut out = Vec::new();
    for (path, sha, slug) in installed {
        let version_id = match by_project.values().find(|v| {
            v.files
                .iter()
                .any(|f| f.hashes.sha1.as_deref() == Some(sha.as_str()))
        }) {
            Some(v) => v.id.clone(),
            None => continue,
        };
        let project = match client.project(slug).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(project = %slug, error = %e, "update check skipped for one project");
                continue;
            }
        };
        let versions = client.versions_of(&project.id, sel).await?;
        let Some(latest) = select_version(&versions, sel) else {
            continue;
        };
        if latest.id == version_id {
            continue;
        }
        let current = versions
            .iter()
            .find(|v| v.id == version_id)
            .cloned()
            .or_else(|| {
                by_project
                    .get(&project.id)
                    .filter(|v| v.id == version_id)
                    .cloned()
            });
        let Some(file) = latest.install_file() else {
            continue;
        };
        out.push(UpdateAvailable {
            project_slug: project.slug.clone(),
            version_id_current: current.as_ref().map(|c| c.id.clone()).unwrap_or_default(),
            version_number_current: current
                .as_ref()
                .map(|c| c.version_number.clone())
                .or_else(|| path.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_default(),
            version_id_latest: latest.id.clone(),
            version_number_latest: latest.version_number.clone(),
            file_name: file.filename.clone(),
            url: file.url.clone(),
            sha1: file.hashes.sha1.clone(),
            size: file.size,
            breaking: current
                .as_ref()
                .map(|c| {
                    c.game_versions
                        .iter()
                        .any(|g| !latest.game_versions.contains(g))
                })
                .unwrap_or(false),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileHashes, ModrinthDependency, ModrinthFile, RequiredEnvs, SearchIndex};

    fn version(
        id: &str,
        num: &str,
        mcs: &[&str],
        loaders: &[&str],
        ty: VersionType,
        server: EnvSupport,
        file: Option<&str>,
    ) -> ModrinthVersion {
        ModrinthVersion {
            id: id.into(),
            project_id: "p1".into(),
            name: format!("v{num}"),
            version_number: num.into(),
            changelog: None,
            game_versions: mcs.iter().map(|s| s.to_string()).collect(),
            loaders: loaders.iter().map(|s| s.to_string()).collect(),
            version_type: ty,
            date_published: Some(format!("2024-01-0{}T00:00:00Z", num.len().min(9))),
            downloads: 1,
            featured: false,
            dependencies: vec![],
            required_envs: RequiredEnvs {
                client: EnvSupport::Optional,
                server,
            },
            files: file
                .map(|f| ModrinthFile {
                    url: format!("https://cdn/{f}"),
                    filename: f.into(),
                    primary: true,
                    size: 10,
                    hashes: FileHashes {
                        sha1: Some("aa".into()),
                        sha512: None,
                    },
                })
                .into_iter()
                .collect(),
        }
    }

    fn project(id: &str, slug: &str, kind: ProjectType, side: EnvSupport) -> Project {
        Project {
            id: id.into(),
            slug: slug.into(),
            project_type: kind,
            title: slug.into(),
            description: String::new(),
            body: None,
            categories: vec![],
            downloads: 0,
            follows: 0,
            icon_url: None,
            server_side: side,
            client_side: EnvSupport::Optional,
            license: None,
            created: None,
            modified: None,
            members: vec![],
        }
    }

    fn sel() -> Selection {
        Selection {
            mc_version: Some("1.21.4".into()),
            loaders: vec!["fabric".into()],
            ..Default::default()
        }
    }

    #[test]
    fn selection_filters_by_mc_loader_and_env() {
        let vs = vec![
            version(
                "old",
                "1.0.0",
                &["1.20.1"],
                &["fabric"],
                VersionType::Release,
                EnvSupport::Required,
                Some("old.jar"),
            ),
            version(
                "other-loader",
                "2.0.0",
                &["1.21.4"],
                &["forge"],
                VersionType::Release,
                EnvSupport::Required,
                Some("f.jar"),
            ),
            version(
                "client-only",
                "2.0.0",
                &["1.21.4"],
                &["fabric"],
                VersionType::Release,
                EnvSupport::Unsupported,
                Some("c.jar"),
            ),
            version(
                "no-file",
                "2.0.0",
                &["1.21.4"],
                &["fabric"],
                VersionType::Release,
                EnvSupport::Required,
                None,
            ),
            version(
                "beta",
                "3.0.0-beta",
                &["1.21.4"],
                &["fabric"],
                VersionType::Beta,
                EnvSupport::Required,
                Some("b.jar"),
            ),
            version(
                "good",
                "2.5.0",
                &["1.21.4"],
                &["fabric"],
                VersionType::Release,
                EnvSupport::Optional,
                Some("g.jar"),
            ),
        ];
        assert_eq!(select_version(&vs, &sel()).unwrap().id, "good");
        // Prerelease opt-in picks the higher-numbered beta.
        let mut s = sel();
        s.allow_prerelease = true;
        assert_eq!(select_version(&vs, &s).unwrap().id, "beta");
        // A plugin server has no loader filter at all.
        let plugins = vec![version(
            "pl",
            "1.1",
            &["1.21.4"],
            &["paper"],
            VersionType::Release,
            EnvSupport::Required,
            Some("p.jar"),
        )];
        let mut s = sel();
        s.loaders.clear();
        assert_eq!(select_version(&plugins, &s).unwrap().id, "pl");
        assert!(select_version(&[], &sel()).is_none());
    }

    #[test]
    fn prefer_changes_the_winner() {
        let mut a = version(
            "a",
            "1.0.0",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("a.jar"),
        );
        a.downloads = 1000;
        a.featured = true;
        let b = version(
            "b",
            "9.9.9",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("b.jar"),
        );
        let vs = vec![b.clone(), a.clone()];
        let mut s = sel();
        s.prefer = Prefer::MostDownloads;
        assert_eq!(select_version(&vs, &s).unwrap().id, "a");
        s.prefer = Prefer::Featured;
        assert_eq!(select_version(&vs, &s).unwrap().id, "a");
        s.prefer = Prefer::Newest;
        assert_eq!(
            select_version(&vs, &s).unwrap().id,
            "b",
            "newest is by publish date, not downloads"
        );
    }

    #[test]
    fn client_only_projects_are_skipped_with_an_explanation() {
        let projects = vec![(
            project(
                "p1",
                "zoomer-shaders",
                ProjectType::Mod,
                EnvSupport::Unsupported,
            ),
            vec![version(
                "v",
                "1",
                &["1.21.4"],
                &["fabric"],
                VersionType::Release,
                EnvSupport::Required,
                Some("z.jar"),
            )],
        )];
        let r = resolve_from(&projects, &sel());
        assert!(r.chosen.is_empty());
        assert_eq!(r.skipped[0].reason, SkipReason::ClientOnly);
        assert!(
            r.skipped[0].message().contains("client-side only"),
            "{}",
            r.skipped[0].message()
        );
    }

    #[test]
    fn every_skip_variant_has_ui_copy() {
        for reason in [
            SkipReason::ClientOnly,
            SkipReason::NoCompatibleVersion,
            SkipReason::DeclaredIncompatible,
            SkipReason::AlreadyInstalled,
            SkipReason::NoArtifact,
        ] {
            let s = Skipped {
                project_slug: "x".into(),
                reason,
                detail: Some("because".into()),
            };
            let m = s.message();
            assert!(m.contains("x") && m.contains("because"), "{m}");
            assert!(m.len() > 20, "{m}");
        }
    }

    #[test]
    fn no_compatible_version_says_what_was_wanted() {
        let projects = vec![(
            project("p1", "mod-a", ProjectType::Mod, EnvSupport::Required),
            vec![version(
                "v",
                "1",
                &["1.16.5"],
                &["forge"],
                VersionType::Release,
                EnvSupport::Required,
                Some("a.jar"),
            )],
        )];
        let r = resolve_from(&projects, &sel());
        let s = &r.skipped[0];
        assert_eq!(s.reason, SkipReason::NoCompatibleVersion);
        assert!(
            s.message().contains("1.21.4") && s.message().contains("fabric"),
            "{}",
            s.message()
        );
    }

    #[test]
    fn required_dependencies_are_pulled_in_and_optional_are_not() {
        let mut dep = version(
            "lib-v",
            "2.0",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("lib.jar"),
        );
        dep.project_id = "plib".into();
        dep.id = "lib-v".into();
        let mut main = version(
            "main-v",
            "1.0",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("main.jar"),
        );
        main.project_id = "pmain".into();
        main.dependencies = vec![
            ModrinthDependency {
                version_id: Some("lib-v".into()),
                project_id: Some("plib".into()),
                file_id: None,
                dependency_type: DependencyType::Required,
            },
            ModrinthDependency {
                version_id: None,
                project_id: Some("popt".into()),
                file_id: None,
                dependency_type: DependencyType::Optional,
            },
        ];
        let mut opt = version(
            "opt-v",
            "1.0",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("opt.jar"),
        );
        opt.project_id = "popt".into();
        let projects = vec![
            (
                project("pmain", "main-mod", ProjectType::Mod, EnvSupport::Required),
                vec![main.clone()],
            ),
            (
                project(
                    "plib",
                    "library-mod",
                    ProjectType::Mod,
                    EnvSupport::Required,
                ),
                vec![lib_v(&dep)],
            ),
            (
                project(
                    "popt",
                    "optional-mod",
                    ProjectType::Mod,
                    EnvSupport::Required,
                ),
                vec![opt],
            ),
        ];
        let r = resolve_from(&projects, &sel());
        let slugs: Vec<&str> = r.chosen.iter().map(|f| f.project_slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["main-mod", "library-mod"],
            "optional dependencies are not installed by default"
        );
        let lib = r
            .chosen
            .iter()
            .find(|f| f.project_slug == "library-mod")
            .unwrap();
        assert_eq!(
            lib.required_by,
            vec!["main-mod"],
            "the UI must be able to say why"
        );

        let mut s = sel();
        s.include_optional_dependencies = true;
        let r = resolve_from(&projects, &s);
        assert_eq!(r.chosen.len(), 3);
    }

    fn lib_v(v: &ModrinthVersion) -> ModrinthVersion {
        let mut x = v.clone();
        x.project_id = "plib".into();
        x
    }

    #[test]
    fn declared_incompatibility_wins_over_the_user_choice() {
        let mut a = version(
            "a-v",
            "1",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("a.jar"),
        );
        a.project_id = "pa".into();
        a.dependencies = vec![ModrinthDependency {
            version_id: None,
            project_id: Some("pb".into()),
            file_id: None,
            dependency_type: DependencyType::Incompatible,
        }];
        let mut b = version(
            "b-v",
            "1",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("b.jar"),
        );
        b.project_id = "pb".into();
        let r = resolve_from(
            &[
                (
                    project("pa", "mod-a", ProjectType::Mod, EnvSupport::Required),
                    vec![a],
                ),
                (
                    project("pb", "mod-b", ProjectType::Mod, EnvSupport::Required),
                    vec![b],
                ),
            ],
            &sel(),
        );
        assert_eq!(r.chosen.len(), 1, "the incompatible one is dropped");
        assert_eq!(r.chosen[0].project_slug, "mod-a");
        assert_eq!(r.skipped[0].reason, SkipReason::DeclaredIncompatible);
        assert!(r.skipped[0].message().contains("mod-a"));
    }

    #[test]
    fn already_installed_can_be_respected_or_ignored() {
        let vs = vec![version(
            "v1",
            "1",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("a.jar"),
        )];
        let p = vec![(
            project("p1", "mod-a", ProjectType::Mod, EnvSupport::Required),
            vs,
        )];
        let mut s = sel();
        s.skip_already_installed = true;
        s.installed.insert("p1".into(), "v1".into());
        let r = resolve_from(&p, &s);
        assert!(r.chosen.is_empty());
        assert_eq!(r.skipped[0].reason, SkipReason::AlreadyInstalled);
        s.skip_already_installed = false;
        assert_eq!(
            resolve_from(&p, &s).chosen.len(),
            1,
            "a repair install must still work"
        );
    }

    #[test]
    fn destinations_follow_project_type() {
        let mut datapack = version(
            "dp",
            "1",
            &["1.21.4"],
            &["fabric"],
            VersionType::Release,
            EnvSupport::Required,
            Some("d.zip"),
        );
        datapack.project_id = "pdp".into();
        let mut plugin = version(
            "pl",
            "1",
            &["1.21.4"],
            &["paper"],
            VersionType::Release,
            EnvSupport::Required,
            Some("p.jar"),
        );
        plugin.project_id = "ppl".into();
        let mut shader = version(
            "sh",
            "1",
            &["1.21.4"],
            &["optifine"],
            VersionType::Release,
            EnvSupport::Optional,
            Some("s.zip"),
        );
        shader.project_id = "psh".into();
        let r = resolve_from(
            &[
                (
                    project("pdp", "pack", ProjectType::Datapack, EnvSupport::Required),
                    vec![datapack],
                ),
                (
                    project("ppl", "plugin", ProjectType::Plugin, EnvSupport::Required),
                    vec![plugin],
                ),
                (
                    project("psh", "shader", ProjectType::Shader, EnvSupport::Optional),
                    vec![shader],
                ),
            ],
            &Selection {
                mc_version: Some("1.21.4".into()),
                loaders: vec![],
                ..Default::default()
            },
        );
        assert_eq!(r.chosen[0].dest_dir, "world/datapacks");
        assert_eq!(r.chosen[1].dest_dir, "plugins");
        assert_eq!(r.chosen[0].dest_rel(), "world/datapacks/d.zip");
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("shader") && w.contains("not installed")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn install_target_paths_and_escapes() {
        let t = InstallTarget::new("/srv/panels/3").with_world("world_nether");
        assert_eq!(
            t.resolve("mods/a.jar").unwrap(),
            PathBuf::from("/srv/panels/3/mods/a.jar")
        );
        assert_eq!(
            t.resolve("world/datapacks/x.zip").unwrap(),
            PathBuf::from("/srv/panels/3/world_nether/datapacks/x.zip")
        );
        assert_eq!(
            t.resolve("datapacks/y.zip").unwrap(),
            PathBuf::from("/srv/panels/3/world_nether/datapacks/y.zip")
        );
        for bad in ["../escape", "mods/../../x", "/etc/passwd"] {
            assert!(t.resolve(bad).is_err(), "{bad}");
        }
        // No world configured → the default world, not an error.
        let t2 = InstallTarget::new(Path::new("/srv/panels/3"));
        assert!(t2
            .resolve("datapacks/z")
            .unwrap()
            .ends_with("world/datapacks/z"));
    }

    #[test]
    fn resolution_summary_reads_naturally() {
        let mut r = Resolution::default();
        assert_eq!(r.summary(), "nothing to do");
        r.chosen.push(ResolvedFile {
            project_id: "p".into(),
            project_slug: "s".into(),
            version_id: "v".into(),
            version_number: "1".into(),
            kind: ProjectType::Mod,
            file_name: "f.jar".into(),
            url: "u".into(),
            sha1: None,
            sha512: None,
            size: 0,
            dest_dir: "mods".into(),
            required_by: vec![],
            enabled: true,
        });
        assert_eq!(r.summary(), "1 file(s) to install");
        r.skipped.push(Skipped {
            project_slug: "x".into(),
            reason: SkipReason::NoArtifact,
            detail: None,
        });
        assert_eq!(r.summary(), "1 file(s) to install, 1 skipped");
        assert!(!r.is_empty());
    }

    #[tokio::test]
    async fn fetch_writes_files_and_survives_re_run() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"MODBYTES".to_vec();
        let sha = {
            use sha1::Digest;
            hex::encode(sha1::Sha1::digest(&payload))
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = payload.clone();
        let srv = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            }
        });
        let client = ModrinthClient::with_base(
            &format!("http://127.0.0.1:{port}"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        let mut res = Resolution::default();
        res.chosen.push(ResolvedFile {
            project_id: "p".into(),
            project_slug: "sodium".into(),
            version_id: "v".into(),
            version_number: "0.6.0".into(),
            kind: ProjectType::Mod,
            file_name: "sodium.jar".into(),
            url: format!("http://127.0.0.1:{port}/sodium.jar"),
            sha1: Some(sha),
            sha512: None,
            size: payload.len() as u64,
            dest_dir: "mods".into(),
            required_by: vec![],
            enabled: true,
        });
        let target = InstallTarget::new(dir.path().to_path_buf());
        let progress = ProgressReporter::silent("m", None, sp_core::model::ProgressPhase::Install);
        let written = fetch(&client, &res, &target, &progress).await.unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(std::fs::read(&written[0]).unwrap(), payload);
        assert!(dir.path().join("mods/sodium.jar").exists());
        // A second fetch must not rewrite the file (reuse-if-valid short-circuits).
        let again = fetch(&client, &res, &target, &progress).await.unwrap();
        assert_eq!(again, written);
        // A tampered file fails verification rather than being accepted.
        std::fs::write(dir.path().join("mods/sodium.jar"), b"tampered").unwrap();
        let err = fetch(&client, &res, &target, &progress).await;
        // The download re-fetches and overwrites: the mismatch is on the reuse check,
        // so either an error or a restored file is acceptable — but never a bad file.
        if err.is_ok() {
            assert_eq!(
                std::fs::read(dir.path().join("mods/sodium.jar")).unwrap(),
                payload
            );
        }
        srv.abort();
    }

    #[test]
    fn search_for_server_builds_the_query() {
        let mut q = SearchQuery::new("sodium")
            .typed(ProjectType::Mod)
            .for_mc("1.21.4")
            .loader("fabric");
        q.server_side_only = true;
        let s = q.to_query_string();
        assert!(s.contains("query=sodium"));
        assert!(s.contains("index=relevance"));
        assert!(s.contains("facets="));
        let decoded = percent_decode(
            s.split("facets=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap(),
        );
        assert!(decoded.contains("project_type:mod"), "{decoded}");
        assert!(decoded.contains("server_side:optional"));
        assert!(decoded.contains("versions:1.21.4"));
        let _ = SearchIndex::Newest;
    }

    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                out.push(
                    u8::from_str_radix(
                        std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                        16,
                    )
                    .unwrap_or(0),
                );
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }
}

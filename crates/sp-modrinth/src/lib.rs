//! Modrinth: mods, plugins, resource packs and `.mrpack` modpacks.
//!
//! Three things make this more than an HTTP client, and they are the reason a mod
//! layer breaks in other panels:
//!
//! 1. **Server-side correctness.** A client-only mod (shaders, minimaps, UI mods)
//!    installed on the server either does nothing or crashes the world load. Every
//!    selection here drops versions whose `required_envs.server` is `unsupported`,
//!    and surfaces `server_side: "unsupported"` on the *project* so a client-only
//!    mod never appears in a server search at all.
//! 2. **Dependency closure.** A mod that requires another mod is installed together
//!    with it, transitively, bounded to a fixed depth with a visited set, so the
//!    server actually boots after "Install" instead of printing three missing-mod
//!    lines on first start.
//! 3. **Nothing unverified.** Every file carries Modrinth's SHA-1/SHA-512 and is
//!    hashed while streaming (`sp_net::download`), and a `.mrpack` install refuses
//!    to overwrite a file whose bytes differ from the pack's index without saying so.
//!
//! API surface used (`https://api.modrinth.com/v2`, rate limited to ~300/min, so all
//! reads go through `sp_net`'s TTL cache and a 429 is *respected*, not retried blindly):
//!
//! * `GET /search` (faceted: `project_type`, `versions`, `categories`, `loaders`)
//! * `GET /project/{slug|id}`, `GET /project/{id}/version?game_versions=&loaders=`
//! * `GET /version/{id}/files`, `POST /version_files` (update checks by hash)
//! * `GET /tag/{category|loader|version_type|license}`, `GET /notices`
//!
//! Assumptions that must be re-verified before the first release (recorded in
//! `docs/providers.md`): the `POST /version_files` body is a JSON array of lowercase
//! hex SHA-1 strings and the response is an array of version objects keyed by
//! `files[].hashes.sha1`. The update-check path degrades to a per-project version
//! query if that ever changes, so a Modrinth schema change cannot break installs.

pub mod client;
pub mod mrpack;
pub mod resolve;

// `resolve::search_for_server` is public and returns `SearchPage`, so the search types
// have to be reachable from the crate root — otherwise the public interface names items
// callers cannot import.
pub use client::{ModrinthClient, SearchIndex, SearchPage, SearchQuery};
pub use mrpack::{PackFile, PackIndex};
pub use resolve::{InstallTarget, Resolution, ResolvedFile, Selection};

use serde::{Deserialize, Serialize};
use sp_core::CoreError;

/// What kind of content a project is. Mirrors Modrinth's `project_type`, plus the
/// two panel-side kinds that share the same API (`modpack` = a project whose only
/// purpose is a version with a `.mrpack` file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectType {
    Mod,
    Plugin,
    ResourcePack,
    Shader,
    Modpack,
    Datapack,
    #[serde(other)]
    Unknown,
}

impl ProjectType {
    /// Where files of this kind go inside a server directory, relative to it.
    ///
    /// `None` means "no single directory answers this": datapacks live per-world
    /// (`<world>/datapacks`) so the caller must resolve the world first, and
    /// resource packs/shaders are client-side and are only ever *offered* to the user
    /// for download, never written into a server. The API turns a `None` kind into a
    /// 400 with this explanation rather than inventing a folder.
    pub fn install_dir(self) -> Option<&'static str> {
        match self {
            Self::Mod => Some("mods"),
            Self::Plugin => Some("plugins"),
            Self::Modpack => Some("."),
            Self::Datapack | Self::ResourcePack | Self::Shader | Self::Unknown => None,
        }
    }

    /// Datapacks need a world; everything else has a fixed home.
    pub fn needs_world(self) -> bool {
        matches!(self, Self::Datapack)
    }

    pub fn is_mod(self) -> bool {
        matches!(self, Self::Mod)
    }
}

/// `project.server_side` / `version.required_envs.server`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EnvSupport {
    /// Must be installed on the server (normal for gameplay mods).
    #[default]
    Required,
    /// Works either side.
    Optional,
    /// Client-only. Never installed on a server by this panel.
    Unsupported,
}

impl EnvSupport {
    pub fn installable_on_server(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VersionType {
    #[default]
    Release,
    Beta,
    Alpha,
}

impl VersionType {
    /// Release > beta > alpha, so a channel preference never silently installs an
    /// alpha build when a release exists.
    pub fn rank(self) -> u8 {
        match self {
            Self::Release => 2,
            Self::Beta => 1,
            Self::Alpha => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub project_id: String,
    pub project_type: ProjectType,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub date_created: Option<String>,
    #[serde(default)]
    pub date_modified: Option<String>,
    #[serde(default)]
    pub latest_version: Option<String>,
    /// `server_side` is on the project, not the search hit; the client fills this in
    /// when it can, and `None` means "unknown, check before installing".
    #[serde(default)]
    pub server_side: Option<EnvSupport>,
    /// Highlight fragments from Modrinth, stripped before rendering.
    #[serde(default)]
    pub hits: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub project_type: ProjectType,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Markdown body; rendered by the UI, never `dangerouslySetInnerHTML`'d raw.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub follows: u64,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub server_side: EnvSupport,
    #[serde(default)]
    pub client_side: EnvSupport,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub modified: Option<String>,
    #[serde(default)]
    pub members: Vec<ProjectMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMember {
    pub user_id: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthVersion {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub version_number: String,
    #[serde(default)]
    pub changelog: Option<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub version_type: VersionType,
    #[serde(default)]
    pub date_published: Option<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub featured: bool,
    #[serde(default)]
    pub dependencies: Vec<ModrinthDependency>,
    #[serde(default)]
    pub required_envs: RequiredEnvs,
    #[serde(default)]
    pub files: Vec<ModrinthFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequiredEnvs {
    #[serde(default)]
    pub client: EnvSupport,
    #[serde(default)]
    pub server: EnvSupport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthDependency {
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub dependency_type: DependencyType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyType {
    #[default]
    Required,
    Optional,
    Incompatible,
    Embedded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthFile {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub hashes: FileHashes,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileHashes {
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub sha512: Option<String>,
}

impl ModrinthVersion {
    /// The file to install: `primary` when marked, else the largest `.jar`, else the
    /// first entry. A version can legitimately list a `.jar` per loader variant.
    pub fn install_file(&self) -> Option<&ModrinthFile> {
        self.files
            .iter()
            .find(|f| f.primary)
            .or_else(|| {
                self.files
                    .iter()
                    .filter(|f| f.filename.to_ascii_lowercase().ends_with(".jar"))
                    .max_by_key(|f| f.size)
            })
            .or_else(|| self.files.first())
    }

    pub fn mc_supports(&self, mc_version: &str) -> bool {
        self.game_versions.iter().any(|v| v == mc_version)
    }

    pub fn loader_matches(&self, loaders: &[String]) -> bool {
        loaders.is_empty()
            || self
                .loaders
                .iter()
                .any(|l| loaders.iter().any(|want| want.eq_ignore_ascii_case(l)))
    }
}

/// A Modrinth notice (maintenance windows, incidents). Shown as a dismissible
/// banner rather than being lost in a log line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub msg: String,
    #[serde(default)]
    pub severity: NoticeSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NoticeSeverity {
    #[default]
    Info,
    Warning,
    Error,
}

/// Any Modrinth error is an `Upstream` error naming the service, so a UI can say
/// "Modrinth is rate-limiting us, retry in 40 s" instead of "request failed".
pub fn upstream_err(body: impl Into<String>, status: u16) -> CoreError {
    CoreError::Upstream {
        service: "modrinth",
        status,
        body: body.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(files: &[(&str, bool, u64)], server: EnvSupport) -> ModrinthVersion {
        ModrinthVersion {
            id: "vers_1".into(),
            project_id: "proj_1".into(),
            name: "Test 1.0.0".into(),
            version_number: "1.0.0".into(),
            changelog: None,
            game_versions: vec!["1.21.4".into(), "1.21.3".into()],
            loaders: vec!["fabric".into(), "quilt".into()],
            version_type: VersionType::Release,
            date_published: Some("2024-12-01T10:00:00Z".into()),
            downloads: 10,
            featured: true,
            dependencies: vec![],
            required_envs: RequiredEnvs {
                client: EnvSupport::Optional,
                server,
            },
            files: files
                .iter()
                .map(|(n, primary, size)| ModrinthFile {
                    url: format!("https://cdn.modrinth.com/data/proj_1/vers_1/{n}"),
                    filename: n.to_string(),
                    primary: *primary,
                    size: *size,
                    hashes: FileHashes {
                        sha1: Some("aa".into()),
                        sha512: None,
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn file_choice_prefers_primary_then_largest_jar() {
        let v = version(
            &[
                ("small.jar", false, 100),
                ("big.jar", false, 500),
                ("sources.jar", true, 1),
            ],
            EnvSupport::Required,
        );
        assert_eq!(
            v.install_file().unwrap().filename,
            "sources.jar",
            "primary wins even when tiny"
        );
        let v = version(
            &[
                ("small.jar", false, 100),
                ("big.jar", false, 500),
                ("README.md", false, 900),
            ],
            EnvSupport::Required,
        );
        assert_eq!(v.install_file().unwrap().filename, "big.jar");
        let v = version(&[("only.zip", false, 5)], EnvSupport::Required);
        assert_eq!(v.install_file().unwrap().filename, "only.zip");
        assert!(version(&[], EnvSupport::Required).install_file().is_none());
    }

    #[test]
    fn compatibility_predicates() {
        let v = version(&[("a.jar", true, 1)], EnvSupport::Required);
        assert!(v.mc_supports("1.21.4"));
        assert!(!v.mc_supports("1.20.1"));
        assert!(v.loader_matches(&["fabric".to_string()]));
        assert!(
            v.loader_matches(&["QUILT".to_string()],),
            "loader compare is case-insensitive"
        );
        assert!(!v.loader_matches(&["forge".to_string()]));
        assert!(v.loader_matches(&[]), "no loader filter matches everything");
    }

    #[test]
    fn env_support_decides_installability() {
        assert!(EnvSupport::Required.installable_on_server());
        assert!(EnvSupport::Optional.installable_on_server());
        assert!(
            !EnvSupport::Unsupported.installable_on_server(),
            "a client-only mod must not reach a server"
        );
    }

    #[test]
    fn version_type_ordering_is_release_first() {
        assert!(VersionType::Release.rank() > VersionType::Beta.rank());
        assert!(VersionType::Beta.rank() > VersionType::Alpha.rank());
    }

    #[test]
    fn project_types_map_to_directories() {
        assert_eq!(ProjectType::Plugin.install_dir(), Some("plugins"));
        assert_eq!(ProjectType::Modpack.install_dir(), Some("."));
        assert_eq!(ProjectType::Mod.install_dir(), Some("mods"));
        assert_eq!(
            ProjectType::Shader.install_dir(),
            None,
            "a shader is not a server file"
        );
        assert_eq!(ProjectType::Datapack.install_dir(), None);
        assert!(ProjectType::Datapack.needs_world());
        assert!(!ProjectType::Mod.needs_world());
        assert!(ProjectType::Mod.is_mod());
        assert!(!ProjectType::Plugin.is_mod());
    }

    #[test]
    fn serde_shapes_match_the_api() {
        let json = r#"{
          "id":"proj_1","slug":"sodium","project_type":"mod","title":"Sodium",
          "description":"Rendering engine","categories":["fabric","performance"],
          "downloads":20000000,"server_side":"optional","client_side":"required",
          "license":"MIT","members":[{"user_id":"u1","role":"Owner","name":"jelly"}]
        }"#;
        let p: Project = serde_json::from_str(json).unwrap();
        assert_eq!(p.project_type, ProjectType::Mod);
        assert_eq!(p.server_side, EnvSupport::Optional);
        assert_eq!(p.client_side, EnvSupport::Required);
        assert_eq!(p.members[0].role.as_deref(), Some("Owner"));
        // An unknown project type must not break a search page.
        let hit: SearchHit = serde_json::from_str(
            r#"{"project_id":"x","project_type":"world_template","slug":"s","title":"t"}"#,
        )
        .unwrap();
        assert_eq!(hit.project_type, ProjectType::Unknown);
        assert_eq!(hit.server_side, None);
        let v: ModrinthVersion = serde_json::from_str(
            r#"{"id":"v","project_id":"p","name":"n","version_number":"1.0",
                "files":[{"url":"https://c/f.jar","filename":"f.jar","size":12,
                          "hashes":{"sha1":"00","sha512":"11"}}],
                "required_envs":{"client":"required","server":"unsupported"},
                "dependencies":[{"project_id":"p2","dependency_type":"incompatible"}]}"#,
        )
        .unwrap();
        assert_eq!(v.required_envs.server, EnvSupport::Unsupported);
        assert_eq!(
            v.dependencies[0].dependency_type,
            DependencyType::Incompatible
        );
        assert_eq!(v.files[0].hashes.sha1.as_deref(), Some("00"));
        assert_eq!(v.files[0].primary, false);
    }

    #[test]
    fn notices_and_error_mapping() {
        let n: Notice =
            serde_json::from_str(r#"{"id":"1","msg":"read-only mode","severity":"warning"}"#)
                .unwrap();
        assert_eq!(n.severity, NoticeSeverity::Warning);
        let e = upstream_err("nope", 429);
        assert_eq!(e.code(), "upstream");
        match e {
            CoreError::Upstream {
                service,
                status,
                body,
            } => {
                assert_eq!(service, "modrinth");
                assert_eq!(status, 429);
                assert_eq!(body, "nope");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            e.status(),
            502,
            "an upstream failure is a bad-gateway to the client"
        );
    }
}

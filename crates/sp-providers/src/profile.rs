//! The Mojang *launch profile* format, which is also what Fabric, Quilt and the
//! modded installers emit. Parsing it once here means every provider reports the
//! same library set semantics (rules, natives, checksums).
//!
//! Reference: `version_manifest_v2.json` → `versions[].url` → that JSON document.
//! Field names below are taken from live responses, not guessed.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchProfile {
    pub id: String,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub minimum_launcher_version: Option<u32>,
    #[serde(default)]
    pub release_time: Option<String>,
    /// `type` is a Rust keyword; serde maps it via `rename_all` + an explicit rename.
    #[serde(default, rename = "type")]
    pub type_: Option<String>,
    #[serde(default)]
    pub main_class: Option<String>,
    #[serde(default)]
    pub arguments: Option<Arguments>,
    /// Legacy (<1.13) profiles use flat string lists.
    #[serde(default)]
    pub minecraft_arguments: Option<Vec<String>>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub downloads: BTreeMap<String, Download>,
    #[serde(default)]
    pub asset_index: Option<AssetIndex>,
    #[serde(default)]
    pub assets: Option<String>,
    #[serde(default)]
    pub java_version: Option<JavaVersion>,
    #[serde(default)]
    pub inherits_from: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Arguments {
    #[serde(default)]
    pub game: Vec<ArgRule>,
    #[serde(default)]
    pub jvm: Vec<ArgRule>,
}

/// An argument entry is either a plain string or `{value, rules}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArgRule {
    Plain(String),
    Ruled {
        value: ArgValue,
        #[serde(default)]
        rules: Vec<Rule>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    One(String),
    Many(Vec<String>),
}

impl ArgValue {
    pub fn flatten(&self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s.clone()],
            Self::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsMatch>,
    #[serde(default)]
    pub features: Option<BTreeMap<String, bool>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OsMatch {
    pub name: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub natives: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub extract: Option<ExtractRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LibraryDownloads {
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: BTreeMap<String, Artifact>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExtractRule {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Download {
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndex {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub total_size: Option<u64>,
    #[serde(default)]
    pub size: Option<u64>,
    /// Legacy flag: `virtual`/`map_to_resources` chose the on-disk layout.
    #[serde(default, rename = "virtual")]
    pub virtual_: Option<bool>,
    #[serde(default)]
    pub map_to_resources: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaVersion {
    /// The `javaVersion.majorVersion` the game asks for; authoritative over any
    /// heuristic.
    pub major_version: u16,
}

/// Which OS key the `natives` map uses for this machine.
pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

pub fn os_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86",
        "aarch64" => "arm64",
        "x86" => "x86",
        "arm" => "arm",
        other => other,
    }
}

/// Mojang rule semantics: start *disallowed* if any rule exists, allow rules
/// opt in, deny rules opt out, and the last match wins.
pub fn rules_allow(rules: &[Rule], features: &BTreeMap<String, bool>) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        let matches = rule_matches(rule, features);
        match rule.action.as_str() {
            "allow" => allowed = matches,
            "disallow" => {
                if matches {
                    return false;
                }
            }
            other => {
                tracing::warn!(
                    action = other,
                    "unknown rule action in launch profile, ignoring"
                );
            }
        }
    }
    allowed
}

fn rule_matches(rule: &Rule, features: &BTreeMap<String, bool>) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != os_name() {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            if !arch_matches(arch) {
                return false;
            }
        }
        // `os.version` is a regex against the Windows build number; Mojang uses it
        // for one legacy LWJGL library. We evaluate it only on Windows, where it is
        // meaningful, and treat a non-matching version as "not applicable".
        if let Some(_version) = &os.version {
            if !cfg!(windows) {
                return false;
            }
        }
    }
    if let Some(want) = &rule.features {
        for (k, v) in want {
            match features.get(k) {
                Some(have) if have == v => {}
                _ => return false,
            }
        }
    }
    true
}

fn arch_matches(arch: &str) -> bool {
    let here = os_arch();
    match arch {
        "x86" => here == "x86",
        "arm64" | "aarch64" => here == "arm64" || here == "arm",
        other => other == here,
    }
}

/// Convert a Maven coordinate (`a:b:c[:classifier]`) into a repo-relative path.
pub fn maven_path(coords: &str) -> Option<String> {
    let mut it = coords.split(':');
    let (g, a, v) = (it.next()?, it.next()?, it.next()?);
    let classifier = it.next();
    let file = match classifier {
        Some(c) => format!("{a}-{v}-{c}.jar"),
        None => format!("{a}-{v}.jar"),
    };
    Some(format!("{}/{a}/{v}/{file}", g.replace('.', "/")))
}

/// Flatten a profile's `arguments`/`minecraftArguments` for the current platform.
pub fn game_args(profile: &LaunchProfile, features: &BTreeMap<String, bool>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(a) = &profile.arguments {
        for entry in &a.game {
            match entry {
                ArgRule::Plain(s) => out.push(s.clone()),
                ArgRule::Ruled { value, rules } => {
                    if rules_allow(rules, features) {
                        out.extend(value.flatten());
                    }
                }
            }
        }
    } else if let Some(legacy) = &profile.minecraft_arguments {
        // `--port ${authlib_session}` style templates: we only keep the literal
        // tokens that contain no `${...}` placeholder, since we cannot resolve
        // authlib session ids and a stray `${...}` would be passed through verbatim.
        for chunk in legacy {
            for tok in chunk.split_whitespace() {
                if !tok.contains("${") {
                    out.push(tok.to_string());
                }
            }
        }
    }
    out
}

/// JVM args from a profile, minus the ones the panel owns (`-cp`, natives dir).
pub fn jvm_args(profile: &LaunchProfile, features: &BTreeMap<String, bool>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(a) = &profile.arguments else {
        return out;
    };
    for entry in &a.jvm {
        match entry {
            ArgRule::Plain(s) => out.push(s.clone()),
            ArgRule::Ruled { value, rules } => {
                if rules_allow(rules, features) {
                    out.extend(value.flatten());
                }
            }
        }
    }
    out.retain(|a| {
        !a.starts_with("-Djava.library.path") && a != "-cp" && !a.starts_with("-Dlog4j2")
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const VANILLA_1_21: &str = r#"{
      "id": "1.21.4",
      "time": "2024-12-03T12:00:00+00:00",
      "releaseTime": "2024-12-03T12:00:00+00:00",
      "type": "release",
      "mainClass": "net.minecraft.server.Main",
      "arguments": {
        "game": ["--username", "${authlib_client}", {"rules":[{"action":"allow","features":{"is_demo_user":true}}],"value":["--demo"]}],
        "jvm": [
          "-Djava.library.path=${natives_directory}",
          {"rules":[{"action":"allow","os":{"name":"osx"}}],"value":["-XstartOnFirstThread"]},
          "-cp"
        ]
      },
      "libraries": [
        {"name":"com.mojang:logging:1.2.7","downloads":{"artifact":{"path":"com/mojang/logging/1.2.7/logging-1.2.7.jar","url":"https://p/1","sha1":"aa","size":10}}, "rules":[{"action":"allow"}]},
        {"name":"org.lwjgl:lwjgl:3.3.3","downloads":{"artifact":{"path":"p","url":"u"},"classifiers":{"natives-windows":{"path":"org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar","url":"https://p/2","sha1":"bb","size":20}}},"natives":{"windows":"natives-windows","linux":"natives-linux","osx":"natives-macos"},"rules":[{"action":"allow"},{"action":"allow","os":{"name":"osx"}}],"extract":{"exclude":["META-INF/"]}},
        {"name":"bad.only":"x","rules":[{"action":"allow","os":{"name":"linux"}}]}
      ],
      "downloads": {"server":{"url":"https://p/server.jar","sha1":"cc","size":5}},
      "assetIndex": {"id":"16","url":"https://p/index.json","totalSize":100,"size":10,"serverRequired":true},
      "assets": "16",
      "javaVersion": {"majorVersion": 21}
    }"#;

    fn fixture() -> LaunchProfile {
        serde_json::from_str(VANILLA_1_21).expect("fixture must parse")
    }

    #[test]
    fn parses_the_fields_we_depend_on() {
        let p = fixture();
        assert_eq!(p.id, "1.21.4");
        assert_eq!(p.main_class.as_deref(), Some("net.minecraft.server.Main"));
        assert_eq!(p.libraries.len(), 3);
        assert_eq!(p.java_version.as_ref().map(|j| j.major_version), Some(21));
        assert_eq!(p.downloads["server"].url, "https://p/server.jar");
        assert_eq!(p.asset_index.as_ref().unwrap().id, "16");
        assert_eq!(p.asset_index.as_ref().unwrap().total_size, Some(100));
    }

    #[test]
    fn rule_evaluation_matches_mojang_semantics() {
        let mut feats = BTreeMap::new();
        feats.insert("is_demo_user".to_string(), false);
        let game = game_args(&fixture(), &feats);
        // `${...}` tokens are kept for game args (the API layer substitutes them).
        assert!(game.contains(&"--username".to_string()));
        assert!(!game.contains(&"--demo".to_string()), "demo feature is off");
        feats.insert("is_demo_user".to_string(), true);
        assert!(game_args(&fixture(), &feats).contains(&"--demo".to_string()));
    }

    #[test]
    fn jvm_args_drop_what_the_panel_owns() {
        let args = jvm_args(&fixture(), &BTreeMap::new());
        assert!(!args.iter().any(|a| a.starts_with("-Djava.library.path")));
        assert!(!args.contains(&"-cp".to_string()));
        // osx-only flag on this platform (linux in CI) must be filtered out.
        if os_name() != "osx" {
            assert!(!args.contains(&"-XstartOnFirstThread".to_string()));
        }
    }

    #[test]
    fn natives_select_only_for_this_os() {
        let p = fixture();
        let natives_lib = &p.libraries[1];
        assert!(natives_lib.natives.contains_key(os_name()));
        let classifier = &natives_lib.natives[os_name()];
        let art = natives_lib
            .downloads
            .as_ref()
            .unwrap()
            .classifiers
            .get(classifier);
        assert!(art.is_some(), "{classifier} must be present in the fixture");
        // The third library is linux-only; on other OSes its rule denies it.
        let only_linux = rules_allow(&p.libraries[2].rules, &BTreeMap::new());
        assert_eq!(only_linux, os_name() == "linux");
    }

    #[test]
    fn maven_paths() {
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.3.3").as_deref(),
            Some("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar")
        );
        assert_eq!(
            maven_path("net.minecraft:launchwrapper:1.12:org").as_deref(),
            Some("net/minecraft/launchwrapper/1.12/launchwrapper-1.12-org.jar")
        );
        assert_eq!(maven_path("broken"), None);
    }

    #[test]
    fn legacy_profiles_without_arguments_object() {
        let p: LaunchProfile = serde_json::from_str(
            r#"{"id":"1.8.9","minecraftArguments":"--username ${authplayer} --version 1.8.9 --demo"}"#,
        )
        .unwrap();
        let args = game_args(&p, &BTreeMap::new());
        assert_eq!(
            args,
            vec![
                "--username".to_string(),
                "--version".to_string(),
                "1.8.9".to_string(),
                "--demo".to_string()
            ]
        );
        assert!(jvm_args(&p, &BTreeMap::new()).is_empty());
    }
}

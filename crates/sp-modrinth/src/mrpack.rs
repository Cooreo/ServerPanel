//! `.mrpack` — the Modrinth modpack format (index version 1).
//!
//! A `.mrpack` is a zip holding `modrinth.index.json` plus `overrides/` (and the
//! optional `client-overrides/` / `server-overrides/`). Two details are where naive
//! readers go wrong, and both are handled here:
//!
//! * **Hashes in the index are Base64**, while the REST API returns hex. A file
//!   downloaded against a `.mrpack` therefore has to be verified with the converted
//!   digest, or every "skip what is already correct" check silently never matches.
//! * **`envs` decides whether a file belongs on a server at all.** A `["client"]`
//!   entry is a shader-pack or a client mod: copying it into a server `mods/` folder
//!   is how "my server crashes on a clean install" gets filed as a bug.
//!
//! Overrides are extracted *before* downloads are planned, because an override can
//! legitimately replace a file the index also lists (`overrides_action`).

use base64::Engine;
use serde::{Deserialize, Serialize};
use sp_core::CoreError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const INDEX_FILE: &str = "modrinth.index.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackMeta {
    /// Format version; only `1` exists today and anything else is refused rather
    /// than half-parsed.
    pub version: u32,
    #[serde(default)]
    pub index_file: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Client,
    #[default]
    Server,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackFileOptions {
    /// `false` means "recommended, not required" — skipped unless the user opts in.
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackHashes {
    /// Base64 in the file, hex everywhere else in this code base.
    pub sha1: String,
    #[serde(default)]
    pub sha512: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackFile {
    pub path: String,
    pub hashes: PackHashes,
    #[serde(default)]
    pub envs: Vec<Side>,
    #[serde(default)]
    pub downloads: Vec<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub options: Option<PackFileOptions>,
}

impl PackFile {
    pub fn sha1_hex(&self) -> Result<String, CoreError> {
        b64_to_hex(&self.hashes.sha1)
    }

    pub fn sha512_hex(&self) -> Result<Option<String>, CoreError> {
        match &self.hashes.sha512 {
            Some(s) => b64_to_hex(s).map(Some),
            None => Ok(None),
        }
    }

    /// Which side(s) need this file. An absent `envs` means both, per the spec.
    pub fn wanted_by(&self, side: Side) -> bool {
        if self.envs.is_empty() {
            return true;
        }
        self.envs.iter().any(|s| *s == side || *s == Side::Both) || matches!(side, Side::Both)
    }

    pub fn is_required(&self) -> bool {
        self.options.as_ref().map(|o| o.required).unwrap_or(true)
    }

    pub fn file_name(&self) -> String {
        self.path
            .rsplit('/')
            .next()
            .unwrap_or("download.bin")
            .to_string()
    }
}

fn b64_to_hex(s: &str) -> Result<String, CoreError> {
    let s = s.trim();
    // Some exports contain hex already; accept both so a hand-written pack works.
    if s.len() % 2 == 0 && !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(s.to_ascii_lowercase());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| CoreError::other(format!("modpack hash is neither base64 nor hex: {e}")))?;
    Ok(hex::encode(bytes))
}

/// A parsed, validated `.mrpack` index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackIndex {
    pub meta: PackMeta,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `minecraft`, `fabric-loader`, `quilt-loader`, `forge`, `neoforge`.
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<PackFile>,
}

impl PackIndex {
    /// Parse and validate the index document alone.
    pub fn from_json(text: &str) -> Result<Self, CoreError> {
        let idx: PackIndex = serde_json::from_str(text).map_err(|e| {
            CoreError::other(format!("{INDEX_FILE} is not valid Modrinth index JSON: {e}"))
        })?;
        if idx.meta.version != 1 {
            return Err(CoreError::other(format!(
                "unsupported .mrpack format version {} (this build understands version 1)",
                idx.meta.version
            )));
        }
        if let Some(f) = &idx.meta.index_file {
            if f != INDEX_FILE {
                return Err(CoreError::other(format!(
                    "pack declares index_file `{f}`; only {INDEX_FILE} is supported"
                )));
            }
        }
        // Validate paths up front: a bad path is rejected before a single byte is
        // written, not halfway through a 40-file install.
        for f in &idx.files {
            validate_rel_path(&f.path).map_err(|e| {
                CoreError::PathEscape(format!("pack file {} is not a safe relative path: {e}", f.path))
            })?;
            if f.downloads.is_empty() {
                return Err(CoreError::other(format!(
                    "pack file {} lists no download URLs",
                    f.path
                )));
            }
            // Fail here rather than mid-install on file 30 of 40.
            f.sha1_hex()?;
        }
        Ok(idx)
    }

    /// Read a `.mrpack` from disk.
    pub fn open(pack: &Path) -> Result<Self, CoreError> {
        let file = std::fs::File::open(pack)?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| {
            CoreError::other(format!("{} is not a zip archive: {e}", pack.display()))
        })?;
        let mut entry = zip.by_name(INDEX_FILE).map_err(|_| {
            CoreError::other(format!(
                "{} has no {INDEX_FILE} at its root — it is not a .mrpack",
                pack.display()
            ))
        })?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut entry, &mut text)?;
        Self::from_json(&text)
    }

    /// Which loader the pack needs, as `(loader, version)`.
    pub fn loader(&self) -> Option<(String, String)> {
        for key in ["fabric-loader", "quilt-loader", "neoforge", "forge"] {
            if let Some(v) = self.dependencies.get(key) {
                return Some((key.to_string(), v.clone()));
            }
        }
        None
    }

    pub fn minecraft_version(&self) -> Option<&str> {
        self.dependencies.get("minecraft").map(String::as_str)
    }

    pub fn loader_server_kind(&self) -> Option<&'static str> {
        Some(match self.loader()?.0.as_str() {
            "fabric-loader" => "fabric",
            "quilt-loader" => "quilt",
            "neoforge" => "neoforge",
            "forge" => "forge",
            _ => return None,
        })
    }

    /// Files to download for `side`, in index order, optionally including the
    /// non-required ones.
    pub fn downloads_for(&self, side: Side, include_optional: bool) -> Vec<&PackFile> {
        self.files
            .iter()
            .filter(|f| f.wanted_by(side))
            .filter(|f| include_optional || f.is_required())
            .collect()
    }
}

fn validate_rel_path(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("empty path".into());
    }
    if p.starts_with('/') || (p.len() > 1 && &p[1..2] == ":") {
        return Err("absolute path".into());
    }
    for seg in p.split('/') {
        match seg {
            "" | "." => return Err(format!("redundant segment `{seg}`")),
            ".." => return Err("parent traversal".into()),
            s if s.chars().any(|c| matches!(c, '\0' | ':' | '<' | '>' | '|')) => {
                return Err(format!("unsafe characters in `{s}`"))
            }
            _ => {}
        }
    }
    Ok(())
}

/// One planned download: the index entry plus where it lands and what to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub path: String,
    pub url: String,
    pub sha1_hex: String,
    pub sha512_hex: Option<String>,
    pub size: Option<u64>,
}

/// Diff the pack against what is already on disk so a re-install fetches only what
/// changed, and a corrupt file is re-fetched rather than kept.
pub fn plan(
    index: &PackIndex,
    server_dir: &Path,
    side: Side,
    include_optional: bool,
) -> Result<Vec<Planned>, CoreError> {
    let mut out = Vec::new();
    for f in index.downloads_for(side, include_optional) {
        let dest = safe_join(server_dir, &f.path)?;
        let want = f.sha1_hex()?;
        // An unreadable file is treated as "not matching", so a permissions problem
        // causes a re-download rather than a failed install.
        let skip = dest.is_file()
            && sp_core::verify::hash_file(&dest)
                .ok()
                .and_then(|have| have.sha1)
                .map(|have| have.eq_ignore_ascii_case(&want))
                .unwrap_or(false);
        if skip {
            tracing::debug!(path = %f.path, "pack file already correct, skipping download");
            continue;
        }
        out.push(Planned {
            path: f.path.clone(),
            url: f
                .downloads
                .first()
                .cloned()
                .ok_or_else(|| CoreError::other(format!("{} lists no URLs", f.path)))?,
            sha1_hex: want,
            sha512_hex: f.sha512_hex()?,
            size: f.file_size,
        });
    }
    Ok(out)
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, CoreError> {
    let mut out = root.to_path_buf();
    for seg in rel.replace('\\', "/").split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(CoreError::PathEscape(rel.to_string())),
            s => out.push(s),
        }
    }
    Ok(out)
}

/// What a pack's overrides contain, before touching anything.
#[derive(Debug, Clone, Default)]
pub struct OverrideSummary {
    /// Archive entry name → declared size.
    pub entries: BTreeMap<String, u64>,
    pub server_specific: usize,
}

impl OverrideSummary {
    pub fn total(&self) -> usize {
        self.entries.len()
    }
    pub fn bytes(&self) -> u64 {
        self.entries.values().sum()
    }
}

/// Inspect the override trees of a pack.
pub fn overrides(pack: &Path) -> Result<OverrideSummary, CoreError> {
    let file = std::fs::File::open(pack)?;
    let zip = zip::ZipArchive::new(file).map_err(|e| CoreError::other(format!("zip: {e}")))?;
    let mut out = OverrideSummary::default();
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| CoreError::other(format!("zip: {e}")))?;
        let name = match entry.enclosed_name() {
            Some(n) => n.to_string_lossy().replace('\\', "/"),
            None => return Err(CoreError::PathEscape(format!("zip entry {}", entry.name()))),
        };
        if entry.is_dir() {
            continue;
        }
        if let Some(rel) = name.strip_prefix("server-overrides/") {
            out.entries.insert(rel.to_string(), entry.size());
            out.server_specific += 1;
        } else if let Some(rel) = name.strip_prefix("overrides/") {
            out.entries.entry(rel.to_string()).or_insert(entry.size());
        }
    }
    Ok(out)
}

/// Extract `overrides/` then `server-overrides/` (the latter wins) into `dest`.
///
/// Existing files are left alone unless `overwrite` is set, because a user's edited
/// `options.txt` or a world config is more valuable than a pack's default; the
/// caller decides and the UI asks first.
pub fn extract_overrides(
    pack: &Path,
    dest: &Path,
    overwrite: bool,
) -> Result<ExtractReport, CoreError> {
    let file = std::fs::File::open(pack)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| CoreError::other(format!("zip: {e}")))?;
    let mut report = ExtractReport::default();
    // Two passes so server-overrides land after (and therefore beat) the common ones.
    for prefix in ["overrides/", "server-overrides/"] {
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| CoreError::other(format!("zip entry: {e}")))?;
            let raw = entry.name().to_string();
            let Some(rel) = raw.strip_prefix(prefix) else { continue };
            if rel.is_empty() || rel.ends_with('/') {
                continue;
            }
            let rel = match entry.enclosed_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => {
                    return Err(CoreError::PathEscape(format!(
                        "pack override `{raw}` tries to write outside the server directory"
                    )))
                }
            };
            let rel = rel.rsplit(prefix).next().unwrap_or(rel.as_str()).to_string();
            validate_rel_path(&rel)?;
            let out = safe_join(dest, &rel)?;
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if out.exists() && !overwrite {
                report.skipped += 1;
                continue;
            }
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf)?;
            std::fs::write(&out, &buf)?;
            report.written += 1;
            report.bytes += buf.len() as u64;
            report.paths.push(rel);
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    pub written: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A tiny but faithful `.mrpack`: base64 hashes, client-only file, override
    /// trees, and a non-required optional file.
    fn make_pack(dir: &Path) -> PathBuf {
        let sha1 = |bytes: &[u8]| -> String {
            use sha1::Digest;
            base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(bytes))
        };
        let jar = b"MODJAR".to_vec();
        let opts = b"spawn-protection=0\n".to_vec();
        let srv_only = b"server.properties\n".to_vec();
        let path = dir.join("pack.mrpack");
        let file = std::fs::File::create(&path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let fo = zip::write::SimpleFileOptions::default();
        w.add_directory("overrides", fo).unwrap();
        w.start_file("overrides/options.txt", fo).unwrap();
        w.write_all(&opts).unwrap();
        w.add_directory("server-overrides", fo).unwrap();
        w.start_file("server-overrides/options.txt", fo).unwrap();
        w.write_all(&srv_only).unwrap();
        w.start_file(INDEX_FILE, fo).unwrap();
        let index = serde_json::json!({
            "meta": {"version": 1, "index_file": INDEX_FILE},
            "name": "Test Pack",
            "version": "1.2.0",
            "author": "someone",
            "description": "a pack for tests",
            "dependencies": {"minecraft": "1.21.4", "fabric-loader": "0.16.9"},
            "files": [
              {"path":"mods/testmod.jar","hashes":{"sha1": sha1(&jar), "sha512": null},
               "envs":["client","server"],"downloads":["https://cdn.modrinth.test/data/x/versions/y/testmod.jar"],
               "fileSize": jar.len(), "options": {"required": true}},
              {"path":"mods/client-only.jar","hashes":{"sha1": sha1(b"client")},
               "envs":["client"],"downloads":["https://cdn.modrinth.test/client-only.jar"],"fileSize":6},
              {"path":"mods/optional.jar","hashes":{"sha1": sha1(b"opt")},
               "downloads":["https://cdn.modrinth.test/optional.jar"],"fileSize":3,
               "options":{"required": false}},
              {"path":"world/datapacks/bonus.zip","hashes":{"sha1": sha1(b"dp")},
               "downloads":["https://cdn.modrinth.test/bonus.zip"],"fileSize":2}
            ]
        });
        w.write_all(serde_json::to_string(&index).unwrap().as_bytes()).unwrap();
        w.finish().unwrap();
        let _ = jar;
        path
    }

    #[test]
    fn index_parses_and_base64_hashes_become_hex() {
        let dir = tempfile::tempdir().unwrap();
        let pack = make_pack(dir.path());
        let idx = PackIndex::open(&pack).unwrap();
        assert_eq!(idx.name, "Test Pack");
        assert_eq!(idx.version.as_deref(), Some("1.2.0"));
        assert_eq!(idx.minecraft_version(), Some("1.21.4"));
        assert_eq!(idx.loader().unwrap().0, "fabric-loader");
        assert_eq!(idx.loader_server_kind(), Some("fabric"));
        assert_eq!(idx.files.len(), 4);
        let hex = idx.files[0].sha1_hex().unwrap();
        assert_eq!(hex.len(), 40, "sha1 must land as 40 hex chars");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{hex}");
        assert_eq!(
            hex,
            {
                use sha1::Digest;
                hex::encode(sha1::Sha1::digest(b"MODJAR"))
            },
            "base64 → hex must match what sp_core::verify computes"
        );
        assert!(idx.files[0].sha512_hex().unwrap().is_none(), "null sha512 stays None");
    }

    #[test]
    fn side_filtering_and_required_flagging() {
        let dir = tempfile::tempdir().unwrap();
        let idx = PackIndex::open(&make_pack(dir.path())).unwrap();
        let server: Vec<&str> = idx
            .downloads_for(Side::Server, false)
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(
            server,
            vec!["mods/testmod.jar", "world/datapacks/bonus.zip"],
            "client-only is excluded, optional is excluded, missing envs means both sides"
        );
        let with_optional: Vec<&str> = idx
            .downloads_for(Side::Server, true)
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(with_optional.contains(&"mods/optional.jar"));
        assert_eq!(idx.downloads_for(Side::Client, true).len(), 4);
        assert!(!idx.files[2].is_required());
        assert!(idx.files[1].sha1_hex().is_ok());
    }

    #[test]
    fn plan_skips_files_that_are_already_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let pack = make_pack(dir.path());
        let idx = PackIndex::open(&pack).unwrap();
        let server = dir.path().join("srv");
        std::fs::create_dir_all(server.join("mods")).unwrap();

        // Nothing on disk yet: both server files are planned.
        let p = plan(&idx, &server, Side::Server, false).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].url, "https://cdn.modrinth.test/data/x/versions/y/testmod.jar");

        // Write the correct bytes → that entry disappears from the plan.
        std::fs::write(server.join("mods/testmod.jar"), b"MODJAR").unwrap();
        let p = plan(&idx, &server, Side::Server, false).unwrap();
        assert_eq!(p.len(), 1, "an already-correct file must not be re-downloaded");
        assert_eq!(p[0].path, "world/datapacks/bonus.zip");

        // Corrupt it → planned again, because the hash no longer matches.
        std::fs::write(server.join("mods/testmod.jar"), b"corrupted!").unwrap();
        let p = plan(&idx, &server, Side::Server, false).unwrap();
        assert_eq!(p.len(), 2, "a corrupt file must be re-fetched, not trusted");
    }

    #[test]
    fn overrides_are_inspected_and_server_ones_win() {
        let dir = tempfile::tempdir().unwrap();
        let pack = make_pack(dir.path());
        let summary = overrides(&pack).unwrap();
        assert_eq!(summary.total(), 1, "one logical path, present in both trees");
        assert_eq!(summary.server_specific, 1);
        assert!(summary.bytes() > 0);

        let dest = dir.path().join("out");
        let report = extract_overrides(&pack, &dest, false).unwrap();
        assert_eq!(report.written, 2, "overrides then server-overrides");
        assert_eq!(
            std::fs::read(dest.join("options.txt")).unwrap(),
            b"server.properties\n",
            "server-overrides must take precedence"
        );

        // Without overwrite, the second pass keeps the user's edit.
        std::fs::write(dest.join("options.txt"), b"my settings").unwrap();
        let again = extract_overrides(&pack, &dest, false).unwrap();
        assert_eq!(again.written, 0);
        assert_eq!(again.skipped, 2);
        assert_eq!(std::fs::read(dest.join("options.txt")).unwrap(), b"my settings");
        // With overwrite, the pack wins and says so.
        let forced = extract_overrides(&pack, &dest, true).unwrap();
        assert_eq!(forced.written, 2);
        assert!(forced.paths.contains(&"options.txt".to_string()));
    }

    #[test]
    fn malformed_and_hostile_packs_are_refused() {
        // Wrong format version.
        let bad = r#"{"meta":{"version":2,"index_file":"modrinth.index.json"},"name":"x","files":[]}"#;
        let err = PackIndex::from_json(bad).unwrap_err();
        assert!(err.to_string().contains("format version"), "{err}");
        // Wrong index file name.
        let err = PackIndex::from_json(
            r#"{"meta":{"version":1,"index_file":"other.json"},"name":"x","files":[]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("other.json"), "{err}");
        // Traversal path inside the index.
        let err = PackIndex::from_json(
            r#"{"meta":{"version":1},"name":"x","files":[{"path":"../escape.jar","hashes":{"sha1":"AA"},"downloads":["https://x"]}]}"#,
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::PathEscape(_)), "{err}");
        // Absolute path.
        assert!(PackIndex::from_json(
            r#"{"meta":{"version":1},"name":"x","files":[{"path":"/etc/passwd","hashes":{"sha1":"AA"},"downloads":["https://x"]}]}"#
        )
        .is_err());
        // Windows drive path.
        assert!(PackIndex::from_json(
            r#"{"meta":{"version":1},"name":"x","files":[{"path":"C:/x.jar","hashes":{"sha1":"AA"},"downloads":["https://x"]}]}"#
        )
        .is_err());
        // No URLs.
        assert!(PackIndex::from_json(
            r#"{"meta":{"version":1},"name":"x","files":[{"path":"mods/a.jar","hashes":{"sha1":"AA"},"downloads":[]}]}"#
        )
        .is_err());
        // Not a zip at all.
        let dir = tempfile::tempdir().unwrap();
        let not_zip = dir.path().join("x.mrpack");
        std::fs::write(&not_zip, b"PK\x03\x04 not really").unwrap();
        assert!(PackIndex::open(&not_zip).is_err());
        // A zip without an index.
        let no_index = dir.path().join("y.mrpack");
        {
            let f = std::fs::File::create(&no_index).unwrap();
            let mut w = zip::ZipWriter::new(f);
            use zip::write::SimpleFileOptions as O;
            w.start_file("README.txt", O::default()).unwrap();
            std::io::Write::write_all(&mut w, b"hi").unwrap();
            w.finish().unwrap();
        }
        let err = PackIndex::open(&no_index).unwrap_err();
        assert!(err.to_string().contains(INDEX_FILE), "{err}");
    }

    #[test]
    fn path_validation_rejects_the_usual_suspects() {
        for bad in ["", "..", "../x", "/x", "C:/x", "a/../b", "mods/a<b>.jar", "mods\0"] {
            assert!(validate_rel_path(bad).is_err(), "{bad} must be rejected");
        }
        for ok in ["mods/a.jar", "world/datapacks/x.zip", "config/sub/y.toml"] {
            assert!(validate_rel_path(ok).is_ok(), "{ok} must be accepted");
        }
    }

    #[test]
    fn hash_decoding_accepts_both_encodings() {
        // base64 (the spec) and hex (a hand-written pack) both work.
        let b64 = base64::engine::general_purpose::STANDARD.encode([0xab; 20]);
        assert_eq!(b64_to_hex(&b64).unwrap(), "ab".repeat(20));
        assert_eq!(b64_to_hex(&"AB".repeat(20)).unwrap(), "ab".repeat(20));
        assert!(b64_to_hex("!!!not base64!!!").is_err());
        assert!(b64_to_hex("").is_err());
    }
}

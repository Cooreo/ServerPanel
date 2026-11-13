//! The self-describing part of an archive.
//!
//! `serverpanel-backup.json` sits at the root of every archive this crate writes.
//! It is what turns "a big file on disk" into something the panel can answer
//! questions about without a second copy of the data: what is in here, how big,
//! when, and *which digest does each file have*.

use crate::selector::SkippedKind;
use serde::{Deserialize, Serialize};
use sp_core::CoreError;
use std::io::Read;

/// Name of the manifest entry inside an archive.
pub const MANIFEST_NAME: &str = "serverpanel-backup.json";
/// Bump when the shape changes in a way older panels cannot read.
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// `/`-separated path relative to the server root.
    pub path: String,
    pub bytes: u64,
    pub modified_ms: i64,
    /// SHA-256 of the file's *contents*, hex, lowercase.
    pub sha256: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: SkippedKind,
    /// Why, when there is more to say (an OS error string, mostly).
    #[serde(default)]
    pub detail: Option<String>,
}

/// Contents of a written archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub panel: String,
    /// `zip` | `tar.zst` — recorded so a reader can trust the extension less.
    pub format: String,
    pub server_name: String,
    pub created_at_ms: i64,
    pub file_count: u32,
    pub total_bytes: u64,
    /// `true` when the world was flushed or the server was stopped.
    pub world_consistent: bool,
    pub entries: Vec<ManifestEntry>,
    /// Directories that must be recreated even though they hold no files.
    #[serde(default)]
    pub empty_dirs: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<SkippedEntry>,
}

impl Manifest {
    pub fn new(format: &str, server_name: String, created_at_ms: i64) -> Self {
        Self {
            version: MANIFEST_VERSION,
            panel: concat!("serverpanel-", env!("CARGO_PKG_VERSION")).to_string(),
            format: format.to_string(),
            server_name,
            created_at_ms,
            file_count: 0,
            total_bytes: 0,
            world_consistent: false,
            entries: Vec::new(),
            empty_dirs: Vec::new(),
            skipped: Vec::new(),
        }
    }

    pub fn push(&mut self, e: ManifestEntry) {
        self.total_bytes += e.bytes;
        self.file_count += 1;
        self.entries.push(e);
    }

    pub fn to_json_vec(&self) -> Result<Vec<u8>, CoreError> {
        serde_json::to_vec_pretty(self).map_err(|e| CoreError::Serde(e.to_string()))
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, CoreError> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| CoreError::other(format!("backup manifest is unreadable: {e}")))?;
        if m.version > MANIFEST_VERSION {
            return Err(CoreError::other(format!(
                "this backup was written by a newer panel (manifest v{}, this build reads v{})",
                m.version, MANIFEST_VERSION
            )));
        }
        Ok(m)
    }

    pub fn from_reader<R: Read>(reader: &mut R) -> Result<Self, CoreError> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        Self::from_json_bytes(&buf)
    }

    pub fn entry(&self, path: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.path.as_str())
    }

    /// A short line for the restore confirmation: "312 files, 1.9 GB, 2026-11-13".
    pub fn describe(&self) -> String {
        let when = chrono::DateTime::from_timestamp_millis(self.created_at_ms)
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "unknown time".into());
        format!(
            "{} files, {} bytes, {}, taken {}",
            self.file_count, self.total_bytes, self.format, when
        )
    }

    /// Refuse to restore a manifest that contradicts itself. A truncated archive,
    /// a hand-edited one, or one from a buggy build should not be able to ask for
    /// `../../.ssh/authorized_keys` merely because the entry list says so.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.entries.len() != self.file_count as usize {
            return Err(CoreError::other(format!(
                "backup manifest lists {} entries but claims {} files",
                self.entries.len(),
                self.file_count
            )));
        }
        let mut sum = 0u64;
        for e in &self.entries {
            if crate::archive::is_safe_rel(&e.path).is_err() {
                return Err(CoreError::PathEscape(format!(
                    "backup manifest contains an unsafe path: {}",
                    e.path
                )));
            }
            if e.sha256.len() != 64 || !e.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(CoreError::other(format!(
                    "backup manifest has a malformed digest for {}",
                    e.path
                )));
            }
            sum = sum.saturating_add(e.bytes);
        }
        // Sizes are advisory, so a mismatch is not fatal — but a manifest claiming
        // 0 bytes for 300 files means the archive is not what we think it is.
        if self.file_count > 0 && sum == 0 {
            return Err(CoreError::other(
                "backup manifest lists files totalling zero bytes; the archive is corrupt",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, bytes: u64) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            bytes,
            modified_ms: 1_700_000_000_000,
            sha256: "ab".repeat(32),
            executable: false,
        }
    }

    #[test]
    fn round_trips_through_json() {
        let mut m = Manifest::new("zip", "creative".into(), 1_731_500_000_000);
        m.push(entry("server.properties", 10));
        m.push(entry("world/level.dat", 90));
        m.empty_dirs.push("plugins".into());
        m.skipped.push(SkippedEntry {
            path: "mods/link.jar".into(),
            reason: SkippedKind::Symlink,
            detail: None,
        });
        m.world_consistent = true;
        let bytes = m.to_json_vec().unwrap();
        let back = Manifest::from_json_bytes(&bytes).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.file_count, 2);
        assert_eq!(back.total_bytes, 100);
        assert_eq!(back.entry("world/level.dat").unwrap().bytes, 90);
        assert!(back.entry("nope").is_none());
        assert_eq!(back.paths().count(), 2);
        let d = back.describe();
        assert!(
            d.contains("2 files") && d.contains("zip") && d.contains("2024"),
            "{d}"
        );
    }

    #[test]
    fn a_newer_manifest_is_refused_with_an_actionable_message() {
        let json = r#"{"version":99,"panel":"serverpanel-9","format":"zip","server_name":"x","created_at_ms":1,"file_count":0,"total_bytes":0,"world_consistent":false,"entries":[]}"#;
        let err = Manifest::from_json_bytes(json.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("newer panel"), "{err}");
    }

    #[test]
    fn validation_catches_the_ways_a_manifest_lies() {
        let mut m = Manifest::new("zip", "s".into(), 1);
        m.push(entry("mods/a.jar", 5));
        m.file_count = 9;
        assert!(
            m.validate()
                .unwrap_err()
                .to_string()
                .contains("claims 9 files"),
            "{}",
            m.validate().unwrap_err()
        );

        let mut m = Manifest::new("zip", "s".into(), 1);
        m.push(entry("../escape", 5));
        assert!(
            matches!(m.validate(), Err(CoreError::PathEscape(_))),
            "{:?}",
            m.validate()
        );

        let mut m = Manifest::new("zip", "s".into(), 1);
        m.push(entry("mods/a.jar", 5));
        m.entries[0].sha256 = "not-a-hash".into();
        assert!(m
            .validate()
            .unwrap_err()
            .to_string()
            .contains("malformed digest"));

        let m = Manifest::new("zip", "s".into(), 1);
        assert!(
            m.validate().is_ok(),
            "an empty manifest is a valid empty backup"
        );

        let mut m = Manifest::new("zip", "s".into(), 1);
        m.push(entry("mods/a.jar", 5));
        m.entries[0].bytes = 0;
        assert!(
            m.validate().is_ok(),
            "per-file 0 bytes is legitimate (an empty file)"
        );
        let mut m = Manifest::new("zip", "s".into(), 1);
        m.file_count = 3;
        m.entries = vec![entry("a", 0), entry("b", 0), entry("c", 0)];
        assert!(m.validate().unwrap_err().to_string().contains("zero bytes"));
    }

    #[test]
    fn unreadable_json_is_an_error_not_a_default() {
        for bad in ["", "{", "[]", "null", r#"{"version":"one"}"#] {
            assert!(
                Manifest::from_json_bytes(bad.as_bytes()).is_err(),
                "{bad:?}"
            );
        }
    }
}

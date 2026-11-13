//! Streaming hashes and download verification.
//!
//! Every downloaded artifact is hashed while it streams to disk (one pass, three
//! hashers) and compared against whatever the publisher provided:
//! * Mojang — SHA-1 per artifact in the version manifest
//! * PaperMC/Purpur/Folia — `sha256` on the download endpoint
//! * Modrinth — SHA-1 and SHA-512 per file, plus the whole-pack SHA-512 in `.mrpack`
//! * Spigot BuildTools output — no published hash; we record ours
//!
//! Comparison is constant-time over the normalised hex to avoid leaking anything
//! about upstream digests through timing (marginal here, free to do).

use crate::error::CoreError;
use sha2::Digest as _;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    pub fn hex_len(&self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    pub fn from_str_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sha1" | "sha-1" => Some(Self::Sha1),
            "sha256" | "sha-256" => Some(Self::Sha256),
            "sha512" | "sha-512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// Infer from the digest length — Modrinth and Mojang both publish bare hex.
    pub fn from_hex_len(len: usize) -> Option<Self> {
        match len {
            40 => Some(Self::Sha1),
            64 => Some(Self::Sha256),
            128 => Some(Self::Sha512),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct Hashes {
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub sha512: Option<String>,
    pub bytes: u64,
}

impl Hashes {
    pub fn digest(&self, algo: HashAlgorithm) -> Option<&str> {
        match algo {
            HashAlgorithm::Sha1 => self.sha1.as_deref(),
            HashAlgorithm::Sha256 => self.sha256.as_deref(),
            HashAlgorithm::Sha512 => self.sha512.as_deref(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sha1.is_none() && self.sha256.is_none() && self.sha512.is_none()
    }

    /// Short form for error messages — never spill a 128-char digest into a toast.
    pub fn digest_short(&self, algo: &str) -> String {
        let full = self
            .digest(match algo {
                "sha512" => HashAlgorithm::Sha512,
                "sha256" => HashAlgorithm::Sha256,
                _ => HashAlgorithm::Sha1,
            })
            .unwrap_or_default();
        if full.len() > 16 {
            format!("{}…", &full[..16])
        } else if full.is_empty() {
            "unknown".to_string()
        } else {
            full.to_string()
        }
    }
}

/// One-pass, three-algorithm hasher used by the download stream.
#[derive(Debug, Default)]
pub struct MultiHasher {
    sha1: sha1::Sha1,
    sha256: sha2::Sha256,
    sha512: sha2::Sha512,
    bytes: u64,
}

impl MultiHasher {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.sha1.update(data);
        self.sha256.update(data);
        self.sha512.update(data);
        self.bytes += data.len() as u64;
    }

    pub fn finish(self) -> Hashes {
        Hashes {
            sha1: Some(hex::encode(self.sha1.finalize())),
            sha256: Some(hex::encode(self.sha256.finalize())),
            sha512: Some(hex::encode(self.sha512.finalize())),
            bytes: self.bytes,
        }
    }
}

/// Hash an in-memory buffer (used for manifest payloads and by tests).
pub fn hash_bytes(data: &[u8]) -> Hashes {
    let mut h = MultiHasher::new();
    h.update(data);
    h.finish()
}

/// Hash a file already on disk.
pub fn hash_file(path: &Path) -> Result<Hashes, CoreError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut h = MultiHasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish())
}

/// Normalise `sha256-abc…`, `SHA-1: ab…`, upper case, and whitespace.
pub fn normalise_digest(input: &str) -> Option<(Option<HashAlgorithm>, String)> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let (label, hex) = match s.split_once([':', '-']) {
        Some((algo, rest)) if rest.len() >= 32 && rest.bytes().all(|b| b.is_ascii_hexdigit()) => {
            (HashAlgorithm::from_str_name(algo), rest)
        }
        _ => (None, s),
    };
    let hex = hex.trim().to_ascii_lowercase();
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let inferred = label.or_else(|| HashAlgorithm::from_hex_len(hex.len()));
    Some((inferred, hex))
}

/// Verify one published digest against a file.
pub fn verify_file(path: &Path, expected: &str) -> Result<Hashes, CoreError> {
    let (algo, want) = normalise_digest(expected)
        .ok_or_else(|| CoreError::validation("sha", format!("unrecognised digest `{expected}`")))?;
    let actual = hash_file(path)?;
    let algo = algo.ok_or_else(|| {
        CoreError::validation("sha", format!("cannot tell which hash `{expected}` is"))
    })?;
    let got = actual.digest(algo).unwrap_or_default().to_ascii_lowercase();
    if constant_time_eq(got.as_bytes(), want.as_bytes()) {
        Ok(actual)
    } else {
        Err(CoreError::ChecksumMismatch {
            file: path.display().to_string(),
            expected: format!("{}:{}", algo.as_str(), want),
            actual: format!("{}:{}", algo.as_str(), got),
        })
    }
}

/// Verify every digest the publisher gave us in a single pass over the file
/// (used by the `.mrpack` installer, where each file carries sha1 *and* sha512).
pub fn verify_hashes(path: &Path, expected: &Hashes) -> Result<(), CoreError> {
    let actual = hash_file(path)?;
    for algo in [
        HashAlgorithm::Sha1,
        HashAlgorithm::Sha256,
        HashAlgorithm::Sha512,
    ] {
        let Some(raw) = expected.digest(algo) else {
            continue;
        };
        let (_, want) = normalise_digest(raw).ok_or_else(|| {
            CoreError::validation("hash", format!("malformed digest `{raw}` in manifest"))
        })?;
        let got = actual.digest(algo).unwrap_or_default();
        if !constant_time_eq(got.as_bytes(), want.as_bytes()) {
            return Err(CoreError::ChecksumMismatch {
                file: path.display().to_string(),
                expected: format!("{}:{}", algo.as_str(), want),
                actual: format!("{}:{}", algo.as_str(), got),
            });
        }
    }
    Ok(())
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

/// 8 hex chars of SHA-1 — what Mojang names vanilla server jars with
/// (`server.jar` used to live at `versions/<sha1>/<sha1>.jar`).
pub fn short_sha1(data: &[u8]) -> String {
    let mut h = sha1::Sha1::new();
    h.update(data);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("blob.bin");
        std::fs::write(&p, content).unwrap();
        (d, p)
    }

    #[test]
    fn known_vectors() {
        // echo -n "" | sha1sum / sha256sum / sha512sum
        let (_t, p) = tmp(b"");
        let h = hash_file(&p).unwrap();
        assert_eq!(
            h.sha1.as_deref().unwrap(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            h.sha256.as_deref().unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(h.sha512.as_deref().unwrap().len(), 128);
        assert_eq!(h.bytes, 0);

        let (_t, p) = tmp(b"abc");
        let h = hash_file(&p).unwrap();
        assert_eq!(
            h.sha256.as_deref().unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            h.sha1.as_deref().unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn streaming_matches_whole_file() {
        let payload: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        let (_t, p) = tmp(&payload);
        let from_file = hash_file(&p).unwrap();
        let mut h = MultiHasher::new();
        for chunk in payload.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(h.finish(), from_file);
    }

    #[test]
    fn normalisation_handles_publisher_formats() {
        let (algo, hexv) = normalise_digest(
            "sha256-E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
        )
        .unwrap();
        assert_eq!(algo, Some(HashAlgorithm::Sha256));
        assert_eq!(hexv.len(), 64);
        assert_eq!(
            normalise_digest("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap()
                .0,
            Some(HashAlgorithm::Sha256)
        );
        assert_eq!(
            normalise_digest("da39a3ee5e6b4b0d3255bfef95601890afd80709")
                .unwrap()
                .0,
            Some(HashAlgorithm::Sha1)
        );
        assert!(normalise_digest("not-a-hash").is_none());
        assert!(normalise_digest("").is_none());
    }

    #[test]
    fn verify_accepts_and_detects_mismatch() {
        let (_t, p) = tmp(b"abc");
        let good = "a9993e364706816aba3e25717850c26c9cd0d89d".to_string();
        verify_file(&p, &good).unwrap();
        verify_file(&p, &format!("sha1-{good}")).unwrap();
        let err = verify_file(&p, &"0".repeat(40)).unwrap_err();
        match err {
            CoreError::ChecksumMismatch {
                expected, actual, ..
            } => {
                assert!(expected.contains("sha1"));
                assert!(actual.contains(&good[..8]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn verify_hashes_checks_every_available_digest() {
        let (_t, p) = tmp(b"abc");
        let good = hash_file(&p).unwrap();
        verify_hashes(&p, &good).unwrap();
        let bad = Hashes {
            sha1: good.sha1.clone(),
            sha256: Some("f".repeat(64)),
            sha512: None,
            bytes: 3,
        };
        let err = verify_hashes(&p, &bad).unwrap_err();
        assert!(err.to_string().contains("sha256"), "{err}");
        // Empty hash set = nothing to check (upstream published none).
        verify_hashes(&p, &Hashes::default()).unwrap();
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn algo_roundtrip() {
        for a in [
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha512,
        ] {
            assert_eq!(HashAlgorithm::from_str_name(a.as_str()), Some(a));
            assert_eq!(HashAlgorithm::from_hex_len(a.hex_len()), Some(a));
        }
        assert_eq!(HashAlgorithm::from_str_name("md5"), None);
        assert_eq!(HashAlgorithm::from_hex_len(32), None);
    }
}

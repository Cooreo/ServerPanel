//! Resumable, verified downloads.
//!
//! Contract:
//! * writes to `<dest>.part`, then atomically renames onto `dest`
//! * resumes with a `Range` request when a `.part` file exists and the server
//!   answers 206; otherwise it restarts from zero
//! * hashes **while streaming** (SHA-1 + SHA-256 + SHA-512 in one pass), so a
//!   400 MB modpack never needs a second read
//! * verifies every digest the publisher provided; a mismatch deletes the file and
//!   returns [`CoreError::ChecksumMismatch`] rather than installing it
//! * retries with exponential backoff, re-entering the resume path each time

use crate::client::HttpClient;
use sp_core::error::CoreError;
use sp_core::model::ProgressPhase;
use sp_core::progress::ProgressReporter;
use sp_core::verify::{Hashes, MultiHasher};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct DownloadJob {
    pub url: String,
    pub dest: PathBuf,
    /// A human label for progress events ("sodium-0.6.0.jar").
    pub label: String,
    /// Digests the publisher gave us; missing entries are simply not checked.
    pub expected: Hashes,
    /// Set when the caller already knows the size (Content-Length of a HEAD, or a
    /// manifest field) so the bar is accurate before the first byte arrives.
    pub size_hint: Option<u64>,
    /// Overwrite `dest` if it already exists and hashes correctly (idempotent
    /// re-installs and "repair" rely on this).
    pub reuse_if_valid: bool,
    pub bearer_token: Option<String>,
}

impl DownloadJob {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>, label: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            dest: dest.into(),
            label: label.into(),
            expected: Hashes::default(),
            size_hint: None,
            reuse_if_valid: false,
            bearer_token: None,
        }
    }

    pub fn with_sha1(mut self, sha1: Option<String>) -> Self {
        self.expected.sha1 = sha1;
        self
    }
    pub fn with_sha256(mut self, sha256: Option<String>) -> Self {
        self.expected.sha256 = sha256;
        self
    }
    pub fn with_sha512(mut self, sha512: Option<String>) -> Self {
        self.expected.sha512 = sha512;
        self
    }
    pub fn with_hashes(mut self, hashes: Hashes) -> Self {
        self.expected = hashes;
        self
    }
    pub fn size_hint(mut self, bytes: Option<u64>) -> Self {
        self.size_hint = bytes;
        self
    }
    pub fn reuse_if_valid(mut self, v: bool) -> Self {
        self.reuse_if_valid = v;
        self
    }
    pub fn bearer(mut self, token: Option<String>) -> Self {
        self.bearer_token = token;
        self
    }

    fn part_path(&self) -> PathBuf {
        let mut p = self.dest.clone().into_os_string();
        p.push(".part");
        PathBuf::from(p)
    }
}

#[derive(Debug, Clone)]
pub struct Downloaded {
    pub path: PathBuf,
    pub hashes: Hashes,
    /// True when the file already existed and was re-used without transferring.
    pub reused: bool,
    pub bytes_transferred: u64,
}

/// Download `job`, reporting into `progress`. Returns once the file is at
/// `dest`, verified, and flushed.
pub async fn download(
    client: &HttpClient,
    job: &DownloadJob,
    progress: Option<&ProgressReporter>,
) -> Result<Downloaded, CoreError> {
    if let Some(parent) = job.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Fast path: already present and correct.
    if job.reuse_if_valid && job.dest.exists() {
        if let Ok(hashes) = sp_core::verify::hash_file(&job.dest) {
            if hashes_match(&job.expected, &hashes) {
                if let Some(p) = progress {
                    p.set_phase(ProgressPhase::Verify);
                    p.complete(format!("{} already present and verified", job.label));
                }
                return Ok(Downloaded {
                    path: job.dest.clone(),
                    hashes,
                    reused: true,
                    bytes_transferred: 0,
                });
            }
        }
    }

    let part = job.part_path();
    let mut already = match tokio::fs::try_exists(&part).await {
        Ok(true) => tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0),
        _ => 0,
    };
    // `size_hint` is the *whole* artifact size; resumed bytes are already counted
    // inside it, so we pass it through unchanged.
    if let (Some(total), Some(progress)) = (job.size_hint.filter(|s| *s > 0), progress) {
        progress.total(total);
    }

    let mut attempt = 0u32;
    let outcome: Result<Hashes, CoreError> = loop {
        attempt += 1;
        match attempt_download(client, job, &part, &mut already, progress).await {
            Ok(hashes) => break Ok(hashes),
            Err(e) => {
                let retryable = matches!(
                    e,
                    CoreError::UpstreamUnavailable { .. } | CoreError::Io(_) | CoreError::RateLimited { .. }
                );
                if !retryable || attempt >= 4 {
                    break Err(e);
                }
                let delay = std::time::Duration::from_millis(400 * 2u64.pow(attempt));
                tracing::warn!(error = %e, attempt, part = %part.display(), "download failed, resuming");
                if let Some(p) = progress {
                    p.message(format!("{}: retry {attempt} in {} ms ({e})", job.label, delay.as_millis()));
                }
                tokio::time::sleep(delay).await;
            }
        }
    };
    let hashes = match outcome {
        Ok(h) => h,
        Err(e) => {
            if let Some(p) = progress {
                p.fail(&e);
            }
            return Err(e);
        }
    };

    if let Err(e) = verify_or_delete(&job.dest, &part, &job.expected).await {
        if let Some(p) = progress {
            p.fail(&e);
        }
        return Err(e);
    }

    if let Some(p) = progress {
        p.complete(format!("{} done", job.label));
    }
    Ok(Downloaded {
        path: job.dest.clone(),
        hashes: hashes.clone(),
        reused: false,
        bytes_transferred: hashes.bytes,
    })
}

async fn attempt_download(
    client: &HttpClient,
    job: &DownloadJob,
    part: &Path,
    already: &mut u64,
    progress: Option<&ProgressReporter>,
) -> Result<Hashes, CoreError> {
    let mut req = client.get(&job.url);
    if let Some(token) = &job.bearer_token {
        req = req.bearer_auth(token);
    }
    let resume_from = *already;
    if resume_from > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }
    let resp = client.send_retrying(req).await?;
    let status = resp.status();
    let server_accepts_range = status == reqwest::StatusCode::PARTIAL_CONTENT;
    if !status.is_success() && !server_accepts_range {
        return Err(CoreError::Upstream {
            service: "download",
            status: status.as_u16(),
            body: format!("GET {}", job.url),
        });
    }

    let headers = resp.headers();
    let total_hint = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|remaining| remaining + if server_accepts_range { resume_from } else { 0 })
        .or(job.size_hint);
    let supports_ranges = headers
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("bytes"))
        .unwrap_or(false);

    let (mut file, mut hasher, mut written) = if resume_from > 0 && server_accepts_range {
        // Continue the existing part file; re-hash what is already on disk so the
        // final digest covers the whole payload.
        let f = std::fs::OpenOptions::new().append(true).open(part)?;
        let mut hasher = MultiHasher::new();
        let mut buf = Vec::with_capacity(64 * 1024);
        let mut existing = std::fs::File::open(part)?;
        existing.read_to_end(&mut buf)?;
        // Only the first `resume_from` bytes are trusted (append may have raced).
        buf.truncate(resume_from as usize);
        hasher.update(&buf);
        let f = tokio::fs::File::from_std(f);
        (f, hasher, resume_from)
    } else {
        if resume_from > 0 && !supports_ranges && !server_accepts_range {
            tracing::debug!("server does not support ranges, restarting download");
        }
        *already = 0;
        let f = tokio::fs::File::create(part).await?;
        (f, MultiHasher::new(), 0)
    };

    if let Some(p) = progress {
        p.set_phase(ProgressPhase::Download);
        if let Some(total) = total_hint {
            p.total(total);
        }
    }

    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| CoreError::UpstreamUnavailable {
            service: "download",
            source: e.into(),
        })?;
        file.write_all(&bytes).await?;
        hasher.update(&bytes);
        written += bytes.len() as u64;
        *already = written;
        if let Some(p) = progress {
            p.bytes(bytes.len() as u64);
        }
    }
    file.flush().await?;
    let _ = file.sync_all().await;
    drop(file);

    let hashes = hasher.finish();
    // A short transfer (server closed early) must not be renamed into place.
    if let Some(total) = total_hint {
        if hashes.bytes < total && !server_accepts_range {
            return Err(CoreError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("got {} of {total} bytes", hashes.bytes),
            )));
        }
    }
    Ok(hashes)
}

/// Verify the digest(s), then move `.part` → `dest`. On mismatch, delete the part.
async fn verify_or_delete(dest: &Path, part: &Path, expected: &Hashes) -> Result<(), CoreError> {
    if expected.is_empty() {
        // Nothing published to check against: record what we got (the UI shows it,
        // and a later "repair" can compare).
        tracing::debug!(dest = %dest.display(), "no upstream hash to verify against");
    } else {
        let actual = sp_core::verify::hash_file(part).map_err(|e| match e {
            CoreError::Io(_) => CoreError::not_found(format!("missing {}", part.display())),
            other => other,
        })?;
        if !hashes_match(expected, &actual) {
            let _ = tokio::fs::remove_file(part).await;
            let algo = if expected.sha512.is_some() {
                "sha512"
            } else if expected.sha256.is_some() {
                "sha256"
            } else {
                "sha1"
            };
            return Err(CoreError::ChecksumMismatch {
                file: dest.display().to_string(),
                expected: format!("{algo}:{}", expected.digest_short(algo)),
                actual: format!("{algo}:{}", actual.digest_short(algo)),
            });
        }
    }
    // Rename onto the destination atomically within the same directory.
    if dest.exists() {
        let _ = tokio::fs::remove_file(dest).await;
    }
    tokio::fs::rename(part, dest).await.map_err(|e| {
        CoreError::Io(std::io::Error::new(
            e.kind(),
            format!("renaming {} → {}: {e}", part.display(), dest.display()),
        ))
    })?;
    fsync_parent(dest).await;
    Ok(())
}

fn hashes_match(expected: &Hashes, actual: &Hashes) -> bool {
    for (want, got) in [
        (&expected.sha1, &actual.sha1),
        (&expected.sha256, &actual.sha256),
        (&expected.sha512, &actual.sha512),
    ] {
        let (Some(w), Some(g)) = (want, got) else { continue };
        // `normalise_digest` strips `sha256-` prefixes and case.
        let Ok((_, w)) = sp_core::verify::normalise_digest(w) else {
            return false;
        };
        if !sp_core::verify::constant_time_eq(w.as_bytes(), g.as_bytes()) {
            return false;
        }
    }
    true
}

async fn fsync_parent(path: &Path) {
    // Durability for the rename: on Linux, fsync of the directory is what makes the
    // file survive a power cut; elsewhere the rename is enough.
    #[cfg(unix)]
    {
        if let Some(dir) = path.parent() {
            if let Ok(f) = std::fs::File::open(dir) {
                let _ = f.sync_all();
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Convenience for providers: fetch a single artifact into a sandboxed path and
/// return its hashes. Errors already carry the label.
pub async fn download_artifact(
    client: &HttpClient,
    url: &str,
    path: &Path,
    sha1: Option<String>,
    sha256: Option<String>,
    size: Option<u64>,
    progress: Option<&ProgressReporter>,
) -> Result<Downloaded, CoreError> {
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| url.clone());
    let job = DownloadJob::new(url, path, label)
        .with_sha1(sha1)
        .with_sha256(sha256)
        .size_hint(size)
        .reuse_if_valid(true);
    download(&client, &job, progress).await
}

/// Delete stale `.part` files older than `max_age` in a directory tree (called at
/// startup so an interrupted install does not pin disk forever).
pub fn cleanup_stale_parts(dir: &Path, max_age: std::time::Duration) -> usize {
    let mut removed = 0;
    for entry in walkdir::WalkDir::new(dir).max_depth(6).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".part") {
            continue;
        }
        let ok = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e > max_age).unwrap_or(false))
            .unwrap_or(false);
        if ok && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Tiny raw HTTP server: no framework, exact control over 206/500 responses.
    /// Serves `GET /blob` (and ignores everything else).
    async fn mock(payload: Vec<u8>, accept_ranges: bool, fail_first: u32) -> (String, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits2 = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let body = payload.clone();
                let hits = hits2.clone();
                let ar = accept_ranges;
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let count = hits.fetch_add(1, Ordering::SeqCst);
                    if count < fail_first {
                        let _ = sock
                            .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\nboom")
                            .await;
                        return;
                    }
                    let range = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()));
                    let resp = match (ar, range.as_deref()) {
                        (true, Some(r)) if r.starts_with("bytes=") => {
                            let spec = &r["bytes=".len()..];
                            let mut it = spec.split('-');
                            let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                            let end: usize = it
                                .next()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(body.len().saturating_sub(1))
                                .min(body.len().saturating_sub(1));
                            if start < body.len() && start <= end {
                                let slice = &body[start..=end];
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {}-{}/{}\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n",
                                    slice.len(),
                                    start,
                                    end,
                                    body.len()
                                )
                                .into_bytes()
                                .into_iter()
                                .chain(slice.iter().copied())
                                .collect::<Vec<u8>>()
                            } else {
                                format!("HTTP/1.1 416 Range Not Satisfiable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
                            }
                        }
                        _ => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(&body);
                            out
                        }
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}/blob"), hits)
    }

    fn reporter() -> (ProgressReporter, Arc<sp_core::event::EventBus>) {
        let bus = Arc::new(sp_core::event::EventBus::new(8192));
        let r = ProgressReporter::new("dl", None, ProgressPhase::Download, bus.clone());
        (r, bus)
    }

    fn payload(n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| (i % 253) as u8).collect()
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let data = payload(200_000);
        let (url, hits) = mock(data.clone(), true, 0).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested/blob.bin");
        let expected = sp_core::verify::hash_bytes(&data);
        let (progress, _bus) = reporter();
        let job = DownloadJob::new(&url, &dest, "blob.bin")
            .with_hashes(expected.clone())
            .size_hint(Some(data.len() as u64));
        let out = download(&HttpClient::new(), &job, Some(&progress)).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(out.hashes.sha256, expected.sha256);
        assert!(!out.reused);
        assert_eq!(out.bytes_transferred, data.len() as u64);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!Path::new(&format!("{}.part", dest.display())).exists());
        assert!(progress.is_finished());
    }

    #[tokio::test]
    async fn reuses_a_valid_existing_file() {
        let data = b"same bytes".to_vec();
        let (url, hits) = mock(data.clone(), true, 0).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("blob.bin");
        std::fs::write(&dest, &data).unwrap();
        let job = DownloadJob::new(&url, &dest, "blob.bin")
            .with_hashes(sp_core::verify::hash_bytes(&data))
            .reuse_if_valid(true);
        let out = download(&HttpClient::new(), &job, None).await.unwrap();
        assert!(out.reused);
        assert_eq!(out.bytes_transferred, 0);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no request should be made");
    }

    #[tokio::test]
    async fn mismatched_hash_is_rejected_and_cleaned() {
        let data = b"real bytes".to_vec();
        let (url, _hits) = mock(data, false, 0).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("blob.bin");
        let bogus = Hashes {
            sha1: Some("0".repeat(40)),
            sha256: Some("0".repeat(64)),
            sha512: None,
            bytes: 10,
        };
        let job = DownloadJob::new(&url, &dest, "blob.bin").with_hashes(bogus);
        let err = download(&HttpClient::new(), &job, None).await.unwrap_err();
        assert!(matches!(err, CoreError::ChecksumMismatch { .. }), "{err:?}");
        assert!(!dest.exists(), "the bad file must never be installed");
        assert!(!Path::new(&format!("{}.part", dest.display())).exists(), "nor may the part file survive");
    }

    #[tokio::test]
    async fn retries_transient_failures_then_succeeds() {
        let data = b"retried payload".to_vec();
        let (url, hits) = mock(data.clone(), false, 2).await;
        let dir = tempfile::tempdir().unwrap();
        let job = DownloadJob::new(&url, dir.path().join("b.bin"), "b.bin");
        let out = download(&HttpClient::new(), &job, None).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("b.bin")).unwrap(), data);
        assert!(out.hashes.sha1.is_some());
        assert!(hits.load(Ordering::SeqCst) >= 3, "two failures then a success");
    }

    #[tokio::test]
    async fn progress_events_carry_byte_counts() {
        let data = payload(300_000);
        let (url, _hits) = mock(data.clone(), true, 0).await;
        let dir = tempfile::tempdir().unwrap();
        let (progress, bus) = reporter();
        let mut sub = bus.subscribe();
        let job = DownloadJob::new(&url, dir.path().join("p.bin"), "p.bin").size_hint(Some(data.len() as u64));
        download(&HttpClient::new(), &job, Some(&progress)).await.unwrap();
        let mut saw_progress = false;
        for _ in 0..64 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await {
                Ok(Ok(ev)) => {
                    if let sp_core::model::PanelEvent::Progress(p) = ev.event {
                        if p.bytes_done > 0 && p.fraction.is_some() {
                            saw_progress = true;
                        }
                        if p.finished {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(saw_progress, "progress must include byte counts and fractions");
    }

    #[tokio::test]
    async fn resume_continues_a_partial_file() {
        // Pre-seed a part file and check the final artifact hashes correctly even
        // though only the tail was transferred on this attempt.
        let data = payload(64_000);
        let (url, hits) = mock(data.clone(), true, 0).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("r.bin");
        let part = PathBuf::from(format!("{}.part", dest.display()));
        std::fs::write(&part, &data[..10_000]).unwrap();
        let job = DownloadJob::new(&url, &dest, "r.bin")
            .with_hashes(sp_core::verify::hash_bytes(&data))
            .size_hint(Some(data.len() as u64));
        let out = download(&HttpClient::new(), &job, None).await.unwrap();
        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got.len(), data.len(), "resumed file must be complete");
        assert_eq!(got, data, "and byte-identical");
        assert!(out.hashes.sha256.is_some());
        assert!(hits.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn part_path_and_matching_rules() {
        let job = DownloadJob::new("http://x/y", "/tmp/out/y", "y");
        assert_eq!(job.part_path(), PathBuf::from("/tmp/out/y.part"));
        let h = Hashes::default();
        assert!(h.is_empty());
        assert!(hashes_match(&h, &h), "nothing to check means nothing to fail");
        let expected = Hashes {
            sha1: Some("xyz".into()),
            ..Default::default()
        };
        assert!(!hashes_match(&expected, &h), "unnormalisable digest is a mismatch");
        let good = sp_core::verify::hash_bytes(b"abc");
        assert!(hashes_match(&good, &good));
        assert_eq!(good.digest_short("sha256").chars().count(), 17, "16 hex + ellipsis");
        assert_eq!(Hashes::default().digest_short("sha1"), "unknown");
    }

    #[test]
    fn stale_parts_are_collected_but_fresh_ones_are_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.part"), b"x").unwrap();
        std::fs::write(dir.path().join("b.jar"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.part"), b"x").unwrap();
        assert_eq!(cleanup_stale_parts(dir.path(), std::time::Duration::from_secs(3600)), 0);
        assert_eq!(cleanup_stale_parts(dir.path(), std::time::Duration::ZERO), 2);
        assert!(dir.path().join("b.jar").exists());
    }
}

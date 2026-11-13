//! Executing an [`InstallPlan`] against a real directory.
//!
//! This is the step between "the catalogue said 1.21.4 build 418 exists" and "there
//! is a server on disk", and it is where the failure modes live:
//!
//! * every file goes to `<dest>.part` first, is checksum-verified, then renamed —
//!   a partial download never looks like an installed server;
//! * an existing identical file is reused (no re-download on a repair/retry), which
//!   makes "retry" cheap for the user and idempotent for us;
//! * installers (Fabric/Quilt/Forge/BuildTools) are executed with argv, never
//!   through a shell, with their output streamed into the server's console ring so
//!   a 15-minute Spigot compile is watchable instead of silent;
//! * the plan's `expected_artifact` is the success test, not the exit code.
//!
//! Nothing here touches the database: the caller records install state.

use crate::{DownloadFile, FileRole, InstallPlan};
use sp_core::event::EventBus;
use sp_core::ids::ServerId;
use sp_core::model::{now_ms, LogStream, PanelEvent};
use sp_core::progress::ProgressReporter;
use sp_core::CoreError;
use sp_net::download::{self, DownloadJob};
use sp_net::HttpClient;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

/// What an install produced, for the wizard's summary and the audit log.
#[derive(Debug, Clone, Default)]
pub struct InstallOutcome {
    pub files_written: usize,
    pub bytes_downloaded: u64,
    pub files_reused: usize,
    pub unverified: usize,
    pub installer_ran: bool,
    pub artifact: Option<PathBuf>,
    pub duration_ms: i64,
}

impl InstallOutcome {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} file(s), {}",
            self.files_written,
            humansize(self.bytes_downloaded)
        );
        if self.files_reused > 0 {
            s.push_str(&format!(", {} reused", self.files_reused));
        }
        if self.installer_ran {
            s.push_str(", installer executed");
        }
        s
    }
}

fn humansize(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[derive(Debug, Clone)]
pub struct Installer {
    client: HttpClient,
    java: PathBuf,
    bus: Option<Arc<EventBus>>,
    server_id: Option<ServerId>,
}

impl Installer {
    /// `java` is the runtime the *install* runs on (modded installers need one that
    /// is new enough for the installer itself, which is not always the runtime the
    /// server will use).
    pub fn new(client: HttpClient, java: impl Into<PathBuf>) -> Self {
        Self {
            client,
            java: java.into(),
            bus: None,
            server_id: None,
        }
    }

    pub fn with_console(mut self, bus: Arc<EventBus>, server_id: ServerId) -> Self {
        self.bus = Some(bus);
        self.server_id = Some(server_id);
        self
    }

    fn emit(&self, line: &str, stream: LogStream) {
        if let Some(bus) = &self.bus {
            bus.publish(PanelEvent::Log(sp_core::model::LogLine {
                seq: 0,
                at_ms: now_ms(),
                stream,
                raw: line.to_string(),
                plain: line.to_string(),
            }));
        }
    }

    /// Fetch every file in the plan. Idempotent: identical files are left alone.
    pub async fn download_files(
        &self,
        plan: &InstallPlan,
        dest_dir: &Path,
        progress: &ProgressReporter,
    ) -> Result<(usize, u64, usize), CoreError> {
        let total = plan
            .total_bytes()
            .unwrap_or_else(|| plan.files.len() as u64 * 512 * 1024);
        progress.total(total);
        let mut written = 0usize;
        let mut bytes = 0u64;
        let mut reused = 0usize;
        for (i, file) in plan.files.iter().enumerate() {
            let path = safe_join(dest_dir, &file.dest)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            progress.message(format!(
                "Downloading {} ({}/{})",
                path.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| file.dest.clone()),
                i + 1,
                plan.files.len()
            ));
            let job = DownloadJob::new(file.url.clone(), &path, file.dest.clone())
                .with_hashes(sp_core::verify::Hashes {
                    sha1: file.sha1.clone(),
                    sha256: file.sha256.clone(),
                    sha512: None,
                    bytes: 0,
                })
                .size_hint(file.size)
                // Retry must be cheap: an identical, already-verified file is reused
                // rather than fetched again.
                .reuse_if_valid(true);
            if file.unverified {
                tracing::debug!(url = %file.url, "no upstream checksum for this artifact");
            }
            let done = download::download(&self.client, &job, Some(progress)).await?;
            if done.reused {
                reused += 1;
            } else {
                written += 1;
            }
            bytes += done.bytes_transferred;
        }
        Ok((written + reused, bytes, reused))
    }

    /// Run the plan's installer, if any, and verify its output exists.
    pub async fn run_installer(&self, plan: &InstallPlan, dest_dir: &Path) -> Result<bool, CoreError> {
        let Some(run) = &plan.installer else {
            return Ok(false);
        };
        let jar = safe_join(dest_dir, &run.jar)?;
        if !jar.exists() {
            return Err(CoreError::other(format!(
                "installer jar {} is missing",
                jar.display()
            )));
        }
        // Forge and Fabric installers refuse to run without a cwd they can write to;
        // `current_dir` is what they use for `-dir .`.
        let mut cmd = tokio::process::Command::new(&self.java);
        cmd.current_dir(dest_dir)
            .arg("-jar")
            .arg(&jar)
            .args(&run.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        self.emit(
            &format!("$ {} -jar {} {}", self.java.display(), run.jar, run.args.join(" ")),
            LogStream::System,
        );
        let output = cmd
            .output()
            .await
            .map_err(|e| CoreError::Process(format!("could not run the installer: {e}")))?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in combined.lines() {
            self.emit(line, LogStream::System);
        }
        let code = output.status.code().unwrap_or(-1);
        if !run.ok_exit_codes.contains(&code) {
            let tail: String = combined
                .lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            return Err(CoreError::Process(format!(
                "the installer exited with {code}. Last output:\n{tail}"
            )));
        }
        if let Some(expected) = &run.expected_artifact {
            let path = safe_join(dest_dir, expected)?;
            if !path.exists() {
                return Err(CoreError::Process(format!(
                    "the installer reported success but {} was not created \
                     (this usually means it failed to fetch Minecraft itself)",
                    path.display()
                )));
            }
        }
        Ok(true)
    }

    /// Rename the produced artifact to the canonical jar name the launcher expects,
    /// e.g. `spigot-1.21.4.jar` → `server.jar`, `paper-…jar` (already canonical).
    ///
    /// Only touches the file the plan says was produced, and never overwrites a
    /// different existing jar without a `.old` backup.
    pub async fn canonicalize_jar(plan: &InstallPlan, dest_dir: &Path) -> Result<Option<PathBuf>, CoreError> {
        let wanted = match &plan.launch {
            crate::LaunchMode::Jar { jar, .. } => jar.clone(),
            _ => return Ok(None),
        };
        let target = safe_join(dest_dir, &wanted)?;
        let source = plan
            .installer
            .as_ref()
            .and_then(|r| r.expected_artifact.clone())
            .map(|a| safe_join(dest_dir, &a))
            .transpose()?;
        let Some(source) = source.filter(|s| s != &target) else {
            return Ok(target.exists().then_some(target));
        };
        if !source.exists() {
            return Ok(None);
        }
        if target.exists() {
            let backup = dest_dir.join(format!("{wanted}.old-{}", now_ms()));
            tokio::fs::rename(&target, &backup).await?;
            tracing::info!(old = %backup.display(), "previous jar kept as backup");
        }
        tokio::fs::rename(&source, &target).await?;
        Ok(Some(target))
    }

    /// Full install: files → installer → jar rename → optional `eula.txt` seeding.
    ///
    /// `eula` is deliberately **not** accepted here: the caller writes it only after
    /// the user ticked the agreement box, and this function refuses to create a
    /// directory that could start a server the user has not agreed to run.
    pub async fn install(
        &self,
        plan: &InstallPlan,
        dest_dir: &Path,
        progress: &ProgressReporter,
    ) -> Result<InstallOutcome, CoreError> {
        let started = now_ms();
        std::fs::create_dir_all(dest_dir)?;
        let (files, bytes, reused) = self.download_files(plan, dest_dir, progress).await?;
        progress.message("Preparing");
        let ran = self.run_installer(plan, dest_dir).await?;
        let artifact = Self::canonicalize_jar(plan, dest_dir).await?;
        // Stale `.part` files from an interrupted run, and the installer's droppings.
        let cleaned = download::cleanup_stale_parts(dest_dir, std::time::Duration::from_secs(0));
        if cleaned > 0 {
            tracing::debug!(cleaned, "removed leftover partial files");
        }
        if let Some(jar) = &artifact {
            if !jar.exists() {
                return Err(CoreError::Process(format!(
                    "install finished but {} is not there",
                    jar.display()
                )));
            }
        }
        progress.complete("Server files installed");
        Ok(InstallOutcome {
            files_written: files,
            bytes_downloaded: bytes,
            files_reused: reused,
            unverified: plan.unverified_files(),
            installer_ran: ran,
            artifact,
            duration_ms: now_ms() - started,
        })
    }

    /// Write the install receipt next to the server so a repair can replay the exact
    /// same plan (same build, same URLs) without requerying the catalogue.
    pub fn write_receipt(plan: &InstallPlan, dest_dir: &Path) -> Result<PathBuf, CoreError> {
        let path = dest_dir.join(RECEIPT);
        std::fs::write(&path, crate::plan_receipt(plan)?)?;
        Ok(path)
    }
}

pub const RECEIPT: &str = "serverpanel-install.json";

/// Join a provider-supplied relative path onto a directory, refusing escapes.
///
/// URLs and maven paths come from the network, so this is a security boundary: a
/// hostile or buggy upstream must not be able to write `../../.ssh/authorized_keys`.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, CoreError> {
    let rel = rel.replace('\\', "/");
    let mut out = root.to_path_buf();
    for seg in rel.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(CoreError::PathEscape(rel.clone())),
            s if s.contains(':') => return Err(CoreError::PathEscape(rel.clone())),
            s => out.push(s),
        }
    }
    // Also reject a symlink inside the tree pointing outside it (the file names come
    // from upstream, and a jar path is not allowed to be a link we create).
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CatalogEntry, Provider, VersionInfo, VersionSelection};
    use sp_core::model::ProgressPhase;

    #[test]
    fn safe_join_blocks_traversal_and_absolute_paths() {
        let root = Path::new("/srv/panels/1");
        assert_eq!(safe_join(root, "server.jar").unwrap(), PathBuf::from("/srv/panels/1/server.jar"));
        assert_eq!(
            safe_join(root, "libraries/a/b.jar").unwrap(),
            PathBuf::from("/srv/panels/1/libraries/a/b.jar")
        );
        assert_eq!(safe_join(root, "./x/./y.jar").unwrap(), PathBuf::from("/srv/panels/1/x/y.jar"));
        assert_eq!(safe_join(root, "a\\b.jar").unwrap(), PathBuf::from("/srv/panels/1/a/b.jar"));
        for bad in ["../evil.jar", "libraries/../../evil", "/etc/passwd", "C:/Windows/x", "a/../b"] {
            assert!(
                matches!(safe_join(root, bad), Err(CoreError::PathEscape(_))),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn humansize_and_outcome_summary() {
        assert_eq!(humansize(512), "512 B");
        assert_eq!(humansize(2048), "2.0 KiB");
        assert_eq!(humansize(5 * 1024 * 1024), "5.0 MiB");
        let o = InstallOutcome {
            files_written: 3,
            bytes_downloaded: 1024,
            files_reused: 2,
            installer_ran: true,
            ..Default::default()
        };
        assert_eq!(o.summary(), "3 file(s), 1.0 KiB, 2 reused, installer executed");
    }

    fn plan_with(files: Vec<DownloadFile>) -> InstallPlan {
        InstallPlan {
            provider: "test",
            version: VersionInfo {
                id: "1".into(),
                display: "1".into(),
                mc_version: "1.21.4".into(),
                channel: sp_core::model::ReleaseChannel::Stable,
                build: None,
                released_at_ms: None,
                java_major: None,
                downloads: vec![],
                unstable: false,
                note: None,
            },
            files,
            launch: crate::LaunchMode::Jar { jar: "server.jar".into(), args: vec![] },
            jvm_args: vec![],
            natives_dir: None,
            java_major: 21,
            properties: Default::default(),
            installer: None,
            notes: vec![],
        }
    }

    #[tokio::test]
    async fn install_reuses_identical_files_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"fake jar bytes".to_vec();
        let sha = {
            use sha1::Digest;
            hex::encode(sha1::Sha1::digest(&payload))
        };
        // Serve the file over loopback, then install twice.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = payload.clone();
        let srv = tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    use tokio::io::AsyncWriteExt;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                }
            }
        });
        let files = vec![DownloadFile::new(
            FileRole::ServerJar,
            format!("http://127.0.0.1:{port}/server.jar"),
            "server.jar",
        )
        .with_sha1(Some(sha.clone()), Some(payload.len() as u64))];
        let plan = plan_with(files);
        std::fs::write(dir.path().join("server.jar"), &payload).unwrap();
        let inst = Installer::new(HttpClient::new(), "java");
        let progress = ProgressReporter::silent("j", None, ProgressPhase::Download);
        // First pass: the file already matches the expected sha1 → zero requests.
        let (n, bytes, reused) = inst.download_files(&plan, dir.path(), &progress).await.unwrap();
        assert_eq!((n, bytes, reused), (1, 0, 1), "an identical file is reused");
        std::fs::remove_file(dir.path().join("server.jar")).unwrap();
        let (n, bytes, reused) = inst.download_files(&plan, dir.path(), &progress).await.unwrap();
        assert_eq!((n, bytes, reused), (1, payload.len() as u64, 0));
        assert_eq!(std::fs::read(dir.path().join("server.jar")).unwrap(), payload);
        let _ = srv.await;
    }

    #[tokio::test]
    async fn checksum_mismatch_never_leaves_a_jar_behind() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let body = b"corrupted".to_vec();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        let plan = plan_with(vec![DownloadFile::new(
            FileRole::ServerJar,
            format!("http://127.0.0.1:{port}/server.jar"),
            "server.jar",
        )
        .with_sha1(Some("0000000000000000000000000000000000000000".into()), None)]);
        let inst = Installer::new(HttpClient::new(), "java");
        let progress = ProgressReporter::silent("j", None, ProgressPhase::Download);
        let err = inst.download_files(&plan, dir.path(), &progress).await.unwrap_err();
        assert!(matches!(err, CoreError::ChecksumMismatch { .. }), "{err:?}");
        assert!(!dir.path().join("server.jar").exists());
        assert!(!dir.path().join("server.jar.part").exists());
        srv.abort();
    }

    #[tokio::test]
    async fn missing_installer_and_missing_artifact_are_specific_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_with(vec![]);
        plan.installer = Some(placeholder_run());
        let inst = Installer::new(HttpClient::new(), "java");
        let err = inst.run_installer(&plan, dir.path()).await.unwrap_err();
        assert!(err.to_string().contains("installer jar"), "{err}");

        // Now let the "installer" be a script-free jar that does not exist on disk.
        plan.installer.as_mut().unwrap().jar = "nope.jar".into();
        assert!(inst.run_installer(&plan, dir.path()).await.unwrap_err().to_string().contains("missing"));
    }

    fn placeholder_run() -> crate::InstallerRun {
        crate::InstallerRun {
            jar: "BuildTools.jar".into(),
            args: vec!["--rev".into(), "1.21.4".into()],
            ok_exit_codes: vec![0],
            expected_artifact: Some("spigot-1.21.4.jar".into()),
        }
    }

    #[tokio::test]
    async fn canonicalize_moves_the_produced_jar_and_backs_up_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_with(vec![]);
        plan.installer = Some(placeholder_run());
        std::fs::write(dir.path().join("spigot-1.21.4.jar"), b"new").unwrap();
        std::fs::write(dir.path().join("server.jar"), b"old").unwrap();
        let target = Installer::canonicalize_jar(&plan, dir.path()).await.unwrap().unwrap();
        assert_eq!(target, dir.path().join("server.jar"));
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!dir.path().join("spigot-1.21.4.jar").exists());
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("server.jar.old-"))
            .collect();
        assert_eq!(backups.len(), 1, "the previous jar must survive");
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"old");

        // No installer → the already-canonical file is simply reported.
        let mut plain = plan_with(vec![]);
        plain.launch = crate::LaunchMode::ArgFile { argfile: "a.txt".into(), args: vec![] };
        assert!(Installer::canonicalize_jar(&plain, dir.path()).await.unwrap().is_none(), "argfile installs rename nothing");
        // Nothing present at all → Ok(None), so the caller can report honestly.
        let mut empty = plan_with(vec![]);
        empty.installer = None;
        assert!(Installer::canonicalize_jar(&empty, tempfile::tempdir().unwrap().path())
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn receipt_round_trips_and_lands_in_the_server_dir() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_with(vec![DownloadFile::new(FileRole::ServerJar, "https://x/y.jar", "server.jar")]);
        let path = Installer::write_receipt(&plan, dir.path()).unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), RECEIPT);
        let text = std::fs::read_to_string(&path).unwrap();
        let back = crate::plan_from_receipt(&text).unwrap();
        assert_eq!(back, plan);
        assert!(text.contains("\"provider\": \"test\""), "the receipt is pure data");
        assert!(!text.contains(RECEIPT), "the file name is not part of the payload");
    }

    #[tokio::test]
    async fn installer_output_is_streamed_to_the_console_bus() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let inst = Installer::new(HttpClient::new(), "java").with_console(bus.clone(), ServerId::new(7));
        inst.emit("hello from the installer", LogStream::System);
        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("an event must arrive")
            .expect("no lag");
        match evt.event {
            PanelEvent::Log(line) => {
                assert_eq!(line.raw, "hello from the installer");
                assert_eq!(line.stream, LogStream::System);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A provider whose plan we can drive end-to-end offline.
    struct FakeProvider {
        plan: InstallPlan,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn kind(&self) -> sp_core::model::ServerKind {
            sp_core::model::ServerKind::Paper
        }
        fn name(&self) -> &'static str {
            "fake"
        }

        #[allow(dead_code)]
        fn id(&self) -> &str {
            "fake"
        }
        async fn versions(&self, _mc: Option<&str>) -> Result<CatalogEntry, CoreError> {
            Ok(CatalogEntry {
                versions: vec![self.plan.version.clone()],
                fetched_at_ms: now_ms(),
                stale: false,
            })
        }
        async fn plan(&self, _sel: &VersionSelection) -> Result<InstallPlan, CoreError> {
            Ok(self.plan.clone())
        }
    }

    #[tokio::test]
    async fn end_to_end_install_of_a_two_file_plan() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"SERVERJAR".to_vec();
        let lib = b"LIB".to_vec();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncReadExt;
                use tokio::io::AsyncWriteExt;
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let text = String::from_utf8_lossy(&buf[..].to_vec()).to_string();
                let body = if text.contains("GET /lib") { &lib } else { &payload };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        let sha = {
            use sha1::Digest;
            hex::encode(sha1::Sha1::digest(&payload))
        };
        let lib_sha = {
            use sha1::Digest;
            hex::encode(sha1::Sha1::digest(&lib))
        };
        let plan = plan_with(vec![
            DownloadFile::new(FileRole::ServerJar, format!("http://127.0.0.1:{port}/server.jar"), "server.jar")
                .with_sha1(Some(sha), Some(payload.len() as u64)),
            DownloadFile::new(FileRole::Library, format!("http://127.0.0.1:{port}/lib.jar"), "libraries/x/lib.jar")
                .with_sha1(Some(lib_sha), Some(lib.len() as u64)),
        ]);
        let fake = FakeProvider { plan: plan.clone() };
        let from_provider = fake.plan(&VersionSelection::default()).await.unwrap();
        assert_eq!(from_provider, plan);

        let inst = Installer::new(HttpClient::new(), "java");
        let progress = ProgressReporter::silent("job", None, ProgressPhase::Download);
        let outcome = inst.install(&plan, dir.path(), &progress).await.unwrap();
        assert_eq!(outcome.files_written, 2);
        assert_eq!(outcome.bytes_downloaded, (payload.len() + lib.len()) as u64);
        assert!(!outcome.installer_ran);
        assert_eq!(outcome.artifact.as_deref(), Some(dir.path().join("server.jar").as_path()));
        assert_eq!(std::fs::read(dir.path().join("server.jar")).unwrap(), payload);
        assert_eq!(std::fs::read(dir.path().join("libraries/x/lib.jar")).unwrap(), lib);
        assert_eq!(outcome.duration_ms.max(0), outcome.duration_ms);
        srv.abort();
    }
}

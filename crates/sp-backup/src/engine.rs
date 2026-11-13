//! The engine: one path from "user clicked Back up now" to a row in `backups`
//! pointing at an archive that is provably readable.
//!
//! Shape of every operation here:
//!
//! 1. **Row first.** The backup exists in the database before any bytes are
//!    written, so a crash leaves a `running` row that `reap_interrupted` turns into
//!    a `failed` one on next boot. A user must never have to wonder whether the
//!    panel forgot about a job halfway through.
//! 2. **Blocking work off the runtime.** Copying and compressing gigabytes inside
//!    an `async fn` starves every other server on the host, so the transfer runs
//!    on `spawn_blocking` and the future only awaits it.
//! 3. **State transitions at the end, in one call.** `complete()` records size,
//!    digest, file count and the consistency flag together; a partially-filled row
//!    is how a UI ends up offering "restore" for a file that is not there.

use crate::archive::{self, FormatWriter};
use crate::manifest::{Manifest, ManifestEntry, SkippedEntry};
use crate::selector::{Cancel, Selector};
use crate::{CreateOutcome, CreateRequest, Destination, PruneReport, Quiesce, RestoreMode, RestoreOptions, RestoreReport};
use sp_core::progress::ProgressReporter;
use sp_core::{Backup, BackupFormat, BackupId, BackupKind, BackupState, CoreError, ServerId};
use sp_db::Db;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Extension per format, so the object key and the format never disagree.
fn extension(format: BackupFormat) -> &'static str {
    match format {
        BackupFormat::Zip => "zip",
        BackupFormat::TarZstd => "tar.zst",
    }
}

/// `12` → `0012`, so a directory listing sorts by server rather than as text.
fn pad_id(id: ServerId) -> String {
    format!("{:05}", id.get())
}

/// Build the archive's file name.
///
/// The name is user-facing (`survival-20261113-214500.zip`), but it is also the
/// object key stored in the database and a path component on disk — so the label
/// is sanitised and the timestamp is generated, never taken from user input.
pub fn object_key(server_id: ServerId, name: &str, format: BackupFormat, created_at_ms: i64) -> String {
    let stamp = chrono::DateTime::from_timestamp_millis(created_at_ms)
        .map(|d| d.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| format!("ms{created_at_ms}"));
    let label = sp_core::paths::sanitize_slug(name);
    let label = if label.is_empty() { "backup" } else { &label };
    format!("{}-{label}-{stamp}.{}", pad_id(server_id), extension(format))
}

pub struct BackupEngine {
    db: Arc<Db>,
    destination: Destination,
    quiesce: Option<Arc<dyn Quiesce>>,
    cancel: Cancel,
}

impl BackupEngine {
    pub fn new(db: Arc<Db>, destination: Destination) -> Self {
        Self { db, destination, quiesce: None, cancel: Cancel::new() }
    }

    pub fn with_quiesce(mut self, q: Arc<dyn Quiesce>) -> Self {
        self.quiesce = Some(q);
        self
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// Where archives physically live (the local destination's root).
    pub fn root(&self) -> Result<&Path, CoreError> {
        match &self.destination {
            Destination::Local { root } => Ok(root),
        }
    }

    /// Ask every scheduled job to stop at the next file boundary (shutdown path).
    pub fn cancel_all(&self) {
        self.cancel.cancel();
    }

    /// Rows stuck in `running` from a previous process, plus their `.part` files.
    pub async fn reap_interrupted(&self) -> Result<u64, CoreError> {
        let n = self.db.backups().reap_interrupted().await?;
        let parts = match self.root() {
            Ok(root) if root.is_dir() => clean_parts(root),
            _ => 0,
        };
        if parts > 0 {
            tracing::info!(parts, "removed unfinished archives left by a crash");
        }
        Ok(n)
    }

    /// What a backup *would* contain, for the confirmation dialog and the "last
    /// changed" column. Never writes anything.
    pub async fn preview(
        &self,
        server_id: ServerId,
        server_dir: &Path,
        excludes: Vec<String>,
        include_worlds: bool,
        include_logs: bool,
    ) -> Result<crate::selector::SelectionPlan, CoreError> {
        let dir = server_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            Selector::default()
                .with_excludes(excludes)
                .with_worlds(include_worlds)
                .with_logs(include_logs)
                .plan(&dir)
        })
        .await
        .map_err(|e| CoreError::other(format!("preview task failed: {e}")))?
    }

    /// Create an archive of `server_dir`.
    pub async fn create(
        &self,
        req: CreateRequest,
        progress: ProgressReporter,
    ) -> Result<CreateOutcome, CoreError> {
        let started = chrono::Utc::now().timestamp_millis();
        let key = object_key(req.server_id, &req.name, req.format, started);
        let row = self
            .db
            .backups()
            .create(
                req.server_id,
                &req.name,
                req.kind,
                req.format,
                &req.destination_id_ref(),
                &key,
            )
            .await?;
        let id = row.id;
        self.db.backups().set_state(id, BackupState::Running, None).await?;

        // Freeze the world first, and make sure we always unfreeze it: a server
        // left with saving disabled loses the session when it is next killed.
        let running = self.is_running(req.server_id);
        let quiesced = if req.quiesce && running {
            match self.quiesce.as_ref() {
                Some(q) => match q.prepare_backup(req.server_id).await {
                    Ok(()) => true,
                    Err(e) => {
                        // A failed flush is not a reason to skip the backup — an
                        // inconsistent backup still beats none, as long as it says
                        // so. So this is a warning, and `world_consistent` is false.
                        tracing::warn!(error = %e, server = %req.server_id, "could not flush the world before backing up");
                        progress.message(format!("could not flush the world first ({e}); continuing, but this backup may be mid-write"));
                        false
                    }
                },
                None => false,
            }
        } else {
            false
        };
        let world_consistent = quiesced || !running;

        let result = self.copy(req.clone(), id, &key, world_consistent, progress.clone()).await;

        if req.quiesce && running {
            if let Some(q) = self.quiesce.as_ref() {
                q.resume_backup(req.server_id).await;
            }
        }

        match result {
            Ok(out) => {
                let done = self
                    .db
                    .backups()
                    .complete(
                        id,
                        out.size_bytes,
                        Some(&out.sha256),
                        out.included_bytes,
                        out.file_count,
                        world_consistent,
                    )
                    .await?;
                progress.complete(format!(
                    "{} ({} MB)",
                    if world_consistent { "backup complete" } else { "backup complete — world was live" },
                    out.size_bytes / (1024 * 1024).max(1)
                ));
                Ok(CreateOutcome { backup: done, copy: out })
            }
            Err(e) => {
                let _ = self.db.backups().set_state(id, BackupState::Failed, Some(&e.to_string())).await;
                let _ = std::fs::remove_file(self.destination.resolve(&key)?);
                progress.fail(&e);
                Err(e)
            }
        }
    }

    /// The blocking half of [`Self::create`]: walk, copy, hash, rename.
    async fn copy(
        &self,
        req: CreateRequest,
        id: BackupId,
        key: &str,
        world_consistent: bool,
        progress: ProgressReporter,
    ) -> Result<CopyOutcome, CoreError> {
        let dest = self.destination.resolve(key)?;
        let server_name = req
            .server_dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("server-{}", req.server_id));
        let created = chrono::Utc::now().timestamp_millis();
        let server_dir = req.server_dir.clone();
        let max_total = req.max_total_bytes;
        let format = req.format;
        let cancel = req.cancel.clone();
        let excludes = req.excludes.clone();
        let (include_worlds, include_logs) = (req.include_worlds, req.include_logs);
        tokio::task::spawn_blocking(move || -> Result<CopyOutcome, CoreError> {
            let selector = Selector::default()
                .with_excludes(excludes)
                .with_worlds(include_worlds)
                .with_logs(include_logs);
            let plan = selector.plan(&server_dir)?;
            if plan.is_empty() {
                return Err(CoreError::InvalidState(format!(
                    "nothing to back up: {} is empty after excludes",
                    server_dir.display()
                )));
            }
            if max_total > 0 && plan.total_bytes > max_total {
                return Err(CoreError::Validation {
                    field: "max_total_bytes".into(),
                    reason: format!(
                        "this backup would write {} bytes, over the {} byte limit — add excludes or raise the limit",
                        plan.total_bytes, max_total
                    ),
                });
            }
            progress.total(plan.total_bytes.max(1));
            progress.message(format!("backing up {}", plan.describe()));

            // Write beside the target (same filesystem, so the rename is atomic)
            // and only rename once the archive is complete and hashed.
            let part = part_path(&dest, id);
            let mut writer = FormatWriter::create(&part, format)?;
            let mut manifest = Manifest::new(
                match format {
                    BackupFormat::Zip => "zip",
                    BackupFormat::TarZstd => "tar.zst",
                },
                server_name,
                created,
            );
            manifest.world_consistent = world_consistent;
            let mut skipped: Vec<SkippedEntry> = plan
                .skipped
                .iter()
                .map(|s| SkippedEntry { path: s.path.clone(), reason: s.reason, detail: None })
                .collect();
            let mut written_bytes = 0u64;
            for dir in &plan.dir_paths {
                if let Err(e) = writer.add_dir(dir) {
                    tracing::debug!(dir, error = %e, "skipping a directory that cannot be stored");
                }
            }
            for f in &plan.files {
                if cancel.is_cancelled() {
                    writer.abort();
                    let _ = std::fs::remove_file(&part);
                    return Err(CoreError::Cancelled);
                }
                progress.message(format!("adding {}", f.rel));
                let mut src = match std::io::BufReader::new(std::fs::File::open(&f.abs)) {
                    Ok(s) => s,
                    Err(e) => {
                        skipped.push(SkippedEntry {
                            path: f.rel.clone(),
                            reason: crate::selector::SkippedKind::Unreadable,
                            detail: Some(e.to_string()),
                        });
                        continue;
                    }
                };
                let (n, sha256) = writer.add_file(&f.rel, &mut src, f.bytes, f.modified_ms, f.executable)?;
                manifest.push(ManifestEntry {
                    path: f.rel.clone(),
                    bytes: n,
                    modified_ms: f.modified_ms,
                    sha256,
                    executable: f.executable,
                });
                written_bytes += n;
                progress.bytes(n);
            }
            manifest.empty_dirs = Selector::empty_dirs(&plan, &manifest.paths().map(str::to_string).collect::<Vec<_>>());
            manifest.skipped = skipped.clone();
            let size = writer.finish_with_manifest(&manifest.to_json_vec()?)?;
            std::fs::rename(&part, &dest)?;
            let hashes = sp_core::verify::hash_file(&dest)?;
            let sha256 = hashes.sha256.ok_or_else(|| CoreError::other("could not hash the new archive"))?;
            Ok(CopyOutcome {
                path: dest,
                file_count: manifest.file_count,
                included_bytes: written_bytes,
                size_bytes: size,
                sha256,
                world_consistent,
                skipped,
                started_ms: created,
            })
        })
        .await
        .map_err(|e| CoreError::other(format!("the backup task panicked: {e}")))?
    }

    /// Prove an archive is intact and consistent with its own manifest.
    ///
    /// The hash catches a file that is not what we wrote; reading every entry
    /// catches one that is, but whose contents cannot be inflated any more — the
    /// difference matters when the cause is the storage rather than the panel.
    pub async fn verify(&self, backup: &Backup) -> Result<VerifyReport, CoreError> {
        let path = self.destination.resolve(&backup.object_key)?;
        let format = archive::format_for(&path, backup.format);
        let expected = backup.sha256.clone();
        let db_size = backup.size_bytes;
        let backup_id = backup.id;
        let backup_name = backup.name.clone();
        let report = tokio::task::spawn_blocking(move || -> Result<VerifyReport, CoreError> {
            let meta = std::fs::metadata(&path).map_err(|_| CoreError::not_found(format!(
                "the archive file for backup {} is missing from {}",
                backup_id,
                path.display()
            )))?;
            let mut rep = VerifyReport {
                path: path.clone(),
                size_bytes: meta.len(),
                ..Default::default()
            };
            let _ = (&backup_id, &backup_name);
            if let Some(want) = &expected {
                let have = sp_core::verify::hash_file(&path)?.sha256.unwrap_or_default();
                rep.sha256_ok = have.eq_ignore_ascii_case(want);
            } else {
                rep.sha256_ok = true; // nothing recorded to compare against
            }
            if let Some(want) = db_size {
                rep.size_ok = want == meta.len();
            } else {
                rep.size_ok = true;
            }
            let summary = archive::summarize(&path, format)?;
            rep.entries = summary.entries.len() as u32;
            rep.uncompressed_bytes = summary.total_bytes;
            if let Some(err) = &summary.manifest_error {
                // The archive is readable but its index of contents is not, which is
                // exactly the state in which "restore" becomes a gamble.
                rep.mismatches.push(format!("the manifest is unreadable: {err}"));
            }
            if let Some(m) = &summary.manifest {
                m.validate()?;
                rep.files = m.entries.len() as u32;
                rep.world_consistent = m.world_consistent;
                rep.skipped = m.skipped.len() as u32;
                // Cross-check: every manifest entry must exist in the archive with
                // the size it promises.
                let mut have: std::collections::BTreeMap<&str, u64> = Default::default();
                for e in &summary.entries {
                    have.insert(e.path.as_str(), e.bytes);
                }
                for want in &m.entries {
                    match have.get(want.path.as_str()) {
                        Some(bytes) if *bytes == want.bytes => {}
                        Some(bytes) => rep.mismatches.push(format!(
                            "{} is {} bytes in the archive, {} in the manifest",
                            want.path, bytes, want.bytes
                        )),
                        None => rep.mismatches.push(format!("{} is listed but missing", want.path)),
                    }
                }
                for e in summary.entries.iter().filter(|e| !m.paths().any(|p| p == e.path)) {
                    if e.kind == archive::EntryKind::File {
                        rep.mismatches.push(format!("{} is in the archive but not the manifest", e.path));
                    }
                }
            }
            Ok(rep)
        })
        .await
        .map_err(|e| CoreError::other(format!("the verify task panicked: {e}")))?;
        if !report.mismatches.is_empty() {
            tracing::warn!(
                backup = %backup_id,
                count = report.mismatches.len(),
                "an archive did not match its own manifest"
            );
        }
        Ok(report)
    }

    /// Restore `backup` over `server_dir`.
    pub async fn restore(
        &self,
        backup: &Backup,
        server_dir: &Path,
        opts: RestoreOptions,
        progress: ProgressReporter,
    ) -> Result<RestoreReport, CoreError> {
        if backup.state != BackupState::Completed {
            return Err(CoreError::InvalidState(format!(
                "a {} backup cannot be restored",
                match backup.state {
                    BackupState::Queued => "queued",
                    BackupState::Running => "in-progress",
                    BackupState::Failed => "failed",
                    BackupState::Restoring => "already restoring",
                    BackupState::Completed => unreachable!("checked above"),
                }
            )));
        }
        if self.is_running(backup.server_id) {
            return Err(CoreError::InvalidState(
                "stop the server before restoring: a running server overwrites restored files, \
                 and Minecraft keeps region files open"
                    .into(),
            ));
        }
        let path = self.destination.resolve(&backup.object_key)?;
        if !path.is_file() {
            return Err(CoreError::not_found(format!(
                "the archive for {} is gone ({})",
                backup.name,
                path.display()
            )));
        }

        // The safety copy happens *before* anything is touched, and a failure here
        // stops the restore rather than leaving the user mid-swap.
        let pre = match self
            .create(
                CreateRequest {
                    server_id: backup.server_id,
                    server_dir: server_dir.to_path_buf(),
                    name: format!("pre-restore {}", backup.name),
                    kind: BackupKind::PreRestore,
                    format: backup.format,
                    destination: self.destination.clone(),
                    excludes: Vec::new(),
                    include_worlds: true,
                    include_logs: true,
                    quiesce: false,
                    max_total_bytes: 0,
                    cancel: Cancel::new(),
                },
                progress.clone(),
            )
            .await
        {
            Ok(o) => Some(o.backup.id),
            Err(e) if matches!(e, CoreError::InvalidState(_)) => {
                // An empty directory cannot be snapshotted; there is nothing to lose.
                tracing::debug!(error = %e, "no pre-restore copy was possible");
                None
            }
            Err(e) => return Err(CoreError::other(format!("refusing to restore without a safety copy: {e}"))),
        };

        let _ = self.db.backups().set_state(backup.id, BackupState::Restoring, None).await;
        let result = self.extract_into(backup, server_dir, &opts, &progress).await;
        if result.is_err() && opts.mode == RestoreMode::Replace {
            if let Ok(dir) = self.staging_dir(backup.id) {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        // Leave `Restoring` whatever happens: a stuck row would block the next
        // restore as "already restoring".
        let _ = self.db.backups().set_state(backup.id, BackupState::Completed, result.as_ref().err().map(|e| e.to_string()).as_deref()).await;
        match result {
            Ok(mut rep) => {
                rep.pre_restore = pre;
                self.db.backups().mark_restored(backup.id).await?;
                progress.complete(format!("restored {} files", rep.files_written));
                Ok(rep)
            }
            Err(e) => {
                if let Some(pre_id) = pre {
                    tracing::error!(
                        pre_restore = %pre_id,
                        "the restore failed; the pre-restore copy is intact and can be restored"
                    );
                }
                Err(e)
            }
        }
    }

    async fn extract_into(
        &self,
        backup: &Backup,
        server_dir: &Path,
        opts: &RestoreOptions,
        progress: &ProgressReporter,
    ) -> Result<RestoreReport, CoreError> {
        let path = self.destination.resolve(&backup.object_key)?;
        let format = archive::format_for(&path, backup.format);
        let dest = server_dir.to_path_buf();
        let staged = match opts.mode {
            RestoreMode::Replace => Some(self.staging_dir(backup.id)?),
            RestoreMode::Overlay => None,
        };
        let target = staged.clone().unwrap_or_else(|| dest.clone());
        let only = opts.only.clone();
        let skip_identical = opts.skip_identical;
        let manifest = tokio::task::spawn_blocking({
            let p = path.clone();
            move || archive::read_manifest(&p, format).ok()
        })
        .await
        .map_err(|e| CoreError::other(format!("manifest task panicked: {e}")))?;
        if let Some(m) = &manifest {
            m.validate()?;
            progress.message(format!("restoring {}", m.describe()));
        } else {
            progress.message("this archive has no manifest, so per-file digests cannot be checked");
        }
        let progress2 = progress.clone();
        let rep = tokio::task::spawn_blocking(move || -> Result<archive::ExtractReport, CoreError> {
            let mut plan = archive::ExtractPlan {
                dest: &target,
                only: &only,
                skip_identical,
                manifest: manifest.as_ref(),
                cancel: opts.cancel.clone(),
            };
            let rep = archive::extract(&path, format, &mut plan)?;
            progress2.message(format!("{} files written", rep.written));
            Ok(rep)
        })
        .await
        .map_err(|e| CoreError::other(format!("the restore task panicked: {e}")))?;

        let mut out = RestoreReport {
            files_written: rep.written,
            files_skipped: rep.skipped_identical + rep.symlinks_refused,
            bytes_written: rep.bytes,
            pre_restore: None,
            previous_dir: None,
        };
        if let Some(staged) = staged {
            out.previous_dir = Some(swap_directories(&dest, &staged, backup.id)?);
        }
        Ok(out)
    }

    fn staging_dir(&self, id: BackupId) -> Result<PathBuf, CoreError> {
        let dir = self
            .root()?
            .join(".staging")
            .join(format!("restore-{}", id.get()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| CoreError::other(format!("could not clear {}: {e}", dir.display())))?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn is_running(&self, id: ServerId) -> bool {
        self.quiesce.as_ref().map(|q| q.is_running(id)).unwrap_or(false)
    }

    /// Apply a server's retention policy: delete what is over the limit.
    pub async fn prune(&self, server_id: ServerId) -> Result<PruneReport, CoreError> {
        let policy = self.db.backups().policy(server_id).await?;
        if !policy.enabled || policy.keep_last == 0 {
            return Ok(PruneReport::default());
        }
        self.prune_with(server_id, &policy).await
    }

    pub async fn prune_with(&self, server_id: ServerId, policy: &sp_core::BackupPolicy) -> Result<PruneReport, CoreError> {
        let mut rep = PruneReport::default();
        if policy.keep_last == 0 {
            return Ok(rep); // "0" means keep everything, not "delete everything"
        }
        let excess = self
            .db
            .backups()
            .retention_excess(server_id, policy.keep_last, policy.keep_days)
            .await?;
        for b in excess {
            // The pre-restore copy of a restore that went wrong is the one backup a
            // retention rule must never take, because it is the only copy of what
            // the user had before their mistake.
            if b.kind == BackupKind::PreRestore && b.restored_count > 0 {
                tracing::info!(backup = %b.id, "keeping a pre-restore copy that was used for a restore");
                continue;
            }
            let path = self.destination.resolve(&b.object_key)?;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    // A file we cannot delete must not lose its row, or the panel
                    // forgets about bytes it still owns.
                    tracing::warn!(path = %path.display(), error = %e, "could not delete an expired archive");
                    rep.missing_files.push(b.id);
                    continue;
                }
            } else {
                rep.missing_files.push(b.id);
            }
            self.db.backups().remove_row(b.id).await?;
            rep.freed_bytes += size;
            rep.deleted.push(b.id);
        }
        if !rep.is_noop() {
            tracing::info!(
                server = %server_id,
                count = rep.deleted.len(),
                freed = rep.freed_bytes,
                "pruned old backups"
            );
        }
        Ok(rep)
    }

    /// Forget a backup and delete its archive.
    pub async fn delete(&self, id: BackupId) -> Result<u64, CoreError> {
        let row = self.db.backups().remove_row(id).await?;
        let path = self.destination.resolve(&row.object_key)?;
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(size)
    }

    /// Adopt an archive the panel did not create (a copy dropped into `backups/`,
    /// or a file a user uploaded).
    pub async fn import(
        &self,
        server_id: ServerId,
        server_dir: &Path,
        source: &Path,
        name: &str,
    ) -> Result<CreateOutcome, CoreError> {
        if !source.is_file() {
            return Err(CoreError::not_found(format!("{} is not a file", source.display())));
        }
        let format = archive::format_for(source, BackupFormat::Zip);
        let src_path = source.to_path_buf();
        let (size, sha, file_count, included, world_consistent) = tokio::task::spawn_blocking(move || -> Result<_, CoreError> {
            let size = std::fs::metadata(&src_path)?.len();
            let sha = sp_core::verify::hash_file(&src_path)?.sha256.unwrap_or_default();
            let summary = archive::summarize(&src_path, format)?;
            let consistent = summary
                .manifest
                .as_ref()
                .map(|m| m.world_consistent)
                .unwrap_or(false);
            Ok((size, sha, summary.file_count() as u32, summary.total_bytes, consistent))
        })
        .await
        .map_err(|e| CoreError::other(format!("the import task panicked: {e}")))??;
        let created = chrono::Utc::now().timestamp_millis();
        let key = object_key(server_id, name, format, created);
        let dest = self.destination.resolve(&key)?;
        if dest != source {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            copy_file(source, &dest)?;
        }
        let row = self
            .db
            .backups()
            .create(server_id, name, BackupKind::Import, format, LOCAL_DESTINATION, &key)
            .await?;
        let backup = self
            .db
            .backups()
            .complete(row.id, size, Some(&sha), included, file_count, world_consistent)
            .await?;
        let _ = server_dir;
        Ok(CreateOutcome {
            backup,
            path: dest,
            file_count,
            included_bytes: included,
            size_bytes: size,
            sha256: sha,
            world_consistent,
            skipped: Vec::new(),
            duration_ms: chrono::Utc::now().timestamp_millis() - created,
        })
    }

    pub async fn list(&self, server_id: ServerId, limit: u32) -> Result<Vec<Backup>, CoreError> {
        Ok(self.db.backups().list_for_server(server_id, limit).await?)
    }
}

/// `<dest>.<id>.part` — beside the target so the rename cannot cross a filesystem.
fn part_path(dest: &Path, id: BackupId) -> PathBuf {
    let name = format!(
        "{}.{}.part",
        dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "backup".into()),
        id.get()
    );
    dest.with_file_name(name)
}

/// Remove `<name>.<id>.part` files left by an interrupted create. They are named
/// so that a crash can never be mistaken for a backup: only a renamed file counts.
fn clean_parts(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for e in entries.flatten() {
        let path = e.path();
        let is_part = path
            .file_name()
            .map(|s| s.to_string_lossy().ends_with(".part"))
            .unwrap_or(false);
        if is_part && path.is_file() {
            match std::fs::remove_file(&path) {
                Ok(()) => n += 1,
                Err(err) => tracing::warn!(path = %path.display(), error = %err, "could not remove a stale .part file"),
            }
        }
    }
    n
}

fn copy_file(from: &Path, to: &Path) -> Result<(), CoreError> {
    std::fs::copy(from, to).map(|_| ())?;
    Ok(())
}

/// Move `current` aside and put `staged` in its place, on the same volume.
///
/// Both renames are per-directory, so a `Replace` restore costs microseconds
/// rather than a full copy of a 4 GB world — and the previous directory survives on
/// disk either way, which is what makes a bad choice of archive fixable.
fn swap_directories(current: &Path, staged: &Path, id: BackupId) -> Result<PathBuf, CoreError> {
    let parent = current
        .parent()
        .ok_or_else(|| CoreError::other(format!("{} has no parent directory", current.display())))?;
    let aside = parent.join(format!(
        "{}.pre-restore-{}",
        current
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "server".into()),
        id.get()
    ));
    if current.exists() {
        std::fs::rename(current, &aside)
            .map_err(|e| CoreError::other(format!("could not park the current directory: {e}")))?;
    }
    match std::fs::rename(staged, current) {
        Ok(()) => Ok(aside),
        Err(e) => {
            // Put back what we moved, or a failed restore is worse than no restore.
            if let Err(re) = std::fs::rename(&aside, current) {
                tracing::error!(error = %re, aside = %aside.display(), "could not restore the parked directory; it is intact at this path");
            }
            Err(CoreError::other(format!(
                "could not move the restored files into place: {e}. Your previous directory is untouched at {}",
                aside.display()
            )))
        }
    }
}

/// The local destination's id in `backup_destinations`. Every archive this engine
/// writes lands there; the row exists so the UI has one model for "where backups
/// live" even when only one place is supported.
pub const LOCAL_DESTINATION: &str = "local";

/// What the copy phase produced, before the database row is finalised.
#[derive(Debug, Clone)]
pub struct CopyOutcome {
    pub path: PathBuf,
    pub file_count: u32,
    pub included_bytes: u64,
    pub size_bytes: u64,
    pub sha256: String,
    pub world_consistent: bool,
    pub skipped: Vec<SkippedEntry>,
    pub started_ms: i64,
}

impl CopyOutcome {
    pub fn duration_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis() - self.started_ms
    }
}

/// What [`BackupEngine::verify`] found.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub entries: u32,
    pub files: u32,
    pub uncompressed_bytes: u64,
    pub skipped: u32,
    pub world_consistent: bool,
    pub sha256_ok: bool,
    pub size_ok: bool,
    /// Differences between the archive's contents and its manifest.
    pub mismatches: Vec<String>,
}

impl VerifyReport {
    pub fn is_healthy(&self) -> bool {
        self.sha256_ok && self.size_ok && self.mismatches.is_empty()
    }
    pub fn summary(&self) -> String {
        if self.is_healthy() {
            format!(
                "{} entries, {} bytes, verified",
                self.entries, self.uncompressed_bytes
            )
        } else {
            let mut why: Vec<&str> = Vec::new();
            if !self.sha256_ok {
                why.push("the file's digest does not match the one recorded at creation");
            }
            if !self.size_ok {
                why.push("its size differs from what was recorded");
            }
            if !self.mismatches.is_empty() {
                why.push(self.mismatches[0].as_str());
            }
            format!("backup is damaged: {}", why.join("; "))
        }
    }
}

impl CreateRequest {
    fn destination_id_ref(&self) -> String {
        LOCAL_DESTINATION.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_are_sorted_by_server_and_cannot_be_injected() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-11-13T21:45:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .timestamp_millis();
        let a = object_key(ServerId::new(7), "Survival World", BackupFormat::Zip, t);
        assert_eq!(a, "00007-survival-world-20261113-214500.zip");
        assert_eq!(
            object_key(ServerId::new(7), "x", BackupFormat::TarZstd, t),
            "00007-x-20261113-214500.tar.zst"
        );
        // 5-digit padding means `10` sorts after `9` in a directory listing.
        assert!(object_key(ServerId::new(10), "a", BackupFormat::Zip, t) > a);
        // A name with path components must not become a path.
        for hostile in ["../../etc/passwd", "a/b\c.jar", "..", "/", "CON.txt"] {
            let k = object_key(ServerId::new(1), hostile, BackupFormat::Zip, t);
            assert!(!k.contains("..") && !k.contains('/'), "{k} from {hostile:?}");
            assert!(k.ends_with(".zip"), "{k}");
        }
        // A clock before the epoch still produces a usable name.
        let k = object_key(ServerId::new(1), "x", BackupFormat::Zip, -5);
        assert!(k.contains("ms-5") || k.contains("1969"), "{k}");
    }

    #[test]
    fn destination_resolution_refuses_escapes() {
        let d = Destination::Local { root: PathBuf::from("/data/backups") };
        assert_eq!(
            d.resolve("7-a-2026.zip").unwrap(),
            PathBuf::from("/data/backups/7-a-2026.zip")
        );
        assert_eq!(
            d.resolve("nested/7-a.zip").unwrap(),
            PathBuf::from("/data/backups/nested/7-a.zip")
        );
        for bad in ["../x", "a/../../x", "/etc/passwd", "."] {
            assert!(d.resolve(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn part_files_live_beside_the_target() {
        let part = part_path(Path::new("/data/backups/7-a.zip"), BackupId::new(42));
        assert_eq!(part, PathBuf::from("/data/backups/7-a.zip.42.part"));
    }

    #[test]
    fn a_manifest_that_cannot_be_parsed_makes_a_backup_unusable_for_restore_decisions() {
        // Regression guard: summarize must stay tolerant (the entries are still
        // readable) while verify reports damage.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.zip");
        let mut w = FormatWriter::create(&path, BackupFormat::Zip).unwrap();
        w.add_file("a.txt", &mut std::io::Cursor::new(b"hi".to_vec()), 2, 0, false).unwrap();
        w.finish_with_manifest(b"{ not json").unwrap();
        let summary = crate::archive::summarize(&path, BackupFormat::Zip).unwrap();
        assert!(summary.manifest.is_none());
        assert_eq!(summary.entries.iter().filter(|e| e.kind == crate::archive::EntryKind::File).count(), 1);
        assert!(summary.manifest_error.as_deref().unwrap_or_default().contains("unreadable"), "{:?}", summary.manifest_error);
    }

    #[test]
    fn verify_report_summary_says_what_is_wrong() {
        let ok = VerifyReport {
            entries: 3,
            uncompressed_bytes: 100,
            sha256_ok: true,
            size_ok: true,
            ..Default::default()
        };
        assert!(ok.is_healthy());
        assert_eq!(ok.summary(), "3 entries, 100 bytes, verified");
        let bad = VerifyReport {
            sha256_ok: false,
            size_ok: true,
            mismatches: vec!["mods/a.jar is listed but missing".into()],
            ..Default::default()
        };
        assert!(!bad.is_healthy());
        let s = bad.summary();
        assert!(s.contains("digest does not match") && s.contains("listed but missing"), "{s}");
    }

    #[test]
    fn clean_parts_removes_only_parts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.zip.3.part"), b"partial").unwrap();
        std::fs::write(dir.path().join("good.zip"), b"real").unwrap();
        std::fs::create_dir(dir.path().join("dir.part")).unwrap();
        assert_eq!(clean_parts(dir.path()), 1);
        assert!(dir.path().join("good.zip").exists());
        assert!(dir.path().join("dir.part").exists(), "a directory is not a stale part");
        assert!(!dir.path().join("a.zip.3.part").exists());
        assert_eq!(clean_parts(Path::new("/nonexistent-dir-for-test")), 0);
    }
}

//! `sp-backup` — snapshots of a server directory, and the only supported way back.
//!
//! # What a backup has to survive
//!
//! A panel that restores worlds is trusted with the one thing a user cannot
//! regenerate. That sets the bar for this crate:
//!
//! * **An archive is either complete or absent.** Every archive is written to a
//!   `.part` file, hashed, and only then renamed into place. A crash mid-write
//!   leaves a `.part` to be cleaned up, never a plausible-looking backup.
//! * **Contents are re-readable, not just hashed.** A SHA-256 proves the file is
//!   the bytes we wrote. It does not prove the entries inside still inflate, and
//!   silent storage corruption is exactly the failure a user discovers too late.
//!   [`engine::BackupEngine::verify`] therefore reads every entry, which forces
//!   the CRC check inside the archive, and compares against the per-entry
//!   digests recorded in the archive's own manifest.
//! * **A restore never eats the current state.** Every restore takes a
//!   `pre-restore` backup of the live directory *first* and keeps it, so a wrong
//!   choice of archive is recoverable. If that backup fails, the restore does not
//!   start.
//! * **Nothing writes outside the server directory.** Entry names are re-parsed,
//!   not trusted: `..`, absolute paths, drive letters and symlinks are rejected
//!   before the first byte lands on disk, for both archive formats.
//!
//! # Formats
//!
//! `zip` (deflate) is the default because a user can open it in Explorer on
//! Windows — the primary platform — without installing anything. `tar.zst` is
//! the option for large trees, where it is roughly twice as fast and compresses
//! better, at the cost of needing a tool that understands zstd.
//!
//! Both carry the same `serverpanel-backup.json` manifest at the root
//! ([`manifest::MANIFEST_NAME`]): per-file size, mtime and SHA-256. It is what
//! makes an archive self-describing — a restore can show what it will write
//! before writing it, `verify` has something to compare to, and a user who
//! copies a `.zip` off the machine can still tell what it is.
//!
//! # Consistency
//!
//! Minecraft writes its region files as it goes, so a backup taken while a
//! server runs can contain a torn chunk. [`Quiesce`] is the seam that fixes it:
//! the engine asks the caller (which holds RCON) to `save-off` + `save-all flush`
//! before, and `save-on` after. A backup taken that way — or while the server was
//! stopped — is recorded `world_consistent`, and the UI can say so instead of
//! implying a guarantee the filesystem does not give.

#![forbid(unsafe_code)]

mod archive;
mod manifest;
mod schedule;
mod selector;

pub use archive::{
    extract, format_for, read_manifest, summarize, ArchiveEntry, ArchiveSummary, EntryKind,
    ExtractPlan, ExtractReport, FormatWriter,
};
pub use engine::{object_key, BackupEngine, CopyOutcome, VerifyReport, LOCAL_DESTINATION};
pub use manifest::{Manifest, ManifestEntry, SkippedEntry, MANIFEST_NAME};
pub use schedule::{is_due, next_fire, BackupScheduler, SchedulerTick, ServerDirs};
pub use selector::{Cancel, SelectionPlan, Selector, SkippedKind};

use sp_core::{Backup, BackupFormat, BackupId, BackupKind, BackupPolicy, ServerId};
use std::path::PathBuf;

/// Where an archive lives. `Local` is a directory on a filesystem the panel can
/// already see; the panel's own `backups/` directory is the default.
///
/// Remote destinations are a configuration of *where the file goes*, not a second
/// backup engine: the archive is written locally, hashed, and then handed over.
/// That boundary is why the engine returns a finished path rather than a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A directory under the panel's control.
    Local { root: PathBuf },
}

impl Destination {
    /// Resolve an object key to a path inside this destination.
    ///
    /// Object keys are stored in the database and are user-visible, so they are
    /// re-validated on the way back out: a row written by an older build (or by
    /// hand) must not be able to escape the destination root.
    pub fn resolve(&self, object_key: &str) -> Result<PathBuf, sp_core::CoreError> {
        let root = match self {
            Destination::Local { root } => root,
        };
        let mut out = root.clone();
        for seg in object_key.replace('\\', "/").split('/') {
            match seg {
                "" | "." => continue,
                // Including a bare `..` here is the point: `a/../..` must fail too.
                ".." => return Err(sp_core::CoreError::PathEscape(object_key.to_string())),
                s => out.push(s),
            }
        }
        if out == *root {
            return Err(sp_core::CoreError::Validation {
                field: "object_key".into(),
                reason: "resolves to the destination directory itself".into(),
            });
        }
        Ok(out)
    }
}

/// The seam between "copy these bytes" and "make the game flush first".
///
/// `sp-backup` deliberately does not depend on `sp-process`: it does not own
/// servers, and RCON is not its business. The panel binary implements this over
/// both, and tests implement it with a counter.
#[async_trait::async_trait]
pub trait Quiesce: Send + Sync {
    /// Whether the server is currently running. A restore requires `false`.
    fn is_running(&self, server_id: ServerId) -> bool;

    /// Freeze world writes and flush them to disk.
    ///
    /// Returning `Ok` is what licenses `world_consistent = true`, so an
    /// implementation must only return `Ok` after the flush has actually been
    /// acknowledged (an RCON command that timed out is not a flush).
    async fn prepare_backup(&self, server_id: ServerId) -> Result<(), sp_core::CoreError>;

    /// Resume world writes. Called even when [`Self::prepare_backup`] failed or the
    /// copy was cancelled — a server left with saving disabled loses the next
    /// hour of play when it is killed.
    async fn resume_backup(&self, server_id: ServerId);
}

/// One backup request.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub server_id: ServerId,
    /// Absolute, canonicalised server directory.
    pub server_dir: PathBuf,
    /// User-visible name; the object key is generated, so a name with `/` cannot
    /// turn into a path.
    pub name: String,
    pub kind: BackupKind,
    pub format: BackupFormat,
    pub destination: Destination,
    /// Extra exclude globs, on top of the engine's defaults.
    pub excludes: Vec<String>,
    pub include_worlds: bool,
    pub include_logs: bool,
    /// Ask [`Quiesce`] to flush before copying. Silently skipped when the server is
    /// not running, because then there is nothing to flush.
    pub quiesce: bool,
    /// Refuse to start if the planned contents exceed this (0 = no limit). A
    /// 400 MB world folder is normal; a server directory that grew to 40 GB
    /// because someone installed 200 mods' caches is a mistake worth catching
    /// before the disk is full.
    pub max_total_bytes: u64,
    pub cancel: Cancel,
}

/// How much of the tree to bring back.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// `Replace` moves the current directory aside and puts the restored tree in
    /// its place; `Overlay` writes the archive's files over the existing ones and
    /// leaves everything else alone.
    pub mode: RestoreMode,
    /// Restore only these relative paths (files or directory prefixes). Lets a
    /// user take `world/` from last week without losing this week's configs.
    pub only: Vec<String>,
    /// Skip files already on disk with a matching SHA-256 — cheap on a large
    /// unchanged world, and what makes an interrupted restore resumable.
    pub skip_identical: bool,
    pub cancel: Cancel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RestoreMode {
    /// Write the archive's files over the server directory.
    #[default]
    Overlay,
    /// Swap the whole directory. Nothing outside the archive survives.
    Replace,
}

/// What happened during a create: the finished row plus what the copy did.
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    /// The row as the database now holds it (state `completed`, digest, sizes).
    pub backup: Backup,
    pub copy: crate::engine::CopyOutcome,
}

/// What happened during a restore.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub files_written: u32,
    pub files_skipped: u32,
    pub bytes_written: u64,
    /// The safety copy taken before anything was overwritten, if it was taken.
    pub pre_restore: Option<BackupId>,
    /// `Replace` mode parks the previous directory here instead of deleting it.
    pub previous_dir: Option<PathBuf>,
}

/// Retention bookkeeping for one server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub deleted: Vec<BackupId>,
    pub freed_bytes: u64,
    /// Rows that could not be reaped because their file was already gone; the row
    /// is still deleted, but the user should know the space never came back.
    pub missing_files: Vec<BackupId>,
}

impl PruneReport {
    pub fn is_noop(&self) -> bool {
        self.deleted.is_empty()
    }
}

/// Policy fields that change *what* is copied, as opposed to how often.
impl BackupPolicy {
    /// The selector settings implied by this policy.
    pub fn selector_for(&self, base: Selector) -> Selector {
        base.with_excludes(self.exclude_patterns.clone())
            .with_worlds(self.include_worlds)
            .with_logs(self.include_logs)
    }
}

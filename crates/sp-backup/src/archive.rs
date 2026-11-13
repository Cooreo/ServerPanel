//! Reading and writing the two archive formats behind one small interface.
//!
//! Everything here is synchronous on purpose: callers wrap it in
//! `tokio::task::spawn_blocking`, which keeps a 4 GB world from being shuffled
//! through `async fn` one chunk at a time, and it puts every path-safety decision
//! in one place that can be tested without a runtime.

use crate::manifest::{Manifest, MANIFEST_NAME};
use crate::selector::Cancel;
use sp_core::{BackupFormat, CoreError};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Largest uncompressed total an archive may claim, per byte of archive.
///
/// A zip bomb promises a small file and expands to terabytes. The ratio is
/// deliberately generous (a fresh world full of empty regions really does compress
/// 20:1) because this has to stop the absurd, not judge the unusual.
const BOMB_RATIO: u64 = 200;
const BOMB_FLOOR: u64 = 8 << 30;

/// Reject anything that could write outside the extraction root.
///
/// This is the whole security story for a restore, so it is stricter than "no
/// `..`": separators of either flavour, drive letters, UNC prefixes, reserved
/// Windows device names and trailing dots/spaces all fail here, rather than
/// producing a file the user cannot open or delete afterwards.
pub fn is_safe_rel(rel: &str) -> Result<(), CoreError> {
    let bad = |why: &str| CoreError::PathEscape(format!("{rel}: {why}"));
    if rel.is_empty() {
        return Err(bad("empty path"));
    }
    if rel.contains('\0') {
        return Err(bad("NUL byte"));
    }
    // Normalise first: `..\..\x` is exactly as dangerous as `../../x`, and a path
    // mixing both separators is a smuggling attempt, so it is refused outright.
    let has_back = rel.contains('\\');
    let has_fwd = rel.contains('/');
    if has_back && has_fwd {
        return Err(bad("mixes both path separators"));
    }
    let unified = rel.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(bad("absolute path"));
    }
    let bytes = unified.as_bytes();
    if bytes.len() > 1 && bytes[1] == b':' {
        return Err(bad("drive-absolute path"));
    }
    if unified.starts_with("//") {
        return Err(bad("UNC or extended-length path"));
    }
    let mut segments = 0usize;
    for seg in unified.split('/') {
        match seg {
            "" | "." => return Err(bad("redundant segment")),
            ".." => return Err(bad("parent traversal")),
            s => {
                segments += 1;
                if is_reserved_windows_name(s) {
                    return Err(bad("`{s}` is a reserved Windows device name"));
                }
                if s.ends_with('.') || s.ends_with(' ') {
                    return Err(bad("trailing dot or space, which Windows strips silently"));
                }
                for c in s.chars() {
                    if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20 {
                        return Err(bad("character not allowed in a file name"));
                    }
                }
            }
        }
    }
    if segments == 0 {
        return Err(bad("no segments"));
    }
    Ok(())
}

fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    RESERVED.contains(&stem.as_str())
}

/// Join `rel` onto `root`, refusing escapes. Shared by extraction and by the
/// "restore only these paths" filter.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, CoreError> {
    is_safe_rel(rel)?;
    let mut out = root.to_path_buf();
    for seg in rel.replace('\\', "/").split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(CoreError::PathEscape(rel.to_string())),
            s => out.push(s),
        }
    }
    if out == *root {
        return Err(CoreError::PathEscape(format!(
            "{rel} resolves to the destination itself"
        )));
    }
    Ok(out)
}

/// A [`Read`] that hashes and counts what passes through it.
///
/// Both formats need the same thing while copying: the digest of the *uncompressed*
/// bytes, so that a restore can prove what it wrote is what was backed up. Wrapping
/// the reader keeps that a single pass over the file for either format.
pub struct CountingReader<'a, R: Read> {
    inner: &'a mut R,
    hasher: sp_core::verify::MultiHasher,
    read: u64,
}

impl<'a, R: Read> CountingReader<'a, R> {
    pub fn new(inner: &'a mut R) -> Self {
        Self { inner, hasher: sp_core::verify::MultiHasher::new(), read: 0 }
    }
    /// Freeze the digests; returns (bytes, sha256, sha1).
    pub fn finish(self) -> (u64, String, String) {
        let h = self.hasher.finish();
        (self.read, h.sha256.unwrap_or_default(), h.sha1.unwrap_or_default())
    }
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
            self.read += n as u64;
        }
        Ok(n)
    }
}

/// One entry as it exists inside an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub bytes: u64,
    pub modified_ms: i64,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Recorded and refused, never followed.
    Symlink,
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveSummary {
    pub entries: Vec<ArchiveEntry>,
    pub total_bytes: u64,
    pub manifest: Option<Manifest>,
    /// Set when a manifest exists but cannot be parsed. `summarize` stays usable
    /// without it — the entries are still readable, and an archive from another
    /// tool simply has no manifest — while `verify` treats it as damage.
    pub manifest_error: Option<String>,
}

impl ArchiveSummary {
    pub fn file_count(&self) -> usize {
        self.entries.iter().filter(|e| e.kind == EntryKind::File).count()
    }
    pub fn paths(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.path.clone())
            .collect()
    }
}

/// Which format a path claims, by extension, falling back to the caller's choice.
pub fn format_for(path: &Path, fallback: BackupFormat) -> BackupFormat {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.zst") || name.ends_with(".tar.zstd") || name.ends_with(".tzst") {
        BackupFormat::TarZstd
    } else if name.ends_with(".zip") {
        BackupFormat::Zip
    } else {
        fallback
    }
}

fn deflate_opts() -> zip::write::SimpleFileOptions {
    // Level 6 is where deflate stops paying for itself on already-deflated region
    // files; `large_file` is not optional — a 4.5 GB world crosses the 4 GiB mark
    // that zip cannot address without zip64.
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true)
}

fn stored_opts() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true)
}

/// Writes one archive. Zip needs its file handle back to write the central
/// directory, so [`FormatWriter::finish`] consumes the writer and returns the size.
pub enum FormatWriter {
    Zip { zip: zip::ZipWriter<BufWriter<File>>, path: PathBuf },
    Tar { tar: tar::Builder<zstd::Encoder<'static, BufWriter<File>>>, path: PathBuf },
}

impl FormatWriter {
    pub fn create(path: &Path, format: BackupFormat) -> Result<Self, CoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        match format {
            BackupFormat::Zip => Ok(Self::Zip { zip: zip::ZipWriter::new(BufWriter::new(file)), path: path.to_path_buf() }),
            BackupFormat::TarZstd => {
                let enc = zstd::Encoder::new(BufWriter::new(file), 3)
                    .map_err(|e| CoreError::other(format!("could not start zstd: {e}")))?;
                Ok(Self::Tar { tar: tar::Builder::new(enc), path: path.to_path_buf() })
            }
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Zip { path, .. } | Self::Tar { path, .. } => path,
        }
    }

    pub fn add_dir(&mut self, rel: &str) -> Result<(), CoreError> {
        is_safe_rel(rel)?;
        match self {
            Self::Zip { zip, .. } => {
                let opts = deflate_opts();
                zip.add_directory(format!("{rel}/"), opts)
                    .map_err(|e| CoreError::other(format!("zip directory {rel}: {e}")))?;
            }
            Self::Tar { tar, .. } => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Directory);
                h.set_path(rel).map_err(|e| CoreError::other(format!("tar path {rel}: {e}")))?;
                h.set_size(0);
                h.set_mode(0o755);
                h.set_mtime(0);
                tar.append_data(&mut h, rel, std::io::empty())
                    .map_err(|e| CoreError::other(format!("tar directory {rel}: {e}")))?;
            }
        }
        Ok(())
    }

    /// Copy `src` in, hashing as it goes, and return (bytes, sha256).
    pub fn add_file(
        &mut self,
        rel: &str,
        src: &mut dyn Read,
        bytes: u64,
        modified_ms: i64,
        executable: bool,
    ) -> Result<(u64, String), CoreError> {
        is_safe_rel(rel)?;
        match self {
            Self::Zip { zip, .. } => {
                let opts = deflate_opts()
                    .last_modified_time(dos_time(modified_ms))
                    .unix_permissions(if executable { 0o755 } else { 0o644 });
                zip.start_file(rel.to_string(), opts)
                    .map_err(|e| CoreError::other(format!("zip start {rel}: {e}")))?;
                let mut reader = CountingReader::new(src);
                std::io::copy(&mut reader, zip)
                    .map_err(|e| CoreError::other(format!("zip write {rel}: {e}")))?;
                let (written, sha256, _) = reader.finish();
                Ok((written, sha256))
            }
            Self::Tar { tar, .. } => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_path(rel).map_err(|e| CoreError::other(format!("tar path {rel}: {e}")))?;
                h.set_size(bytes);
                h.set_mtime((modified_ms / 1000).max(0) as u64);
                h.set_mode(if executable { 0o755 } else { 0o644 });
                // Ownership is meaningless in this context and dangerous to
                // preserve: a restore must not recreate files owned by uid 0.
                h.set_uid(0);
                h.set_gid(0);
                let mut reader = CountingReader::new(src);
                tar.append_data(&mut h, rel, &mut reader)
                    .map_err(|e| CoreError::other(format!("tar write {rel}: {e}")))?;
                let (written, sha256, _) = reader.finish();
                Ok((written, sha256))
            }
        }
    }

    /// Append the manifest, close the archive and flush it to the platter.
    pub fn finish_with_manifest(self, manifest: &[u8]) -> Result<u64, CoreError> {
        let this = self;
        match this {
            Self::Zip { mut zip, path } => {
                let opts = stored_opts();
                zip.start_file(MANIFEST_NAME.to_string(), opts)
                    .map_err(|e| CoreError::other(format!("zip manifest: {e}")))?;
                let mut cursor = std::io::Cursor::new(manifest.to_vec());
                std::io::copy(&mut cursor, &mut zip)
                    .map_err(|e| CoreError::other(format!("zip manifest body: {e}")))?;
                let mut bw = zip
                    .finish()
                    .map_err(|e| CoreError::other(format!("zip finish: {e}")))?;
                bw.flush()?;
                let f = bw.into_inner().map_err(|e| CoreError::other(format!("zip into_inner: {e}")))?;
                f.sync_all()?;
                drop(f);
                Ok(std::fs::metadata(&path)?.len())
            }
            Self::Tar { mut tar, path } => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_path(MANIFEST_NAME)
                    .map_err(|e| CoreError::other(format!("tar manifest path: {e}")))?;
                h.set_size(manifest.len() as u64);
                h.set_mtime(0);
                h.set_mode(0o644);
                let mut cursor = std::io::Cursor::new(manifest.to_vec());
                tar.append_data(&mut h, MANIFEST_NAME, &mut cursor)
                    .map_err(|e| CoreError::other(format!("tar manifest: {e}")))?;
                let enc = tar
                    .into_inner()
                    .map_err(|e| CoreError::other(format!("tar finish: {e}")))?;
                let mut bw = enc
                    .finish()
                    .map_err(|e| CoreError::other(format!("zstd finish: {e}")))?;
                bw.flush()?;
                let f = bw.into_inner().map_err(|e| CoreError::other(format!("zstd into_inner: {e}")))?;
                f.sync_all()?;
                drop(f);
                Ok(std::fs::metadata(&path)?.len())
            }
        }
    }

    /// Drop the half-written archive. Used when a create fails: the `.part` must
    /// not survive to be mistaken for a backup later.
    pub fn abort(self) {
        let path = match &self {
            Self::Zip { path, .. } | Self::Tar { path, .. } => path.clone(),
        };
        drop(self);
        let _ = std::fs::remove_file(path);
    }
}

/// zip stores MS-DOS timestamps, which start at 1980 — an epoch-clamped mtime
/// keeps a skewed clock from producing a date the format cannot represent.
fn dos_time(ms: i64) -> zip::DateTime {
    let secs = (ms / 1000).max(315_532_800) as i64;
    let d = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_else(|| {
        chrono::DateTime::from_timestamp(315_532_800, 0).expect("1980 is representable")
    });
    let u = |f: &str, fallback: u16| d.format(f).to_string().parse::<u16>().unwrap_or(fallback);
    zip::DateTime::from_date_and_time(
        u("%Y", 1980),
        u("%m", 1),
        u("%d", 1),
        u("%H", 0) as u8,
        u("%M", 0) as u8,
        u("%S", 0) as u8,
    )
    .unwrap_or_else(|_| zip::DateTime::default())
}

/// Read every entry — forcing decompression, which is what detects silent
/// corruption — without writing anything.
pub fn summarize(path: &Path, format: BackupFormat) -> Result<ArchiveSummary, CoreError> {
    let archive_bytes = std::fs::metadata(path)?.len();
    let mut out = ArchiveSummary::default();
    match format {
        BackupFormat::Zip => {
            let file = File::open(path)?;
            let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| {
                CoreError::other(format!("{} is not a readable zip: {e}", path.display()))
            })?;
            for i in 0..zip.len() {
                let mut f = zip
                    .by_index(i)
                    .map_err(|e| CoreError::other(format!("zip index {i}: {e}")))?;
                let name = f.name().to_string();
                if name == MANIFEST_NAME {
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf)?;
                    match Manifest::from_json_bytes(&buf) {
                        Ok(m) => out.manifest = Some(m),
                        Err(e) => out.manifest_error = Some(e.to_string()),
                    }
                    continue;
                }
                is_safe_rel(&name)?;
                if f.is_dir() {
                    out.entries.push(ArchiveEntry { path: name, bytes: 0, modified_ms: 0, kind: EntryKind::Dir });
                    continue;
                }
                if f.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
                    out.entries.push(ArchiveEntry { path: name, bytes: 0, modified_ms: 0, kind: EntryKind::Symlink });
                    continue;
                }
                // The copy is the check: reading forces zip to verify the stored
                // CRC32 against the bytes it just inflated.
                let mut sink = std::io::sink();
                let n = std::io::copy(&mut f, &mut sink)
                    .map_err(|e| CoreError::other(format!("corrupt entry {name}: {e}")))?;
                out.total_bytes += n;
                out.entries.push(ArchiveEntry {
                    path: name,
                    bytes: n,
                    modified_ms: dos_to_ms(f.last_modified()),
                    kind: EntryKind::File,
                });
            }
        }
        BackupFormat::TarZstd => {
            let file = File::open(path)?;
            let dec = zstd::Decoder::new(BufReader::new(file)).map_err(|e| {
                CoreError::other(format!("{} is not readable zstd: {e}", path.display()))
            })?;
            let mut ar = tar::Archive::new(dec);
            ar.set_unpack_xattrs(false);
            ar.set_ignore_zeros(true);
            for entry in ar.entries()? {
                let mut entry = entry.map_err(|e| CoreError::other(format!("tar entry: {e}")))?;
                let name = entry.path()?.to_string_lossy().replace('\\', "/");
                if name == MANIFEST_NAME {
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf)?;
                    match Manifest::from_json_bytes(&buf) {
                        Ok(m) => out.manifest = Some(m),
                        Err(e) => out.manifest_error = Some(e.to_string()),
                    }
                    continue;
                }
                is_safe_rel(&name)?;
                let ty = entry.header().entry_type();
                let kind = if ty.is_dir() {
                    EntryKind::Dir
                } else if matches!(ty, tar::EntryType::Symlink | tar::EntryType::Link) {
                    EntryKind::Symlink
                } else {
                    EntryKind::File
                };
                let declared = entry.header().size().unwrap_or(0);
                let bytes = if kind == EntryKind::File {
                    let mut sink = std::io::sink();
                    std::io::copy(&mut entry, &mut sink)?
                } else {
                    0
                };
                out.total_bytes += bytes;
                out.entries.push(ArchiveEntry {
                    path: name,
                    bytes: bytes.max(declared * 0),
                    modified_ms: entry.header().mtime().unwrap_or(0) as i64 * 1000,
                    kind,
                });
            }
        }
    }
    let cap = archive_bytes.saturating_mul(BOMB_RATIO).max(BOMB_FLOOR);
    if out.total_bytes > cap {
        return Err(CoreError::other(format!(
            "archive expands to {} bytes from a {} byte file, past any plausible ratio; refusing to read further",
            out.total_bytes, archive_bytes
        )));
    }
    out.entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn dos_to_ms(dt: zip::DateTime) -> i64 {
    let y = dt.year() as i64;
    let mo = dt.time().hour() as i64;
    // Reconstructing a UTC instant from an MS-DOS date without a timezone library
    // round-trip is not worth a dependency; the manifest carries the real mtime and
    // the archive is the authority on contents, not timestamps.
    chrono::NaiveDate::from_ymd_opt(y as i32, dt.month() as u32, dt.day() as u32)
        .map(|d| {
            d.and_hms_opt(dt.time().hour(), dt.time().minute(), dt.time().second())
                .map(|t| t.and_utc().timestamp_millis())
                .unwrap_or(0)
        })
        .unwrap_or_else(|| {
            let _ = mo;
            0
        })
}

/// The manifest alone — cheap, and what the UI needs to show a backup's contents
/// before restoring it.
pub fn read_manifest(path: &Path, format: BackupFormat) -> Result<Manifest, CoreError> {
    match format {
        BackupFormat::Zip => {
            let file = File::open(path)?;
            let mut zip = zip::ZipArchive::new(BufReader::new(file))
                .map_err(|e| CoreError::other(format!("{}: not a zip ({e})", path.display())))?;
            let mut f = zip.by_name(MANIFEST_NAME).map_err(|_| {
                CoreError::other(format!(
                    "{} has no {MANIFEST_NAME}, so it was not created by ServerPanel",
                    path.display()
                ))
            })?;
            Manifest::from_reader(&mut f)
        }
        BackupFormat::TarZstd => {
            let file = File::open(path)?;
            let dec = zstd::Decoder::new(BufReader::new(file))
                .map_err(|e| CoreError::other(format!("{}: not zstd ({e})", path.display())))?;
            let mut ar = tar::Archive::new(dec);
            for entry in ar.entries()? {
                let mut entry = entry?;
                if entry.path()?.to_string_lossy().replace('\\', "/") == MANIFEST_NAME {
                    return Manifest::from_reader(&mut entry);
                }
            }
            Err(CoreError::other(format!("{} has no {MANIFEST_NAME}", path.display())))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    pub written: u32,
    pub bytes: u64,
    pub skipped_identical: u32,
    pub dirs_created: u32,
    pub symlinks_refused: u32,
}

/// Extraction knobs shared by both formats.
pub struct ExtractPlan<'a> {
    pub dest: &'a Path,
    /// Only entries whose path is one of these, or under one of these.
    pub only: &'a [String],
    pub skip_identical: bool,
    /// Per-file digests to verify against. `None` skips verification, which is how
    /// an archive from another tool is still restorable.
    pub manifest: Option<&'a Manifest>,
    pub cancel: Cancel,
}

fn keep(path: &str, only: &[String]) -> bool {
    if only.is_empty() {
        return true;
    }
    only.iter().any(|o| {
        let o = o.replace('\\', "/").trim_end_matches('/').to_string();
        path == o || path.starts_with(&format!("{o}/"))
    })
}

fn budget_left(archive_bytes: u64, used: u64) -> Result<u64, CoreError> {
    let cap = archive_bytes.saturating_mul(BOMB_RATIO).max(BOMB_FLOOR);
    cap.checked_sub(used)
        .ok_or_else(|| CoreError::other("archive expands beyond the safety limit; extraction stopped"))
}

/// Extract into `dest`, verifying digests where a manifest is available.
///
/// Every file is written to a `.spart` sibling and renamed, so an interrupted
/// restore leaves the previous file in place instead of a half-written region
/// file that the game then refuses to load.
pub fn extract(
    path: &Path,
    format: BackupFormat,
    plan: &mut ExtractPlan<'_>,
) -> Result<ExtractReport, CoreError> {
    let mut rep = ExtractReport::default();
    let archive_bytes = std::fs::metadata(path)?.len();
    let mut expanded = 0u64;
    match format {
        BackupFormat::Zip => {
            let file = File::open(path)?;
            let mut zip = zip::ZipArchive::new(BufReader::new(file))
                .map_err(|e| CoreError::other(format!("{}: not a zip ({e})", path.display())))?;
            for i in 0..zip.len() {
                if plan.cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                let mut f = zip
                    .by_index(i)
                    .map_err(|e| CoreError::other(format!("zip index {i}: {e}")))?;
                let name = f.name().to_string();
                let is_dir = f.is_dir();
                if name == MANIFEST_NAME {
                    // A zero-length manifest is "no manifest": an archive from
                    // another tool is still restorable, just unverifiable per file.
                    continue;
                }
                is_safe_rel(&name)?;
                if !keep(&name, plan.only) {
                    continue;
                }
                let out = safe_join(plan.dest, &name)?;
                if is_dir {
                    std::fs::create_dir_all(&out)?;
                    rep.dirs_created += 1;
                    continue;
                }
                if f.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
                    rep.symlinks_refused += 1;
                    continue;
                }
                expanded += f.size();
                budget_left(archive_bytes, expanded)?;
                write_entry(&out, &mut f, &name, plan, &mut rep)?;
            }
        }
        BackupFormat::TarZstd => {
            let file = File::open(path)?;
            let dec = zstd::Decoder::new(BufReader::new(file))
                .map_err(|e| CoreError::other(format!("{}: not zstd ({e})", path.display())))?;
            let mut ar = tar::Archive::new(dec);
            ar.set_preserve_permissions(false);
            ar.set_unpack_xattrs(false);
            for entry in ar.entries()? {
                if plan.cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                let mut entry = entry?;
                let name = entry.path()?.to_string_lossy().replace('\\', "/");
                if name == MANIFEST_NAME {
                    // A zero-length manifest is "no manifest": an archive from
                    // another tool is still restorable, just unverifiable per file.
                    continue;
                }
                is_safe_rel(&name)?;
                if !keep(&name, plan.only) {
                    continue;
                }
                let ty = entry.header().entry_type();
                let out = safe_join(plan.dest, &name)?;
                if ty.is_dir() {
                    std::fs::create_dir_all(&out)?;
                    rep.dirs_created += 1;
                    continue;
                }
                if !ty.is_file() {
                    rep.symlinks_refused += 1;
                    continue;
                }
                expanded += entry.header().size()?;
                budget_left(archive_bytes, expanded)?;
                let mode = entry.header().mode().unwrap_or(0o644);
                write_entry(&out, &mut entry, &name, plan, &mut rep)?;
                #[cfg(unix)]
                if mode & 0o111 != 0 {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
    }
    Ok(rep)
}

/// The copy shared by both formats: hash while writing, verify, rename.
fn write_entry<R: Read>(
    out: &Path,
    src: &mut R,
    name: &str,
    plan: &ExtractPlan<'_>,
    rep: &mut ExtractReport,
) -> Result<(), CoreError> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
        rep.dirs_created += 1;
    }
    let want = plan.manifest.and_then(|m| m.entry(name)).map(|e| e.sha256.clone());
    if plan.skip_identical {
        if let Some(want) = &want {
            if out.is_file()
                && sp_core::verify::hash_file(out)
                    .ok()
                    .and_then(|h| h.sha256)
                    .is_some_and(|have| have.eq_ignore_ascii_case(want))
            {
                rep.skipped_identical += 1;
                return Ok(());
            }
        }
    }
    let tmp = tmp_path(out);
    let mut written = 0u64;
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        let mut hashing = CountingReader::new(src);
        written = std::io::copy(&mut hashing, &mut w)
            .map_err(|e| CoreError::other(format!("write {name}: {e}")))?;
        let (_, sha256, _) = hashing.finish();
        w.flush()?;
        drop(w);
        if let Some(want) = &want {
            if !want.eq_ignore_ascii_case(&sha256) {
                let _ = std::fs::remove_file(&tmp);
                return Err(CoreError::ChecksumMismatch {
                    file: name.to_string(),
                    expected: short(want),
                    actual: short(&sha256),
                });
            }
        }
    }
    std::fs::rename(&tmp, out)?;
    rep.written += 1;
    rep.bytes += written;
    Ok(())
}

fn short(h: &str) -> String {
    h.chars().take(12).collect()
}

fn tmp_path(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map(|s| format!(".{}.spart", s.to_string_lossy()))
        .unwrap_or_else(|| ".spart".to_string());
    out.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_paths_are_refused_before_anything_is_written() {
        for bad in [
            "",
            "/",
            "//server/share",
            "\\\\server\\share",
            "C:/windows/system32",
            "C:\\windows\\system32",
            "../escape",
            "mods/../../escape",
            "..",
            "mods/./a.jar",
            "mods//a.jar",
            "mods/a\0.jar",
            "mods/CON",
            "mods/nul.txt",
            "mods/com1",
            "mods/trailing.",
            "mods/trailing ",
            "mods/bad<name>.jar",
            "mods/bad|pipe",
            r"mods\..\escape",
            "mods/../evil",
        ] {
            assert!(is_safe_rel(bad).is_err(), "{bad:?} must be refused");
        }
        for ok in [
            "mods/a.jar",
            "world/region/r.0.0.mca",
            "server.properties",
            "a",
            "world/datapacks/My Pack-1.0/pack.mcmeta",
            "mods\\a.jar",
        ] {
            assert!(is_safe_rel(ok).is_ok(), "{ok:?} must be accepted: {:?}", is_safe_rel(ok));
        }
    }

    #[test]
    fn mixed_separators_are_refused_as_smuggling() {
        // `a\b/c` is what an attacker writes when a filter looks only at `/`.
        assert!(is_safe_rel("a\\b/c").is_err());
        assert!(is_safe_rel("a/b\\c").is_err());
    }

    #[test]
    fn safe_join_refuses_the_root_itself() {
        let root = Path::new("/srv/panels/1");
        assert_eq!(
            safe_join(root, "mods/a.jar").unwrap(),
            PathBuf::from("/srv/panels/1/mods/a.jar")
        );
        assert_eq!(
            safe_join(root, "mods\\a.jar").unwrap(),
            PathBuf::from("/srv/panels/1/mods/a.jar")
        );
        assert!(safe_join(root, "..").is_err());
        assert!(safe_join(root, "a/../..").is_err());
    }

    #[test]
    fn format_is_chosen_by_extension_with_the_callers_fallback() {
        let f = |s: &str, fb| format_for(Path::new(s), fb);
        assert_eq!(f("a.zip", BackupFormat::TarZstd), BackupFormat::Zip);
        assert_eq!(f("A.ZIP", BackupFormat::TarZstd), BackupFormat::Zip);
        assert_eq!(f("a.tar.zst", BackupFormat::Zip), BackupFormat::TarZstd);
        assert_eq!(f("a.tar.zstd", BackupFormat::Zip), BackupFormat::TarZstd);
        assert_eq!(f("a.tzst", BackupFormat::Zip), BackupFormat::TarZstd);
        assert_eq!(f("backup-2026", BackupFormat::Zip), BackupFormat::Zip);
        assert_eq!(f("backup-2026", BackupFormat::TarZstd), BackupFormat::TarZstd);
    }

    #[test]
    fn tmp_names_cannot_collide_with_the_real_file() {
        let out = Path::new("/srv/world/level.dat");
        let tmp = tmp_path(out);
        assert_eq!(tmp.file_name().unwrap().to_string_lossy(), ".level.dat.spart");
        assert_eq!(tmp.parent(), out.parent());
    }

    #[test]
    fn counting_reader_hashes_exactly_what_it_passed_through() {
        let data = b"the quick brown fox jumps over the lazy dog".to_vec();
        let mut cursor = std::io::Cursor::new(data.clone());
        let mut r = CountingReader::new(&mut cursor);
        let mut sink = Vec::new();
        std::io::copy(&mut r, &mut sink).unwrap();
        let (n, sha256, sha1) = r.finish();
        assert_eq!(n, data.len() as u64);
        assert_eq!(sink, data);
        let expected = sp_core::verify::hash_bytes(&data);
        assert_eq!(sha256, expected.sha256.unwrap());
        assert_eq!(sha1, expected.sha1.unwrap());
    }
}

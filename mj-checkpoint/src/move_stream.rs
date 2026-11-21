//! Single-stream framing for Move archive + `.mj/keep/` transfer.
//!
//! Wire format:
//! 1. 8-byte magic header: `MJMV0001`
//! 2. `u64` little-endian `archive_len`, followed by `archive_len` raw bytes
//!    of the checkpoint archive.
//! 3. `u64` little-endian `keep_tar_len`, followed by `keep_tar_len` raw bytes
//!    of the `.mj/keep/` tar stream (0 bytes when no keep directory is sent).
//! 4. EOF (trailing bytes are rejected).
//!
//! Both parts travel in one stream under one SSH grant and route. The receiver
//! verifies the archive's SHA-256 and extracts `.mj/keep/` with strict
//! symlink/path-containment checks, persisting the archive atomically only once
//! both parts arrive complete and verified.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mj_core::hex::lower_hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

/// Magic header identifying version 1 of the Mjolnir Move stream framing.
pub const MOVE_STREAM_MAGIC: &[u8; 8] = b"MJMV0001";

const STREAM_BUF_SIZE: usize = 64 * 1024;

/// Summary returned by the sender after writing a framed Move stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveStreamExportReport {
    pub archive_bytes: u64,
    pub archive_sha256: String,
    pub keep_tar_bytes: u64,
}

/// Verified receipt returned by the receiver after consuming a framed Move stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveStreamReceipt {
    pub archive_bytes: u64,
    pub archive_sha256: String,
    pub keep_tar_bytes: u64,
}

/// Write a framed Move stream (archive + optional `.mj/keep/` tar) to `writer`.
///
/// `skip` paths are relative to `keep_dir` and excluded from the tar stream.
pub fn write_move_stream<W: Write>(
    writer: &mut W,
    archive_path: &Path,
    keep_dir: Option<&Path>,
    skip: &[PathBuf],
) -> Result<MoveStreamExportReport> {
    let mut archive_file = File::open(archive_path)
        .with_context(|| format!("open archive {}", archive_path.display()))?;
    let archive_len = archive_file
        .metadata()
        .with_context(|| format!("stat archive {}", archive_path.display()))?
        .len();

    let normalized_skips = normalize_skip_paths(skip)?;

    // Build the keep tar into a temporary file on disk so we know its exact
    // length prefix without buffering large scratch trees in memory.
    let mut keep_tar_temp: Option<NamedTempFile> = None;
    let keep_tar_len = if let Some(dir) = keep_dir {
        ensure_no_symlinked_keep_ancestors(dir)?;
        if dir.exists() {
            let mut temp = NamedTempFile::new().context("create temp keep tar")?;
            let bytes = build_keep_tar(dir, &normalized_skips, &mut temp)?;
            temp.as_file().sync_all().context("sync temp keep tar")?;
            keep_tar_temp = Some(temp);
            bytes
        } else {
            0
        }
    } else {
        0
    };

    writer
        .write_all(MOVE_STREAM_MAGIC)
        .context("write move stream magic")?;
    writer
        .write_all(&archive_len.to_le_bytes())
        .context("write archive length")?;

    let mut hasher = Sha256::new();
    let copied_archive = copy_with_hash(&mut archive_file, writer, Some(&mut hasher))
        .context("stream archive payload")?;
    ensure!(
        copied_archive == archive_len,
        "archive size changed during export: expected {archive_len}, wrote {copied_archive}"
    );
    let archive_sha256 = lower_hex(hasher.finalize());

    writer
        .write_all(&keep_tar_len.to_le_bytes())
        .context("write keep tar length")?;
    if let Some(mut temp) = keep_tar_temp {
        use std::io::Seek;
        temp.seek(io::SeekFrom::Start(0))
            .context("rewind temp keep tar")?;
        let copied_keep =
            copy_with_hash(temp.as_file_mut(), writer, None).context("stream keep tar payload")?;
        ensure!(
            copied_keep == keep_tar_len,
            "keep tar size mismatch during export: expected {keep_tar_len}, wrote {copied_keep}"
        );
    }

    writer.flush().context("flush move stream")?;
    Ok(MoveStreamExportReport {
        archive_bytes: archive_len,
        archive_sha256,
        keep_tar_bytes: keep_tar_len,
    })
}

/// Receive and verify a framed Move stream from `reader`.
///
/// The archive is written to a temporary file beside `archive_dest`, verified
/// against `expected_sha256`, and atomically persisted into `archive_dest`
/// only after both the archive and the `.mj/keep/` tar complete cleanly.
pub fn receive_move_stream<R: Read>(
    reader: &mut R,
    archive_dest: &Path,
    expected_sha256: &str,
    keep_dest: Option<&Path>,
) -> Result<MoveStreamReceipt> {
    let mut magic = [0u8; 8];
    reader
        .read_exact(&mut magic)
        .context("read move stream magic")?;
    ensure!(
        &magic == MOVE_STREAM_MAGIC,
        "invalid move stream magic header"
    );

    let archive_len = read_u64_le(reader).context("read archive length")?;
    let archive_parent = archive_dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("archive_dest must have a parent directory")?;
    fs::create_dir_all(archive_parent)
        .with_context(|| format!("create archive dir {}", archive_parent.display()))?;

    let mut archive_temp = NamedTempFile::new_in(archive_parent)
        .with_context(|| format!("create temp archive in {}", archive_parent.display()))?;
    let mut hasher = Sha256::new();
    let archive_bytes =
        copy_exact_with_hash(reader, archive_temp.as_file_mut(), archive_len, Some(&mut hasher))
            .context("receive archive payload")?;
    archive_temp
        .as_file()
        .sync_all()
        .context("sync received archive")?;

    let archive_sha256 = lower_hex(hasher.finalize());
    ensure!(
        archive_sha256.eq_ignore_ascii_case(expected_sha256),
        "archive SHA-256 mismatch: expected {expected_sha256}, got {archive_sha256}"
    );

    let keep_tar_len = read_u64_le(reader).context("read keep tar length")?;
    let mut staged_keep: Option<(tempfile::TempDir, &Path)> = None;
    if keep_tar_len > 0 {
        let dest = keep_dest.context("received non-empty keep tar without a keep_dest path")?;
        ensure_no_symlinked_keep_ancestors(dest)?;
        let keep_parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .context("keep_dest must have a parent directory")?;
        fs::create_dir_all(keep_parent)
            .with_context(|| format!("create keep parent dir {}", keep_parent.display()))?;
        ensure_no_symlinked_keep_ancestors(dest)?;
        let mut keep_temp = NamedTempFile::new().context("create temp keep tar receiver")?;
        let keep_bytes = copy_exact_with_hash(reader, keep_temp.as_file_mut(), keep_tar_len, None)
            .context("receive keep tar payload")?;
        ensure!(keep_bytes == keep_tar_len, "truncated keep tar payload");
        keep_temp
            .as_file()
            .sync_all()
            .context("sync temp keep tar receiver")?;
        let stage_dir = tempfile::Builder::new()
            .prefix(".mj-keep-stage-")
            .tempdir_in(keep_parent)
            .with_context(|| format!("create keep staging dir in {}", keep_parent.display()))?;
        extract_keep_tar_strict(keep_temp.path(), stage_dir.path())?;
        staged_keep = Some((stage_dir, dest));
    }

    // Ensure there are no unexpected trailing bytes before committing either
    // the archive or the staged keep directory.
    let mut trailing = [0u8; 1];
    let extra = reader.read(&mut trailing).context("check move stream EOF")?;
    ensure!(extra == 0, "unexpected trailing bytes after move stream");

    // Swap the staged keep directory into place with a rollback guard so that
    // if `archive_temp.persist(archive_dest)` fails, the prior `keep_dest` is
    // restored untouched.
    let keep_commit = if let Some((stage_dir, dest)) = staged_keep {
        Some(swap_staged_keep_dir(stage_dir, dest)?)
    } else {
        None
    };

    archive_temp
        .persist(archive_dest)
        .map_err(|err| err.error)
        .with_context(|| format!("persist verified archive to {}", archive_dest.display()))?;

    if let Some(commit) = keep_commit {
        commit.commit();
    }

    Ok(MoveStreamReceipt {
        archive_bytes,
        archive_sha256,
        keep_tar_bytes: keep_tar_len,
    })
}

/// Guard holding the backup of a replaced `keep_dest` until `archive_dest`
/// persists. Dropping without calling [`Self::commit`] rolls `keep_dest` back.
struct StagedKeepCommit {
    dest: PathBuf,
    backup: Option<PathBuf>,
    committed: bool,
}

impl StagedKeepCommit {
    fn commit(mut self) {
        self.committed = true;
        if let Some(backup_path) = self.backup.take() {
            let _ = fs::remove_dir_all(&backup_path);
        }
    }
}

impl Drop for StagedKeepCommit {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = fs::remove_dir_all(&self.dest);
        if let Some(backup_path) = self.backup.take() {
            let _ = fs::rename(&backup_path, &self.dest);
        }
    }
}

/// Swap `stage_dir` into `dest`, keeping any prior `dest` in a sibling backup
/// until the returned [`StagedKeepCommit`] is committed.
fn swap_staged_keep_dir(stage_dir: tempfile::TempDir, dest: &Path) -> Result<StagedKeepCommit> {
    let stage_path = stage_dir.keep();
    let backup = match fs::symlink_metadata(dest) {
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink(),
                "refusing symlink at keep dest {}",
                dest.display()
            );
            let parent = dest
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .context("keep_dest must have a parent directory")?;
            let backup_dir = tempfile::Builder::new()
                .prefix(".mj-keep-old-")
                .tempdir_in(parent)
                .context("create backup dir for keep swap")?;
            let backup_path = backup_dir.keep();
            let _ = fs::remove_dir(&backup_path);
            fs::rename(dest, &backup_path)
                .with_context(|| format!("move existing keep dir {} aside", dest.display()))?;
            Some(backup_path)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            let _ = fs::remove_dir_all(&stage_path);
            return Err(err).with_context(|| format!("lstat keep dest {}", dest.display()));
        }
    };

    if let Err(err) = fs::rename(&stage_path, dest) {
        if let Some(backup_path) = backup {
            let _ = fs::rename(&backup_path, dest);
        }
        let _ = fs::remove_dir_all(&stage_path);
        return Err(err).with_context(|| format!("commit staged keep dir to {}", dest.display()));
    }

    Ok(StagedKeepCommit {
        dest: dest.to_path_buf(),
        backup,
        committed: false,
    })
}

fn read_u64_le<R: Read>(reader: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn copy_with_hash<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut hasher: Option<&mut Sha256>,
) -> Result<u64> {
    let mut buf = vec![0u8; STREAM_BUF_SIZE];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(h) = hasher.as_deref_mut() {
            h.update(&buf[..n]);
        }
        writer.write_all(&buf[..n])?;
        total = total.checked_add(n as u64).context("stream byte count overflow")?;
    }
    Ok(total)
}

fn copy_exact_with_hash<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    expected: u64,
    mut hasher: Option<&mut Sha256>,
) -> Result<u64> {
    let mut buf = vec![0u8; STREAM_BUF_SIZE];
    let mut remaining = expected;
    while remaining > 0 {
        let to_read = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = reader.read(&mut buf[..to_read])?;
        if n == 0 {
            bail!(
                "truncated move stream: expected {expected} bytes, got {}",
                expected - remaining
            );
        }
        if let Some(h) = hasher.as_deref_mut() {
            h.update(&buf[..n]);
        }
        writer.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(expected)
}

fn normalize_skip_paths(skip: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    let mut out = BTreeSet::new();
    for raw in skip {
        let norm = normalize_relative_path(raw)?;
        out.insert(norm);
    }
    Ok(out)
}

fn normalize_relative_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("invalid relative path in keep stream: {}", path.display());
            }
        }
    }
    ensure!(
        !out.as_os_str().is_empty(),
        "empty relative path in keep stream"
    );
    Ok(out)
}

fn is_skipped(rel: &Path, skips: &BTreeSet<PathBuf>) -> bool {
    skips.iter().any(|skip| rel == skip || rel.starts_with(skip))
}

/// Verify that neither `keep_dir` nor its checkout-owned ancestors (its
/// immediate parent, and `.mj` when under `.../.mj/keep/<session-id>`) are
/// symlinks (including dangling symlinks).
///
/// Ancestors above the checkout root (such as `/var` -> `/private/var` on
/// macOS) are intentionally left to the OS while any symlink at `.mj`,
/// `.mj/keep`, or `.mj/keep/<session-id>` is refused before reading or
/// creating directories.
fn ensure_no_symlinked_keep_ancestors(keep_dir: &Path) -> Result<()> {
    let mut to_check: Vec<&Path> = Vec::new();
    if let Some(parent) = keep_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        if parent.file_name() == Some(std::ffi::OsStr::new("keep"))
            && let Some(grandparent) = parent.parent().filter(|p| !p.as_os_str().is_empty())
            && grandparent.file_name() == Some(std::ffi::OsStr::new(".mj"))
        {
            to_check.push(grandparent);
        }
        to_check.push(parent);
    }
    to_check.push(keep_dir);

    for path in to_check {
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "refusing symlink in keep path or ancestor: {}",
                    path.display()
                );
                ensure!(
                    meta.file_type().is_dir(),
                    "keep path or ancestor is not a directory: {}",
                    path.display()
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("lstat keep path {}", path.display()));
            }
        }
    }
    Ok(())
}

fn build_keep_tar<W: Write>(
    keep_dir: &Path,
    skips: &BTreeSet<PathBuf>,
    writer: &mut W,
) -> Result<u64> {
    let mut counting = CountingWriter::new(writer);
    {
        let mut builder = tar::Builder::new(&mut counting);
        builder.follow_symlinks(false);
        append_keep_dir_recursive(&mut builder, keep_dir, Path::new(""), skips)?;
        builder.finish().context("finish keep tar")?;
    }
    Ok(counting.bytes)
}

fn append_keep_dir_recursive<W: Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    rel: &Path,
    skips: &BTreeSet<PathBuf>,
) -> Result<()> {
    let current = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let mut entries: Vec<_> = fs::read_dir(&current)
        .with_context(|| format!("read keep directory {}", current.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("iterate keep directory {}", current.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let entry_path = entry.path();
        let entry_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(entry.file_name())
        } else {
            rel.join(entry.file_name())
        };
        if is_skipped(&entry_rel, skips) {
            continue;
        }
        let meta = fs::symlink_metadata(&entry_path)
            .with_context(|| format!("lstat {}", entry_path.display()))?;
        let file_type = meta.file_type();
        ensure!(
            !file_type.is_symlink(),
            "refusing symlink inside keep directory: {}",
            entry_rel.display()
        );
        if file_type.is_dir() {
            builder
                .append_dir(&entry_rel, &entry_path)
                .with_context(|| format!("append keep dir {}", entry_rel.display()))?;
            append_keep_dir_recursive(builder, root, &entry_rel, skips)?;
        } else if file_type.is_file() {
            let mut file = File::open(&entry_path)
                .with_context(|| format!("open keep file {}", entry_path.display()))?;
            builder
                .append_file(&entry_rel, &mut file)
                .with_context(|| format!("append keep file {}", entry_rel.display()))?;
        } else {
            bail!(
                "refusing unsupported file type inside keep directory: {}",
                entry_rel.display()
            );
        }
    }
    Ok(())
}

/// Extract a `.mj/keep/` tar archive strictly inside `dest`, refusing any
/// symlink (including dangling symlinks), hard link, absolute path, or `..`
/// traversal, and preserving file/directory permission bits masked to `0o777`.
pub fn extract_keep_tar_strict(tar_path: &Path, dest: &Path) -> Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink(),
                "refusing symlink in keep dest: {}",
                dest.display()
            );
            ensure!(
                meta.file_type().is_dir(),
                "keep dest is not a directory: {}",
                dest.display()
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            ensure_no_symlinked_keep_ancestors(dest)?;
            fs::create_dir_all(dest)
                .with_context(|| format!("create keep dest {}", dest.display()))?;
            ensure_no_symlinked_keep_ancestors(dest)?;
        }
        Err(err) => {
            return Err(err).with_context(|| format!("lstat keep dest {}", dest.display()));
        }
    }

    let file = File::open(tar_path)
        .with_context(|| format!("open received keep tar {}", tar_path.display()))?;
    let mut archive = tar::Archive::new(file);
    let entries = archive.entries().context("read keep tar entries")?;
    #[cfg(unix)]
    let mut deferred_dir_modes: Vec<(PathBuf, u32)> = Vec::new();

    for entry in entries {
        let mut entry = entry.context("read keep tar entry")?;
        let entry_type = entry.header().entry_type();
        ensure!(
            entry_type.is_file() || entry_type.is_dir(),
            "refusing non-file/non-directory entry in keep tar: {entry_type:?}"
        );
        let mode = entry.header().mode().ok().map(|m| m & 0o777);

        let raw_path = entry.path().context("read keep tar entry path")?.into_owned();
        let rel = normalize_relative_path(&raw_path)?;
        let target = dest.join(&rel);

        // Verify every intermediate component inside `dest` via `symlink_metadata`
        // (never `Path::exists()`, which returns false for dangling symlinks).
        let mut cursor = dest.to_path_buf();
        if let Some(parent) = rel.parent() {
            for comp in parent.components() {
                if let Component::Normal(part) = comp {
                    cursor.push(part);
                    match fs::symlink_metadata(&cursor) {
                        Ok(meta) => {
                            ensure!(
                                !meta.file_type().is_symlink(),
                                "refusing symlink in keep path: {}",
                                cursor.display()
                            );
                            ensure!(
                                meta.file_type().is_dir(),
                                "keep parent is not a directory: {}",
                                cursor.display()
                            );
                        }
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {
                            fs::create_dir(&cursor).with_context(|| {
                                format!("create keep subdir {}", cursor.display())
                            })?;
                        }
                        Err(err) => {
                            return Err(err)
                                .with_context(|| format!("lstat keep subdir {}", cursor.display()));
                        }
                    }
                }
            }
        }

        if entry_type.is_dir() {
            match fs::symlink_metadata(&target) {
                Ok(meta) => {
                    ensure!(
                        !meta.file_type().is_symlink(),
                        "refusing symlink in keep path: {}",
                        target.display()
                    );
                    ensure!(
                        meta.file_type().is_dir(),
                        "keep dir target is not a directory: {}",
                        target.display()
                    );
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&target)
                        .with_context(|| format!("create keep dir {}", target.display()))?;
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("lstat keep dir {}", target.display()));
                }
            }
            #[cfg(unix)]
            if let Some(dir_mode) = mode {
                use std::os::unix::fs::PermissionsExt;
                // Temporarily keep owner rwx so children inside this directory
                // can be extracted; exact `dir_mode & 0o777` is restored
                // bottom-up after all entries are extracted.
                let safe_mode = (dir_mode & 0o777) | 0o700;
                fs::set_permissions(&target, fs::Permissions::from_mode(safe_mode))
                    .with_context(|| format!("chmod keep dir {}", target.display()))?;
                deferred_dir_modes.push((target.clone(), dir_mode & 0o777));
            }
        } else {
            // Check `symlink_metadata` directly so dangling symlinks are caught too.
            match fs::symlink_metadata(&target) {
                Ok(meta) => {
                    ensure!(
                        !meta.file_type().is_symlink(),
                        "refusing symlink in keep path: {}",
                        target.display()
                    );
                    ensure!(
                        meta.file_type().is_file(),
                        "keep file target is not a regular file: {}",
                        target.display()
                    );
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("lstat keep file {}", target.display()));
                }
            }
            let file_mode = mode.unwrap_or(0o600);
            let mut out = open_keep_file_nofollow(&target, file_mode)?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("extract keep file {}", target.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                out.set_permissions(fs::Permissions::from_mode(file_mode))
                    .with_context(|| format!("chmod keep file {}", target.display()))?;
            }
        }
    }

    // Restore restrictive directory modes bottom-up (deepest first).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        deferred_dir_modes.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (dir_path, dir_mode) in deferred_dir_modes {
            fs::set_permissions(&dir_path, fs::Permissions::from_mode(dir_mode))
                .with_context(|| format!("restore keep dir mode {}", dir_path.display()))?;
        }
    }

    Ok(())
}

fn open_keep_file_nofollow(path: &Path, mode: u32) -> Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode & 0o777);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    opts.open(path)
        .with_context(|| format!("create keep file {}", path.display()))
}

struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::thread;

    #[cfg(unix)]
    fn os_pipe() -> (File, File) {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid 2-element array for `pipe`.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "libc::pipe failed");
        // SAFETY: `fds[0]` and `fds[1]` are freshly created open descriptors.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    #[cfg(unix)]
    #[test]
    fn streams_over_128kib_through_real_pipe_with_skip_and_sha256() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        let dst_archive = dir.path().join("dst.hel.zip");
        let src_keep = dir.path().join("src_keep");
        let dst_keep = dir.path().join("dst_keep");

        // >128 KiB archive payload to exercise >64 KiB pipe buffers.
        let archive_payload: Vec<u8> = (0..(192 * 1024)).map(|i| (i % 251) as u8).collect();
        fs::write(&src_archive, &archive_payload).expect("write src archive");
        let expected_sha = lower_hex(Sha256::digest(&archive_payload));

        // >128 KiB keep payload plus a skipped file.
        fs::create_dir_all(src_keep.join("nested")).expect("mkdir src_keep");
        let keep_big: Vec<u8> = (0..(140 * 1024)).map(|i| ((i * 7) % 253) as u8).collect();
        fs::write(src_keep.join("nested/big.bin"), &keep_big).expect("write keep big");
        fs::write(src_keep.join("skip-me.txt"), b"do not send").expect("write skip file");

        let (mut read_end, mut write_end) = os_pipe();
        let src_archive_clone = src_archive.clone();
        let src_keep_clone = src_keep.clone();
        let writer_handle = thread::spawn(move || {
            let report = write_move_stream(
                &mut write_end,
                &src_archive_clone,
                Some(&src_keep_clone),
                &[PathBuf::from("skip-me.txt")],
            )
            .expect("write_move_stream");
            drop(write_end);
            report
        });

        let receipt = receive_move_stream(
            &mut read_end,
            &dst_archive,
            &expected_sha,
            Some(&dst_keep),
        )
        .expect("receive_move_stream");
        let export = writer_handle.join().expect("writer thread");

        assert_eq!(export.archive_bytes, archive_payload.len() as u64);
        assert_eq!(receipt.archive_bytes, export.archive_bytes);
        assert_eq!(receipt.archive_sha256, expected_sha);
        assert_eq!(receipt.keep_tar_bytes, export.keep_tar_bytes);
        assert!(receipt.keep_tar_bytes > 128 * 1024);

        assert_eq!(fs::read(&dst_archive).expect("read dst archive"), archive_payload);
        assert_eq!(
            fs::read(dst_keep.join("nested/big.bin")).expect("read keep big"),
            keep_big
        );
        assert!(
            !dst_keep.join("skip-me.txt").exists(),
            "skipped path must not be extracted"
        );
    }

    #[test]
    fn rejects_sha256_mismatch_and_truncated_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        let dst_archive = dir.path().join("dst.hel.zip");
        fs::write(&src_archive, b"archive-content").expect("write src");

        let mut framed = Vec::new();
        let report =
            write_move_stream(&mut framed, &src_archive, None, &[]).expect("write framed");

        // 1. SHA-256 mismatch must fail and leave dst_archive absent.
        let bad_sha = "0".repeat(64);
        let err = receive_move_stream(&mut Cursor::new(&framed), &dst_archive, &bad_sha, None)
            .expect_err("expected sha mismatch");
        assert!(err.to_string().contains("SHA-256 mismatch"));
        assert!(!dst_archive.exists());

        // 2. Truncated stream must fail and leave dst_archive absent.
        let truncated = &framed[..framed.len() - 5];
        let err = receive_move_stream(
            &mut Cursor::new(truncated),
            &dst_archive,
            &report.archive_sha256,
            None,
        )
        .expect_err("expected truncated stream error");
        assert!(
            err.to_string().contains("truncated") || err.to_string().contains("read"),
            "unexpected error: {err}"
        );
        assert!(!dst_archive.exists());
    }

    #[test]
    fn rejects_symlink_escape_in_keep_tar_extraction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad_tar = dir.path().join("escape.tar");
        let dst_keep = dir.path().join("dst_keep");

        // Build a tar containing a symlink entry.
        {
            let file = File::create(&bad_tar).expect("create bad tar");
            let mut builder = tar::Builder::new(file);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_link(&mut header, "evil-link", "/etc/passwd")
                .expect("append symlink");
            builder.finish().expect("finish bad tar");
        }

        let err = extract_keep_tar_strict(&bad_tar, &dst_keep)
            .expect_err("symlink entry must be rejected");
        assert!(err.to_string().contains("refusing non-file/non-directory"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_at_extraction_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let outside_dir = tempfile::tempdir().expect("outside tempdir");
        let outside_target = outside_dir.path().join("pwned.txt");

        let src_keep = dir.path().join("src_keep");
        fs::create_dir_all(&src_keep).expect("mkdir src_keep");
        fs::write(src_keep.join("secret.txt"), b"sensitive").expect("write secret");

        let tar_path = dir.path().join("keep.tar");
        {
            let mut tar_file = File::create(&tar_path).expect("create tar");
            build_keep_tar(&src_keep, &BTreeSet::new(), &mut tar_file).expect("build tar");
        }

        let dst_keep = dir.path().join("dst_keep");
        fs::create_dir_all(&dst_keep).expect("mkdir dst_keep");
        symlink(&outside_target, dst_keep.join("secret.txt")).expect("create dangling symlink");

        let err = extract_keep_tar_strict(&tar_path, &dst_keep)
            .expect_err("dangling symlink at extraction target must be rejected");
        assert!(
            err.to_string().contains("refusing symlink"),
            "unexpected error: {err}"
        );
        assert!(
            !outside_target.exists(),
            "outside target must not be created through dangling symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_executable_and_private_permissions_on_round_trip() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        let dst_archive = dir.path().join("dst.hel.zip");
        let src_keep = dir.path().join("src_keep");
        let dst_keep = dir.path().join("dst_keep");

        fs::write(&src_archive, b"archive-bytes").expect("write archive");
        let expected_sha = lower_hex(Sha256::digest(b"archive-bytes"));

        fs::create_dir_all(&src_keep).expect("mkdir src_keep");
        let script = src_keep.join("run.sh");
        let secret = src_keep.join("secret.key");
        fs::write(&script, b"#!/bin/sh\necho ok\n").expect("write script");
        fs::write(&secret, b"top-secret").expect("write secret");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod 755");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).expect("chmod 600");

        let mut framed = Vec::new();
        write_move_stream(&mut framed, &src_archive, Some(&src_keep), &[]).expect("write stream");
        receive_move_stream(
            &mut Cursor::new(&framed),
            &dst_archive,
            &expected_sha,
            Some(&dst_keep),
        )
        .expect("receive stream");

        let script_mode = fs::metadata(dst_keep.join("run.sh"))
            .expect("stat run.sh")
            .permissions()
            .mode()
            & 0o777;
        let secret_mode = fs::metadata(dst_keep.join("secret.key"))
            .expect("stat secret.key")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(script_mode, 0o755, "executable mode must be preserved");
        assert_eq!(secret_mode, 0o600, "private 0600 mode must be preserved");
    }

    #[test]
    fn failed_receive_leaves_existing_keep_dest_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        let dst_archive = dir.path().join("dst.hel.zip");
        let src_keep = dir.path().join("src_keep");
        let dst_keep = dir.path().join("dst_keep");

        fs::write(&src_archive, b"archive-bytes").expect("write src archive");
        let expected_sha = lower_hex(Sha256::digest(b"archive-bytes"));
        fs::create_dir_all(&src_keep).expect("mkdir src_keep");
        fs::write(src_keep.join("new.txt"), b"incoming").expect("write new keep file");

        // Pre-populate dst_keep with an existing file that must survive a failed receive.
        fs::create_dir_all(&dst_keep).expect("mkdir dst_keep");
        fs::write(dst_keep.join("existing.txt"), b"original").expect("write existing");

        let mut framed = Vec::new();
        write_move_stream(&mut framed, &src_archive, Some(&src_keep), &[]).expect("write framed");
        // Append unexpected trailing garbage so receive_move_stream fails at EOF check.
        framed.extend_from_slice(b"EXTRA");

        let err = receive_move_stream(
            &mut Cursor::new(&framed),
            &dst_archive,
            &expected_sha,
            Some(&dst_keep),
        )
        .expect_err("trailing bytes must fail receive");
        assert!(err.to_string().contains("unexpected trailing bytes"));
        assert!(!dst_archive.exists(), "archive must not be persisted on failure");
        assert_eq!(
            fs::read(dst_keep.join("existing.txt")).expect("existing keep file must remain"),
            b"original"
        );
        assert!(
            !dst_keep.join("new.txt").exists(),
            "staged keep files must not leak into keep_dest on failure"
        );
    }

    #[test]
    fn failed_archive_persist_rolls_back_keep_dest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        // Make `dst_archive` an existing directory so `archive_temp.persist` fails.
        let dst_archive = dir.path().join("dst_is_dir.hel.zip");
        fs::create_dir_all(&dst_archive).expect("mkdir dst_archive");

        let src_keep = dir.path().join("src_keep");
        let dst_keep = dir.path().join("dst_keep");
        fs::write(&src_archive, b"archive-bytes").expect("write src archive");
        let expected_sha = lower_hex(Sha256::digest(b"archive-bytes"));
        fs::create_dir_all(&src_keep).expect("mkdir src_keep");
        fs::write(src_keep.join("new.txt"), b"incoming").expect("write new keep");

        fs::create_dir_all(&dst_keep).expect("mkdir dst_keep");
        fs::write(dst_keep.join("existing.txt"), b"original").expect("write existing");

        let mut framed = Vec::new();
        write_move_stream(&mut framed, &src_archive, Some(&src_keep), &[]).expect("write framed");

        let err = receive_move_stream(
            &mut Cursor::new(&framed),
            &dst_archive,
            &expected_sha,
            Some(&dst_keep),
        )
        .expect_err("persisting archive onto a directory must fail");
        assert!(err.to_string().contains("persist verified archive"));
        assert_eq!(
            fs::read(dst_keep.join("existing.txt")).expect("existing keep must be rolled back"),
            b"original"
        );
        assert!(
            !dst_keep.join("new.txt").exists(),
            "new keep file must be rolled back on archive persist failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_keep_ancestors_on_export_and_receive() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).expect("mkdir outside");
        fs::write(outside.join("secret.txt"), b"outside-data").expect("write outside");

        let src_archive = dir.path().join("src.hel.zip");
        fs::write(&src_archive, b"archive-bytes").expect("write archive");
        let expected_sha = lower_hex(Sha256::digest(b"archive-bytes"));

        // 1. Export must reject a symlinked `.mj/keep` ancestor.
        let bad_src_checkout = dir.path().join("bad-src");
        fs::create_dir_all(bad_src_checkout.join(".mj")).expect("mkdir .mj");
        symlink(&outside, bad_src_checkout.join(".mj/keep")).expect("symlink keep");
        let mut out = Vec::new();
        let err = write_move_stream(
            &mut out,
            &src_archive,
            Some(&bad_src_checkout.join(".mj/keep/sess-1")),
            &[],
        )
        .expect_err("export through symlinked .mj/keep must fail");
        assert!(err.to_string().contains("refusing symlink"));

        // 2. Export must reject a symlinked `.mj` ancestor.
        let bad_mj_checkout = dir.path().join("bad-mj");
        fs::create_dir_all(&bad_mj_checkout).expect("mkdir checkout");
        symlink(&outside, bad_mj_checkout.join(".mj")).expect("symlink .mj");
        let err = write_move_stream(
            &mut Vec::new(),
            &src_archive,
            Some(&bad_mj_checkout.join(".mj/keep/sess-1")),
            &[],
        )
        .expect_err("export through symlinked .mj must fail");
        assert!(err.to_string().contains("refusing symlink"));

        // Build a valid framed stream to test receive rejection.
        let good_keep = dir.path().join("good-keep");
        fs::create_dir_all(&good_keep).expect("mkdir good keep");
        fs::write(good_keep.join("note.txt"), b"hello").expect("write note");
        let mut framed = Vec::new();
        write_move_stream(&mut framed, &src_archive, Some(&good_keep), &[]).expect("write valid");

        // 3. Receive must reject a symlinked `.mj/keep` ancestor and leave `outside` untouched.
        let bad_dst_checkout = dir.path().join("bad-dst");
        fs::create_dir_all(bad_dst_checkout.join(".mj")).expect("mkdir dst .mj");
        symlink(&outside, bad_dst_checkout.join(".mj/keep")).expect("symlink dst keep");
        let dst_archive = dir.path().join("dst.hel.zip");
        let err = receive_move_stream(
            &mut Cursor::new(&framed),
            &dst_archive,
            &expected_sha,
            Some(&bad_dst_checkout.join(".mj/keep/sess-1")),
        )
        .expect_err("receive through symlinked .mj/keep must fail");
        assert!(err.to_string().contains("refusing symlink"));
        assert!(!outside.join("sess-1").exists());
        assert!(!dst_archive.exists());
    }
}

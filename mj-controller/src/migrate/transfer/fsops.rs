//! Filesystem helpers shared by the receiver's staging and publication code.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::super::protocol::validate_relative_path;

pub(super) fn symlink_metadata_opt(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

pub(super) fn exists(path: &Path) -> Result<bool> {
    Ok(symlink_metadata_opt(path)?.is_some())
}

/// Remove a file, symlink or directory tree. A missing path is already removed.
pub(super) fn remove_path(path: &Path) -> Result<()> {
    let Some(metadata) = symlink_metadata_opt(path)? else {
        return Ok(());
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("remove {}", path.display()))
}

/// `root/rel`, with every ancestor below `root` a real directory. An incoming
/// symlink that is later named as a parent must never redirect a write to a
/// path outside `root`, so a non-directory ancestor is replaced, not followed.
/// With `create` false a missing or non-directory ancestor yields `None`.
pub(super) fn resolve_in_scope(root: &Path, rel: &Path, create: bool) -> Result<Option<PathBuf>> {
    validate_relative_path(rel)?;
    let mut current = root.to_path_buf();
    let count = rel.components().count();
    for (index, component) in rel.components().enumerate() {
        current.push(component);
        if index + 1 == count {
            break;
        }
        match symlink_metadata_opt(&current)? {
            Some(metadata) if metadata.is_dir() => {}
            Some(_) if create => {
                std::fs::remove_file(&current)
                    .with_context(|| format!("replace {}", current.display()))?;
                std::fs::create_dir(&current)
                    .with_context(|| format!("create {}", current.display()))?;
            }
            None if create => std::fs::create_dir(&current)
                .with_context(|| format!("create {}", current.display()))?,
            _ => return Ok(None),
        }
    }
    Ok(Some(current))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PublishMode {
    /// Rename into place; the source disappears.
    Move,
    /// Leave the source so the publication can be repeated from it.
    Copy,
}

/// Put `src` at `dst`. Directories that exist on both sides are merged;
/// anything else at `dst` is replaced. A missing `src` was already published.
pub(super) fn publish_entry(src: &Path, dst: &Path, mode: PublishMode) -> Result<()> {
    let Some(source) = symlink_metadata_opt(src)? else {
        return Ok(());
    };
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let target = symlink_metadata_opt(dst)?;
    if source.is_dir() && target.as_ref().is_some_and(|target| target.is_dir()) {
        for child in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let child = child?;
            publish_entry(&child.path(), &dst.join(child.file_name()), mode)?;
        }
        if mode == PublishMode::Move {
            std::fs::remove_dir(src).with_context(|| format!("remove {}", src.display()))?;
        }
        return Ok(());
    }
    if target.is_some() {
        remove_path(dst)?;
    }
    match mode {
        PublishMode::Copy => copy_tree_into_place(src, dst),
        PublishMode::Move => match std::fs::rename(src, dst) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
                copy_tree_into_place(src, dst)?;
                remove_path(src)
            }
            Err(error) => {
                Err(error).with_context(|| format!("move {} to {}", src.display(), dst.display()))
            }
        },
    }
}

/// Copy beside `dst` under a temporary name, then rename, so `dst` is either
/// absent or complete.
fn copy_tree_into_place(src: &Path, dst: &Path) -> Result<()> {
    let mut name = dst
        .file_name()
        .context("publication target has no name")?
        .to_owned();
    name.push(format!(".mj-migrate-tmp-{}", std::process::id()));
    let temporary = dst.with_file_name(name);
    remove_path(&temporary)?;
    if let Err(error) = copy_tree(src, &temporary) {
        let _ = remove_path(&temporary);
        return Err(error);
    }
    std::fs::rename(&temporary, dst)
        .with_context(|| format!("rename {} into place", temporary.display()))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(src).with_context(|| format!("inspect {}", src.display()))?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dst)?;
        #[cfg(not(unix))]
        bail!("cannot publish symlink {} on this platform", src.display());
    } else if metadata.is_dir() {
        std::fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))?;
        for child in std::fs::read_dir(src)? {
            let child = child?;
            copy_tree(&child.path(), &dst.join(child.file_name()))?;
        }
        std::fs::set_permissions(dst, metadata.permissions())?;
    } else if metadata.is_file() {
        std::fs::copy(src, dst)
            .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
        std::fs::File::open(dst)?.sync_all()?;
    } else {
        bail!("cannot publish special file {}", src.display());
    }
    Ok(())
}

pub(super) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(super) fn set_unix_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("set mode on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Bytes available to this user on the filesystem holding `path`, or its
/// nearest existing ancestor. `None` where the platform gives no answer.
pub(super) fn available_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut probe = path;
        while !probe.exists() {
            probe = probe.parent()?;
        }
        let c_path = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `c_path` is a valid NUL-terminated string and `stat` points
        // to writable storage of the right type.
        let status = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
        if status != 0 {
            return None;
        }
        // SAFETY: statvfs returned success, so it initialized the struct.
        let stat = unsafe { stat.assume_init() };
        #[allow(
            clippy::unnecessary_cast,
            reason = "the statvfs field types differ by platform"
        )]
        let bytes = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
        Some(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

pub(super) fn mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
        })
}

pub(super) fn unix_mode(metadata: &std::fs::Metadata, default: u32) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = default;
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        default
    }
}

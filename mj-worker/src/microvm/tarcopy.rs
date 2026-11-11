//! `cp` semantics on tar streams, shared by the host engine and the guest agent.
//!
//! A copy packs one file or directory under its own name. The receiver
//! decides where that name lands, the way `podman cp` does: an existing
//! directory receives the item inside it; otherwise the destination path
//! becomes the item's new name and its parent must exist.
//!
//! Either side may be unpacking a stream written by the other, and the guest
//! is the less trusted one, so unpacking accepts only regular files,
//! directories and symlinks, never leaves the destination, and never writes
//! through a symlink.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};

/// Write `source` as a tar stream whose entries are named from its final path component.
pub fn pack<W: Write>(source: &Path, out: W) -> Result<W> {
    ensure!(
        !source.as_os_str().as_bytes().ends_with(b"/."),
        "copying the contents of a directory with a trailing \"/.\" is not supported: {}",
        source.display()
    );
    let name = source
        .file_name()
        .ok_or_else(|| anyhow!("{} has no final path component to copy", source.display()))?;
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    if metadata.is_dir() {
        builder
            .append_dir_all(name, source)
            .with_context(|| format!("pack directory {}", source.display()))?;
    } else {
        builder
            .append_path_with_name(source, name)
            .with_context(|| format!("pack {}", source.display()))?;
    }
    builder.into_inner().context("finish tar stream")
}

/// Unpack a tar stream produced by [`pack`] at `destination`.
///
/// `owner` is the owner every created entry receives; `None` leaves entries
/// owned by the unpacking process.
pub fn unpack<R: Read>(reader: R, destination: &Path, owner: Option<(u32, u32)>) -> Result<()> {
    // `metadata` follows a symlinked destination directory on purpose: that is
    // the destination the caller named. Entries below it never follow links.
    let (root, rename) = match fs::metadata(destination) {
        Ok(metadata) if metadata.is_dir() => (destination.to_path_buf(), None),
        Ok(_) | Err(_) => {
            let name = destination
                .file_name()
                .ok_or_else(|| anyhow!("{} has no final path component", destination.display()))?
                .to_owned();
            let parent = match destination.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => PathBuf::from("."),
            };
            ensure!(
                fs::metadata(&parent).map(|m| m.is_dir()).unwrap_or(false),
                "destination directory {} does not exist",
                parent.display()
            );
            (parent, Some(name))
        }
    };
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("read tar stream")? {
        let mut entry = entry.context("read tar entry")?;
        let relative = entry.path().context("read tar entry path")?.into_owned();
        let relative = rename_top(&relative, rename.as_deref())?;
        let target = checked_target(&root, &relative)?;
        // Never carry setuid or setgid across the VM boundary: a guest-made
        // setuid file would become a privileged file on the host.
        let mode = entry.header().mode().unwrap_or(0o644) & 0o1777;
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                match fs::symlink_metadata(&target) {
                    Ok(existing) if existing.is_dir() => {}
                    Ok(_) => {
                        fs::remove_file(&target)
                            .with_context(|| format!("replace {}", target.display()))?;
                        fs::create_dir(&target)
                            .with_context(|| format!("create {}", target.display()))?;
                    }
                    Err(_) => fs::create_dir(&target)
                        .with_context(|| format!("create {}", target.display()))?,
                }
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                    .with_context(|| format!("set mode of {}", target.display()))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                remove_non_directory(&target)?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(mode)
                    .open(&target)
                    .with_context(|| format!("create {}", target.display()))?;
                std::io::copy(&mut entry, &mut file)
                    .with_context(|| format!("write {}", target.display()))?;
                // The creation mode is filtered by the umask.
                file.set_permissions(fs::Permissions::from_mode(mode))
                    .with_context(|| format!("set mode of {}", target.display()))?;
            }
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name()
                    .context("read symlink target")?
                    .ok_or_else(|| anyhow!("symlink entry {} has no target", relative.display()))?
                    .into_owned();
                remove_non_directory(&target)?;
                symlink(&link, &target)
                    .with_context(|| format!("create symlink {}", target.display()))?;
            }
            other => bail!(
                "unsupported tar entry type {other:?} for {}",
                relative.display()
            ),
        }
        if let Some((uid, gid)) = owner {
            lchown(&target, uid, gid)?;
        }
        if !matches!(entry.header().entry_type(), tar::EntryType::Symlink)
            && let Ok(mtime) = entry.header().mtime()
        {
            set_mtime(&target, mtime)?;
        }
    }
    Ok(())
}

fn rename_top(path: &Path, rename: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let mut components = path.components();
    let mut result = PathBuf::new();
    match components.next() {
        Some(Component::Normal(first)) => result.push(rename.unwrap_or(first)),
        _ => bail!("tar entry path {} is not a relative path", path.display()),
    }
    for component in components {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            _ => bail!("tar entry path {} leaves its destination", path.display()),
        }
    }
    Ok(result)
}

/// `root` joined with `relative`, creating missing parents and refusing a
/// symlink anywhere between them.
fn checked_target(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    let parts: Vec<OsString> = relative
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        if index + 1 == parts.len() {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!(
                "refusing to unpack through {}: it is not a directory",
                current.display()
            ),
            Err(_) => {
                fs::create_dir(&current).with_context(|| format!("create {}", current.display()))?
            }
        }
    }
    Ok(current)
}

fn remove_non_directory(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() => bail!(
            "refusing to replace directory {} with a file",
            target.display()
        ),
        Ok(_) => fs::remove_file(target).with_context(|| format!("replace {}", target.display())),
        Err(_) => Ok(()),
    }
}

fn lchown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))?;
    // SAFETY: `path_bytes` is a valid NUL-terminated string for the call.
    if unsafe { libc::lchown(path_bytes.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("change owner of {}", path.display()));
    }
    Ok(())
}

fn set_mtime(path: &Path, seconds: u64) -> Result<()> {
    let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: seconds as _,
            tv_nsec: 0,
        },
    ];
    // SAFETY: both pointers are valid for the call; AT_SYMLINK_NOFOLLOW keeps
    // the update on the entry itself.
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path_bytes.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("set modification time of {}", path.display()));
    }
    Ok(())
}

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

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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
///
/// Every operation is relative to a directory descriptor this function holds,
/// and no step follows a symlink, so another process that swaps a checked
/// directory for a symlink while the engine runs as root cannot redirect the
/// unpacking: the swap is seen as an error, not followed.
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
    let root_fd = open_path(
        libc::AT_FDCWD,
        &root,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
    )
    .with_context(|| format!("open {}", root.display()))?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("read tar stream")? {
        let mut entry = entry.context("read tar entry")?;
        let relative = entry.path().context("read tar entry path")?.into_owned();
        let relative = rename_top(&relative, rename.as_deref())?;
        let (parent_path, name) = (
            relative.parent().map(Path::to_path_buf).unwrap_or_default(),
            relative
                .file_name()
                .ok_or_else(|| anyhow!("tar entry {} has no name", relative.display()))?
                .to_owned(),
        );
        let parent = open_parents(&root_fd, &parent_path)?;
        let mode = entry.header().mode().unwrap_or(0o644) & 0o1777;
        let mtime = entry.header().mtime().ok();
        let created: Option<OwnedFd> = match entry.header().entry_type() {
            tar::EntryType::Directory => {
                match stat_at(&parent, &name)? {
                    Some(kind) if kind == libc::S_IFDIR => {}
                    Some(_) => {
                        unlink_at(&parent, &name, 0)?;
                        make_dir_at(&parent, &name)?;
                    }
                    None => make_dir_at(&parent, &name)?,
                }
                Some(
                    open_path(
                        parent.as_raw_fd(),
                        Path::new(&name),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        0,
                    )
                    .with_context(|| format!("open {}", relative.display()))?,
                )
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                match stat_at(&parent, &name)? {
                    Some(kind) if kind == libc::S_IFDIR => bail!(
                        "refusing to replace directory {} with a file",
                        relative.display()
                    ),
                    Some(_) => unlink_at(&parent, &name, 0)?,
                    None => {}
                }
                let fd = open_path(
                    parent.as_raw_fd(),
                    Path::new(&name),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
                .with_context(|| format!("create {}", relative.display()))?;
                let mut file = fs::File::from(fd);
                std::io::copy(&mut entry, &mut file)
                    .with_context(|| format!("write {}", relative.display()))?;
                Some(OwnedFd::from(file))
            }
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name()
                    .context("read symlink target")?
                    .ok_or_else(|| anyhow!("symlink entry {} has no target", relative.display()))?
                    .into_owned();
                match stat_at(&parent, &name)? {
                    Some(kind) if kind == libc::S_IFDIR => bail!(
                        "refusing to replace directory {} with a symlink",
                        relative.display()
                    ),
                    Some(_) => unlink_at(&parent, &name, 0)?,
                    None => {}
                }
                let target = c_string(link.as_os_str().as_bytes())?;
                let link_name = c_string(name.as_bytes())?;
                // SAFETY: both strings are NUL-terminated and `parent` is an open directory.
                if unsafe {
                    libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), link_name.as_ptr())
                } != 0
                {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("create symlink {}", relative.display()));
                }
                if let Some((uid, gid)) = owner {
                    // SAFETY: as above; AT_SYMLINK_NOFOLLOW keeps the change on the link.
                    if unsafe {
                        libc::fchownat(
                            parent.as_raw_fd(),
                            link_name.as_ptr(),
                            uid,
                            gid,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } != 0
                    {
                        return Err(std::io::Error::last_os_error())
                            .with_context(|| format!("change owner of {}", relative.display()));
                    }
                }
                None
            }
            other => bail!(
                "unsupported tar entry type {other:?} for {}",
                relative.display()
            ),
        };
        if let Some(fd) = created {
            let raw = fd.as_raw_fd();
            // The creation mode is filtered by the umask; set it exactly.
            // SAFETY: `raw` is an open descriptor owned by `fd`.
            if unsafe { libc::fchmod(raw, mode as libc::mode_t) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("set mode of {}", relative.display()));
            }
            if let Some((uid, gid)) = owner {
                // SAFETY: as above.
                if unsafe { libc::fchown(raw, uid, gid) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("change owner of {}", relative.display()));
                }
            }
            if let Some(seconds) = mtime {
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
                // SAFETY: `times` holds two timespecs and `raw` is open.
                if unsafe { libc::futimens(raw, times.as_ptr()) } != 0 {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!("set modification time of {}", relative.display())
                    });
                }
            }
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

fn c_string(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).context("a path contains a NUL byte")
}

/// `openat` that reports `errno`.
fn open_path(
    dir: libc::c_int,
    path: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<OwnedFd> {
    let path = c_string(path.as_os_str().as_bytes())?;
    // SAFETY: `path` is NUL-terminated; `dir` is AT_FDCWD or an open directory.
    let fd = unsafe { libc::openat(dir, path.as_ptr(), flags, libc::c_uint::from(mode)) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The file type bits of `name` in `dir`, or `None` if it does not exist; a symlink is not followed.
fn stat_at(dir: &OwnedFd, name: &std::ffi::OsStr) -> Result<Option<libc::mode_t>> {
    let name = c_string(name.as_bytes())?;
    // SAFETY: `stat` is plain old data filled by the call; `name` is NUL-terminated.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above, `dir` is an open directory.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Ok(Some(stat.st_mode & libc::S_IFMT));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(None);
    }
    Err(error).context("stat a tar destination")
}

fn unlink_at(dir: &OwnedFd, name: &std::ffi::OsStr, flags: libc::c_int) -> Result<()> {
    let name = c_string(name.as_bytes())?;
    // SAFETY: `name` is NUL-terminated and `dir` is an open directory.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error()).context("replace a tar destination");
    }
    Ok(())
}

fn make_dir_at(dir: &OwnedFd, name: &std::ffi::OsStr) -> Result<()> {
    let name = c_string(name.as_bytes())?;
    // SAFETY: `name` is NUL-terminated and `dir` is an open directory.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error()).context("create a tar directory");
    }
    Ok(())
}

/// Open (creating missing ones) the directories of `relative` below `root`,
/// one component at a time, each relative to the previous descriptor and
/// refusing symlinks.
fn open_parents(root: &OwnedFd, relative: &Path) -> Result<OwnedFd> {
    let mut current = root
        .try_clone()
        .context("duplicate the destination descriptor")?;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let opened = open_path(current.as_raw_fd(), Path::new(part), flags, 0);
        let next = match opened {
            Ok(fd) => fd,
            Err(error) if is_errno(&error, libc::ENOENT) => {
                make_dir_at(&current, part)?;
                open_path(current.as_raw_fd(), Path::new(part), flags, 0)?
            }
            Err(error) if is_errno(&error, libc::ELOOP) || is_errno(&error, libc::ENOTDIR) => {
                bail!(
                    "refusing to unpack through {}: it is not a directory",
                    relative.display()
                )
            }
            Err(error) => return Err(error),
        };
        current = next;
    }
    Ok(current)
}

fn is_errno(error: &anyhow::Error, errno: libc::c_int) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        == Some(errno)
}

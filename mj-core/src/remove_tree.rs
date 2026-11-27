//! Recursive directory removal that snapshots each directory before unlinking.
//!
//! `std::fs::remove_dir_all` walks a directory with a streaming `readdir` and
//! unlinks entries as it goes. On fuse-overlayfs, unlinking an entry can shift
//! the offsets of the entries behind it; the walk then skips part of the tail,
//! and the final `rmdir` fails with `ENOTEMPTY` even though the tree was
//! quiescent (#1712). Collecting every name in a directory before removing any
//! of them keeps the walk independent of that mutation.
//!
//! On Unix the walk is anchored to open directory handles and uses no-follow
//! opens, so a directory swapped for a symlink while it runs cannot redirect
//! deletion into the link target. It never lists a parent directory to remove a
//! known child, so a writable, searchable parent that is not readable still
//! works, as it did with `std::fs::remove_dir_all`. Windows delegates to the
//! standard remover, which already handles directory links, read-only files,
//! and open handles natively. Removal is best effort by design: a directory
//! that still has entries after the pass is reported as
//! [`RemoveOutcome::Leftovers`] and kept for the next sweep, never treated as a
//! reason to fail a committed operation.

#[cfg(unix)]
use cap_fs_ext::DirExt;
#[cfg(unix)]
pub use cap_std::fs::Dir;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// What one recursive removal pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The path and everything below it is gone, or was already absent.
    Removed,
    /// At least one entry below this path survived, so the path remains.
    Leftovers { path: PathBuf },
}

impl RemoveOutcome {
    pub fn is_removed(&self) -> bool {
        matches!(self, Self::Removed)
    }

    pub fn is_leftovers(&self) -> bool {
        matches!(self, Self::Leftovers { .. })
    }
}

/// Remove `path` without relying on a streaming `readdir`.
///
/// Entries are collected per directory before any of them is removed, so a
/// filesystem whose directory offsets shift during deletion cannot make the
/// walk skip an entry it has not seen. A missing path is already removed; a
/// symlink is unlinked rather than followed; any other non-directory is
/// refused with `NotADirectory`, matching `std::fs::remove_dir_all`.
#[cfg(unix)]
pub fn remove_dir_all(path: &Path) -> io::Result<RemoveOutcome> {
    remove_dir_all_with(path, &mut collect_entries)
}

/// Windows keeps the standard library's removal semantics: it handles
/// directory links, read-only files, and open handles natively. The
/// fuse-overlayfs `readdir` shift this module exists for is a Unix filesystem
/// behavior.
#[cfg(windows)]
pub fn remove_dir_all(path: &Path) -> io::Result<RemoveOutcome> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoveOutcome::Removed);
        }
        Ok(metadata) if metadata.is_dir() || metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(not_a_directory(path)),
        Err(error) => return Err(error),
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(RemoveOutcome::Removed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RemoveOutcome::Removed),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
            Ok(RemoveOutcome::Leftovers {
                path: path.to_owned(),
            })
        }
        Err(error) => Err(error),
    }
}

/// [`remove_dir_all`], but a leftover tree is an error.
///
/// Use this where the tree must be gone before the caller continues, such as a
/// staging directory that is about to be replaced. Cleanup and garbage
/// collection use [`remove_dir_all`] and leave leftovers for the next sweep.
pub fn remove_dir_all_checked(path: &Path) -> io::Result<()> {
    match remove_dir_all(path)? {
        RemoveOutcome::Removed => Ok(()),
        RemoveOutcome::Leftovers { path } => Err(io::Error::new(
            io::ErrorKind::DirectoryNotEmpty,
            format!("{} still has entries after removal", path.display()),
        )),
    }
}

#[cfg(all(test, unix))]
fn remove_dir_all_checked_with(
    path: &Path,
    list: &mut impl FnMut(&Dir) -> io::Result<Vec<SnapshotEntry>>,
) -> io::Result<()> {
    match remove_dir_all_with(path, list)? {
        RemoveOutcome::Removed => Ok(()),
        RemoveOutcome::Leftovers { path } => Err(io::Error::new(
            io::ErrorKind::DirectoryNotEmpty,
            format!("{} still has entries after removal", path.display()),
        )),
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    Symlink,
    File,
    Unknown,
}

#[cfg(unix)]
struct SnapshotEntry {
    name: OsString,
    kind: EntryKind,
}

#[cfg(unix)]
fn collect_entries(directory: &Dir) -> io::Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    for entry in directory.entries()? {
        let entry = entry?;
        let kind = match entry.file_type() {
            Ok(kind) if kind.is_dir() => EntryKind::Directory,
            Ok(kind) if kind.is_symlink() => EntryKind::Symlink,
            Ok(kind) if kind.is_file() => EntryKind::File,
            // FIFOs, sockets, and filesystems that report an unknown type are
            // resolved with one metadata call when they are removed.
            Ok(_) | Err(_) => EntryKind::Unknown,
        };
        entries.push(SnapshotEntry {
            name: entry.file_name(),
            kind,
        });
    }
    Ok(entries)
}

#[cfg(unix)]
fn remove_dir_all_with(
    path: &Path,
    list: &mut impl FnMut(&Dir) -> io::Result<Vec<SnapshotEntry>>,
) -> io::Result<RemoveOutcome> {
    let directory = match open_root_dir(path)? {
        RootEntry::Missing => return Ok(RemoveOutcome::Removed),
        RootEntry::Leftovers => {
            return Ok(RemoveOutcome::Leftovers {
                path: path.to_owned(),
            });
        }
        RootEntry::Directory(directory) => directory,
    };
    match remove_open_dir_contents(&directory, path, list) {
        Ok(RemoveOutcome::Removed) => {
            // Removing by path, not through the handle, needs only write and
            // search permission on the parent. The opened handle is not used
            // for the final rmdir, which would make cap-std list the parent.
            drop(directory);
            remove_root_path(path)
        }
        Ok(leftovers) => Ok(leftovers),
        // The directory may be empty but not searchable. The standard remover
        // could still unlink it from a writable, searchable parent; try the
        // same, and leave any non-empty tree for the next sweep.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            drop(directory);
            match remove_root_path(path) {
                // The directory was empty and only its listing was refused.
                Ok(RemoveOutcome::Removed) => Ok(RemoveOutcome::Removed),
                // The tree is still non-empty; keep the permission error so a
                // caller can make a read-only tree writable and retry.
                Ok(RemoveOutcome::Leftovers { .. }) => Err(error),
                Err(root_error) => Err(root_error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Remove a file, symlink, or directory `name` below an already-open `parent`.
/// Unlike [`remove_dir_all_in`], a non-directory entry is unlinked instead of
/// being refused.
#[cfg(unix)]
pub fn remove_entry_in(parent: &Dir, name: &OsStr) -> io::Result<RemoveOutcome> {
    let metadata = match parent.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoveOutcome::Removed);
        }
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        remove_dir_all_in(parent, name)
    } else {
        remove_entry_file(parent, name)?;
        removed_child_or_leftover(parent, name, Path::new(name))
    }
}

/// Remove `name` below an already-open `parent`, anchoring every step to that
/// handle. This is for a background cleaner that must not re-resolve the
/// parent's path after the caller validated it.
#[cfg(unix)]
pub fn remove_dir_all_in(parent: &Dir, name: &OsStr) -> io::Result<RemoveOutcome> {
    remove_dir_all_in_with(parent, name, &mut collect_entries)
}

#[cfg(unix)]
fn remove_dir_all_in_with(
    parent: &Dir,
    name: &OsStr,
    list: &mut impl FnMut(&Dir) -> io::Result<Vec<SnapshotEntry>>,
) -> io::Result<RemoveOutcome> {
    let metadata = match parent.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoveOutcome::Removed);
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        remove_entry_file(parent, name)?;
        return removed_child_or_leftover(parent, name, name.as_ref());
    }
    if !metadata.is_dir() {
        return Err(not_a_directory(Path::new(name)));
    }
    let directory = match parent.open_dir_nofollow(name) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoveOutcome::Removed);
        }
        Err(error) if is_not_directory(&error) => {
            remove_entry_file(parent, name)?;
            return Ok(RemoveOutcome::Removed);
        }
        Err(error) => return Err(error),
    };
    let display = Path::new(name);
    match remove_open_dir_contents(&directory, display, list) {
        Ok(RemoveOutcome::Removed) => {
            drop(directory);
            remove_child_dir(parent, name, display)
        }
        Ok(leftovers) => Ok(leftovers),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            drop(directory);
            match remove_child_dir(parent, name, display) {
                Ok(RemoveOutcome::Removed) => Ok(RemoveOutcome::Removed),
                Ok(RemoveOutcome::Leftovers { .. }) => Err(error),
                Err(root_error) => Err(root_error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn remove_child_dir(parent: &Dir, name: &OsStr, display: &Path) -> io::Result<RemoveOutcome> {
    match parent.remove_dir(name) {
        Ok(()) => removed_child_or_leftover(parent, name, display),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            removed_child_or_leftover(parent, name, display)
        }
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
            Ok(RemoveOutcome::Leftovers {
                path: display.to_owned(),
            })
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn removed_child_or_leftover(
    parent: &Dir,
    name: &OsStr,
    display: &Path,
) -> io::Result<RemoveOutcome> {
    match parent.symlink_metadata(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RemoveOutcome::Removed),
        Ok(_) => Ok(RemoveOutcome::Leftovers {
            path: display.to_owned(),
        }),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
enum RootEntry {
    Directory(Dir),
    Missing,
    Leftovers,
}

/// Open the tree root directly, without following a symlink in its last
/// component and without opening (or listing) its parent.
#[cfg(unix)]
fn open_root_dir(path: &Path) -> io::Result<RootEntry> {
    // Classify without opening: opening a FIFO or socket for reading would
    // block, and they are files to unlink rather than trees to walk.
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RootEntry::Missing),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            remove_path_file(path)?;
            return Ok(match removed_or_leftover(path)? {
                RemoveOutcome::Removed => RootEntry::Missing,
                RemoveOutcome::Leftovers { .. } => RootEntry::Leftovers,
            });
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(not_a_directory(path)),
        Err(error) => return Err(error),
    }
    match open_root_file(path) {
        Ok(file) => {
            let metadata = file.metadata()?;
            if !metadata.is_dir() {
                drop(file);
                return Err(not_a_directory(path));
            }
            Ok(RootEntry::Directory(Dir::from_std_file(file)))
        }
        Err(error) => match std::fs::symlink_metadata(path) {
            Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                Ok(RootEntry::Missing)
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                remove_path_file(path)?;
                Ok(match removed_or_leftover(path)? {
                    RemoveOutcome::Removed => RootEntry::Missing,
                    RemoveOutcome::Leftovers { .. } => RootEntry::Leftovers,
                })
            }
            Ok(metadata) if !metadata.is_dir() => Err(not_a_directory(path)),
            // A real directory that could not be opened (permissions, a busy
            // mount) is a real failure, not "already removed".
            Ok(_) => Err(error),
            Err(metadata_error) => Err(metadata_error),
        },
    }
}

/// Open the tree root as a directory without following a symlink.
///
/// Unix requires a directory in the open itself (`O_DIRECTORY`): a FIFO
/// swapped in after the metadata check is then rejected instead of blocking a
/// read-only open forever.
#[cfg(unix)]
fn open_root_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    options.open(path)
}

/// Open `path` as a directory without following a symlink in its last
/// component. The returned handle can anchor later removals to this directory.
#[cfg(unix)]
pub fn open_dir_nofollow(path: &Path) -> io::Result<Dir> {
    open_root_file(path).map(Dir::from_std_file)
}

fn not_a_directory(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotADirectory,
        format!("{} is not a directory", path.display()),
    )
}

#[cfg(unix)]
fn remove_path_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn remove_root_path(path: &Path) -> io::Result<RemoveOutcome> {
    match std::fs::remove_dir(path) {
        // A replacement may already occupy the name again.
        Ok(()) => removed_or_leftover(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Ok(_) => Ok(RemoveOutcome::Leftovers {
                    path: path.to_owned(),
                }),
                Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                    Ok(RemoveOutcome::Removed)
                }
                Err(_) => Err(error),
            }
        }
        // Entries appeared after our snapshot, or the name was replaced by a
        // non-directory: leave the path for the sweep.
        Err(error)
            if error.kind() == io::ErrorKind::DirectoryNotEmpty
                || error.kind() == io::ErrorKind::NotADirectory =>
        {
            match std::fs::symlink_metadata(path) {
                Ok(_) => Ok(RemoveOutcome::Leftovers {
                    path: path.to_owned(),
                }),
                Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                    Ok(RemoveOutcome::Removed)
                }
                Err(_) => Err(error),
            }
        }
        // The caller emptied the tree but could not unlink it (for example a
        // parent without write permission). Preserve the real failure instead
        // of turning it into a "leftovers" result.
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn removed_or_leftover(path: &Path) -> io::Result<RemoveOutcome> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RemoveOutcome::Removed),
        Ok(_) => Ok(RemoveOutcome::Leftovers {
            path: path.to_owned(),
        }),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn remove_open_dir_contents(
    directory: &Dir,
    root: &Path,
    list: &mut impl FnMut(&Dir) -> io::Result<Vec<SnapshotEntry>>,
) -> io::Result<RemoveOutcome> {
    // The whole list, including the types reported by `readdir`, is taken
    // before the first removal. This keeps a shifting offset from hiding an
    // entry and avoids a metadata syscall for the common case.
    let entries = match list(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoveOutcome::Removed);
        }
        Err(error) => return Err(error),
    };
    let mut leftovers = false;
    for SnapshotEntry { name, kind } in entries {
        match kind {
            EntryKind::Directory => {
                if remove_directory_entry(directory, root, &name, list)?.is_leftovers() {
                    leftovers = true;
                }
            }
            EntryKind::Symlink | EntryKind::File => {
                if remove_entry_or_leftover(directory, &name)? {
                    leftovers = true;
                }
            }
            EntryKind::Unknown => {
                let metadata = match directory.symlink_metadata(&name) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    if remove_directory_entry(directory, root, &name, list)?.is_leftovers() {
                        leftovers = true;
                    }
                } else if remove_entry_or_leftover(directory, &name)? {
                    leftovers = true;
                }
            }
        }
    }
    // A leftover subtree must not stop the rest of this directory from being
    // cleaned up. The parent cannot be removed while it remains, so report it
    // after visiting every sibling.
    if leftovers {
        Ok(RemoveOutcome::Leftovers {
            path: root.to_owned(),
        })
    } else {
        Ok(RemoveOutcome::Removed)
    }
}

#[cfg(unix)]
fn remove_directory_entry(
    directory: &Dir,
    root: &Path,
    name: &OsStr,
    list: &mut impl FnMut(&Dir) -> io::Result<Vec<SnapshotEntry>>,
) -> io::Result<RemoveOutcome> {
    let child = match directory.open_dir_nofollow(name) {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoveOutcome::Removed);
        }
        // A directory that changed into a file or symlink after the snapshot
        // is unlinked; the no-follow open never enters the link target.
        Err(error) if is_not_directory(&error) => {
            if remove_entry_or_leftover(directory, name)? {
                return Ok(RemoveOutcome::Leftovers {
                    path: root.to_owned(),
                });
            }
            return Ok(RemoveOutcome::Removed);
        }
        Err(error) => return Err(error),
    };
    match remove_open_dir_contents(&child, root, list) {
        Ok(RemoveOutcome::Removed) => {}
        Ok(leftovers) => return Ok(leftovers),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // The child may be empty but not searchable; the parent handle can
            // still unlink it by name.
            drop(child);
            return match directory.remove_dir(name) {
                Ok(()) => Ok(RemoveOutcome::Removed),
                Err(remove_dir_error) if remove_dir_error.kind() == io::ErrorKind::NotFound => {
                    Ok(RemoveOutcome::Removed)
                }
                // Still non-empty: preserve the permission error so cleanup can
                // retry after making a read-only tree writable.
                Err(remove_dir_error)
                    if remove_dir_error.kind() == io::ErrorKind::DirectoryNotEmpty =>
                {
                    Err(error)
                }
                Err(_) => Err(error),
            };
        }
        Err(error) => return Err(error),
    }
    // Windows directory handles are opened without FILE_SHARE_DELETE; close the
    // handle before unlinking the directory it names. On Unix it is harmless.
    drop(child);
    match directory.remove_dir(name) {
        Ok(()) => Ok(RemoveOutcome::Removed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RemoveOutcome::Removed),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
            Ok(RemoveOutcome::Leftovers {
                path: root.to_owned(),
            })
        }
        Err(error) => Err(error),
    }
}

/// Remove a non-directory entry, or report it as a leftover if it became a
/// directory after the snapshot. A cached `File`/`Symlink` type can race with a
/// writer that replaces it; that must not abort cleanup of later siblings.
#[cfg(unix)]
fn remove_entry_or_leftover(directory: &Dir, name: &OsStr) -> io::Result<bool> {
    match remove_entry_file(directory, name) {
        Ok(()) => Ok(false),
        Err(error) => match directory.symlink_metadata(name) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
            Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => Ok(false),
            _ => Err(error),
        },
    }
}

#[cfg(unix)]
fn remove_entry_file(directory: &Dir, name: &OsStr) -> io::Result<()> {
    match directory.remove_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn is_not_directory(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::NotADirectory {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::cell::{Cell, RefCell};

    // Hard-won: #1712: fuse-overlayfs shifted readdir offsets when the same
    // walk unlinked entries, so remove_dir_all skipped the tail and its final
    // rmdir returned ENOTEMPTY.
    #[cfg(unix)]
    #[test]
    fn removal_snapshots_each_directory_once_before_unlinking_any_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        for index in 0..32 {
            std::fs::write(root.join(format!("note-{index}.md")), "x").unwrap();
        }
        std::fs::write(root.join("nested/child.md"), "x").unwrap();
        let calls = RefCell::new(0_usize);
        let mut list = |directory: &Dir| {
            *calls.borrow_mut() += 1;
            let mut names = collect_entries(directory)?;
            // Emulate fuse-overlayfs re-listing a mutated directory: entries
            // behind an unlinked one move down and are no longer returned.
            if *calls.borrow() > 2 {
                names.truncate(names.len() / 2);
            }
            Ok(names)
        };
        assert_eq!(
            remove_dir_all_with(&root, &mut list).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!root.exists());
        assert_eq!(
            *calls.borrow(),
            2,
            "each directory must be listed exactly once, before its entries are removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_created_after_the_snapshot_is_left_for_the_next_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("old.md"), "x").unwrap();
        let writer_root = root.clone();
        let mut list = move |directory: &Dir| {
            let names = collect_entries(directory)?;
            // A writer recreates an entry after this directory was listed.
            std::fs::write(writer_root.join("late.md"), "x").unwrap();
            Ok(names)
        };
        assert!(matches!(
            remove_dir_all_with(&root, &mut list).unwrap(),
            RemoveOutcome::Leftovers { .. }
        ));
        assert!(root.join("late.md").exists());
        assert_eq!(remove_dir_all(&root).unwrap(), RemoveOutcome::Removed);
        assert!(!root.exists());
    }

    #[cfg(unix)]
    // Hard-won: a leftover subtree must not stop cleanup of later siblings.
    #[test]
    fn a_file_that_becomes_a_directory_is_left_for_the_next_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("changing"), "x").unwrap();
        let sibling = root.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("note.md"), "x").unwrap();
        let mut list = |directory: &Dir| {
            let entries = collect_entries(directory)?;
            if directory.symlink_metadata("changing").is_ok() {
                // Replace the snapshotted file with a directory.
                directory.remove_file("changing")?;
                directory.create_dir("changing")?;
                directory.write("changing/late.md", b"x")?;
            }
            Ok(entries)
        };
        assert!(matches!(
            remove_dir_all_with(&root, &mut list).unwrap(),
            RemoveOutcome::Leftovers { .. }
        ));
        assert!(root.join("changing/late.md").exists());
        assert!(!sibling.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_leftover_subtree_does_not_stop_sibling_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        let leftover = root.join("leftover");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("marker"), "x").unwrap();
        let sibling_a = root.join("sibling-a");
        let sibling_b = root.join("sibling-b");
        std::fs::create_dir_all(&sibling_a).unwrap();
        std::fs::create_dir_all(&sibling_b).unwrap();
        std::fs::write(sibling_a.join("note.md"), "x").unwrap();
        std::fs::write(sibling_b.join("note.md"), "x").unwrap();
        let mut list = |directory: &Dir| {
            let entries = collect_entries(directory)?;
            if directory.symlink_metadata("marker").is_ok() {
                // Recreate an entry after this directory was snapshotted.
                directory.write("late.md", b"x")?;
            }
            Ok(entries)
        };
        assert!(matches!(
            remove_dir_all_with(&root, &mut list).unwrap(),
            RemoveOutcome::Leftovers { .. }
        ));
        assert!(leftover.join("late.md").exists());
        assert!(!sibling_a.exists());
        assert!(!sibling_b.exists());
    }

    #[cfg(unix)]
    #[test]
    fn checked_removal_reports_a_leftover_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        let writer_root = root.clone();
        let mut list = move |directory: &Dir| {
            let names = collect_entries(directory)?;
            std::fs::write(writer_root.join("late.md"), "x").unwrap();
            Ok(names)
        };
        let error = remove_dir_all_checked_with(&root, &mut list).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::DirectoryNotEmpty);
        assert!(root.join("late.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_stray_file_is_removed_through_a_directory_handle() {
        let dir = tempfile::tempdir().unwrap();
        let parent = open_dir_nofollow(dir.path()).unwrap();
        std::fs::write(dir.path().join("stray"), "x").unwrap();
        assert_eq!(
            remove_entry_in(&parent, OsStr::new("stray")).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!dir.path().join("stray").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_child_removed_through_a_directory_handle_stays_confined() {
        let dir = tempfile::tempdir().unwrap();
        let parent_path = dir.path().join("candidates");
        std::fs::create_dir_all(&parent_path).unwrap();
        let parent = open_dir_nofollow(&parent_path).unwrap();
        std::fs::create_dir_all(parent_path.join("candidate")).unwrap();
        std::fs::write(parent_path.join("candidate/note.md"), "x").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.md"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, parent_path.join("link")).unwrap();
        assert_eq!(
            remove_dir_all_in(&parent, OsStr::new("candidate")).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!parent_path.join("candidate").exists());
        assert_eq!(
            remove_dir_all_in(&parent, OsStr::new("link")).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(outside.join("keep.md").exists());
    }

    #[test]
    fn a_regular_file_at_the_root_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let error = remove_dir_all(&file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert!(file.is_file());
    }

    #[test]
    fn a_missing_path_is_already_removed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            remove_dir_all(&dir.path().join("missing")).unwrap(),
            RemoveOutcome::Removed
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_unlinked_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.md"), "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(remove_dir_all(&link).unwrap(), RemoveOutcome::Removed);
        assert!(target.join("keep.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_symlink_inside_the_tree_is_unlinked_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.md"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
        assert_eq!(remove_dir_all(&tree).unwrap(), RemoveOutcome::Removed);
        assert!(outside.join("keep.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_root_swapped_for_a_symlink_during_the_walk_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.md"), "x").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.md"), "x").unwrap();
        let moved = dir.path().join("moved");
        let swapped = Cell::new(false);
        let mut list = |directory: &Dir| {
            let names = collect_entries(directory)?;
            if !swapped.replace(true) {
                // The walk has already opened this directory; replace its name
                // with a symlink before it removes the children.
                std::fs::rename(&root, &moved).unwrap();
                std::os::unix::fs::symlink(&outside, &root).unwrap();
            }
            Ok(names)
        };
        assert!(matches!(
            remove_dir_all_with(&root, &mut list).unwrap(),
            RemoveOutcome::Leftovers { .. }
        ));
        assert!(outside.join("keep.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_directory_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid only reads the caller's effective user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("note.md"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let error = remove_dir_all_checked(&root).unwrap_err();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_directory_without_search_permission_is_removed() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid only reads the caller's effective user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o400)).unwrap();
        let outcome = remove_dir_all(&root);
        if root.exists() {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(outcome.unwrap(), RemoveOutcome::Removed);
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_unlinked_reports_the_real_error() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid only reads the caller's effective user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let root = parent.join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.md"), "x").unwrap();
        // Read and search, but not write: the tree can be emptied but not
        // unlinked from its parent.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
        let error = remove_dir_all_checked(&root).unwrap_err();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(windows)]
    #[test]
    fn nested_directories_and_read_only_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let read_only = root.join("nested/read-only.md");
        std::fs::write(&read_only, "x").unwrap();
        let mut permissions = std::fs::metadata(&read_only).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&read_only, permissions).unwrap();
        assert_eq!(remove_dir_all(&root).unwrap(), RemoveOutcome::Removed);
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn removal_does_not_need_to_list_the_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let root = parent.join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.md"), "x").unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300)).unwrap();
        let outcome = remove_dir_all(&root);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(outcome.unwrap(), RemoveOutcome::Removed);
        assert!(!root.exists());
    }
}

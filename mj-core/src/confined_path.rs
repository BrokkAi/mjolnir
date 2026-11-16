//! Paths below a directory Mjolnir owns, reached without following a link
//! that the directory's contents could have planted to send a read or a
//! change somewhere else.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};

/// Refuse `relative` below `root` when `root`, or any part of `relative`
/// under it, is a symbolic link. With `missing_allowed`, the check ends at the
/// first part that does not exist, since nothing below it exists either.
pub fn reject_symlinks(root: &Path, relative: &Path, missing_allowed: bool) -> Result<()> {
    if std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("{} is a symbolic link", root.display());
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "{} reaches {} through a symbolic link",
                    root.join(relative).display(),
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if missing_allowed && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

/// A directory opened without following a link, through which every later
/// read, write, and delete below it resolves one name at a time, never
/// following a link at any step. A path check followed by a separate
/// operation on the same path text can be redirected by renaming a checked
/// directory away and planting a link in its place (#1419); resolving every
/// operation from a handle that was itself opened without following a link
/// leaves nothing to swap between the check and the use.
pub struct ConfinedDir {
    dir: Dir,
    path: PathBuf,
}

/// What a name in a [`ConfinedDir`] holds, without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// A link or a special file; confined walks never enter or read it.
    Other,
}

/// Whether [`ConfinedDir::write_atomically`] may replace an existing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    Replace,
    CreateNew,
}

#[derive(Debug)]
pub enum PublishError {
    /// The name holds a link, a directory, or another non-regular file.
    NotARegularFile,
    /// [`Publish::CreateNew`] found the name already taken.
    AlreadyExists,
    Failed(anyhow::Error),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARegularFile => formatter.write_str("destination is not a regular file"),
            Self::AlreadyExists => formatter.write_str("file already exists"),
            Self::Failed(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<anyhow::Error> for PublishError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl ConfinedDir {
    /// Open `root`, refusing it when it is a link. Returns `None` when it does
    /// not exist. Directories above `root` are trusted as they are.
    pub fn open_root(root: &Path) -> Result<Option<Self>> {
        let (parent_path, name) = split_root(root)?;
        let parent = match Dir::open_ambient_dir(parent_path, ambient_authority()) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("open {}", parent_path.display()));
            }
        };
        match parent.open_dir_nofollow(name) {
            Ok(dir) => Ok(Some(Self {
                dir,
                path: root.to_path_buf(),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| no_follow_directory(root)),
        }
    }

    /// Open `root` as [`Self::open_root`] does, creating it and its missing
    /// parents first.
    pub fn create_root(root: &Path) -> Result<Self> {
        let (parent_path, name) = split_root(root)?;
        std::fs::create_dir_all(parent_path)
            .with_context(|| format!("create {}", parent_path.display()))?;
        let parent = Dir::open_ambient_dir(parent_path, ambient_authority())
            .with_context(|| format!("open {}", parent_path.display()))?;
        Ok(Self {
            dir: create_and_open(&parent, name, root)?,
            path: root.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open the directory at `relative` below this one. Returns `None` when a
    /// part of it does not exist; refuses a part that is a link.
    pub fn open_dir(&self, relative: &Path) -> Result<Option<Self>> {
        let mut current = self.try_clone()?;
        for name in normal_components(relative)? {
            let path = current.path.join(name);
            match current.dir.open_dir_nofollow(name) {
                Ok(dir) => current = Self { dir, path },
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).with_context(|| no_follow_directory(&path)),
            }
        }
        Ok(Some(current))
    }

    /// Open the directory at `relative` below this one, creating each missing
    /// part; refuses a part that is a link.
    pub fn create_dir_all(&self, relative: &Path) -> Result<Self> {
        let mut current = self.try_clone()?;
        for name in normal_components(relative)? {
            let path = current.path.join(name);
            let dir = create_and_open(&current.dir, name, &path)?;
            current = Self { dir, path };
        }
        Ok(current)
    }

    /// The names directly in this directory and what each holds.
    pub fn entries(&self) -> Result<Vec<(OsString, EntryKind)>> {
        let listing = || format!("list {}", self.path.display());
        let mut entries = Vec::new();
        for entry in self.dir.entries().with_context(listing)? {
            let entry = entry.with_context(listing)?;
            let name = entry.file_name();
            let file_type = entry
                .file_type()
                .with_context(|| format!("inspect {}", self.path.join(&name).display()))?;
            let kind = if file_type.is_symlink() {
                EntryKind::Other
            } else if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            entries.push((name, kind));
        }
        Ok(entries)
    }

    /// Read the file `name` in this directory. Returns `None` when it does not
    /// exist; refuses a link.
    pub fn read_to_string(&self, name: &OsStr) -> Result<Option<String>> {
        let path = self.path.join(name);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = match self.dir.open_with(name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open {} without following a link", path.display()));
            }
        };
        let mut content = String::new();
        file.into_std()
            .read_to_string(&mut content)
            .with_context(|| format!("read {}", path.display()))?;
        Ok(Some(content))
    }

    /// Publish `body` as the file `name` in this directory through a
    /// temporary file in this directory, so a reader sees the old file or the
    /// complete new one. A new file is readable by its owner only; a replaced
    /// file keeps its permissions. Refuses a `name` that is not a regular
    /// file, such as a link, rather than replacing it.
    pub fn write_atomically(
        &self,
        name: &OsStr,
        body: &[u8],
        publish: Publish,
    ) -> Result<(), PublishError> {
        let path = self.path.join(name);
        let existing = match self.dir.symlink_metadata(name) {
            Ok(metadata) if !metadata.is_file() => return Err(PublishError::NotARegularFile),
            Ok(_) if publish == Publish::CreateNew => return Err(PublishError::AlreadyExists),
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(PublishError::Failed(
                    anyhow::Error::new(error).context(format!("inspect {}", path.display())),
                ));
            }
        };
        let mut random = [0u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow!("generate temporary filename: {error}"))?;
        let temporary = OsString::from(format!(
            ".{}.{}.{:016x}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            u64::from_le_bytes(random)
        ));
        let result = (|| -> Result<(), PublishError> {
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            let mut file = self
                .dir
                .open_with(&temporary, &options)
                .with_context(|| format!("create a temporary file for {}", path.display()))?
                .into_std();
            match existing {
                Some(permissions) => {
                    let permissions = permissions
                        .into_std(&file)
                        .and_then(|permissions| file.set_permissions(permissions));
                    permissions
                        .with_context(|| format!("keep the permissions of {}", path.display()))?;
                }
                #[cfg(unix)]
                None => {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))
                        .with_context(|| format!("restrict {}", path.display()))?;
                }
                #[cfg(not(unix))]
                None => {}
            }
            file.write_all(body)
                .with_context(|| format!("write {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", path.display()))?;
            drop(file);
            match publish {
                // A rename replaces whatever entry holds `name` at that
                // moment, a link included, and never follows it.
                Publish::Replace => self
                    .dir
                    .rename(&temporary, &self.dir, name)
                    .with_context(|| format!("replace {}", path.display()))?,
                // A link fails when `name` exists, so a file that appeared
                // since the check above is never replaced.
                Publish::CreateNew => {
                    match self.dir.hard_link(&temporary, &self.dir, name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            return Err(PublishError::AlreadyExists);
                        }
                        Err(error) => {
                            return Err(anyhow::Error::new(error)
                                .context(format!("publish {}", path.display()))
                                .into());
                        }
                    }
                    self.dir.remove_file(&temporary).with_context(|| {
                        format!("remove the temporary file for {}", path.display())
                    })?;
                }
            }
            self.sync()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.dir.remove_file(&temporary);
        }
        result
    }

    /// Remove the file or link `name` in this directory; a link is removed
    /// itself, never what it points to.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        self.dir.remove_file(name)
    }

    /// Remove the empty directory `name` in this directory.
    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        self.dir.remove_dir(name)
    }

    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            dir: self
                .dir
                .try_clone()
                .with_context(|| format!("duplicate the handle of {}", self.path.display()))?,
            path: self.path.clone(),
        })
    }

    /// Make a rename or new entry in this directory durable. Directory fsync
    /// is only available on Unix, as in `config::sync_directory`.
    fn sync(&self) -> Result<()> {
        // The handle itself may be an `O_PATH` descriptor, which cannot be
        // synced, so sync a readable handle on the same directory.
        #[cfg(unix)]
        self.dir
            .open(".")
            .and_then(|directory| directory.into_std().sync_all())
            .with_context(|| format!("sync directory {}", self.path.display()))?;
        Ok(())
    }
}

fn split_root(root: &Path) -> Result<(&Path, &OsStr)> {
    let name = root
        .file_name()
        .with_context(|| format!("{} does not end in a directory name", root.display()))?;
    let parent = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((parent, name))
}

fn normal_components(relative: &Path) -> Result<Vec<&OsStr>> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => bail!(
                "{} is not a plain relative path below a confined directory",
                relative.display()
            ),
        })
        .collect()
}

fn create_and_open(parent: &Dir, name: &OsStr, path: &Path) -> Result<Dir> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    parent
        .open_dir_nofollow(name)
        .with_context(|| no_follow_directory(path))
}

fn no_follow_directory(path: &Path) -> String {
    format!(
        "open {} as a directory without following a link",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1419: a directory swapped for a link between the check and the write redirected the write.
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_link_after_opening_does_not_redirect_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let root_path = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(root_path.join("notes")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root_path.join("notes/old.md"), "old").unwrap();
        std::fs::write(outside.join("old.md"), "victim").unwrap();

        let root = ConfinedDir::open_root(&root_path).unwrap().unwrap();
        let notes = root.open_dir(Path::new("notes")).unwrap().unwrap();
        std::fs::rename(root_path.join("notes"), root_path.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, root_path.join("notes")).unwrap();

        notes
            .write_atomically(OsStr::new("new.md"), b"sync content", Publish::CreateNew)
            .unwrap();
        notes.remove_file(OsStr::new("old.md")).unwrap();

        assert_eq!(
            std::fs::read_to_string(root_path.join("moved/new.md")).unwrap(),
            "sync content"
        );
        assert!(!root_path.join("moved/old.md").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("old.md")).unwrap(),
            "victim"
        );
        assert!(!outside.join("new.md").exists());
        // A fresh walk refuses the planted link rather than following it.
        assert!(root.open_dir(Path::new("notes")).is_err());
    }
}

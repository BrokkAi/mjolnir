//! Paths below a directory Mjolnir owns, reached without following a link
//! that the directory's contents could have planted to send a read or a
//! change somewhere else.

use std::path::Path;

use anyhow::{Context, Result, bail};

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

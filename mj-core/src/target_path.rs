//! Paths on the machine a session runs on.
//!
//! Every session target is a POSIX machine: a Linux container, an SSH or EC2
//! host, or this machine when it is Unix (a Windows controller runs no local
//! sessions). The controller keeps those paths as `Path`/`PathBuf` like its
//! own, but they mean POSIX paths on every controller. On Windows,
//! `Path::is_absolute` requires a drive or UNC prefix and `Path::join` inserts
//! `\`, so neither may decide or render a target path; these functions do.

use std::path::{Component, Path, PathBuf};

/// Whether `path` starts at the POSIX root of its target. On Unix this is
/// `Path::is_absolute`.
pub fn is_absolute(path: &Path) -> bool {
    matches!(path.components().next(), Some(Component::RootDir))
}

/// Whether `path` is absolute on this controller or on a session target. For
/// records and drafts that may name either side, such as a mount source on a
/// local or an SSH container host; the side that uses the path checks it again.
pub fn is_absolute_on_host_or_target(path: &Path) -> bool {
    path.is_absolute() || is_absolute(path)
}

/// Join `relative` onto a target path `base`. The result keeps POSIX
/// separators on a Windows controller too, so it reads the same wherever it
/// is shown or sent. Like [`Path::join`], an absolute `relative`, by either
/// the target's or the controller's spelling, replaces `base` instead of
/// appending to it.
///
/// `base` is a target path by contract: POSIX-absolute, or home-relative as
/// SSH and EC2 paths are. A controller-native absolute base (a drive or UNC
/// path, which only names this machine) joins natively.
pub fn join(base: &Path, relative: &Path) -> PathBuf {
    if !cfg!(windows) {
        return base.join(relative);
    }
    if is_absolute(relative) {
        return PathBuf::from(text(relative));
    }
    if relative.is_absolute() {
        return relative.to_path_buf();
    }
    if !is_absolute(base) && base.is_absolute() {
        return base.join(relative);
    }
    let mut joined = text(base);
    for component in relative.components() {
        if !joined.is_empty() && !joined.ends_with('/') {
            joined.push('/');
        }
        joined.push_str(&component.as_os_str().to_string_lossy());
    }
    PathBuf::from(joined)
}

/// The POSIX text of a target path, for an argv element or a remote command.
pub fn text(path: &Path) -> String {
    let text = path.to_string_lossy();
    // Only Windows joins components with `\`; on POSIX a backslash is an
    // ordinary file name character and must survive.
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

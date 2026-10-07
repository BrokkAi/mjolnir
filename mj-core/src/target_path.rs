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

/// Join `relative` onto `base`. When `base` is a target path, the result keeps
/// POSIX separators on a Windows controller too, so it reads the same wherever
/// it is shown or sent.
pub fn join(base: &Path, relative: &Path) -> PathBuf {
    if !cfg!(windows) || !is_absolute(base) {
        return base.join(relative);
    }
    let mut joined = text(base);
    for component in relative.components() {
        if !joined.ends_with('/') {
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

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

/// Join `relative` onto `base`. When `base` is a target path, absolute or
/// relative to the target user's home (as SSH and EC2 worker roots are), the
/// result keeps POSIX separators on a Windows controller too, so it reads the
/// same wherever it is shown or sent. Only a base this controller would call
/// absolute, with a Windows drive or UNC prefix, is joined as a host path.
pub fn join(base: &Path, relative: &Path) -> PathBuf {
    if !cfg!(windows) {
        return base.join(relative);
    }
    if base.is_absolute() {
        // Pushed part by part: `relative` is often Git's `/`-separated text,
        // and a verbatim `\\?\` base would keep a `/` as a file name character.
        let mut joined = base.to_path_buf();
        for component in relative.components() {
            joined.push(component);
        }
        return joined;
    }
    // As with `Path::join`, an absolute `relative` replaces the base.
    if is_absolute(relative) {
        return PathBuf::from(text(relative));
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
/// A path with a Windows drive or UNC prefix is this controller's own, as in
/// [`join`], and keeps its spelling.
pub fn text(path: &Path) -> String {
    let text = path.to_string_lossy();
    // Only Windows joins components with `\`; on POSIX a backslash is an
    // ordinary file name character and must survive.
    if cfg!(windows) && !path.is_absolute() {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

// The POSIX behavior only differs from the standard library on Windows.
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    // Hard-won: #1238: home-relative SSH worker roots and remote clones were joined with `\` on Windows.
    #[test]
    fn target_paths_keep_posix_separators_on_a_windows_controller() {
        // The raw spelling, not `text`, which would hide a `\`.
        let join = |base: &str, relative: &str| {
            super::join(Path::new(base), Path::new(relative))
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            join(".local/share/hel/workers/s", "control.sock"),
            ".local/share/hel/workers/s/control.sock"
        );
        assert_eq!(
            join("/home/dev/project", r".mj\clones\s"),
            "/home/dev/project/.mj/clones/s"
        );
        assert_eq!(join("/home/dev/project", ""), "/home/dev/project");
        assert_eq!(join("", "memory"), "memory");
        assert_eq!(join("/workspace", "/home/dev"), "/home/dev");
        assert_eq!(join(r"C:\Users\dev", ".codex"), r"C:\Users\dev\.codex");
        assert_eq!(
            crate::local_git::resolve_git_path(Path::new("/home/dev/project"), ".git/info/exclude")
                .unwrap()
                .to_string_lossy(),
            "/home/dev/project/.git/info/exclude"
        );
    }
}

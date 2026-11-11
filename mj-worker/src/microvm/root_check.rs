//! Check a flattened image's root filesystem for what a retained-storage
//! upgrade needs, without booting it or running anything from it.
//!
//! The controller's Podman preflight runs `command -v tar`, `command -v flock`,
//! `mkdir -p /var/lib/hel && test -w /var/lib/hel` in a throwaway container of
//! the new image before it stops anything. A throwaway VM would be charged
//! against the memory pool and would need its own cleanup after a crash, so the
//! engine answers the same questions from the image's disk: the base `root.ext4`
//! is mounted read-only in a private mount namespace (it disappears with the
//! process, so no mount or loop device can outlive the check) and the
//! questions are answered by walking it with the symlink rules of a chroot.
//! Nothing in the image is executed.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::state::Layout;
use super::{image, sys};

/// The programs the control-state transfer runs in the guest.
const REQUIRED_PROGRAMS: [&str; 3] = ["sh", "tar", "flock"];
/// Where the worker keeps the state the transfer carries.
const CONTROL_ROOT: &str = "/var/lib/hel";
/// The kernel's limit on symlinks followed in one lookup.
const MAX_LINKS: usize = 40;

/// Refuse the replacement of a VM that runs as `user` with environment `path`
/// by the image `image_id` unless the transfer can run in it.
pub fn check_image(layout: &Layout, image_id: &str, user: &str, path: &str) -> Result<()> {
    ensure!(
        matches!(
            user.split(':').next().unwrap_or_default(),
            "" | "0" | "root"
        ),
        "the image check supports sandboxes that run as root, not {user:?}"
    );
    let disk = image::root_image(layout, image_id);
    ensure!(disk.is_file(), "image {image_id} has no root disk");
    let unshare = sys::find_tool("unshare")?;
    let mount = sys::find_tool("mount")?;
    let engine = std::env::current_exe().context("locate the engine binary")?;
    let directory = Path::new("/run/mj-microvm-check").join(sys::scratch_suffix());
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let script = r#"set -eu; "$1" -o loop,ro,noload,nodev,nosuid,noexec "$2" "$3"; exec "$4" microvm root-check-inner "$3" "$5""#;
    let output = sys::run_unchecked(
        &unshare,
        &[
            "--mount".as_ref(),
            "--propagation".as_ref(),
            "private".as_ref(),
            "--".as_ref(),
            "sh".as_ref(),
            "-c".as_ref(),
            script.as_ref(),
            "mj-root-check".as_ref(),
            mount.as_os_str(),
            disk.as_os_str(),
            directory.as_os_str(),
            engine.as_os_str(),
            path.as_ref(),
        ],
    );
    // The mount went with the namespace; only the empty mount point is left.
    let removed = fs::remove_dir(&directory);
    let output = output?;
    removed.with_context(|| format!("remove {}", directory.display()))?;
    ensure!(
        output.status.success(),
        "{}",
        sys::tail(&String::from_utf8_lossy(&output.stderr), 20)
    );
    Ok(())
}

/// The inner half of `check_image`, run inside the mount namespace with the
/// image mounted at `root`.
pub fn check_mounted(root: &Path, path: &str) -> Result<()> {
    let problems = problems(root, path);
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "the new image cannot carry the worker's state over: {}",
        problems.join("; ")
    )
}

fn problems(root: &Path, path: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for program in REQUIRED_PROGRAMS {
        if !path.split(':').any(|directory| {
            let directory = if directory.is_empty() { "." } else { directory };
            matches!(
                resolve(root, &format!("{directory}/{program}")),
                Ok(Some(file)) if is_executable_file(&file)
            )
        }) {
            problems.push(format!("`{program}` is not an executable on PATH ({path})"));
        }
    }
    if let Err(reason) = creatable_directory(root, CONTROL_ROOT) {
        problems.push(format!("{CONTROL_ROOT} cannot be created: {reason:#}"));
    }
    problems
}

fn is_executable_file(file: &Path) -> bool {
    fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `path` as a chroot at `root` would find it, following symlinks inside the
/// image only: `None` when it does not exist. An absolute link restarts at
/// `root` and `..` never leaves it.
fn resolve(root: &Path, path: &str) -> Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    let mut pending: VecDeque<String> = components(path);
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match part.as_str() {
            "." => {}
            ".." => {
                if current != root {
                    current.pop();
                }
            }
            name => {
                let next = current.join(name);
                let Ok(metadata) = fs::symlink_metadata(&next) else {
                    return Ok(None);
                };
                if metadata.file_type().is_symlink() {
                    links += 1;
                    ensure!(links <= MAX_LINKS, "too many levels of symbolic links");
                    let target = fs::read_link(&next)
                        .with_context(|| format!("read link {}", next.display()))?;
                    let target = target.to_string_lossy().into_owned();
                    if target.starts_with('/') {
                        current = root.to_path_buf();
                    }
                    for part in components(&target).into_iter().rev() {
                        pending.push_front(part);
                    }
                } else {
                    ensure!(
                        pending.is_empty() || metadata.is_dir(),
                        "{} is not a directory",
                        next.display()
                    );
                    current = next;
                }
            }
        }
    }
    Ok(Some(current))
}

fn components(path: &str) -> VecDeque<String> {
    Path::new(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_owned()),
            _ => None,
        })
        .collect()
}

/// What `mkdir -p path && test -w path` needs of the image for root: every
/// existing component is a directory (through links), and the rest can be
/// created.
fn creatable_directory(root: &Path, path: &str) -> Result<()> {
    let mut walked = String::new();
    for part in components(path) {
        walked.push('/');
        walked.push_str(&part);
        match resolve(root, &walked)? {
            Some(found) => ensure!(found.is_dir(), "{walked} is not a directory"),
            None => return Ok(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tree() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for directory in ["usr/bin", "usr/local/bin", "var/lib", "bin.real"] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        symlink("usr/bin", root.path().join("bin")).unwrap();
        let busybox = root.path().join("usr/bin/busybox");
        fs::write(&busybox, b"\x7fELF").unwrap();
        fs::set_permissions(&busybox, fs::Permissions::from_mode(0o755)).unwrap();
        for program in ["sh", "tar", "flock"] {
            // Absolute links resolve against the image, not the host.
            symlink("/bin/busybox", root.path().join("usr/bin").join(program)).unwrap();
        }
        root
    }

    const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

    #[test]
    fn busybox_style_image_with_symlinked_directories_passes() {
        let root = tree();
        assert_eq!(problems(root.path(), PATH), Vec::<String>::new());
    }

    // Security boundary: a hostile image's absolute link or `..` must not
    // reach the host, and a link loop must end.
    #[test]
    fn lookups_never_leave_the_image_and_link_loops_end() {
        let root = tree();
        fs::write(root.path().parent().unwrap().join("host-secret"), "x").unwrap();
        symlink(
            "/../../../../etc/passwd",
            root.path().join("usr/bin/escape"),
        )
        .unwrap();
        // The host's /etc/passwd exists, the image's does not.
        assert_eq!(resolve(root.path(), "/usr/bin/escape").unwrap(), None);
        symlink("../../../host-secret", root.path().join("usr/bin/dots")).unwrap();
        assert_eq!(resolve(root.path(), "/usr/bin/dots").unwrap(), None);
        symlink("loop-b", root.path().join("usr/bin/loop-a")).unwrap();
        symlink("loop-a", root.path().join("usr/bin/loop-b")).unwrap();
        assert!(resolve(root.path(), "/usr/bin/loop-a").is_err());
    }

    #[test]
    fn each_missing_requirement_is_named() {
        let root = tree();
        fs::remove_file(root.path().join("usr/bin/flock")).unwrap();
        let named = problems(root.path(), PATH);
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("`flock`"), "{named:?}");

        // A file that is not executable does not count, and a PATH that
        // misses the directory does not find it.
        fs::set_permissions(
            root.path().join("usr/bin/busybox"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(problems(root.path(), PATH).len(), 3);
        fs::set_permissions(
            root.path().join("usr/bin/busybox"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(problems(root.path(), "/opt/none").len(), 3);

        // `mkdir -p /var/lib/hel` cannot pass through a file.
        fs::remove_dir(root.path().join("var/lib")).unwrap();
        fs::write(root.path().join("var/lib"), "file").unwrap();
        let named = problems(root.path(), PATH);
        assert!(
            named
                .iter()
                .any(|p| p.contains("/var/lib/hel cannot be created")),
            "{named:?}"
        );
    }
}

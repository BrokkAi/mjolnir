//! Which executable file a running process is actually running.
//!
//! Two Mjolnir builds can carry the same version string, the same protocol
//! number and the same path, and still be different code: a rebuild replaces
//! the file while the processes started from the previous one keep running it.
//! Nothing in the daemon protocol can tell those apart, so the question is
//! answered from the operating system instead, and answered in one place
//! because the restart path, `mj daemon status` and `mj doctor` all ask it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A file, identified by what it is rather than by what it is called.
///
/// A running executable can be renamed (NFS moves an unlinked open file to a
/// `.nfs*` name) or replaced at its original path, so names prove nothing.
/// Device and inode distinguish two open files cheaply, and two names for one
/// inode — a hard link — correctly compare equal.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutableFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn executable_file_identity(path: &Path) -> std::io::Result<ExecutableFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path)?;
    Ok(ExecutableFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Whether process `pid` runs the same executable file as this process.
///
/// `Ok(None)` means the question could not be answered: the process is gone,
/// or this platform does not expose a process's executable.
#[cfg(target_os = "linux")]
pub fn process_runs_this_executable(pid: u32) -> Result<Option<bool>> {
    let current = executable_file_identity(Path::new("/proc/self/exe"))
        .context("inspect this process's executable")?;
    let process_path = PathBuf::from(format!("/proc/{pid}/exe"));
    let other = match executable_file_identity(&process_path) {
        Ok(identity) => identity,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect process {pid} executable via {}",
                    process_path.display()
                )
            });
        }
    };
    Ok(Some(current == other))
}

#[cfg(target_os = "macos")]
pub fn process_runs_this_executable(pid: u32) -> Result<Option<bool>> {
    let process_id = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[process_id]),
        true,
        sysinfo::ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always),
    );
    let Some(process) = system.process(process_id) else {
        return Ok(None);
    };
    let Some(path) = process.exe() else {
        return Ok(None);
    };
    let current = std::env::current_exe().context("find this process's executable")?;
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(false)),
        Err(error) => return Err(error).context("inspect process executable"),
    };
    // macOS reports a pathname, not Linux's reference to the running inode.
    // A newer file at that same pathname also means a different build.
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok(Some(
        executable_file_identity(&current)? == executable_file_identity(path)?
            && modified <= process.start_time(),
    ))
}

/// Platforms that do not expose a process's executable answer "unknown", so
/// every caller has one shape to handle.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_runs_this_executable(pid: u32) -> Result<Option<bool>> {
    let _ = pid;
    Ok(None)
}

/// The file a process is running, for a message a person reads.
///
/// Linux names an unlinked executable with a ` (deleted)` suffix. That suffix
/// is the fact the reader needs, so it is kept rather than trimmed.
pub fn process_executable_path(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        let process_id = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[process_id]),
            true,
            sysinfo::ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always),
        );
        system
            .process(process_id)
            .and_then(|process| process.exe())
            .map(Path::to_path_buf)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The file this process is running, named the same way.
pub fn running_executable_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link("/proc/self/exe").ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().ok()
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn executable_identity_detects_an_nfs_style_replaced_binary() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("mj");
        let hard_link = directory.path().join("mj-hard-link");
        let retained = directory.path().join(".nfs0000000000000001");

        std::fs::write(&current, b"old executable").unwrap();
        std::fs::hard_link(&current, &hard_link).unwrap();
        assert_eq!(
            executable_file_identity(&current).unwrap(),
            executable_file_identity(&hard_link).unwrap(),
            "two names for the same executable inode must compare equal"
        );

        std::fs::rename(&current, &retained).unwrap();
        std::fs::write(&current, b"new executable").unwrap();
        assert_ne!(
            executable_file_identity(&retained).unwrap(),
            executable_file_identity(&current).unwrap(),
            "an NFS-retained old executable must differ from its replacement"
        );
    }

    #[test]
    fn current_process_is_running_the_current_executable() {
        assert_eq!(
            process_runs_this_executable(std::process::id()).unwrap(),
            Some(true)
        );
        assert_eq!(process_runs_this_executable(u32::MAX).unwrap(), None);
    }

    #[test]
    fn a_live_process_reports_the_file_it_runs() {
        let own = process_executable_path(std::process::id()).unwrap();
        assert_eq!(Some(own), running_executable_path());
    }
}

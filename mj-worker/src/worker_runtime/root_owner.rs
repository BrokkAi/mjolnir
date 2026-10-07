//! Exclusive ownership of every mutable file in a worker root.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub struct WorkerRootOwner {
    root: PathBuf,
    _file: File,
}

impl WorkerRootOwner {
    pub fn acquire(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("create worker root {}", root.display()))?;
        let root = std::fs::canonicalize(root)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("worker.lock"))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!("a worker already owns {}", root.display());
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("lock worker root");
            }
        }
        // Legacy workers do not hold worker.lock. Never take their files away
        // during the first upgrade to a worker that enforces root ownership.
        #[cfg(unix)]
        if mj_core::local_sockets::connect_unix_stream(&root.join("control.sock")).is_ok() {
            bail!("a worker is already running at {}", root.display());
        }
        Ok(Self { root, _file: file })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The descriptor is inherited only by login re-exec, never by harnesses.
    #[cfg(unix)]
    pub fn set_reexec_inheritance(&self, inherit: bool) -> Result<i32> {
        use std::os::fd::AsRawFd;
        let fd = self._file.as_raw_fd();
        let flags = if inherit { 0 } else { libc::FD_CLOEXEC };
        // SAFETY: this owner keeps fd alive; F_SETFD only changes descriptor flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
            return Err(std::io::Error::last_os_error()).context("set worker lock inheritance");
        }
        Ok(fd)
    }

    /// Recover the descriptor carried by the worker's own login re-exec.
    #[cfg(unix)]
    pub fn from_reexec(root: &Path, fd: i32) -> Result<Self> {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(fd >= 3, "invalid worker root lock descriptor");
        // Check existence before constructing the descriptor's owning File.
        // SAFETY: F_GETFD inspects an integer descriptor without dereferencing it.
        anyhow::ensure!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
            "worker root lock descriptor was not inherited"
        );
        // SAFETY: the hidden CLI argument transfers this descriptor exactly once.
        let file = unsafe { File::from_raw_fd(fd) };
        let root = std::fs::canonicalize(root)?;
        let actual = file.metadata()?;
        let expected = std::fs::metadata(root.join("worker.lock"))?;
        anyhow::ensure!(
            actual.dev() == expected.dev() && actual.ino() == expected.ino(),
            "inherited worker lock belongs to another root"
        );
        file.try_lock()
            .context("retain inherited worker root ownership")?;
        let owner = Self { root, _file: file };
        owner.set_reexec_inheritance(false)?;
        Ok(owner)
    }

    /// Only the owner may replace the shared diagnostic log or startup records.
    #[cfg(unix)]
    pub fn prepare_startup(&self, reexec: bool) -> Result<()> {
        use std::os::fd::AsRawFd;
        if !reexec {
            for name in [super::WORKER_EXIT_FILE, super::WORKER_STARTUP_FILE] {
                match std::fs::remove_file(self.root.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("clear previous worker diagnostics"),
                }
            }
        }
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .append(reexec)
            .truncate(!reexec)
            .open(self.root.join("worker.log"))?;
        // SAFETY: dup2 installs the open log as stderr; log remains valid here.
        if unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            return Err(std::io::Error::last_os_error()).context("redirect owner worker log");
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn acquire_after_fork_exec_window(root: &Path) -> Result<WorkerRootOwner> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match WorkerRootOwner::acquire(root) {
            Ok(owner) => return Ok(owner),
            Err(error) if error.to_string().starts_with("a worker already owns ") => {
                if Instant::now() >= deadline {
                    return Err(error).context("worker root stayed busy after a fork/exec window");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) struct ForkedPreExecChild {
    control: std::os::unix::net::UnixStream,
    child: std::thread::JoinHandle<()>,
}

#[cfg(all(test, unix))]
impl ForkedPreExecChild {
    pub(crate) fn start() -> Self {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;

        let (parent, child_control) = UnixStream::pair().unwrap();
        let child = std::thread::spawn(move || {
            let child_fd = child_control.as_raw_fd();
            let mut command = std::process::Command::new("/bin/true");
            // Hold the forked copy of the lock until the parent allows exec.
            // SAFETY: the hook uses only async-signal-safe read/write syscalls.
            unsafe {
                command.pre_exec(move || {
                    let ready = b'r';
                    if libc::write(child_fd, (&ready as *const u8).cast(), 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut release = 0u8;
                    if libc::read(child_fd, (&mut release as *mut u8).cast(), 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let output = mj_core::subprocess::run_capturing_stdout(&mut command).unwrap();
            assert!(output.status.success());
        });
        let mut ready = [0u8; 1];
        (&parent).read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'r']);
        Self {
            control: parent,
            child,
        }
    }

    pub(crate) fn release_after(self, delay: std::time::Duration) -> std::thread::JoinHandle<()> {
        use std::io::Write;

        let Self { mut control, child } = self;
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            control.write_all(b"e").unwrap();
            child.join().unwrap();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1257: a forked pre-exec child keeps the flock after the Rust owner drops.
    #[test]
    fn root_is_exclusive_before_a_control_socket_exists_and_released_on_exit() {
        let root = tempfile::tempdir().unwrap();
        let owner = WorkerRootOwner::acquire(root.path()).unwrap();
        assert!(WorkerRootOwner::acquire(root.path()).is_err());
        let other = tempfile::tempdir().unwrap();
        let _independent = WorkerRootOwner::acquire(other.path()).unwrap();
        #[cfg(unix)]
        let acquired = {
            let child = ForkedPreExecChild::start();
            drop(owner);
            let release = child.release_after(std::time::Duration::from_millis(30));
            let acquired = acquire_after_fork_exec_window(root.path());
            release.join().unwrap();
            acquired
        };
        #[cfg(not(unix))]
        let acquired = {
            drop(owner);
            acquire_after_fork_exec_window(root.path())
        };
        assert!(acquired.is_ok());
    }

    #[test]
    fn a_refused_owner_cannot_modify_the_incumbents_files() {
        let root = tempfile::tempdir().unwrap();
        let _owner = WorkerRootOwner::acquire(root.path()).unwrap();
        for name in [
            "worker.log",
            "worker-startup.json",
            "worker-exit.json",
            "worker.pid",
        ] {
            std::fs::write(root.path().join(name), "incumbent").unwrap();
        }
        assert!(WorkerRootOwner::acquire(root.path()).is_err());
        for name in [
            "worker.log",
            "worker-startup.json",
            "worker-exit.json",
            "worker.pid",
        ] {
            assert_eq!(
                std::fs::read_to_string(root.path().join(name)).unwrap(),
                "incumbent"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    const CHILD: &str = "MJ_ROOT_OWNER_CHILD";

    #[test]
    fn worker_root_process_child() {
        let Some(root) = std::env::var_os(CHILD) else {
            return;
        };
        let root = PathBuf::from(root);
        match std::env::var("MJ_ROOT_OWNER_MODE").unwrap().as_str() {
            "contender" => {
                assert!(WorkerRootOwner::acquire(&root).is_err());
            }
            "reexec" => {
                let owner = WorkerRootOwner::acquire(&root).unwrap();
                let fd = owner.set_reexec_inheritance(true).unwrap();
                let error = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "worker_runtime::root_owner::process_tests::worker_root_process_child",
                        "--nocapture",
                    ])
                    .env("MJ_ROOT_OWNER_MODE", "inherited")
                    .env("MJ_ROOT_OWNER_FD", fd.to_string())
                    .exec();
                panic!("re-exec failed: {error}");
            }
            "exit-without-drop" => {
                let owner = WorkerRootOwner::acquire(&root).unwrap();
                std::fs::write(owner.root().join("unclean-exit"), "owned").unwrap();
                std::process::exit(0);
            }
            "inherited" => {
                let fd = std::env::var("MJ_ROOT_OWNER_FD").unwrap().parse().unwrap();
                let owner = WorkerRootOwner::from_reexec(&root, fd).unwrap();
                assert!(WorkerRootOwner::acquire(&root).is_err());
                // Harness subprocesses must not retain ownership after the
                // worker exits. The adopted descriptor is close-on-exec.
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                    libc::FD_CLOEXEC
                );
                std::fs::write(owner.root().join("inherited"), "owned").unwrap();
            }
            mode => panic!("unknown root owner child mode {mode}"),
        }
    }

    fn child(root: &Path, mode: &str) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "worker_runtime::root_owner::process_tests::worker_root_process_child",
                "--nocapture",
            ])
            .env(CHILD, root)
            .env("MJ_ROOT_OWNER_MODE", mode);
        let output = mj_core::subprocess::run_capturing_stdout(&mut command).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_separate_process_cannot_touch_the_incumbent_before_socket_bind() {
        let root = tempfile::tempdir().unwrap();
        let _owner = WorkerRootOwner::acquire(root.path()).unwrap();
        let journal = root.path().join("relay-events.jsonl");
        std::fs::write(&journal, "incumbent journal").unwrap();
        child(root.path(), "contender");
        assert_eq!(
            std::fs::read_to_string(journal).unwrap(),
            "incumbent journal"
        );
    }

    #[test]
    fn process_exit_releases_ownership_without_rust_cleanup() {
        let root = tempfile::tempdir().unwrap();
        child(root.path(), "exit-without-drop");
        assert!(root.path().join("unclean-exit").exists());
        assert!(WorkerRootOwner::acquire(root.path()).is_ok());
    }

    #[test]
    fn root_ownership_survives_reexec_and_is_released_when_the_process_exits() {
        let root = tempfile::tempdir().unwrap();
        child(root.path(), "reexec");
        assert_eq!(
            std::fs::read_to_string(root.path().join("inherited")).unwrap(),
            "owned"
        );
        assert!(WorkerRootOwner::acquire(root.path()).is_ok());
    }
}

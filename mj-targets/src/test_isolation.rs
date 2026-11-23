//! Running one test alone in a child of the test binary, for tests that need
//! process-global state (the data directory, the database writer, the working
//! directory) to themselves.

use std::path::Path;
use std::process::Command;

/// The `--exact` name of a test in this binary, given its `module_path!()`.
///
/// `module_path!()` carries the crate name, which libtest's filter does not.
/// mj-daemon's tests use this too, so any crate name is stripped.
pub fn test_name(module_path: &str, test: &str) -> String {
    let module = module_path
        .split_once("::")
        .map_or(module_path, |(_, module)| module);
    format!("{module}::{test}")
}

/// Re-run one test alone, in a child of this test binary.
///
/// The data directory, the installed database writer, the tracing subscriber
/// and the process working directory are all process-global, so a test that
/// needs its own has to be the only test in its process. The child runs the
/// named test with `--exact`, and [`IsolatedTest::run`] fails the parent with
/// the child's own output when it does not pass.
///
/// Build the name with [`test_name`], which strips the crate prefix that
/// `module_path!()` carries and libtest's filter does not accept.
pub struct IsolatedTest {
    name: String,
    command: Command,
}

impl IsolatedTest {
    /// A child runs one whole flow on one test thread. An in-place Move, for
    /// example, polls the checkpoint and provisioning chain inline, whose
    /// debug-build frames are tens of kilobytes each; at the 8 MiB default
    /// that sits within a few pages of the limit, so an unrelated change can
    /// overflow a child that passed before. Give the child room instead of
    /// leaving every isolated test to the edge of its stack.
    const CHILD_STACK_BYTES: &'static str = "33554432";

    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let mut command = Command::new(std::env::current_exe().expect("this test binary"));
        command
            .args(["--exact", &name, "--nocapture"])
            .env("RUST_MIN_STACK", Self::CHILD_STACK_BYTES)
            // The instance name selects the default viewer port and the
            // store layout, so a child owns it: an inherited one changes
            // output the child's goldens pin. A test that needs an instance
            // sets it with `env`.
            .env_remove("MJ_INSTANCE");
        // Control commands run a staged current worker, so a child that
        // exercises them needs a worker source. A test names its own with
        // `env`, which replaces this one.
        #[cfg(unix)]
        command.env(
            "MJ_WORKER_BINARY",
            mj_core::test_hooks::fake_worker_dispatcher(),
        );
        Self { name, command }
    }

    /// Set a variable for the child. The marker a test uses to tell the child
    /// apart from the parent goes here too.
    pub fn env(
        mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> Self {
        self.command.env(key, value);
        self
    }

    /// Remove a variable from the child, such as the default worker source.
    pub fn env_remove(mut self, key: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env_remove(key);
        self
    }

    /// Include an ignored integration case in the isolated child process.
    #[cfg(unix)]
    pub fn include_ignored(mut self) -> Self {
        self.command.arg("--ignored");
        self
    }

    /// Give the child its own configuration and data directories under `root`.
    pub fn isolated_store(self, root: &Path) -> Self {
        self.env("MJ_DATA_DIR", root.join("data"))
            .env("MJ_CONFIG_DIR", root.join("config"))
    }

    /// Run the child and return its output without judging it.
    pub fn output(self) -> std::process::Output {
        #[cfg(unix)]
        {
            self.output_unix()
        }
        #[cfg(not(unix))]
        {
            let mut this = self;
            this.command
                .output()
                .unwrap_or_else(|error| panic!("run isolated {}: {error}", this.name))
        }
    }

    #[cfg(unix)]
    fn output_unix(mut self) -> std::process::Output {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        self.command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = self
            .command
            .spawn()
            .unwrap_or_else(|error| panic!("run isolated {}: {error}", self.name));
        let mut guard = IsolatedProcessGroupGuard::new(child.id());
        let mut stdout_pipe = child.stdout.take().expect("isolated child stdout");
        let mut stderr_pipe = child.stderr.take().expect("isolated child stderr");
        let stdout_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout_pipe.read_to_end(&mut bytes).map(|_| bytes)
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr_pipe.read_to_end(&mut bytes).map(|_| bytes)
        });
        // Wait for the isolated child to exit without reaping it yet so its PID
        // (and therefore its process group ID) cannot be recycled while we
        // terminate any surviving members of that process group.
        wait_for_exit_without_reaping(child.id());
        guard.terminate();
        let status = child
            .wait()
            .unwrap_or_else(|error| panic!("wait for isolated {}: {error}", self.name));
        let stdout = stdout_reader
            .join()
            .expect("isolated child stdout reader panicked")
            .unwrap_or_else(|error| panic!("read isolated {} stdout: {error}", self.name));
        let stderr = stderr_reader
            .join()
            .expect("isolated child stderr reader panicked")
            .unwrap_or_else(|error| panic!("read isolated {} stderr: {error}", self.name));
        std::process::Output {
            status,
            stdout,
            stderr,
        }
    }

    /// Run the child and fail this test with its output if it did not pass.
    pub fn run(self) -> std::process::Output {
        let name = self.name.clone();
        let output = self.output();
        assert!(
            output.status.success(),
            "isolated {name} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

#[cfg(unix)]
struct IsolatedProcessGroupGuard {
    pgid: Option<libc::pid_t>,
}

#[cfg(unix)]
impl IsolatedProcessGroupGuard {
    fn new(pid: u32) -> Self {
        Self {
            pgid: i32::try_from(pid).ok().filter(|&pid| pid > 1),
        }
    }

    fn terminate(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        mj_core::subprocess::terminate_process_group(pgid, libc::SIGKILL);
    }
}

#[cfg(unix)]
impl Drop for IsolatedProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn wait_for_exit_without_reaping(pid: u32) {
    let id = pid as libc::id_t;
    loop {
        // SAFETY: `siginfo_t` is POD and `waitid` writes to it on success.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result =
            unsafe { libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if result == 0 {
            return;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    // Hard-won: #1625: isolated tests leaked surviving process-group members on exit.
    #[test]
    fn isolated_test_terminates_its_process_group_on_exit() {
        const NAME: &str = "isolated_test_terminates_its_process_group_on_exit";
        const SOCKET: &str = "MJ_TEST_ISOLATED_PGID_LEAK_SOCKET";
        if let Some(socket_path) = std::env::var_os(SOCKET) {
            let stream = UnixStream::connect(socket_path).expect("connect leak sentinel");
            let fd: OwnedFd = stream.into();
            let mut command = Command::new("sleep");
            command.arg("600").stdin(Stdio::from(fd));
            #[allow(clippy::zombie_processes)]
            // Intentionally left running without wait() to verify IsolatedTest teardown.
            let _child = command
                .spawn()
                .expect("spawn sleep in isolated process group");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sentinel.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let acceptor = std::thread::spawn(move || listener.accept().expect("accept sentinel").0);
        IsolatedTest::new(test_name(module_path!(), NAME))
            .env(SOCKET, &socket_path)
            .run();
        let mut sentinel = acceptor.join().expect("sentinel accept thread panicked");
        // The kernel closes all file descriptors of a killed process even if
        // container PID 1 does not reap zombies, so EOF confirms termination.
        // Wait with poll(2) rather than SO_RCVTIMEO: macOS returns EINVAL from
        // setsockopt(SO_RCVTIMEO) on an AF_UNIX stream once the peer has closed.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut pollfd = libc::pollfd {
            fd: sentinel.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = loop {
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32;
            // SAFETY: `pollfd` points to one live `libc::pollfd` with an open descriptor.
            let rc = unsafe { libc::poll(&mut pollfd, 1, remaining_ms) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("poll sentinel stream failed: {err}");
            }
            break rc;
        };
        assert!(
            ready > 0,
            "isolated process-group member should be terminated on exit"
        );
        let mut byte = [0_u8; 1];
        let read = sentinel
            .read(&mut byte)
            .expect("isolated process-group member should be terminated on exit");
        assert_eq!(read, 0, "sentinel stream must hit EOF once sleep is killed");
    }
}

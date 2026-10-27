use std::{
    fs::{OpenOptions, TryLockError},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

/// Make any daemon started by `command` own its lifetime against this test.
///
/// Fixture teardown cannot run when the test process is killed without
/// unwinding, so every test daemon also watches this process and exits with
/// it. The environment is inherited by the daemon a client spawns.
pub fn own_test_daemons(command: &mut Command) -> &mut Command {
    command.env(DAEMON_OWNER_ENV, std::process::id().to_string())
}

/// The variable [`own_test_daemons`] sets, for launchers other than
/// [`Command`] such as a pseudo console.
pub const DAEMON_OWNER_ENV: &str = "MJ_DAEMON_OWNER_PID";

/// The environment that keeps an `mj` launched under `root` away from the
/// host's agent homes, commands and credentials. Profile discovery reads
/// these on every launch, so every terminal fixture applies all of it; a
/// variable missing from one copy is how a fixture leaks host credentials.
/// Callers still restrict PATH to the commands their platform needs.
// Only the terminal fixtures launch a dashboard.
#[allow(dead_code)]
pub struct AgentIsolation {
    pub set: Vec<(&'static str, PathBuf)>,
    pub remove: [&'static str; 4],
}

#[allow(dead_code)]
impl AgentIsolation {
    pub fn under(root: &Path) -> Self {
        Self {
            set: vec![
                ("HOME", root.to_path_buf()),
                // `mj_core::path_input::user_home` reads it on Windows.
                ("USERPROFILE", root.to_path_buf()),
                ("XDG_CONFIG_HOME", root.join("config")),
                ("XDG_DATA_HOME", root.join("data")),
                ("CODEX_HOME", root.join(".codex")),
                ("CLAUDE_CONFIG_DIR", root.join(".claude")),
                // Each fixture has a dashboard and daemon. Host-sized pools
                // multiply into thousands of threads on large CI machines
                // during parallel runs.
                ("TOKIO_WORKER_THREADS", "2".into()),
                ("RAYON_NUM_THREADS", "2".into()),
            ],
            remove: ["MJ_BIFROST_BIN", "KIMI_CODE_HOME", "GROK_HOME", "MUSE_HOME"],
        }
    }

    pub fn apply(&self, command: &mut Command) {
        for (name, value) in &self.set {
            command.env(name, value);
        }
        for name in self.remove {
            command.env_remove(name);
        }
    }
}

/// Own an implicitly started daemon until its temporary store can be removed.
pub struct DaemonStorage {
    directory: Option<tempfile::TempDir>,
    config: PathBuf,
    data: PathBuf,
}

impl DaemonStorage {
    pub fn new(directory: tempfile::TempDir, config: PathBuf, data: PathBuf) -> Self {
        Self {
            directory: Some(directory),
            config,
            data,
        }
    }

    pub fn path(&self) -> &Path {
        self.directory.as_ref().expect("fixture storage").path()
    }

    /// Wait until a daemon owns this store.
    ///
    /// A client starts its daemon in the background, so a fixture handed to a
    /// test before that daemon takes the store can be removed while the daemon
    /// is still starting. The daemon then recreates every directory it needs,
    /// leaving storage behind that teardown already removed and reported gone.
    // This module compiles into each test binary, and only the terminal tests
    // start a daemon in the background.
    #[allow(dead_code)]
    pub fn wait_until_owned(&self, deadline: Instant) -> bool {
        // Metadata is published only after the daemon owns the store. Taking
        // the lock to probe startup can instead lock out that daemon's one
        // admission attempt, making an otherwise isolated fixture fail.
        let metadata = self.data.join("daemon.json");
        while !self.daemon_pid(&metadata).is_some_and(process_exists) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// Stop whichever daemon owns this store, so the storage can be removed.
    ///
    /// Three facts about daemon startup shape this. It locks the store before
    /// it publishes `daemon.json`, it publishes that metadata before it starts
    /// listening, and a stop request needs both. A fixture torn down inside
    /// either window must keep asking rather than give up once: a daemon left
    /// running holds the store until this test process exits, which leaked one
    /// storage directory for every interrupted test.
    fn stop_writer(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let metadata = self.data.join("daemon.json");
        let mut pid = None;
        let mut refusal = None;
        while self.store_is_owned()? {
            if metadata.exists() {
                // Shutdown removes the metadata early, so the pid must be read
                // before the stop request that may succeed.
                if pid.is_none() {
                    pid = self.daemon_pid(&metadata);
                }
                match self.request_stop() {
                    // Metadata is removed early in shutdown, so the file is no
                    // evidence either way. Waiting covers an idle shutdown that
                    // won the race with the request too.
                    Ok(()) if self.wait_for_exit(deadline, pid)? => return Ok(()),
                    Ok(()) => {}
                    // A daemon publishes its metadata before it listens, so a
                    // stop this early has nothing to connect to yet.
                    Err(error) => refusal = Some(error),
                }
            }
            if Instant::now() >= deadline {
                return self.terminate_owner(pid, refusal);
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// The pid recorded in the daemon metadata, when it is readable. A file
    /// being written, or removed by a shutdown in progress, simply has no pid
    /// to offer yet.
    fn daemon_pid(&self, metadata: &Path) -> Option<u32> {
        let body = std::fs::read(metadata).ok()?;
        let document: serde_json::Value = serde_json::from_slice(&body).ok()?;
        document
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
    }

    /// Stop a daemon that never answered a request. Deleting the files under a
    /// live writer is never correct, so the process goes first. The daemon runs
    /// in its own process group, so signalling that group cannot reach this
    /// test process.
    #[cfg(unix)]
    fn terminate_owner(&self, pid: Option<u32>, refusal: Option<anyhow::Error>) -> Result<()> {
        let Some(pid) = pid else {
            let detail = refusal
                .map(|error| format!(": {error:#}"))
                .unwrap_or_default();
            bail!("fixture controller owns its store without publishing a usable pid{detail}");
        };
        let target =
            i32::try_from(pid).context("fixture daemon pid does not fit a signal target")?;
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            mj_core::subprocess::terminate_process_group(target, signal);
            if self.wait_for_exit(Instant::now() + Duration::from_secs(5), Some(pid))? {
                return Ok(());
            }
        }
        bail!("fixture daemon {pid} outlived SIGKILL or kept its store")
    }

    #[cfg(not(unix))]
    fn terminate_owner(&self, pid: Option<u32>, refusal: Option<anyhow::Error>) -> Result<()> {
        let Some(pid) = pid else {
            let detail = refusal
                .map(|error| format!(": {error:#}"))
                .unwrap_or_default();
            bail!("fixture controller owns its store without publishing a usable pid{detail}");
        };
        let process_id = sysinfo::Pid::from_u32(pid);
        for signal in [sysinfo::Signal::Term, sysinfo::Signal::Kill] {
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[process_id]), true);
            let Some(process) = system.process(process_id) else {
                return Ok(());
            };
            if !process.kill_with(signal).unwrap_or_else(|| process.kill()) {
                bail!("could not terminate fixture daemon {pid}");
            }
            if self.wait_for_exit(Instant::now() + Duration::from_secs(5), Some(pid))? {
                return Ok(());
            }
        }
        bail!("fixture daemon {pid} survived forced termination or kept its store")
    }

    /// Whether a process still holds the store's sole-writer lock.
    fn store_is_owned(&self) -> Result<bool> {
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.data.join("controller.lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("open fixture controller ownership lock"),
        };
        match lock.try_lock() {
            Ok(()) => {
                mj_core::file_lock::FileLock::new(lock)
                    .release()
                    .context("release fixture controller ownership probe")?;
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn request_stop(&self) -> Result<()> {
        let output = mj_core::subprocess::run_with_input(
            own_test_daemons(
                Command::new(env!("CARGO_BIN_EXE_mj"))
                    .args(["daemon", "stop"])
                    .env("MJ_CONFIG_DIR", &self.config)
                    .env("MJ_DATA_DIR", &self.data),
            ),
            &[],
        )?;
        if !output.status.success() {
            bail!(
                "fixture daemon stop failed: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Whether the store became free and its owner left before `deadline`.
    ///
    /// A daemon releases the store before it finishes writing its log, so the
    /// released lock alone is not enough: anything the departing process writes
    /// after the storage is removed recreates the directory tree underneath it.
    fn wait_for_exit(&self, deadline: Instant, pid: Option<u32>) -> Result<bool> {
        if !self.wait_for_release(deadline)? {
            return Ok(false);
        }
        let Some(pid) = pid else { return Ok(true) };
        while process_exists(pid) {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(true)
    }

    /// Whether the store's owner released it before `deadline`.
    fn wait_for_release(&self, deadline: Instant) -> Result<bool> {
        while self.store_is_owned()? {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(true)
    }
}

/// Whether a process is still running. A zombie has exited and cannot hold or
/// write the fixture store, even when the process table retains it because
/// PID 1 does not reap.
pub fn process_exists(pid: u32) -> bool {
    let process_id = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[process_id]), true);
    system
        .process(process_id)
        .is_some_and(|process| process.status() != sysinfo::ProcessStatus::Zombie)
}

impl Drop for DaemonStorage {
    fn drop(&mut self) {
        if let Err(error) = self
            .stop_writer()
            .with_context(|| format!("clean up fixture {}", self.path().display()))
        {
            let retained = self.directory.take().expect("fixture storage").keep();
            let message = format!(
                "Could not stop fixture daemon: {error:#}; retained {}",
                retained.display()
            );
            if thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
        if thread::panicking() {
            match std::fs::read_dir(self.data.join("logs")) {
                Ok(entries) => {
                    for entry in entries {
                        match entry.and_then(|entry| {
                            let mut file = std::fs::File::open(entry.path())?;
                            let length = file.metadata()?.len();
                            file.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
                            let mut bytes = Vec::new();
                            file.take(8192).read_to_end(&mut bytes)?;
                            Ok(bytes)
                        }) {
                            Ok(bytes) => {
                                eprintln!("fixture log tail: {}", String::from_utf8_lossy(&bytes))
                            }
                            Err(error) => eprintln!("read fixture log: {error}"),
                        }
                    }
                }
                Err(error) => eprintln!("read fixture logs: {error}"),
            }
        }
    }
}

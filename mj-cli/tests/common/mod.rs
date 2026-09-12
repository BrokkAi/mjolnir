use std::{
    fs::{OpenOptions, TryLockError},
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
    command.env("MJ_DAEMON_OWNER_PID", std::process::id().to_string())
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
    pub fn wait_until_owned(&self, deadline: Instant) -> Result<bool> {
        while !self.store_is_owned()? {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(true)
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
            // Closing the file releases the lock this probe just took.
            Ok(()) => Ok(false),
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

/// Whether a process still exists. Signal 0 runs the existence and permission
/// checks without delivering anything.
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
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
    }
}

use std::{
    fs::{OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

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

    fn stop_writer(&self) -> Result<()> {
        let metadata = self.data.join("daemon.json");
        if metadata.exists() {
            let output = hel::hel_subprocess::run_with_input(
                Command::new(env!("CARGO_BIN_EXE_mj"))
                    .args(["daemon", "stop"])
                    .env("MJ_CONFIG_DIR", &self.config)
                    .env("MJ_DATA_DIR", &self.data),
                &[],
            )?;
            // An idle daemon can finish exiting while the stop client connects.
            if !output.status.success() && metadata.exists() {
                bail!(
                    "fixture daemon stop failed: {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        // Metadata is removed early in shutdown. Wait for the store owner too,
        // including when idle shutdown won the race with our stop request.
        let owner = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.data.join("controller.lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("open fixture controller ownership lock"),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match owner.try_lock() {
                Ok(()) => return Ok(()),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(TryLockError::WouldBlock) => bail!("fixture controller still owns its store"),
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
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

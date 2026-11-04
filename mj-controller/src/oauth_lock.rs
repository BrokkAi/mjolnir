//! Empty directory leases compatible with the harnesses' `proper-lockfile` locks.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

pub(crate) struct OAuthRefreshLock {
    // Serialize heartbeat and release so an in-flight beat cannot touch a
    // directory acquired by the next owner after our release.
    path: Arc<Mutex<Option<PathBuf>>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl OAuthRefreshLock {
    pub(crate) async fn acquire(
        path: PathBuf,
        wait: Duration,
        stale_after: Duration,
        heartbeat_interval: Duration,
    ) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::fs::create_dir(&path).await {
                Ok(()) => {
                    let path = Arc::new(Mutex::new(Some(path)));
                    let heartbeat_path = path.clone();
                    let heartbeat = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(heartbeat_interval).await;
                            let held = heartbeat_path.lock().unwrap();
                            let Some(path) = held.as_ref() else { break };
                            if let Err(error) = touch(path, SystemTime::now()) {
                                tracing::warn!(path = %path.display(), %error, "heartbeat OAuth refresh lock");
                            }
                        }
                    });
                    return Ok(Self { path, heartbeat });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if tokio::time::Instant::now() >= deadline {
                        bail!(
                            "timed out waiting for OAuth refresh lock {}",
                            path.display()
                        );
                    }
                    if !break_stale_lock(&path, stale_after).await {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                Err(error) => return Err(error).context("acquire OAuth refresh lock"),
            }
        }
    }
}

impl Drop for OAuthRefreshLock {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if let Some(path) = self.path.lock().unwrap().take() {
            match std::fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "release OAuth refresh lock")
                }
            }
        }
    }
}

pub(crate) fn mtime(path: &Path) -> std::io::Result<Option<SystemTime>> {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.modified().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn touch(path: &Path, modified: SystemTime) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options
        .open(path)?
        .set_times(std::fs::FileTimes::new().set_modified(modified))
}

async fn break_stale_lock(path: &Path, stale_after: Duration) -> bool {
    let modified = match mtime(path) {
        Ok(Some(modified)) => modified,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "inspect OAuth refresh lock");
            return false;
        }
    };
    // proper-lockfile can round its writes into the future.
    let Some(age) = SystemTime::now()
        .duration_since(modified)
        .ok()
        .filter(|age| *age >= stale_after)
    else {
        return false;
    };
    match tokio::fs::remove_dir(path).await {
        Ok(()) => {
            tracing::warn!(path = %path.display(), age_seconds = age.as_secs(), "removed an abandoned OAuth refresh lock");
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "remove stale OAuth refresh lock");
            false
        }
    }
}

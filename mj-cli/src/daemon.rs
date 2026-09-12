//! Application-owned daemon startup and attachment maintenance.
use mj_controller::controller::ControllerStoreGuard;
use mj_core::config::data_dir;
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, anyhow, ensure};
pub(crate) use mj_client::daemon::*;
pub(crate) use mj_controller::daemon::run_daemon_process;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
const DEV_RESTART_STALE_DAEMON_ENV: &str = "MJ_DEV_RESTART_STALE_DAEMON";
const START_TIMEOUT: Duration = Duration::from_secs(8);
#[derive(Debug)]
struct DaemonStartGuard(fs::File);

impl Drop for DaemonStartGuard {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "could not release daemon startup lock");
        }
    }
}

async fn acquire_start_guard(path: PathBuf) -> Result<DaemonStartGuard> {
    let deadline = Instant::now() + STOP_TIMEOUT + START_TIMEOUT;
    loop {
        let path = path.clone();
        let guard = tokio::task::spawn_blocking(move || -> Result<Option<DaemonStartGuard>> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut options = OpenOptions::new();
            options.create(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&path).context("open daemon startup lock")?;
            match file.try_lock() {
                Ok(()) => Ok(Some(DaemonStartGuard(file))),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(error)) => {
                    Err(error).context("lock daemon startup")
                }
            }
        })
        .await
        .context("daemon startup lock task failed")??;
        if let Some(guard) = guard {
            return Ok(guard);
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for another client to finish starting the Mjolnir daemon"
        );
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

pub async fn connect_or_start() -> Result<DaemonClient> {
    // Serialize replacement and publication across clients, then re-read the
    // endpoint. A client waiting here must reuse the winner's daemon.
    let _startup = acquire_start_guard(data_dir().join("daemon-start.lock")).await?;
    if let Ok(metadata) = read_metadata_any() {
        ensure_supported_daemon_protocol(metadata.protocol_version)?;
    }
    maybe_replace_stale_development_daemon().await?;
    if let Ok(metadata) = read_metadata_any()
        && metadata.protocol_version != PROTOCOL_VERSION
    {
        replace_daemon(&metadata).await?;
    }
    if let Ok(mut client) = connect_existing().await
        && matches!(
            client.request(DaemonAction::Ping).await,
            Ok(DaemonReply::Pong)
        )
    {
        return Ok(client);
    }

    // Metadata disappears before shutdown releases the sole-writer lock.
    // Do not launch a child that can only fail to acquire that lock.
    let handoff_deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if let Some(guard) = tokio::task::spawn_blocking(ControllerStoreGuard::try_acquire)
            .await
            .context("probe controller ownership task failed")??
        {
            drop(guard);
            break;
        }
        // A daemon started outside this client's startup lock may be becoming ready.
        if let Ok(mut client) = connect_existing().await
            && matches!(
                client.request(DaemonAction::Ping).await,
                Ok(DaemonReply::Pong)
            )
        {
            return Ok(client);
        }
        ensure!(
            Instant::now() < handoff_deadline,
            "controller store is still owned by another process after {}s; daemon startup was not attempted",
            STOP_TIMEOUT.as_secs()
        );
        tokio::time::sleep(RETRY_DELAY).await;
    }

    tokio::task::spawn_blocking(|| -> Result<u32> {
        let executable = daemon_launch_executable()?;
        let mut command = std::process::Command::new(executable);
        command
            .arg("daemon-run")
            // This switch makes the invoking development client authoritative. It
            // has no meaning inside the persistent daemon or its child processes.
            .env_remove(DEV_RESTART_STALE_DAEMON_ENV);
        mj_core::subprocess::spawn_detached(&mut command, &data_dir().join("daemon.log"))
    })
    .await
    .context("spawn daemon task failed")??;

    let deadline = Instant::now() + START_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match connect_existing().await {
            Ok(mut client) => match client.request(DaemonAction::Ping).await {
                Ok(DaemonReply::Pong) => return Ok(client),
                Ok(reply) => last_error = Some(anyhow!("unexpected startup reply {reply:?}")),
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow!("Mjolnir daemon did not become ready"))).with_context(
        || {
            format!(
                "start Mjolnir daemon; details are in {}",
                data_dir().join("daemon.log").display()
            )
        },
    )
}

fn daemon_launch_executable() -> Result<PathBuf> {
    // current_exe resolves the old pathname, which can disappear on upgrade.
    // Execute the running inode so the daemon also matches this client's protocol.
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().context("find current mj executable")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutableFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn executable_file_identity(path: &Path) -> std::io::Result<ExecutableFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::metadata(path)?;
    Ok(ExecutableFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
fn daemon_uses_current_executable(pid: u32) -> Result<Option<bool>> {
    let current = executable_file_identity(Path::new("/proc/self/exe"))
        .context("inspect the development client executable")?;
    let daemon_path = PathBuf::from(format!("/proc/{pid}/exe"));
    let daemon = match executable_file_identity(&daemon_path) {
        Ok(identity) => identity,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect daemon {pid} executable via {}",
                    daemon_path.display()
                )
            });
        }
    };
    Ok(Some(current == daemon))
}

#[cfg(target_os = "macos")]
fn daemon_uses_current_executable(pid: u32) -> Result<Option<bool>> {
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
    let current = std::env::current_exe().context("find development client executable")?;
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(false)),
        Err(error) => return Err(error).context("inspect daemon executable"),
    };
    // macOS reports a pathname, not Linux's reference to the running inode.
    // A newer file at that same pathname also means the daemon is stale.
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok(Some(
        executable_file_identity(&current)? == executable_file_identity(path)?
            && modified <= process.start_time(),
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
static DEVELOPMENT_DAEMON_REFRESH: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn development_workers_changed_since(
    started: SystemTime,
    resolve: impl Fn(&str) -> Result<mj_controller::controller::WorkerBinaryAvailability>,
) -> Result<bool> {
    use mj_controller::controller::WorkerBinaryAvailability;

    for arch in ["aarch64", "x86_64"] {
        match resolve(arch) {
            Ok(WorkerBinaryAvailability::Local { path, .. }) => {
                let modified = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .with_context(|| format!("inspect development worker {}", path.display()))?;
                if modified > started {
                    return Ok(true);
                }
            }
            Ok(WorkerBinaryAvailability::Remote { .. }) => {}
            Err(error) => tracing::debug!(arch, %error, "development worker source is unavailable"),
        }
    }
    Ok(false)
}

async fn maybe_replace_stale_development_daemon() -> Result<()> {
    if std::env::var_os(DEV_RESTART_STALE_DAEMON_ENV).is_none() {
        return Ok(());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    DEVELOPMENT_DAEMON_REFRESH
        .get_or_try_init(|| async {
            let Ok(metadata) = read_metadata_any() else {
                return Ok(());
            };
            let pid = metadata.pid;
            let started: SystemTime = chrono::DateTime::parse_from_rfc3339(&metadata.started_at)
                .context("parse development daemon start time")?
                .into();
            let stale = tokio::task::spawn_blocking(move || -> Result<bool> {
                Ok(daemon_uses_current_executable(pid)? == Some(false)
                    || development_workers_changed_since(
                        started,
                        mj_controller::controller::worker_binary_prerequisite_for_arch,
                    )?)
            })
            .await
            .context("inspect development daemon task failed")??;
            if !stale {
                return Ok(());
            }
            eprintln!(
                "Mjolnir daemon {} is using an older development build; restarting it.",
                metadata.pid
            );
            replace_daemon(&metadata).await
        })
        .await?;
    Ok(())
}

/// Clear the way for a different daemon executable. Ask the running daemon to
/// stop over the frozen management subset first — graceful for every protocol
/// version — and only signal it when the wire is unreachable.
async fn replace_daemon(metadata: &DaemonMetadata) -> Result<()> {
    if let Ok(inner) = DaemonClient::connect(metadata.clone()).await
        && ManagementClient::new(inner).stop_and_wait().await.is_ok()
    {
        return Ok(());
    }
    // Another client may have completed the replacement while this client was
    // waiting on the old endpoint. Never signal the process named by stale
    // metadata after the owner-only record has advanced to another daemon.
    if read_metadata_any().is_ok_and(|current| {
        current.pid != metadata.pid
            || current.address != metadata.address
            || current.token != metadata.token
    }) {
        return Ok(());
    }
    signal_daemon(metadata).await
}

async fn signal_daemon(metadata: &DaemonMetadata) -> Result<()> {
    #[cfg(unix)]
    {
        let mut system = sysinfo::System::new();
        system.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(metadata.pid)]),
            true,
        );
        // The PID comes from the owner-only metadata file; the argv check
        // guards against PID recycling, not against other Mjolnir builds — old
        // daemons are exactly what this function exists to retire.
        let Some(process) = system.process(sysinfo::Pid::from_u32(metadata.pid)) else {
            return Ok(());
        };
        let is_hel_daemon = process
            .cmd()
            .get(1)
            .is_some_and(|argument| argument.to_str() == Some("daemon-run"));
        ensure!(
            is_hel_daemon,
            "refusing to signal PID {} because it does not look like a Mjolnir daemon (`mj daemon-run`)",
            metadata.pid
        );
        // SAFETY: the PID comes from owner-only daemon metadata and SIGTERM is
        // handled as graceful cancellation by every supported daemon.
        let result = unsafe { libc::kill(metadata.pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("stop superseded Mjolnir daemon");
            }
        }
        wait_for_exit(metadata.pid).await.with_context(|| {
            format!(
                "superseded Mjolnir daemon {} was signalled but was still running after {}s",
                metadata.pid,
                STOP_TIMEOUT.as_secs()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        bail!("stop the incompatible Mjolnir daemon, then retry")
    }
}

pub fn maintain_attachment(
    client_id: String,
    pid: u32,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    match connect_or_start().await {
                        Ok(mut daemon) => {
                            if let Err(error) = daemon
                                .attach(client_id.clone(), pid)
                                .await
                            {
                                tracing::warn!(%error, "could not refresh daemon client presence");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not reconnect dashboard to Mjolnir daemon");
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;
    #[cfg(target_os = "linux")]
    #[test]
    fn detached_daemon_can_launch_after_its_client_binary_is_removed() {
        const STAGE: &str = "MJ_TEST_REMOVED_DAEMON_EXECUTABLE";
        const TEST: &str =
            "daemon::tests::detached_daemon_can_launch_after_its_client_binary_is_removed";
        if let Some(directory) = std::env::var_os(STAGE) {
            let directory = PathBuf::from(directory);
            if directory.join("client").exists() {
                fs::remove_file(directory.join("client")).unwrap();
                let mut command = std::process::Command::new(daemon_launch_executable().unwrap());
                command.args(["--exact", TEST]);
                mj_core::subprocess::spawn_detached(&mut command, &directory.join("child.log"))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !directory.join("launched").exists() {
                    assert!(Instant::now() < deadline, "detached child did not launch");
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                fs::write(directory.join("launched"), b"ok").unwrap();
            }
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("client");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let mut command = std::process::Command::new(&executable);
        command.args(["--exact", TEST]).env(STAGE, directory.path());
        let output = mj_core::subprocess::run_with_input(&mut command, b"").unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(directory.path().join("launched").exists());
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_startup_wait_does_not_retain_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon-start.lock");
        let owner = acquire_start_guard(path.clone()).await.unwrap();
        let mut waiter = tokio::spawn(acquire_start_guard(path.clone()));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err()
        );
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(owner);
        let replacement = tokio::time::timeout(Duration::from_secs(2), acquire_start_guard(path))
            .await
            .unwrap()
            .unwrap();
        drop(replacement);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn executable_identity_detects_an_nfs_style_replaced_binary() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("mj");
        let hard_link = directory.path().join("mj-hard-link");
        let retained = directory.path().join(".nfs0000000000000001");

        fs::write(&current, b"old executable").unwrap();
        fs::hard_link(&current, &hard_link).unwrap();
        assert_eq!(
            executable_file_identity(&current).unwrap(),
            executable_file_identity(&hard_link).unwrap(),
            "two names for the same executable inode must not restart the daemon"
        );

        fs::rename(&current, &retained).unwrap();
        fs::write(&current, b"new executable").unwrap();
        assert_ne!(
            executable_file_identity(&retained).unwrap(),
            executable_file_identity(&current).unwrap(),
            "an NFS-retained old executable must differ from its replacement"
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn current_process_is_running_the_current_executable() {
        assert_eq!(
            daemon_uses_current_executable(std::process::id()).unwrap(),
            Some(true)
        );
        assert_eq!(daemon_uses_current_executable(u32::MAX).unwrap(), None);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn development_refresh_detects_workers_installed_or_rebuilt_after_startup() {
        use mj_controller::controller::WorkerBinaryAvailability;

        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("mj-worker");
        let started = UNIX_EPOCH + Duration::from_secs(200);
        let resolve = |arch: &str| {
            anyhow::ensure!(arch == "aarch64" && worker.exists(), "no worker for {arch}");
            Ok(WorkerBinaryAvailability::Local {
                path: worker.clone(),
                source: "test".into(),
            })
        };
        assert!(!development_workers_changed_since(started, resolve).unwrap());
        fs::write(&worker, "worker").unwrap();
        let file = fs::File::options().write(true).open(&worker).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(started - Duration::from_secs(1)))
            .unwrap();
        assert!(!development_workers_changed_since(started, resolve).unwrap());
        file.set_times(fs::FileTimes::new().set_modified(started + Duration::from_secs(1)))
            .unwrap();
        assert!(development_workers_changed_since(started, resolve).unwrap());
    }
}

//! Application-owned daemon startup and attachment maintenance.
use mj_controller::controller::ControllerStoreGuard;
use mj_core::config::data_dir;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, anyhow, bail, ensure};
pub(crate) use mj_client::daemon::*;
pub(crate) use mj_client::executable::{
    process_executable_path, process_runs_this_executable, running_executable_path,
};
pub(crate) use mj_controller::daemon::run_daemon_process;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
const DEV_RESTART_STALE_DAEMON_ENV: &str = "MJ_DEV_RESTART_STALE_DAEMON";
/// How long a launched daemon may take to publish its endpoint before the
/// client says out loud that startup is still running. Startup opens the store,
/// applies migrations, pins worker sources and reconciles interrupted sessions
/// before it can answer anything, so a busy machine or a large instance can
/// pass this point and still be healthy.
const START_NOTICE_DELAY: Duration = Duration::from_secs(8);
/// The hard upper bound on waiting for a launched daemon, so the command
/// cannot hang forever behind a wedged startup.
const START_TIMEOUT: Duration = Duration::from_secs(60);
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
    let startup = acquire_start_guard(data_dir().join("daemon-start.lock")).await?;
    connect_or_start_holding(&startup).await
}

/// The body of [`connect_or_start`] for a caller that already holds the
/// daemon startup lock.
///
/// That lock is not reentrant within one process, so a caller that needs it
/// held across more than one step — a restart holds it across the stop and the
/// replacement — must come through here instead of calling
/// [`connect_or_start`] again. The guard is taken by reference only so the
/// requirement is visible at every call site.
async fn connect_or_start_holding(_startup: &DaemonStartGuard) -> Result<DaemonClient> {
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

    let log_path = data_dir().join("daemon.log");
    let launched = tokio::task::spawn_blocking({
        let log_path = log_path.clone();
        move || -> Result<LaunchedDaemon> {
            // Everything the daemon writes from here on belongs to this launch.
            let log_offset = fs::metadata(&log_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let executable = daemon_launch_executable()?;
            let mut command = std::process::Command::new(executable);
            command
                .arg("daemon-run")
                // This switch makes the invoking development client authoritative. It
                // has no meaning inside the persistent daemon or its child processes.
                .env_remove(DEV_RESTART_STALE_DAEMON_ENV);
            let pid = mj_core::subprocess::spawn_detached(&mut command, &log_path)?;
            Ok(LaunchedDaemon { pid, log_offset })
        }
    })
    .await
    .context("spawn daemon task failed")??;

    let outcome = wait_for_ready_daemon(
        || process_is_alive(launched.pid),
        || async {
            let mut client = connect_existing().await?;
            match client.request(DaemonAction::Ping).await? {
                DaemonReply::Pong => Ok(client),
                reply => Err(anyhow!("unexpected startup reply {reply:?}")),
            }
        },
        |waited| {
            eprintln!(
                "Mjolnir daemon {} has been starting for {}s; still waiting.",
                launched.pid,
                waited.as_secs()
            );
        },
    )
    .await;
    let reason = match outcome {
        StartupOutcome::Ready(client) => return Ok(client),
        // A daemon that fails to initialize explains itself in its log and
        // exits without ever publishing an endpoint. That explanation is the
        // answer; the client's own failure to reach the absent endpoint is not.
        StartupOutcome::Exited => {
            format!("Mjolnir daemon {} exited before it was ready", launched.pid)
        }
        StartupOutcome::StillStarting { last_error } => format!(
            "Mjolnir daemon {} is still starting after {}s and has not accepted a request (last attempt: {last_error:#})",
            launched.pid,
            START_TIMEOUT.as_secs()
        ),
    };
    let output = launched.output_since_launch(&log_path).await;
    Err(launched.failure(reason, output, &log_path))
}

/// The daemon a restart produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartedDaemon {
    pub pid: u32,
    /// Whether the daemon runs this client's own executable file. `None` on a
    /// platform where a process's executable cannot be identified.
    pub runs_this_build: Option<bool>,
}

/// How many times a restart stops the daemon it finds and starts its own.
///
/// The startup lock excludes a client that has not started its daemon yet, but
/// not one that started it just before this restart took the lock, so one
/// retry is worth making. A second loss means that client is producing daemons
/// faster than they can be replaced, which is a report rather than a race to
/// keep running.
const RESTART_ATTEMPTS: usize = 2;

/// Stop the running Mjolnir daemon and start one from this client's executable.
///
/// The startup lock is held from before the stop until the replacement has
/// answered a request. Every client that starts a daemon takes that lock
/// first, including clients built before this function existed, so none of
/// them can install a daemon of its own in the gap the stop opens. Without
/// that, an attached client running an older build wins the gap and the
/// restart reports someone else's daemon as the one it was asked for.
pub async fn restart_daemon() -> Result<RestartedDaemon> {
    let startup = acquire_start_guard(data_dir().join("daemon-start.lock")).await?;
    for attempt in 1..=RESTART_ATTEMPTS {
        if let Ok(metadata) = read_metadata_any() {
            replace_daemon(&metadata).await?;
        }
        let mut client = connect_or_start_holding(&startup).await?;
        let status = client.status().await?;
        let identity = process_runs_this_executable(status.pid)?;
        if identity != Some(false) || attempt == RESTART_ATTEMPTS {
            return restart_verdict(
                status.pid,
                identity,
                process_executable_path(status.pid).as_deref(),
                running_executable_path().as_deref(),
            );
        }
    }
    unreachable!("the final attempt always returns a verdict")
}

/// Turn one executable-identity answer into the restart's result.
///
/// This is separate from the restart itself so the outcome can be exercised
/// without starting daemons. It is also the decision the command used to skip:
/// reporting success on a changed process id alone is what let a restart
/// announce a daemon running code the caller had just replaced.
fn restart_verdict(
    pid: u32,
    identity: Option<bool>,
    daemon: Option<&Path>,
    client: Option<&Path>,
) -> Result<RestartedDaemon> {
    if identity == Some(false) {
        let daemon = daemon.map_or_else(
            || "another executable".to_owned(),
            |path| path.display().to_string(),
        );
        let client = client.map_or_else(
            || "this client's executable".to_owned(),
            |path| path.display().to_string(),
        );
        bail!(
            "Mjolnir daemon {pid} runs {daemon}, not this build ({client}). \
             Another attached client started it. Close clients from the \
             previous build, then run `mj daemon restart` again."
        );
    }
    Ok(RestartedDaemon {
        pid,
        runs_this_build: identity,
    })
}

/// Why waiting for a launched daemon stopped.
enum StartupOutcome<T> {
    /// It answered a request.
    Ready(T),
    /// The launched process is gone.
    Exited,
    /// It is still alive but has not answered within [`START_TIMEOUT`].
    StillStarting { last_error: anyhow::Error },
}

/// Wait for a launched daemon to answer a request.
///
/// A daemon that is still alive is still starting: initialization runs before
/// it publishes its endpoint, so a slow start is not a failure and the wait
/// continues. Only the process leaving, or the hard [`START_TIMEOUT`] bound,
/// ends the wait without a client. The clock is Tokio's, so tests can drive it.
async fn wait_for_ready_daemon<T, Probe, Waiting, Notice>(
    is_alive: impl Fn() -> bool,
    mut probe: Probe,
    notice: Notice,
) -> StartupOutcome<T>
where
    Probe: FnMut() -> Waiting,
    Waiting: Future<Output = Result<T>>,
    Notice: FnOnce(Duration),
{
    let started = tokio::time::Instant::now();
    let deadline = started + START_TIMEOUT;
    let mut notice = Some(notice);
    loop {
        let last_error = match probe().await {
            Ok(ready) => return StartupOutcome::Ready(ready),
            Err(error) => error,
        };
        if !is_alive() {
            return StartupOutcome::Exited;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return StartupOutcome::StillStarting { last_error };
        }
        if now.duration_since(started) >= START_NOTICE_DELAY
            && let Some(notice) = notice.take()
        {
            notice(now.duration_since(started));
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// The daemon this client launched and where its log stood at launch.
#[derive(Debug, Clone, Copy)]
struct LaunchedDaemon {
    pid: u32,
    log_offset: u64,
}

impl LaunchedDaemon {
    /// The last lines the daemon appended to its log since this launch.
    ///
    /// The log is shared by every daemon launch, so only the bytes written
    /// after this launch can describe this daemon. Startup failures are a
    /// short `Error:` report, so a few lines carry the whole explanation.
    async fn output_since_launch(&self, log_path: &Path) -> String {
        const KEPT_LINES: usize = 20;
        let log_path = log_path.to_path_buf();
        let offset = self.log_offset;
        let appended = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = fs::File::open(&log_path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut appended = Vec::new();
            file.read_to_end(&mut appended)?;
            Ok(appended)
        })
        .await;
        let appended = match appended {
            Ok(Ok(appended)) => appended,
            Ok(Err(error)) => {
                tracing::warn!(%error, "could not read the daemon log after a failed launch");
                return String::new();
            }
            Err(error) => {
                tracing::warn!(%error, "daemon log read task failed");
                return String::new();
            }
        };
        let text = String::from_utf8_lossy(&appended);
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .collect();
        let skipped = lines.len().saturating_sub(KEPT_LINES);
        lines[skipped..].join("\n")
    }

    fn failure(&self, reason: String, output: String, log_path: &Path) -> anyhow::Error {
        let error = if output.is_empty() {
            anyhow!("{reason}; it wrote nothing to {}", log_path.display())
        } else {
            anyhow!("{reason}; it reported:\n{output}")
        };
        error.context(format!(
            "start Mjolnir daemon; details are in {}",
            log_path.display()
        ))
    }
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
                Ok(process_runs_this_executable(pid)? == Some(false)
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
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
        // Hard-linked beside the test binary rather than copied: a copy's
        // write descriptor is inherited by children other tests fork until
        // they exec, and exec of the copy in that window fails with ETXTBSY.
        let test_binary = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(test_binary.parent().unwrap()).unwrap();
        let executable = directory.path().join("client");
        fs::hard_link(&test_binary, &executable).unwrap();
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
    /// The restart holds one guard across its stop and its start. That is only
    /// safe because the lock is not reentrant: a second acquisition inside the
    /// same process blocks exactly as another process would, so a restart that
    /// called `connect_or_start` again would wait on itself until the deadline.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_startup_lock_is_not_reentrant_within_one_process() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon-start.lock");
        let held = acquire_start_guard(path.clone()).await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                acquire_start_guard(path.clone())
            )
            .await
            .is_err(),
            "a second acquisition in this process must block while the first is held"
        );
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), acquire_start_guard(path))
            .await
            .unwrap()
            .unwrap();
    }
    #[test]
    fn a_restart_onto_this_build_succeeds_and_says_so() {
        let verdict = restart_verdict(
            4242,
            Some(true),
            Some(Path::new("/opt/mj/mj")),
            Some(Path::new("/opt/mj/mj")),
        )
        .unwrap();
        assert_eq!(
            verdict,
            RestartedDaemon {
                pid: 4242,
                runs_this_build: Some(true)
            }
        );
    }
    #[test]
    fn a_restart_that_cannot_identify_the_daemon_succeeds_without_the_guarantee() {
        let verdict = restart_verdict(7, None, None, None).unwrap();
        assert_eq!(
            verdict,
            RestartedDaemon {
                pid: 7,
                runs_this_build: None
            }
        );
    }
    #[test]
    fn a_restart_onto_another_build_fails_and_names_both_executables() {
        let error = restart_verdict(
            99,
            Some(false),
            Some(Path::new("/checkout/target/debug/mj (deleted)")),
            Some(Path::new("/checkout/target/debug/mj")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("99"), "{error}");
        assert!(
            error.contains("/checkout/target/debug/mj (deleted)"),
            "{error}"
        );
        assert!(error.contains("(/checkout/target/debug/mj)"), "{error}");
        assert!(error.contains("mj daemon restart"), "{error}");
    }
    #[tokio::test(start_paused = true)]
    async fn slow_startup_is_awaited_rather_than_reported_as_a_failure() {
        use std::cell::Cell;

        let attempts = Cell::new(0usize);
        let announced = Cell::new(None);
        let started = tokio::time::Instant::now();
        let outcome = wait_for_ready_daemon(
            || true,
            || {
                attempts.set(attempts.get() + 1);
                let ready = attempts.get() > 500;
                async move {
                    if ready {
                        Ok("client")
                    } else {
                        Err(anyhow!("connection refused"))
                    }
                }
            },
            |waited| announced.set(Some(waited)),
        )
        .await;
        assert!(matches!(outcome, StartupOutcome::Ready("client")));
        let waited = tokio::time::Instant::now().duration_since(started);
        assert!(
            waited > START_NOTICE_DELAY && waited < START_TIMEOUT,
            "the test must cross the notice delay without reaching the bound, but waited {waited:?}"
        );
        assert!(announced.get().is_some(), "a long wait must say so");
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_exits_is_reported_at_once() {
        use std::cell::Cell;

        let attempts = Cell::new(0usize);
        let started = tokio::time::Instant::now();
        let outcome: StartupOutcome<()> = wait_for_ready_daemon(
            || attempts.get() < 3,
            || {
                attempts.set(attempts.get() + 1);
                async { Err(anyhow!("connection refused")) }
            },
            |_| panic!("an exit must not wait long enough to announce itself"),
        )
        .await;
        assert!(
            matches!(outcome, StartupOutcome::Exited),
            "an exited daemon must be reported as exited"
        );
        assert!(tokio::time::Instant::now().duration_since(started) < START_NOTICE_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn an_alive_daemon_that_never_answers_stops_at_the_bound() {
        let started = tokio::time::Instant::now();
        let outcome: StartupOutcome<()> = wait_for_ready_daemon(
            || true,
            || async { Err(anyhow!("connection refused")) },
            |_| {},
        )
        .await;
        assert!(matches!(outcome, StartupOutcome::StillStarting { .. }));
        assert!(tokio::time::Instant::now().duration_since(started) >= START_TIMEOUT);
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

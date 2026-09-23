//! Application-owned daemon startup and attachment maintenance.
use mj_controller::controller::ControllerStoreGuard;
use mj_core::config::data_dir;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, anyhow, bail, ensure};
pub(crate) use mj_client::daemon::*;
pub(crate) use mj_client::executable::{
    describe_executable, process_executable_path, process_runs_this_executable,
    running_executable_path,
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
    let mut notice_at = Instant::now() + START_NOTICE_DELAY;
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
        if Instant::now() >= notice_at {
            eprintln!("Mjolnir is waiting for another client to finish the daemon handoff.");
            notice_at = Instant::now() + Duration::from_secs(30);
        }
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
    if let Ok(metadata) = tokio::task::spawn_blocking(read_metadata_any)
        .await
        .context("read daemon metadata task failed")?
    {
        ensure_supported_daemon_protocol(&metadata)?;
    }
    maybe_replace_stale_development_daemon().await?;
    if let Some(client) = prepare_existing_daemon().await? {
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
        if let Some(client) = prepare_existing_daemon().await? {
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
            command.env_remove("MJ_UPGRADE_RESUME_FILE");
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
            ping_daemon(&mut client).await?;
            check_store_readiness().await?;
            Ok(client)
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

async fn check_store_readiness() -> Result<()> {
    tokio::task::spawn_blocking(mj_controller::database::check_read_compatibility)
        .await
        .context("check database readiness task failed")?
}

async fn ping_daemon(client: &mut DaemonClient) -> Result<()> {
    let reply = tokio::time::timeout(Duration::from_secs(2), client.request(DaemonAction::Ping))
        .await
        .context("daemon did not answer its readiness probe")??;
    ensure!(
        matches!(reply, DaemonReply::Pong),
        "unexpected startup reply {reply:?}"
    );
    Ok(())
}

/// Local protocol readiness precedes the asynchronously started HTTP server.
/// API and desktop callers share this wait instead of asking users to retry.
pub async fn wait_for_web_viewer(client: &mut DaemonClient) -> Result<String> {
    wait_for_web_viewer_with_timeout(client, START_TIMEOUT).await
}

async fn wait_for_web_viewer_with_timeout(
    client: &mut DaemonClient,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(timeout, async {
        loop {
            match client.status().await?.phone_status {
                WebViewerStatus::Ready { viewer_url, .. } => return Ok(viewer_url),
                WebViewerStatus::Starting => tokio::time::sleep(RETRY_DELAY).await,
                WebViewerStatus::Disabled => bail!("the web viewer is disabled in config.toml"),
                WebViewerStatus::Stopped => bail!("the web viewer stopped during startup"),
                WebViewerStatus::Error { message } => {
                    bail!("the web viewer failed to start: {message}")
                }
            }
        }
    })
    .await
    .context("timed out waiting for the web viewer to become ready")?
}

fn daemon_release_order(metadata: &DaemonMetadata) -> Result<std::cmp::Ordering> {
    let daemon =
        semver::Version::parse(&metadata.build_version).context("parse daemon build version")?;
    let client =
        semver::Version::parse(env!("CARGO_PKG_VERSION")).context("parse client build version")?;
    Ok(daemon.cmp_precedence(&client))
}

/// Called only while holding the startup lock. Wire compatibility alone does
/// not imply application readiness: an older release may lack migrations or
/// fixes without changing the protocol. A same-version development build can
/// also need a migration. Only the replacement daemon may perform that work.
async fn prepare_existing_daemon() -> Result<Option<DaemonClient>> {
    let metadata = tokio::task::spawn_blocking(read_metadata_any)
        .await
        .context("read daemon metadata task failed")?;
    let Ok(metadata) = metadata else {
        return Ok(None);
    };
    ensure_supported_daemon_protocol(&metadata)?;
    let release_order = daemon_release_order(&metadata)?;
    if metadata.protocol_version < PROTOCOL_VERSION || release_order.is_lt() {
        ensure!(
            !release_order.is_gt(),
            "refusing to replace newer daemon {} with client {}",
            metadata.build_version,
            env!("CARGO_PKG_VERSION")
        );
        replace_daemon(&metadata).await?;
        return Ok(None);
    }
    let Ok(mut client) = DaemonClient::connect(metadata.clone()).await else {
        return Ok(None);
    };
    if ping_daemon(&mut client).await.is_err() {
        return Ok(None);
    }
    if let Err(error) = check_store_readiness().await {
        let needs_migration = error.chain().any(|cause| {
            cause
                .downcast_ref::<mj_core::storage::StoreSchemaMismatch>()
                .is_some_and(|mismatch| {
                    mismatch.reason == mj_core::storage::StoreSchemaMismatchReason::NeedsMigration
                })
        });
        if !needs_migration {
            return Err(error);
        }
        ensure!(
            !release_order.is_gt(),
            "a newer daemon has not completed database initialization: {error:#}"
        );
        replace_daemon(&metadata).await?;
        return Ok(None);
    }
    Ok(Some(client))
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
            stop_daemon(&metadata).await?;
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

/// Automatic upgrades have no authority to cancel work, even when a daemon
/// is slow or temporarily unreachable. Explicit restart uses `stop_daemon`.
async fn replace_daemon(metadata: &DaemonMetadata) -> Result<()> {
    let mut notice_at = Instant::now() + START_NOTICE_DELAY;
    loop {
        let previous = metadata.clone();
        let still_current = tokio::task::spawn_blocking(move || {
            process_is_alive(previous.pid)
                && !read_metadata_any().is_ok_and(|current| {
                    current.pid != previous.pid || current.token != previous.token
                })
        })
        .await
        .context("inspect daemon handoff owner")?;
        if !still_current {
            return Ok(());
        }
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            let mut client = DaemonClient::connect(metadata.clone()).await?;
            if daemon_admission_ignores_workers(metadata) {
                match client.request(DaemonAction::PrepareUpgrade).await? {
                    DaemonReply::Done => Ok(true),
                    DaemonReply::UpgradePending
                        if daemon_admission_counts_open_requests(metadata) =>
                    {
                        // The refusal may be only open HTTP requests, which
                        // this daemon wrongly counts as its own work. Hand off
                        // when they are all that remain, after the same
                        // observed lifecycle check as older daemons. This is
                        // an observed check, not atomic admission.
                        let Some(blockers) = upgrade_blockers(metadata).await else {
                            return Ok(false);
                        };
                        if !blockers.iter().all(|label| is_open_request_label(label)) {
                            return Ok(false);
                        }
                        let snapshot = client.runtime_snapshot(String::new(), 0, true).await?;
                        if !legacy_snapshot_is_idle(&snapshot) {
                            return Ok(false);
                        }
                        client.stop().await?;
                        Ok(true)
                    }
                    DaemonReply::UpgradePending => Ok(false),
                    reply => bail!("unexpected upgrade admission reply {reply:?}"),
                }
            } else {
                // These daemons cannot give a trustworthy atomic admission.
                // Inspect their own activity without opening or migrating
                // their store; this is an observed idle check only.
                let snapshot = client.runtime_snapshot(String::new(), 0, true).await?;
                if !legacy_snapshot_is_idle(&snapshot) {
                    return Ok(false);
                }
                client.stop().await?;
                Ok(true)
            }
        })
        .await;
        match ready {
            Ok(Ok(true)) => {
                // An acknowledged handoff is not permission to impose a kill
                // deadline. Keep following this process until it exits.
                if wait_for_exit(metadata.pid).await.is_ok() {
                    return Ok(());
                }
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "automatic upgrade cannot establish safe handoff yet")
            }
            Err(error) => {
                tracing::debug!(%error, "automatic upgrade is waiting for the daemon to answer")
            }
            Ok(Ok(false)) => {}
        }
        if Instant::now() >= notice_at {
            eprintln!(
                "{}",
                upgrade_wait_notice(metadata, upgrade_blockers(metadata).await)
            );
            notice_at = Instant::now() + Duration::from_secs(30);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Whether the daemon's `PrepareUpgrade` answer can be trusted.
///
/// Protocol 33 added atomic admission, but 2.17.0 shipped it with a gate that
/// also counted worker turns, reviews, and every session without a snapshot,
/// so one unreachable worker refuses the handoff forever. 2.18.0 narrowed the
/// gate to daemon-owned work without changing the protocol, so the build
/// version is the only way to tell them apart.
fn daemon_admission_ignores_workers(metadata: &DaemonMetadata) -> bool {
    metadata.protocol_version >= 33
        && semver::Version::parse(&metadata.build_version)
            .is_ok_and(|version| version >= semver::Version::new(2, 18, 0))
}

/// Whether the daemon counts every open HTTP request as upgrade work.
///
/// Through 2.20.0 the HTTP layer held an upgrade permit for each request and
/// response body, so a long `mj wait` or event read refused `PrepareUpgrade`
/// until it ended. Later daemons count only the operations those requests
/// start. The protocol did not change, so only the build version tells.
fn daemon_admission_counts_open_requests(metadata: &DaemonMetadata) -> bool {
    semver::Version::parse(&metadata.build_version)
        .is_ok_and(|version| version < semver::Version::new(2, 21, 0))
}

/// The label such a daemon gives its open HTTP requests, with or without a
/// count (`HTTP request x3`).
fn is_open_request_label(label: &str) -> bool {
    label == "HTTP request" || label.starts_with("HTTP request x")
}

/// The line shown while an automatic upgrade waits for the old daemon.
fn upgrade_wait_notice(metadata: &DaemonMetadata, blockers: Option<Vec<String>>) -> String {
    let mut blockers = blockers.unwrap_or_default();
    let mut open_requests = false;
    if daemon_admission_counts_open_requests(metadata) {
        blockers.retain(|label| {
            let request = is_open_request_label(label);
            open_requests |= request;
            !request
        });
    }
    let waiting = if blockers.is_empty() {
        "Mjolnir upgrade is waiting for ongoing work; existing sessions remain available."
            .to_owned()
    } else {
        format!(
            "Mjolnir upgrade is waiting for: {}; existing sessions remain available.",
            blockers.join(", ")
        )
    };
    if open_requests {
        format!(
            "{waiting} It does not wait for open HTTP requests: daemon {} counts them as work, \
             but they end when it stops and do not need to finish first.",
            metadata.build_version
        )
    } else {
        waiting
    }
}

/// Names the daemon-owned work holding the handoff open, for the wait notice.
/// A daemon that predates `UpgradeBlockers` fails the frame and closes the
/// connection; that, any other failure, and an empty answer all yield `None`,
/// and the caller falls back to the unnamed notice.
async fn upgrade_blockers(metadata: &DaemonMetadata) -> Option<Vec<String>> {
    let labels = tokio::time::timeout(Duration::from_secs(2), async {
        let mut client = DaemonClient::connect(metadata.clone()).await.ok()?;
        match client.request(DaemonAction::UpgradeBlockers).await.ok()? {
            DaemonReply::UpgradeBlockers(labels) => Some(labels),
            _ => None,
        }
    })
    .await
    .ok()??;
    (!labels.is_empty()).then_some(labels)
}

/// Whether a daemon without trustworthy admission can be replaced now.
///
/// The daemon is the control plane, so only its own lifecycle work blocks a
/// handoff: a lifecycle operation in flight, or a record that is provisioning,
/// checkpointing, closing, or being destroyed. Running and Disconnected
/// sessions live in workers that outlive the daemon, and reviews are agent
/// sessions in those same workers, so neither blocks.
fn legacy_snapshot_is_idle(snapshot: &RuntimeSnapshot) -> bool {
    use mj_core::state::SessionState;
    snapshot.lifecycles.is_empty()
        && !snapshot.records.iter().any(|record| {
            matches!(
                record.state,
                SessionState::Provisioning
                    | SessionState::Checkpointing
                    | SessionState::Closing
                    | SessionState::Destroying
            )
        })
}

async fn stop_daemon(metadata: &DaemonMetadata) -> Result<()> {
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

/// What the keep-alive last saw when it refreshed this client's presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonPresence {
    /// A daemon answered and knows this client is attached.
    Attached,
    /// No daemon answered. The string is the reason, for the person reading it.
    Missing(String),
    /// A newer installed daemon owns the instance. The terminal must preserve
    /// its local state and re-exec that build before reading the store again.
    Upgraded(UpgradeTarget),
}

/// The keep-alive task and what it reports about the daemon.
pub struct Attachment {
    pub task: tokio::task::JoinHandle<()>,
    pub presence: tokio::sync::watch::Receiver<DaemonPresence>,
}

/// Keep this client's presence fresh in whatever daemon is running.
///
/// This is a two-second timer, not a user action, so it only ever reconnects.
/// It deliberately does not start a daemon: a timer cannot express the intent
/// to start one, and when the client's own executable has been replaced on
/// disk the daemon it would start runs code the user has already discarded.
/// That is exactly how a stopped daemon used to come back on an old build,
/// beating the restart that had just stopped it. When no daemon answers, the
/// surface says so and offers its explicit restart instead.
pub fn maintain_attachment(
    client_id: String,
    pid: u32,
    cancellation: CancellationToken,
) -> Attachment {
    let (presence_tx, presence) = tokio::sync::watch::channel(DaemonPresence::Attached);
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    let observed = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = tokio::time::timeout(Duration::from_secs(5), observe_attachment(&client_id, pid)) => {
                            match result {
                                Ok(Ok(presence)) => presence,
                                result => {
                                    let reason = format!("could not refresh daemon attachment: {result:?}");
                                    tracing::warn!(%reason);
                                    DaemonPresence::Missing(reason)
                                }
                            }
                        }
                    };
                    // `send_if_modified` so a daemon that keeps answering does
                    // not wake the render loop every two seconds.
                    presence_tx.send_if_modified(|current| {
                        let changed = *current != observed;
                        if changed {
                            *current = observed;
                        }
                        changed
                    });
                }
            }
        }
    });
    Attachment { task, presence }
}

async fn observe_attachment(client_id: &str, pid: u32) -> Result<DaemonPresence> {
    if let Some(target) = tokio::task::spawn_blocking(upgraded_daemon_executable)
        .await
        .context("inspect upgraded daemon task failed")??
    {
        return Ok(DaemonPresence::Upgraded(target));
    }
    connect_existing()
        .await?
        .attach(client_id.to_owned(), pid)
        .await?;
    Ok(DaemonPresence::Attached)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeTarget {
    pub executable: PathBuf,
    pub generation: String,
}

fn upgraded_daemon_executable() -> Result<Option<UpgradeTarget>> {
    let Ok(metadata) = read_metadata_any() else {
        return Ok(None);
    };
    let newer = daemon_release_order(&metadata)?.is_gt();
    if !newer && metadata.protocol_version <= PROTOCOL_VERSION {
        return Ok(None);
    }
    let generation = format!("{}:{}", metadata.pid, metadata.started_at);
    // Do not loop if another installer replaced the executable again during handoff.
    if std::env::var("MJ_UPGRADE_DAEMON").ok().as_deref() == Some(&generation) {
        return Ok(None);
    }
    Ok(process_executable_path(metadata.pid)
        .filter(|path| path.is_file())
        .map(|executable| UpgradeTarget {
            executable,
            generation,
        }))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn legacy_record(id: &str, state: mj_core::state::SessionState) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "title": id,
            "harness_kind": "codex",
            "last_profile": "codex",
            "bundle_id": "project",
            "target_template_id": "podman",
            "state": state,
            "created_at": "2026-09-22T00:00:00Z",
            "updated_at": "2026-09-22T00:00:00Z",
        })
    }

    fn legacy_snapshot(records: Vec<serde_json::Value>) -> RuntimeSnapshot {
        serde_json::from_value(serde_json::json!({
            "revision": 1,
            "config": mj_core::config::Config::default(),
            "records": records,
            "sessions": [],
            "lifecycles": [],
        }))
        .expect("legacy snapshot fixture")
    }

    /// 2.17.0 answers `PrepareUpgrade` but its gate never releases while a
    /// worker is unreachable, so only later builds get the atomic handshake.
    #[test]
    fn only_daemons_with_the_narrowed_gate_use_atomic_admission() {
        let metadata = |protocol_version, build_version: &str| DaemonMetadata {
            protocol_version,
            pid: 1,
            address: "127.0.0.1:1".parse().unwrap(),
            token: "test".into(),
            started_at: "test".into(),
            build_version: build_version.into(),
        };
        assert!(!daemon_admission_ignores_workers(&metadata(32, "2.16.0")));
        assert!(!daemon_admission_ignores_workers(&metadata(33, "2.17.0")));
        assert!(daemon_admission_ignores_workers(&metadata(33, "2.18.0")));
        assert!(daemon_admission_ignores_workers(&metadata(33, "2.19.0")));
    }

    #[test]
    fn only_daemons_before_2_21_count_open_requests_as_upgrade_work() {
        let metadata = |build_version: &str| DaemonMetadata {
            protocol_version: 33,
            pid: 1,
            address: "127.0.0.1:1".parse().unwrap(),
            token: "test".into(),
            started_at: "test".into(),
            build_version: build_version.into(),
        };
        assert!(daemon_admission_counts_open_requests(&metadata("2.19.0")));
        assert!(daemon_admission_counts_open_requests(&metadata("2.20.0")));
        assert!(!daemon_admission_counts_open_requests(&metadata("2.21.0")));
        assert!(is_open_request_label("HTTP request"));
        assert!(is_open_request_label("HTTP request x4"));
        assert!(!is_open_request_label("session lifecycle"));
    }

    #[test]
    fn the_wait_notice_names_lifecycle_work_and_says_why_requests_do_not_count() {
        let metadata = |build_version: &str| DaemonMetadata {
            protocol_version: 33,
            pid: 1,
            address: "127.0.0.1:1".parse().unwrap(),
            token: "test".into(),
            started_at: "test".into(),
            build_version: build_version.into(),
        };
        let labels = |labels: &[&str]| Some(labels.iter().map(|l| (*l).to_owned()).collect());
        assert_eq!(
            upgrade_wait_notice(
                &metadata("2.19.0"),
                labels(&["HTTP request x2", "session lifecycle"])
            ),
            "Mjolnir upgrade is waiting for: session lifecycle; existing sessions remain available. \
             It does not wait for open HTTP requests: daemon 2.19.0 counts them as work, \
             but they end when it stops and do not need to finish first."
        );
        assert_eq!(
            upgrade_wait_notice(&metadata("2.21.0"), labels(&["session lifecycle"])),
            "Mjolnir upgrade is waiting for: session lifecycle; existing sessions remain available."
        );
        assert_eq!(
            upgrade_wait_notice(&metadata("2.21.0"), None),
            "Mjolnir upgrade is waiting for ongoing work; existing sessions remain available."
        );
    }

    /// A pre-33 daemon is replaceable while workers are busy: only its own
    /// lifecycle work blocks the handoff.
    #[test]
    fn legacy_idle_ignores_worker_state_and_reviews() {
        use mj_core::state::SessionState;
        // A Running record with no session view at all is the busiest case the
        // old check could see: it could not confirm the worker was idle.
        let mut busy = legacy_snapshot(vec![
            legacy_record("running", SessionState::Running),
            legacy_record("gone", SessionState::Disconnected),
        ]);
        busy.reviews.push(mj_client::review::RuntimeReviewView {
            session_id: "running".into(),
            tier: mj_core::review::lanes::ReviewTier::Quick,
            phase: mj_core::review::driver::TurnReviewPhase::CapturingDelta,
            roles: Vec::new(),
            status: "reviewing".into(),
            verdict: None,
        });
        assert!(
            legacy_snapshot_is_idle(&busy),
            "worker turns and reviews survive a daemon handoff"
        );

        let closing = legacy_snapshot(vec![legacy_record("closing", SessionState::Closing)]);
        assert!(
            !legacy_snapshot_is_idle(&closing),
            "a daemon-owned lifecycle state still blocks the handoff"
        );
    }

    async fn viewer_fixture(
        statuses: Vec<WebViewerStatus>,
    ) -> (DaemonClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address: listener.local_addr().unwrap(),
            token: "test".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
        };
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut statuses = statuses.into_iter();
            while let Ok(request) = read_frame::<RequestEnvelope>(&mut stream).await {
                assert!(matches!(request.action, DaemonAction::Status));
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id: request.request_id,
                        result: Ok(DaemonReply::Status(DaemonStatus {
                            pid: 1,
                            started_at: "test".into(),
                            build_version: "test".into(),
                            attached_clients: 0,
                            phone_status: statuses.next().unwrap_or(WebViewerStatus::Starting),
                        })),
                    },
                )
                .await
                .unwrap();
            }
        });
        (DaemonClient::connect(metadata).await.unwrap(), task)
    }

    #[tokio::test]
    async fn api_and_desktop_wait_for_the_viewer_without_a_retry_command() {
        let (mut client, task) = viewer_fixture(vec![
            WebViewerStatus::Starting,
            WebViewerStatus::Starting,
            WebViewerStatus::Ready {
                viewer_url: "http://127.0.0.1:1234".into(),
                viewer_code: "test".into(),
                qr_login_url: None,
                fallback_reason: None,
            },
        ])
        .await;
        assert_eq!(
            wait_for_web_viewer(&mut client).await.unwrap(),
            "http://127.0.0.1:1234"
        );
        drop(client);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn viewer_startup_reports_failure_and_bounds_a_stalled_start() {
        let (mut client, task) = viewer_fixture(vec![
            WebViewerStatus::Starting,
            WebViewerStatus::Error {
                message: "port unavailable".into(),
            },
        ])
        .await;
        assert!(
            wait_for_web_viewer(&mut client)
                .await
                .unwrap_err()
                .to_string()
                .contains("port unavailable")
        );
        drop(client);
        task.await.unwrap();
        let (mut client, task) = viewer_fixture(vec![]).await;
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_millis(100);
        assert!(
            wait_for_web_viewer_with_timeout(&mut client, timeout)
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(start.elapsed() >= timeout);
        drop(client);
        task.await.unwrap();
    }
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
    /// The keep-alive is a timer, not a user action. It used to call
    /// `connect_or_start`, which is how a daemon stopped by a restart came
    /// back on the attached client's older build before the restart could
    /// start its own.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_attachment_keep_alive_reports_a_missing_daemon_without_starting_one() {
        const CHILD: &str = "MJ_TEST_KEEP_ALIVE_NEVER_STARTS_A_DAEMON";
        const TEST: &str = "daemon::tests::the_attachment_keep_alive_reports_a_missing_daemon_without_starting_one";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        assert!(
            !metadata_path().exists(),
            "this isolated store starts without a daemon"
        );
        let cancellation = CancellationToken::new();
        let attachment = maintain_attachment(
            "keep-alive-test".to_owned(),
            std::process::id(),
            cancellation.clone(),
        );
        let mut presence = attachment.presence.clone();
        tokio::time::timeout(Duration::from_secs(20), presence.changed())
            .await
            .expect("the keep-alive reports within a few ticks")
            .expect("the keep-alive is still running");
        assert!(
            matches!(&*presence.borrow(), DaemonPresence::Missing(_)),
            "a missing daemon must be reported, not replaced"
        );
        cancellation.cancel();
        attachment.task.await.unwrap();
        // Starting a daemon begins by taking the startup lock, which creates
        // this file. Its absence is the proof that no start was attempted;
        // the missing metadata alone would only prove none succeeded.
        assert!(
            !data_dir().join("daemon-start.lock").exists(),
            "the keep-alive must never attempt to start a daemon"
        );
        assert!(
            !metadata_path().exists(),
            "the keep-alive must never start a daemon"
        );
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

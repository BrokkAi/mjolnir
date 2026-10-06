//! Application-owned daemon startup and attachment maintenance.
use mj_controller::controller::ControllerStoreGuard;
use mj_core::config::data_dir;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, anyhow, bail, ensure};
use mj_client::build_identity::{
    BuildIdentity, DaemonBuildOrder, compare_daemon_build, this_build,
};
pub(crate) use mj_client::daemon::*;
pub(crate) use mj_client::executable::{
    describe_executable, describe_running_daemon_and_client_builds, process_executable_path,
    process_runs_this_executable, running_executable_path,
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
/// Large stores can take more than a minute to rebuild a table during an
/// upgrade. Keep startup bounded while allowing those migrations to finish.
const START_TIMEOUT: Duration = Duration::from_secs(300);
/// Where slow-startup notices go while a splash owns the terminal. `None`
/// means stderr, as for every command that has no splash.
static STARTUP_NOTICES: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>> =
    std::sync::Mutex::new(None);

/// Reports startup that is taking a while. A splash on the alternate screen
/// shows the message itself, since stderr would scribble over its frame.
fn startup_notice(message: String) {
    let route = STARTUP_NOTICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let undelivered = match route.as_ref() {
        Some(splash) => splash.send(message).err().map(|error| error.0),
        None => Some(message),
    };
    if let Some(message) = undelivered {
        eprintln!("{message}");
    }
}

/// Sends startup notices to the returned receiver until the route drops.
pub(crate) struct StartupNoticeRoute(());

impl StartupNoticeRoute {
    pub(crate) fn open() -> (Self, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        *STARTUP_NOTICES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
        (Self(()), receiver)
    }
}

impl Drop for StartupNoticeRoute {
    fn drop(&mut self) {
        STARTUP_NOTICES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// The daemon startup lock held exclusively: the only hold under which a
/// client may start, stop or replace the daemon.
#[derive(Debug)]
struct DaemonStartGuard {
    _lock: StartLock,
}

/// The daemon startup lock held shared: enough to use the running daemon,
/// never to start or replace one. Any number of clients hold it at once, and
/// none of them while a client holds it exclusively.
#[derive(Debug)]
struct SharedStartGuard {
    _lock: StartLock,
}

#[derive(Debug)]
struct StartLock(fs::File);

impl Drop for StartLock {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "could not release daemon startup lock");
        }
    }
}

async fn acquire_start_guard(path: PathBuf) -> Result<DaemonStartGuard> {
    acquire_start_lock(path, false)
        .await
        .map(|lock| DaemonStartGuard { _lock: lock })
}

async fn acquire_shared_start_guard(path: PathBuf) -> Result<SharedStartGuard> {
    acquire_start_lock(path, true)
        .await
        .map(|lock| SharedStartGuard { _lock: lock })
}

async fn acquire_start_lock(path: PathBuf, shared: bool) -> Result<StartLock> {
    let asked = Instant::now();
    let mut notice_at = asked + START_NOTICE_DELAY;
    loop {
        let path = path.clone();
        let guard = tokio::task::spawn_blocking(move || -> Result<Option<StartLock>> {
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
            let locked = if shared {
                file.try_lock_shared()
            } else {
                file.try_lock()
            };
            match locked {
                Ok(()) => Ok(Some(StartLock(file))),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(error)) => {
                    Err(error).context("lock daemon startup")
                }
            }
        })
        .await
        .context("daemon startup lock task failed")??;
        if let Some(guard) = guard {
            let waited = asked.elapsed();
            if waited >= LOGGED_PHASE {
                tracing::info!(
                    waited_ms = waited.as_millis(),
                    shared,
                    "waited for the daemon startup lock another client held"
                );
            }
            return Ok(guard);
        }
        if Instant::now() >= notice_at {
            startup_notice(shared_handoff_notice(running_daemon_blockers().await));
            notice_at = Instant::now() + Duration::from_secs(30);
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// The line shown while another client holds the startup lock. When that
/// client is replacing the daemon, the running daemon names what the handoff
/// is waiting for, so this client can say it too.
fn shared_handoff_notice(blockers: Option<Vec<String>>) -> String {
    match blockers {
        Some(blockers) => format!(
            "Mjolnir is waiting for another client to finish the daemon handoff; the running daemon is finishing: {}.",
            blockers.join(", ")
        ),
        None => "Mjolnir is waiting for another client to finish the daemon handoff.".to_owned(),
    }
}

/// Whether daemon `pid` has exited or is no longer the published daemon,
/// as after an automatic upgrade replaced it.
pub(crate) async fn daemon_was_replaced(pid: u32) -> bool {
    tokio::task::spawn_blocking(move || {
        !process_is_alive(pid) || read_metadata_any().map_or(true, |current| current.pid != pid)
    })
    .await
    .unwrap_or(false)
}

/// What the running daemon, if any, reports as holding a handoff open.
async fn running_daemon_blockers() -> Option<Vec<String>> {
    let metadata = tokio::task::spawn_blocking(read_metadata_any)
        .await
        .ok()?
        .ok()?;
    upgrade_blockers(&metadata).await
}

/// Startup phases shorter than this are not logged; longer ones always are,
/// so a slow replacement leaves a record of where its time went.
const LOGGED_PHASE: Duration = Duration::from_millis(250);

pub async fn connect_or_start() -> Result<DaemonClient> {
    let lock_path = data_dir().join("daemon-start.lock");
    // Most clients find a daemon they can use. Deciding that needs only a
    // shared hold, so a burst of clients reconnecting after a handoff checks
    // the daemon side by side instead of one at a time, which multiplied each
    // check by the number of clients waiting. A shared hold still waits out a
    // client that holds the lock to start or replace the daemon, so every
    // waiter uses the daemon that client publishes.
    if std::env::var_os(DEV_RESTART_STALE_DAEMON_ENV).is_none() {
        let shared = acquire_shared_start_guard(lock_path.clone()).await?;
        if let ExistingDaemon::Use(client) = inspect_existing_daemon(&shared).await? {
            return Ok(client);
        }
    }
    // Starting or replacing the daemon is serialized across clients, and the
    // decision is taken again under the exclusive hold: another client may
    // have published a daemon since the shared check.
    let startup = acquire_start_guard(lock_path).await?;
    let held = Instant::now();
    let connected = connect_or_start_holding(&startup).await;
    // Every other client waits while this one holds the lock, so how long it
    // held it is what they waited for.
    let held = held.elapsed();
    if held >= LOGGED_PHASE {
        tracing::info!(held_ms = held.as_millis(), "held the daemon startup lock");
    }
    connected
}

/// The body of [`connect_or_start`] for a caller that already holds the
/// daemon startup lock.
///
/// That lock is not reentrant within one process, so a caller that needs it
/// held across more than one step — a restart holds it across the stop and the
/// replacement — must come through here instead of calling
/// [`connect_or_start`] again. The guard is taken by reference only so the
/// requirement is visible at every call site.
async fn connect_or_start_holding(startup: &DaemonStartGuard) -> Result<DaemonClient> {
    if let Ok(metadata) = tokio::task::spawn_blocking(read_metadata_any)
        .await
        .context("read daemon metadata task failed")?
    {
        ensure_supported_daemon_protocol(&metadata)?;
    }
    maybe_replace_stale_development_daemon().await?;
    if let Some(client) = prepare_existing_daemon(startup).await? {
        return Ok(client);
    }

    // Metadata disappears before shutdown releases the sole-writer lock.
    // Do not launch a child that can only fail to acquire that lock.
    let released = Instant::now();
    let handoff_deadline = released + STOP_TIMEOUT;
    loop {
        if let Some(guard) = tokio::task::spawn_blocking(ControllerStoreGuard::try_acquire)
            .await
            .context("probe controller ownership task failed")??
        {
            drop(guard);
            let waited = released.elapsed();
            if waited >= LOGGED_PHASE {
                tracing::info!(
                    waited_ms = waited.as_millis(),
                    "waited for the previous daemon to release the store"
                );
            }
            break;
        }
        // A daemon started outside this client's startup lock may be becoming ready.
        if let Some(client) = prepare_existing_daemon(startup).await? {
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
            // The program runs through `/proc/self/exe` on Linux, which `ps`
            // would otherwise show as the daemon's name. `mj daemon-run` is
            // what a person looks for (launch finding R13-9); nothing reads
            // the daemon's argv[0], and the checks that recognize a daemon
            // read argv[1].
            #[cfg(unix)]
            std::os::unix::process::CommandExt::arg0(&mut command, "mj");
            let pid = mj_core::subprocess::spawn_detached(&mut command, &log_path)?;
            Ok(LaunchedDaemon { pid, log_offset })
        }
    })
    .await
    .context("spawn daemon task failed")??;
    let launched_at = Instant::now();
    tracing::info!(pid = launched.pid, "launched the daemon");

    let outcome = wait_for_ready_daemon(
        || process_is_alive(launched.pid),
        || async {
            let mut client = connect_existing().await?;
            ping_daemon(&mut client).await?;
            check_store_readiness().await?;
            Ok(client)
        },
        |waited| {
            startup_notice(format!(
                "Mjolnir daemon {} has been starting for {}s; still waiting.",
                launched.pid,
                waited.as_secs()
            ));
        },
    )
    .await;
    let reason = match outcome {
        StartupOutcome::Ready(client) => {
            tracing::info!(
                pid = launched.pid,
                ready_ms = launched_at.elapsed().as_millis(),
                "the launched daemon is ready"
            );
            return Ok(client);
        }
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
    let (log_path, output) = launched.output_since_launch(&log_path).await;
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

/// How the running daemon's build stands against this client's.
#[derive(Debug, Clone, Copy)]
struct DaemonBuild {
    /// The release versions alone, for the steps that apply only across
    /// releases: replacing an older release before connecting to it, and an
    /// attached client's re-execution into an upgraded release.
    release: std::cmp::Ordering,
    /// The full order: release, then commit time, then executable time.
    order: DaemonBuildOrder,
}

impl DaemonBuild {
    fn daemon_is_newer(self) -> bool {
        self.order == DaemonBuildOrder::Newer
    }
}

/// The one place a client decides whether the running daemon is newer than
/// itself. Ordinary startup, the development refresh and `mj daemon restart`
/// all ask here while they hold the startup lock, so the answer they act on
/// is the one for the daemon that lock lets them replace.
fn daemon_build(metadata: &DaemonMetadata) -> Result<DaemonBuild> {
    let daemon =
        BuildIdentity::parse(&metadata.build_version).context("parse daemon build version")?;
    let client = this_build();
    Ok(DaemonBuild {
        release: daemon.version().cmp_precedence(client.version()),
        order: compare_daemon_build(&daemon, client),
    })
}

/// The sentence naming both builds, followed by what this client does.
fn daemon_build_notice(metadata: &DaemonMetadata, decision: &str) -> String {
    format!(
        "{}. {decision}",
        describe_running_daemon_and_client_builds(metadata.pid, &metadata.build_version)
    )
}

/// Say once per process that this client keeps using a daemon it will not
/// replace. Every command, and every dashboard action, comes through startup,
/// and the reason does not change between them.
fn kept_daemon_notice(message: String) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        startup_notice(message);
    } else {
        tracing::info!("{message}");
    }
}

/// What the running daemon, if any, is to this client.
enum ExistingDaemon {
    /// None answers, so one has to be started.
    Absent,
    /// It answers and this client may use it.
    Use(DaemonClient),
    /// It answers but has to be replaced first. `notice` is what to tell the
    /// person when the replacement starts.
    Replace {
        metadata: DaemonMetadata,
        notice: Option<String>,
    },
}

/// The one decision about the running daemon. Wire compatibility alone does
/// not imply application readiness: an older release may lack migrations or
/// fixes without changing the protocol. A same-version development build can
/// also need a migration. Only the replacement daemon may perform that work.
///
/// Deciding needs the startup lock in either mode, so the daemon it looks at
/// is not being started or replaced meanwhile. Only an exclusive holder may
/// act on [`ExistingDaemon::Replace`], through [`prepare_existing_daemon`].
async fn inspect_existing_daemon(_held: &SharedStartGuard) -> Result<ExistingDaemon> {
    decide_existing_daemon().await
}

async fn decide_existing_daemon() -> Result<ExistingDaemon> {
    let metadata = tokio::task::spawn_blocking(read_metadata_any)
        .await
        .context("read daemon metadata task failed")?;
    let Ok(metadata) = metadata else {
        return Ok(ExistingDaemon::Absent);
    };
    ensure_supported_daemon_protocol(&metadata)?;
    let build = daemon_build(&metadata)?;
    if metadata.protocol_version < PROTOCOL_VERSION || build.release.is_lt() {
        ensure!(
            !build.daemon_is_newer(),
            "refusing to replace a newer daemon with this client: {}",
            daemon_build_notice(&metadata, "Run the daemon's build instead.")
        );
        return Ok(ExistingDaemon::Replace {
            metadata,
            notice: None,
        });
    }
    let Ok(mut client) = DaemonClient::connect(metadata.clone()).await else {
        return Ok(ExistingDaemon::Absent);
    };
    if ping_daemon(&mut client).await.is_err() {
        return Ok(ExistingDaemon::Absent);
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
            !build.daemon_is_newer(),
            "a newer daemon has not completed database initialization: {error:#}"
        );
        return Ok(ExistingDaemon::Replace {
            metadata,
            notice: None,
        });
    }
    match build.order {
        DaemonBuildOrder::Same => {}
        DaemonBuildOrder::Older => {
            let notice = daemon_build_notice(
                &metadata,
                "This client's build is newer; replacing the daemon.",
            );
            return Ok(ExistingDaemon::Replace {
                metadata,
                notice: Some(notice),
            });
        }
        // Same release, protocol and readable store: the older client can use
        // the newer daemon, and must not replace it.
        DaemonBuildOrder::Newer => kept_daemon_notice(daemon_build_notice(
            &metadata,
            "The daemon's build is newer, so it keeps running and this client uses it.",
        )),
        DaemonBuildOrder::Unknown => kept_daemon_notice(daemon_build_notice(
            &metadata,
            "Mjolnir cannot tell which build is newer, so the daemon keeps running and this \
             client uses it. Run `mj daemon restart` to replace it with this build.",
        )),
    }
    Ok(ExistingDaemon::Use(client))
}

/// Act on [`decide_existing_daemon`] while holding the startup lock
/// exclusively: use the running daemon, or replace it and report that none is
/// running yet.
async fn prepare_existing_daemon(_startup: &DaemonStartGuard) -> Result<Option<DaemonClient>> {
    match decide_existing_daemon().await? {
        ExistingDaemon::Absent => Ok(None),
        ExistingDaemon::Use(client) => Ok(Some(client)),
        ExistingDaemon::Replace { metadata, notice } => {
            match notice {
                Some(notice) => startup_notice(notice),
                None => tracing::info!(
                    daemon_protocol = metadata.protocol_version,
                    client_protocol = PROTOCOL_VERSION,
                    daemon_build = %metadata.build_version,
                    "the daemon is an older protocol or release, or its store needs a migration; replacing it"
                ),
            }
            replace_daemon(&metadata).await?;
            Ok(None)
        }
    }
}

/// Refuse to stop a daemon when the one that would replace it cannot read the
/// configuration and would exit at startup, leaving the instance stopped.
/// `action` finishes "The Mjolnir daemon was not ...".
fn ensure_config_loads(config_path: &Path, action: &str) -> Result<()> {
    mj_core::config::Config::load_from(config_path).map(|_| ()).map_err(|error| {
        anyhow!(
            "{error:#}. The Mjolnir daemon was not {action}; fix the configuration and try again."
        )
    })
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
    mj_core::config::ensure_may_control_store(&data_dir(), "restart the Mjolnir daemon")?;
    // A daemon that cannot read its configuration exits at once. Find that out
    // while the old one still runs.
    ensure_config_loads(&mj_core::config::config_path(), "restarted")?;
    let startup = acquire_start_guard(data_dir().join("daemon-start.lock")).await?;
    for attempt in 1..=RESTART_ATTEMPTS {
        if let Ok(metadata) = read_metadata_any() {
            // An explicit restart may replace a build it cannot order, but
            // never a newer one: that is a downgrade, which `mj daemon stop`
            // makes deliberate.
            ensure!(
                !daemon_build(&metadata)?.daemon_is_newer(),
                "{}",
                daemon_build_notice(
                    &metadata,
                    "The daemon's build is newer, so it was not restarted. Run `mj daemon \
                     restart` from the daemon's build, or run `mj daemon stop` first to \
                     start this older build instead."
                )
            );
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
    /// Include process diagnostics and detached stderr. Failures before logger
    /// initialization have only stderr; later failures also have a process log.
    async fn output_since_launch(&self, log_path: &Path) -> (PathBuf, String) {
        let log_path = log_path.to_path_buf();
        let fallback_path = log_path.clone();
        let offset = self.log_offset;
        let pid = self.pid;
        let output = tokio::task::spawn_blocking(move || -> Result<(PathBuf, String)> {
            let directory = log_path
                .parent()
                .context("daemon stderr path has no parent")?;
            let diagnostics = crate::logging::daemon_log_path(directory, pid)?;
            let mut output = String::new();
            if let Some(path) = &diagnostics {
                output.push_str(&launch_log_tail(path, 0)?);
            }
            let stderr = launch_log_tail(&log_path, offset)?;
            if !stderr.is_empty() {
                if !output.is_empty() {
                    output.push_str("\nDaemon stderr:\n");
                }
                output.push_str(&stderr);
            }
            Ok((diagnostics.unwrap_or(log_path), output))
        })
        .await;
        match output {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                tracing::warn!(%error, "could not read the daemon logs after a failed launch");
                (
                    fallback_path,
                    format!("could not read daemon logs: {error:#}"),
                )
            }
            Err(error) => {
                tracing::warn!(%error, "daemon log read task failed");
                (
                    fallback_path,
                    format!("daemon log read task failed: {error}"),
                )
            }
        }
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

fn launch_log_tail(path: &Path, offset: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        fs::File::open(path).with_context(|| format!("open daemon log {}", path.display()))?;
    // Keep both memory and failure messages bounded during a noisy startup.
    let start = offset.max(file.metadata()?.len().saturating_sub(64 * 1024));
    file.seek(SeekFrom::Start(start))?;
    let mut appended = Vec::new();
    file.take(64 * 1024).read_to_end(&mut appended)?;
    let text = String::from_utf8_lossy(&appended);
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    Ok(lines[lines.len().saturating_sub(20)..].join("\n"))
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
            // The variable makes this client authoritative over stale builds,
            // not over newer ones. Ordinary startup explains the kept daemon.
            if daemon_build(&metadata)?.daemon_is_newer() {
                return Ok(());
            }
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
            startup_notice(format!(
                "Mjolnir daemon {} is using an older development build; restarting it.",
                metadata.pid
            ));
            replace_daemon(&metadata).await
        })
        .await?;
    Ok(())
}

/// Automatic upgrades have no authority to cancel work, even when a daemon
/// is slow or temporarily unreachable. Explicit restart uses `stop_daemon`.
async fn replace_daemon(metadata: &DaemonMetadata) -> Result<()> {
    mj_core::config::ensure_may_control_store(
        &data_dir(),
        &format!(
            "replace Mjolnir daemon {} (build {})",
            metadata.pid, metadata.build_version
        ),
    )?;
    ensure_config_loads(&mj_core::config::config_path(), "replaced")?;
    // The replacement daemon pins this build's worker sources before it can
    // serve; for a new build that is seconds of copying and hashing. Do it
    // while the old daemon drains, and wait for it before the launch so the
    // two never hash the same files at once.
    let warming = tokio::task::spawn_blocking(|| {
        let started = Instant::now();
        (
            mj_controller::controller::warm_worker_binary_sources(),
            started.elapsed(),
        )
    });
    let handed_off = hand_off_daemon(metadata).await;
    match warming.await {
        Ok((Ok(()), took)) if took >= LOGGED_PHASE => tracing::info!(
            duration_ms = took.as_millis(),
            "pinned this build's worker sources during the handoff"
        ),
        Ok((Ok(()), _)) => {}
        // Startup pins the sources again and reports its own failure; this
        // one only says the head start was lost.
        Ok((Err(error), _)) => tracing::warn!(
            error = format!("{error:#}"),
            "could not pin this build's worker sources during the handoff"
        ),
        Err(error) => tracing::warn!(%error, "worker source pinning task failed"),
    }
    handed_off
}

/// Ask the daemon to hand off and follow it until it exits.
async fn hand_off_daemon(metadata: &DaemonMetadata) -> Result<()> {
    let asked = Instant::now();
    let mut notice_at = asked + START_NOTICE_DELAY;
    let mut attempts = 0_u32;
    // The latest reason the daemon has not accepted, reported with each notice.
    let mut not_yet = String::new();
    tracing::info!(
        pid = metadata.pid,
        build = %metadata.build_version,
        "asking the daemon to hand off to this build"
    );
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
            tracing::info!(
                pid = metadata.pid,
                waited_ms = asked.elapsed().as_millis(),
                "the daemon was replaced by another client"
            );
            return Ok(());
        }
        attempts += 1;
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            let mut client = DaemonClient::connect(metadata.clone()).await?;
            match client.request(DaemonAction::PrepareUpgrade).await? {
                DaemonReply::Done => Ok(true),
                DaemonReply::UpgradePending => Ok(false),
                reply => bail!("unexpected upgrade admission reply {reply:?}"),
            }
        })
        .await;
        match ready {
            Ok(Ok(true)) => {
                let accepted = Instant::now();
                tracing::info!(
                    pid = metadata.pid,
                    attempts,
                    waited_ms = asked.elapsed().as_millis(),
                    "the daemon accepted the handoff"
                );
                // An acknowledged handoff is not permission to impose a kill
                // deadline. Keep following this process until it exits.
                let exited = wait_for_exit(metadata.pid).await;
                tracing::info!(
                    pid = metadata.pid,
                    exited = exited.is_ok(),
                    exit_ms = accepted.elapsed().as_millis(),
                    "waited for the handed-off daemon to exit"
                );
                if exited.is_ok() {
                    return Ok(());
                }
            }
            Ok(Err(error)) => {
                not_yet = format!("{error:#}");
                tracing::debug!(%error, "automatic upgrade cannot establish safe handoff yet")
            }
            Err(error) => {
                not_yet = format!("the daemon did not answer within 5s ({error})");
                tracing::debug!(%error, "automatic upgrade is waiting for the daemon to answer")
            }
            Ok(Ok(false)) => not_yet = "the daemon is finishing accepted work".to_owned(),
        }
        if Instant::now() >= notice_at {
            tracing::info!(
                pid = metadata.pid,
                attempts,
                waited_ms = asked.elapsed().as_millis(),
                reason = %not_yet,
                "the daemon has not accepted the handoff yet"
            );
            startup_notice(upgrade_wait_notice(upgrade_blockers(metadata).await));
            notice_at = Instant::now() + Duration::from_secs(30);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The line shown while an automatic upgrade waits for the old daemon.
fn upgrade_wait_notice(blockers: Option<Vec<String>>) -> String {
    match blockers {
        Some(blockers) => format!(
            "Mjolnir upgrade is waiting for: {}; existing sessions remain available.",
            blockers.join(", ")
        ),
        None => "Mjolnir upgrade is waiting for ongoing work; existing sessions remain available."
            .to_owned(),
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

/// Stop the daemon `metadata` names after it did not answer a protocol
/// stop: SIGTERM on Unix, which every supported daemon handles as graceful
/// cancellation. Windows has no SIGTERM, and the daemon has no console to
/// receive Ctrl-Break, so it is terminated; its state is in the store and
/// its workers keep running, so the next daemon recovers it as after a crash.
async fn signal_daemon(metadata: &DaemonMetadata) -> Result<()> {
    #[cfg(unix)]
    {
        if !daemon_still_runs(metadata.pid)? {
            return Ok(());
        }
        // SAFETY: the PID comes from owner-only daemon metadata and SIGTERM is
        // handled as graceful cancellation by every supported daemon.
        let result = unsafe { libc::kill(metadata.pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("stop superseded Mjolnir daemon");
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        // Windows does not reuse a PID while a handle to its process is
        // open, so the process checked below is the one terminated.
        // SAFETY: OpenProcess takes no pointers; a null handle is checked.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, metadata.pid) };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            // Gone already, or not ours to stop.
            if !daemon_still_runs(metadata.pid)? {
                return Ok(());
            }
            return Err(error).context("open superseded Mjolnir daemon");
        }
        struct Handle(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Handle {
            fn drop(&mut self) {
                // SAFETY: the handle came from OpenProcess and is closed once.
                unsafe { CloseHandle(self.0) };
            }
        }
        let handle = Handle(handle);
        if !daemon_still_runs(metadata.pid)? {
            return Ok(());
        }
        // SAFETY: the handle is open with PROCESS_TERMINATE.
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error()).context("stop superseded Mjolnir daemon");
        }
    }
    wait_for_exit(metadata.pid).await.with_context(|| {
        format!(
            "superseded Mjolnir daemon {} was stopped but was still running after {}s",
            metadata.pid,
            STOP_TIMEOUT.as_secs()
        )
    })
}

/// Whether `pid` still runs: true when it runs `mj daemon-run`, false when
/// it is gone, and an error for any other process. The PID comes from the
/// owner-only metadata file; the argv check guards against PID recycling,
/// not against other Mjolnir builds — old daemons are exactly what a restart
/// retires.
fn daemon_still_runs(pid: u32) -> Result<bool> {
    let process_id = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    // The command line is read only when the refresh asks for it.
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[process_id]),
        true,
        sysinfo::ProcessRefreshKind::new().with_cmd(sysinfo::UpdateKind::Always),
    );
    let Some(process) = system.process(process_id) else {
        return Ok(false);
    };
    ensure!(
        process
            .cmd()
            .get(1)
            .is_some_and(|argument| argument == "daemon-run"),
        "refusing to stop PID {pid} because it does not look like a Mjolnir daemon (`mj daemon-run`)"
    );
    Ok(true)
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

/// Finish a one-shot command under the daemon's newer build, as a dashboard
/// does when an upgrade replaces its daemon. `args` name only what is left to
/// do, so nothing the command already did is repeated. Returns only when the
/// newer build could not be started.
pub(crate) fn continue_under_upgraded_build(
    target: &UpgradeTarget,
    args: &[String],
) -> anyhow::Error {
    tracing::info!(
        executable = %target.executable.display(),
        ?args,
        "continuing this command under the daemon's newer build"
    );
    eprintln!(
        "Mjolnir upgraded; continuing with {}.",
        target.executable.display()
    );
    let mut command = std::process::Command::new(&target.executable);
    command
        .args(args)
        .env("MJ_UPGRADE_DAEMON", &target.generation)
        .env("MJOLNIR_NO_UPDATE_CHECK", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        anyhow::Error::new(command.exec()).context(format!(
            "continue under the upgraded build {}",
            target.executable.display()
        ))
    }
    #[cfg(not(unix))]
    {
        match mj_core::subprocess::run_interactive(&mut command) {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => error.context(format!(
                "continue under the upgraded build {}",
                target.executable.display()
            )),
        }
    }
}

pub(crate) fn upgraded_daemon_executable() -> Result<Option<UpgradeTarget>> {
    let Ok(metadata) = read_metadata_any() else {
        return Ok(None);
    };
    let newer = daemon_build(&metadata)?.release.is_gt();
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
                let Some(phone_status) = statuses.next() else {
                    // An exhausted script deliberately leaves this request
                    // unanswered. The timed-out client closes it; no response
                    // write can race with that expected disconnect.
                    use tokio::io::AsyncReadExt;
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                    break;
                };
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
                            phone_status,
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
    /// Clients that only use the running daemon check it side by side, while a
    /// client starting or replacing the daemon excludes all of them, and
    /// they it. This is what lets a burst of reconnecting clients proceed
    /// together yet still wait for a handoff to publish its daemon.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shared_holds_overlap_and_exclude_a_replacing_client() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon-start.lock");
        let first = acquire_shared_start_guard(path.clone()).await.unwrap();
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            acquire_shared_start_guard(path.clone()),
        )
        .await
        .expect("a second shared hold waited for the first")
        .unwrap();
        let mut replacing = tokio::spawn(acquire_start_guard(path.clone()));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut replacing)
                .await
                .is_err(),
            "an exclusive hold was granted beside shared ones"
        );
        drop((first, second));
        let replacing = tokio::time::timeout(Duration::from_secs(2), replacing)
            .await
            .expect("the exclusive hold waited after the shared ones ended")
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(150),
                acquire_shared_start_guard(path.clone())
            )
            .await
            .is_err(),
            "a shared hold was granted while a client was replacing the daemon"
        );
        drop(replacing);
        tokio::time::timeout(Duration::from_secs(2), acquire_shared_start_guard(path))
            .await
            .expect("a shared hold waited after the replacement ended")
            .unwrap();
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
    // Hard-won: cbbc2e0d: the unattended keep-alive restarted a daemon from a stale client executable.
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
    // Hard-won: 2be31d67: restart stopped the old daemon before discovering the replacement config was invalid.
    #[test]
    fn a_configuration_the_new_daemon_cannot_read_refuses_the_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "version = {}\n\n[profiles.codex]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n\
                 [profiles.codex.environment]\nFAKE_TOKEN = {{ from_secret = \"\" }}\n",
                mj_core::config::CONFIG_VERSION
            ),
        )
        .unwrap();
        let error = ensure_config_loads(&path, "restarted")
            .unwrap_err()
            .to_string();
        assert!(error.contains("from_secret names nothing"), "{error}");
        assert!(error.contains("was not restarted"), "{error}");
    }

    // Hard-won: 871a4d9b: an unusable profile credential prevented a replacement that should leave other profiles available.
    #[test]
    fn a_profile_that_cannot_start_does_not_refuse_the_replacement() {
        // The reported failure: a Codex home that authenticates with an API key
        // that neither the profile nor the environment supplies, beside a
        // secret that is not defined. Either makes only that profile unusable;
        // the new daemon reads the configuration and keeps every other profile
        // working.
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("codex");
        fs::create_dir(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\n\
             base_url = 'https://api.deepseek.com'\nwire_api = 'responses'\nenv_key = 'MJ_TEST_UNSET_PROVIDER_KEY'\n",
        )
        .unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "version = {}\n\n[profiles.codex]\nkind = \"codex\"\nhome = {:?}\n\n\
                 [profiles.other]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n\
                 [profiles.other.environment]\nFAKE_TOKEN = {{ from_secret = \"FAKE_TOKEN\" }}\n",
                mj_core::config::CONFIG_VERSION,
                home.to_string_lossy()
            ),
        )
        .unwrap();
        ensure_config_loads(&path, "replaced").unwrap();
        let config = mj_core::config::Config::load_from(&path).unwrap();
        let error = config.profiles["codex"]
            .ensure_ready("codex")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "MJ_TEST_UNSET_PROVIDER_KEY = { from_secret = \"MJ_TEST_UNSET_PROVIDER_KEY\" }"
            ),
            "{error}"
        );
        let error = format!(
            "{:#}",
            config.profiles["other"].ensure_ready("other").unwrap_err()
        );
        assert!(error.contains("FAKE_TOKEN = { from_secret"), "{error}");
        assert!(error.contains("secrets.toml"), "{error}");
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
    // Hard-won: ef16209e: restart reported success after another client relaunched the old executable.
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
    // Hard-won: b7ced524: startup was reported failed after eight seconds while the live daemon was still initializing.
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
                let ready =
                    tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(75);
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
        assert!(
            waited >= Duration::from_secs(75),
            "a migration may exceed the old 60-second bound"
        );
    }

    #[tokio::test]
    async fn failed_launch_reports_its_diagnostic_log_and_only_its_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        fs::create_dir(&logs).unwrap();
        let stderr = directory.path().join("daemon.log");
        let old = "old launch must not appear\n";
        fs::write(&stderr, format!("{old}this launch's stderr\n")).unwrap();
        fs::write(
            logs.join("mj-daemon-20260930T000000.000Z-8.log"),
            "another daemon's diagnostics",
        )
        .unwrap();
        let diagnostics = logs.join("mj-daemon-20260930T000000.000Z-7.log");
        fs::write(
            &diagnostics,
            format!(
                "{}\nERROR invalid project path\n",
                "startup line\n".repeat(10_000)
            ),
        )
        .unwrap();
        let launched = LaunchedDaemon {
            pid: 7,
            log_offset: old.len() as u64,
        };
        let (path, output) = launched.output_since_launch(&stderr).await;
        assert_eq!(path, diagnostics);
        assert!(output.contains("ERROR invalid project path"));
        assert!(output.contains("this launch's stderr"));
        assert!(!output.contains("old launch"));
        assert!(!output.contains("another daemon"));
        assert!(output.len() < 1024);
        let message = format!(
            "{:#}",
            launched.failure("startup timed out".into(), output, &path)
        );
        assert!(message.contains(&diagnostics.display().to_string()));
    }

    #[tokio::test]
    async fn a_failure_before_logging_starts_reports_detached_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let stderr = directory.path().join("daemon.log");
        fs::write(&stderr, "failed before logger initialization\n").unwrap();
        let launched = LaunchedDaemon {
            pid: 7,
            log_offset: 0,
        };
        let (path, output) = launched.output_since_launch(&stderr).await;
        assert_eq!(path, stderr);
        assert_eq!(output, "failed before logger initialization");
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

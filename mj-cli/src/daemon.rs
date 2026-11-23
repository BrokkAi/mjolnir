//! Application-owned daemon startup and attachment maintenance.
use mj_controller::controller::ControllerStoreGuard;
use mj_core::config::{DaemonRuntime, data_dir};
use mj_core::daemon_exit::{
    DaemonExitRecord, daemon_exits_dir, format_exit_reason, read_daemon_exit_record,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, anyhow, bail, ensure};
use mj_client::build_identity::{
    BuildIdentity, DaemonBuildOrder, compare_daemon_build, this_build,
};
pub(crate) use mj_client::daemon::*;
pub(crate) use mj_client::executable::process_executable_path;
pub(crate) use mj_daemon::daemon::run_daemon_process;

/// Forward machine-owned commands before touching client stores or credentials.
pub(crate) async fn run_where_daemon_lives(args: &[String], interactive: bool) -> Result<bool> {
    let interactive = interactive && !args.iter().any(|arg| arg == "--json");
    let placement = load_daemon_config().await?;
    if placement.runtime != mj_core::config::DaemonRuntime::Ssh {
        // Lifecycle commands for a host or container daemon run here, through
        // management discovery and the engine. Running them inside the
        // container would start a stopped daemon before reporting on it.
        if args.first().map(String::as_str) == Some("daemon") {
            return Ok(false);
        }
        return container::exec_in_container(args, interactive).await;
    }
    let host = placement
        .host
        .as_deref()
        .context("[daemon] host is required for SSH placement")?;
    ensure!(
        !host.starts_with('-'),
        "[daemon] host cannot be an SSH option"
    );
    let instance = placement.remote_instance();
    if args == ["daemon", "status"] {
        println!("Daemon host: {host}");
        println!(
            "Daemon instance: {}",
            instance.as_deref().unwrap_or("the remote's own")
        );
        println!(
            "Client config: {}",
            mj_core::config::config_path().display()
        );
        let client = this_build();
        println!("Client build: {}", client.published());
        match mj_client::daemon::ssh::discover(
            &placement,
            instance.as_deref(),
            mj_client::daemon::ssh::DiscoveryIntent::Existing,
        )
        .await
        {
            Ok(endpoint) => {
                let remote = BuildIdentity::parse(&endpoint.build_version)?;
                println!("Daemon build: {}", remote.published());
                println!("same_source: {}", client.same_source(&remote));
            }
            Err(error) => eprintln!("Daemon build unavailable: {error:#}"),
        }
    }
    let mut argv = vec![placement.remote_command().to_owned()];
    if let Some(instance) = instance {
        argv.extend(["--instance".into(), instance]);
    }
    argv.extend_from_slice(args);
    let mut command = std::process::Command::new("ssh");
    if interactive {
        command.arg("-t");
    }
    command
        .args(&placement.ssh_args)
        .arg(host)
        .arg(mj_core::targets::daemon_login_command(&argv));
    let status =
        mj_core::runtime::spawn_blocking(move || mj_core::subprocess::run_inherited(&mut command))
            .await
            .context("SSH command task panicked")??;
    if status.code() == Some(127) {
        eprintln!(
            "[daemon] remote_command {:?} was not found on {host}; install mj remotely with scripts/install.sh (or install the same release)",
            placement.remote_command()
        );
    }
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(true)
}

pub(crate) mod choice;
pub(crate) mod container;
pub(crate) mod wait_exit;
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

/// The splash replaces its status line; stderr notices remain infrequent.
fn startup_notice_interval() -> Duration {
    if STARTUP_NOTICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(30)
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

type StartLock = mj_core::file_lock::FileLock;

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
    let asked = tokio::time::Instant::now();
    let prewarm_at = asked + START_NOTICE_DELAY.saturating_sub(BLOCKER_PREWARM_LEAD);
    let mut notice_at = asked + START_NOTICE_DELAY;
    let mut blockers = PendingBlockers::default();
    loop {
        let path = path.clone();
        let guard = mj_core::runtime::spawn_blocking(move || -> Result<Option<StartLock>> {
            if let Some(parent) = path.parent() {
                mj_core::config::create_owner_only_directory(parent)?;
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
                Ok(()) => Ok(Some(StartLock::new(file))),
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
        let now = tokio::time::Instant::now();
        if now >= notice_at {
            startup_notice(shared_handoff_notice(
                blockers.refresh(running_daemon_blockers).await,
            ));
            notice_at = now + startup_notice_interval();
        } else if now >= prewarm_at {
            blockers.ensure_started(running_daemon_blockers);
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// The line shown while another client holds the startup lock. When that
/// client is replacing the daemon, the running daemon names what the handoff
/// is waiting for, so this client can say it too.
fn shared_handoff_notice(blockers: Option<&[String]>) -> String {
    match blockers {
        Some(blockers) => format!(
            "Mjolnir is waiting for another client to finish the daemon handoff; the running daemon is finishing: {}.",
            blockers.join(", ")
        ),
        None => "Mjolnir is waiting for another client to finish the daemon handoff.".to_owned(),
    }
}

/// Whether the daemon identified by `instance_id` has exited or been
/// replaced, as after an automatic upgrade.
pub(crate) async fn daemon_was_replaced(instance_id: &str) -> bool {
    match connect_existing().await {
        Ok(mut client) => match client.identity().await {
            Ok(identity) => identity.instance_id != instance_id,
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// What the running daemon, if any, reports as holding a handoff open.
async fn running_daemon_blockers() -> Option<Vec<String>> {
    let metadata = mj_core::runtime::spawn_blocking(read_metadata_any)
        .await
        .ok()?
        .ok()?;
    upgrade_blockers(&metadata).await
}

/// Startup phases shorter than this are not logged; longer ones always are,
/// so a slow replacement leaves a record of where its time went.
const LOGGED_PHASE: Duration = Duration::from_millis(250);

pub async fn connect_or_start() -> Result<DaemonClient> {
    connector::DaemonConnector::shared()
        .await?
        .connect_or_start(connect_or_start_local)
        .await
}

async fn connect_or_start_local() -> Result<DaemonClient> {
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
    maybe_replace_stale_development_daemon(startup).await?;
    let mut prior_unpublished = match prepare_existing_daemon(startup).await? {
        PreparedDaemon::Ready(client) => return Ok(client),
        PreparedDaemon::Launch { prior_unpublished } => prior_unpublished,
    };

    // Metadata disappears before shutdown releases the sole-writer lock.
    // Do not launch a child that can only fail to acquire that lock. A
    // container daemon owns a store in its volume, so the host guard is not
    // about it at all.
    let configured_container = load_daemon_config()
        .await
        .map(|daemon| daemon.runtime.engine().is_some())
        .unwrap_or(false);
    if !configured_container {
        let released = Instant::now();
        let handoff_deadline = released + STOP_TIMEOUT;
        loop {
            if let Some(guard) = mj_core::runtime::spawn_blocking(ControllerStoreGuard::try_acquire)
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
            match prepare_existing_daemon(startup).await? {
                PreparedDaemon::Ready(client) => return Ok(client),
                PreparedDaemon::Launch {
                    prior_unpublished: prior,
                } => prior_unpublished = prior,
            }
            ensure!(
                Instant::now() < handoff_deadline,
                "controller store is still owned by another process after {}s; daemon startup was not attempted",
                STOP_TIMEOUT.as_secs()
            );
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }

    let log_path = data_dir().join("daemon.log");
    let launched = launch_daemon(&log_path, prior_unpublished.as_ref()).await?;
    let launched_at = Instant::now();
    let label = launched.label();
    tracing::info!(daemon = %label, "launched the daemon");

    let outcome = wait_for_ready_daemon(
        || launched.is_alive(),
        || async {
            if let Some(container) = &launched.container {
                return connect_ready_container(&container.metadata).await;
            }
            let mut client = connect_existing().await?;
            ping_daemon(&mut client).await?;
            check_store_readiness().await?;
            Ok(client)
        },
        |waited| {
            startup_notice(format!(
                "Mjolnir daemon {label} has been starting for {}s; still waiting.",
                waited.as_secs()
            ));
        },
    )
    .await;
    let reason = match outcome {
        StartupOutcome::Ready(client) => {
            if let Some(container) = &launched.container {
                write_launcher_metadata(&container.metadata)?;
            }
            tracing::info!(
                daemon = %label,
                ready_ms = launched_at.elapsed().as_millis(),
                "the launched daemon is ready"
            );
            return Ok(client);
        }
        // A daemon that fails to initialize explains itself in its log and
        // exits without ever publishing an endpoint. That explanation is the
        // answer; the client's own failure to reach the absent endpoint is not.
        StartupOutcome::Exited => {
            // An engine may still finish an interrupted exec. Keep its credentials.
            format!("Mjolnir daemon {label} exited before it was ready")
        }
        StartupOutcome::StillStarting {
            last_error,
            probe_pending,
        } => startup_wait_failure(&label, last_error, probe_pending),
    };
    let (log_path, output) = launched.output_since_launch(&log_path).await;
    Err(launched.failure(reason, output, &log_path))
}

fn startup_wait_failure(
    label: &str,
    last_error: Option<anyhow::Error>,
    probe_pending: bool,
) -> String {
    let mut reason = format!(
        "Mjolnir daemon {label} is still starting after {}s",
        START_TIMEOUT.as_secs()
    );
    if probe_pending {
        reason.push_str(". A readiness probe is still pending");
    }
    if let Some(error) = last_error {
        reason.push_str(&format!(". Last completed probe failed: {error:#}"));
    }
    reason
}

/// The placement of a daemon that predates the `Identity` action.
///
/// This is the legacy 2.65.0 path: those container daemons shared the host's
/// process namespace on Linux, so `/proc/<pid>/environ` named their container.
/// A daemon of this protocol answers for itself instead. Keep this only while
/// an upgrade can start from 2.65.x, and delete it with the legacy launcher.
fn legacy_placement(pid: u32) -> DaemonPlacement {
    if pid == 0 {
        return DaemonPlacement::Container {
            name: "<unknown>".into(),
            runtime: None,
        };
    }
    #[cfg(target_os = "linux")]
    {
        let prefix = format!("{}=", mj_client::daemon::DAEMON_CONTAINER_ENV);
        let container = fs::read(format!("/proc/{pid}/environ"))
            .ok()
            .and_then(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
                    .map(|name| String::from_utf8_lossy(name).into_owned())
            });
        match container {
            Some(name) => DaemonPlacement::Container {
                name,
                runtime: None,
            },
            None => DaemonPlacement::Host,
        }
    }
    #[cfg(not(target_os = "linux"))]
    DaemonPlacement::Host
}

fn daemon_placement(
    metadata: &DaemonMetadata,
    reported: Option<DaemonPlacement>,
) -> DaemonPlacement {
    reported
        .filter(|placement| metadata.pid != 0 || placement.container().is_some())
        .unwrap_or_else(|| legacy_placement(metadata.pid))
}

/// Publish the host's `daemon.json` for a daemon whose own filesystem is a
/// container volume. The body is owner-only and replaced atomically, like the
/// daemon's own publication.
pub(crate) fn write_launcher_metadata(metadata: &DaemonMetadata) -> Result<()> {
    use std::io::Write as _;
    let path = metadata_path();
    let parent = path
        .parent()
        .context("container daemon metadata path has no parent")?;
    mj_core::config::create_owner_only_directory(parent)?;
    let temporary = parent.join(format!(".daemon.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(metadata)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(&body)?;
    file.sync_all()?;
    #[cfg(windows)]
    {
        // Windows does not replace an existing destination with rename.
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error)
                .with_context(|| format!("replace daemon metadata {}", path.display()));
        }
    }
    fs::rename(&temporary, &path)
        .with_context(|| format!("publish daemon metadata {}", path.display()))?;
    Ok(())
}

/// What a daemon reports about itself, or the legacy answer when an older
/// daemon cannot report at all.
async fn daemon_identity_or_legacy(metadata: &DaemonMetadata) -> DaemonIdentity {
    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        let mut client = DaemonClient::connect(metadata.clone()).await.ok()?;
        request_daemon_identity(&mut client).await.ok()
    })
    .await
    .ok()
    .flatten();
    if let Some(mut identity) = observed {
        identity.placement = daemon_placement(metadata, Some(identity.placement));
        identity
    } else {
        DaemonIdentity {
            instance_id: String::new(),
            build_version: metadata.build_version.clone(),
            placement: legacy_placement(metadata.pid),
        }
    }
}

/// Where the configuration puts the daemon on this computer.
enum ConfiguredPlacement {
    /// On the host; `reason` says why when a container was asked for.
    Host {
        reason: Option<String>,
    },
    Container(Box<container::ContainerSpec>),
}

impl ConfiguredPlacement {
    /// The container's name, compared with the daemon's reported placement.
    fn container(&self) -> Option<&str> {
        match self {
            Self::Host { .. } => None,
            Self::Container(spec) => Some(&spec.name),
        }
    }
}

/// Resolve `[daemon]` to a placement. Both the launch and the decision to
/// keep or replace a running daemon ask this, so they cannot disagree about
/// where the daemon belongs.
async fn resolve_placement(daemon: &mj_core::config::DaemonConfig) -> Result<ConfiguredPlacement> {
    if daemon.runtime.engine().is_none() {
        return Ok(ConfiguredPlacement::Host { reason: None });
    }
    Ok(match container::placement(daemon).await? {
        container::Placement::Container(spec) => ConfiguredPlacement::Container(spec),
        container::Placement::Host { reason } => ConfiguredPlacement::Host {
            reason: Some(reason),
        },
    })
}

async fn load_daemon_config() -> Result<mj_core::config::DaemonConfig> {
    mj_core::runtime::spawn_blocking(|| {
        mj_core::config::DaemonConfig::load_placement(&mj_core::config::config_path())
    })
    .await
    .context("read daemon configuration task failed")?
}

/// Whether the daemon, as it describes itself, is this build.
///
/// The daemon reports the executable file time in its build identity, so a
/// host daemon is this build only when that file is this client's. A daemon in
/// a container runs the image's own `mj`, which the launch accepted only
/// because the image was built from this build's version and revision; its
/// file time is the image's, so version and revision decide there.
pub(crate) fn daemon_runs_this_build(identity: &DaemonIdentity) -> Result<Option<bool>> {
    let daemon =
        BuildIdentity::parse(&identity.build_version).context("parse daemon build version")?;
    let client = this_build();
    Ok(Some(if identity.placement.container().is_some() {
        daemon.same_source(client)
    } else {
        &daemon == client
    }))
}

/// A session is an overlay on the account's login environment. Reconstruct that
/// base instead of maintaining a second list of target, profile, harness and
/// shell overrides here. The existing identity also covers older workers.
fn host_daemon_login_environment()
-> Result<Option<mj_harness_install::login_environment::Environment>> {
    if std::env::var_os("MJ_SESSION_ID").is_some()
        || std::env::var_os("MJ_SESSION_BASH_ENV").is_some()
    {
        return mj_harness_install::login_environment::discover()
            .map(Some)
            .context("restore account login environment for the daemon");
    }
    Ok(None)
}

fn configure_host_daemon_environment(
    command: &mut std::process::Command,
    login_environment: Option<mj_harness_install::login_environment::Environment>,
) -> Result<()> {
    if let Some(mut environment) = login_environment {
        for name in ["MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE"] {
            // Absence is significant too: a login script must not redirect the
            // instance selected by this client.
            environment.remove(name);
        }
        command.env_clear().envs(environment);
    }
    // Resolve against the client's cwd before the child changes directory.
    // config_dir/data_dir own override and named-instance precedence.
    command.env(
        "MJ_CONFIG_DIR",
        std::path::absolute(mj_core::config::config_dir())?,
    );
    let data = std::path::absolute(data_dir())?;
    command.env("MJ_DATA_DIR", &data);
    if let Some(instance) = std::env::var_os("MJ_INSTANCE") {
        command.env("MJ_INSTANCE", instance);
    }
    command.current_dir(data);
    Ok(())
}

/// Start `mj daemon-run` where `[daemon] runtime` says: a detached process on
/// this computer, or a process in this instance's daemon container. Either way
/// the caller follows the launched process until the daemon answers.
async fn launch_daemon(
    log_path: &Path,
    prior_unpublished: Option<&ContainerDaemonRecord>,
) -> Result<LaunchedDaemon> {
    retire_legacy_node();
    // An unreadable file is the daemon's to report: it refuses to start and
    // says why, which is the answer this launch then shows.
    let daemon = load_daemon_config().await.unwrap_or_default();
    match resolve_placement(&daemon).await? {
        ConfiguredPlacement::Host { reason: None } => {}
        ConfiguredPlacement::Host {
            reason: Some(reason),
        } => startup_notice(format!(
            "Settings run the Mjolnir daemon in {}, but {reason}. It runs on this computer \
             instead.",
            daemon.runtime.engine().unwrap_or("a container")
        )),
        ConfiguredPlacement::Container(spec) => {
            let launched = container::launch(&spec, prior_unpublished).await?;
            tracing::info!(
                container = %launched.container_name,
                port = launched.metadata.address.port(),
                "launched the daemon in its container"
            );
            return Ok(LaunchedDaemon {
                pid: 0,
                log_offset: 0,
                container: Some(launched),
            });
        }
    }
    if let Some(prior) = prior_unpublished {
        remove_container_owner_if_matching(prior.address, &prior.token)?;
    }
    mj_core::runtime::spawn_blocking({
        let log_path = log_path.to_path_buf();
        move || -> Result<LaunchedDaemon> {
            // Everything the daemon writes from here on belongs to this launch.
            let log_offset = fs::metadata(&log_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let mut command = host_daemon_command()?;
            command
                // This switch makes the invoking development client authoritative. It
                // has no meaning inside the persistent daemon or its child processes.
                .env_remove(DEV_RESTART_STALE_DAEMON_ENV);
            command.env_remove("MJ_UPGRADE_RESUME_FILE");
            command.env_remove(DAEMON_CONTAINER_ENV);
            // A host daemon picks its own endpoint and token; only a container
            // launcher chooses them.
            command.env_remove("MJ_DAEMON_LISTEN");
            command.env_remove("MJ_DAEMON_TOKEN");
            let pid = mj_core::subprocess::spawn_detached(&mut command, &log_path)?;
            Ok(LaunchedDaemon {
                pid,
                log_offset,
                container: None,
            })
        }
    })
    .await
    .context("spawn daemon task failed")?
}

fn host_daemon_command() -> Result<std::process::Command> {
    use std::process::Command;

    let executable = daemon_launch_executable()?;
    let exit_dir = std::path::absolute(daemon_exits_dir(&data_dir()))?;
    let mut command = Command::new(&executable);
    command
        .args(["daemon-wait-exit", "--exit-dir"])
        .arg(exit_dir)
        .arg("--");
    #[cfg(unix)]
    std::os::unix::process::CommandExt::arg0(&mut command, "mj");

    let login_environment = host_daemon_login_environment()?;
    #[cfg(target_os = "linux")]
    let scope = daemon_scope_launcher(login_environment.as_ref());
    configure_host_daemon_environment(&mut command, login_environment)?;

    #[cfg(target_os = "linux")]
    if let Some((systemd_run, runtime_dir)) = scope {
        // Keep the waiter outside the scope so it can record a scope-level kill.
        // Resolve PATH before the launcher changes into the daemon data directory.
        let instance = mj_core::config::instance_name().unwrap_or_else(|| "default".to_owned());
        command
            .arg(std::path::absolute(systemd_run)?)
            .args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--description=Mjolnir daemon ({instance})"))
            .args(["--", "/proc/self/exe", "daemon-run"])
            .env("XDG_RUNTIME_DIR", runtime_dir);
        return Ok(command);
    }

    command.arg(executable).arg("daemon-run");
    Ok(command)
}

#[cfg(target_os = "linux")]
fn daemon_scope_launcher(
    login_environment: Option<&mj_harness_install::login_environment::Environment>,
) -> Option<(PathBuf, PathBuf)> {
    // Check the environment systemd-run will receive, including when a session
    // client restores the account's login environment.
    let variable = |name| match login_environment {
        Some(environment) => environment.get(name).map(std::ffi::OsString::from),
        None => std::env::var_os(name),
    };
    // SAFETY: geteuid only reads the caller's effective user ID.
    let uid = unsafe { libc::geteuid() };
    // A restored login environment can omit the runtime directory PAM assigns.
    let runtime_dir = variable("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    if !runtime_dir.join("systemd/private").exists() {
        return None;
    }
    let user_service = format!("user@{uid}.service");
    let cgroups = fs::read_to_string("/proc/self/cgroup").ok()?;
    if cgroups
        .lines()
        .filter_map(|line| line.splitn(3, ':').nth(2))
        .any(|path| {
            Path::new(path)
                .iter()
                .any(|component| component == user_service.as_str())
        })
    {
        return None;
    }
    // #1584: cgroup v2 checks write access at the common ancestor, so
    // rootless Podman execs need to start inside the delegated user subtree.
    let systemd_run = mj_harness_install::program_path::find_executable_on_path(
        "systemd-run",
        variable("PATH").as_deref(),
    )?;
    // logind stops the user manager at logout without lingering. Keep the
    // detached daemon and its bare workers in their original cgroup then.
    let user = mj_harness_install::login_environment::account_name().ok()?;
    // logind C-escapes account names for linger filenames. Check names that
    // appear verbatim there so an escaped filename cannot match another user.
    if !user
        .bytes()
        .all(|byte| (b' '..=b'~').contains(&byte) && !matches!(byte, b'/' | b'\\' | b'\'' | b'"'))
        || !Path::new("/var/lib/systemd/linger").join(user).is_file()
    {
        return None;
    }
    Some((systemd_run, runtime_dir))
}

/// Releases 2.66.0 through 2.70.x started an `mj node` beside every host
/// daemon. That node exits when it loses its daemon and finds no
/// `node-endpoint.json`, so removing the file here, after the old daemon is
/// gone and before its replacement starts, retires it. Delete this once no
/// supported upgrade starts from those releases.
fn retire_legacy_node() {
    let data = data_dir();
    for name in ["node-endpoint.json", "node-id", "node.lock"] {
        match fs::remove_file(data.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, file = name, "could not remove a file of the retired node");
            }
        }
    }
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate the daemon token: {error}"))?;
    Ok(mj_core::hex::lower_hex(bytes))
}

async fn check_store_readiness() -> Result<()> {
    mj_core::runtime::spawn_blocking(mj_controller::database::check_read_compatibility)
        .await
        .context("check database readiness task failed")?
}

async fn ping_daemon(client: &mut DaemonClient) -> Result<()> {
    let reply = client.request_single(DaemonAction::Ping).await?;
    ensure!(
        matches!(reply, DaemonReply::Pong),
        "unexpected startup reply {reply:?}"
    );
    Ok(())
}

/// Wait for the web access publication that owns all local API connection
/// details: advertised URL, listener address, and certificate pin.
pub async fn wait_for_web_access(
    client: &mut DaemonClient,
) -> Result<mj_client::web::WebViewerAccess> {
    wait_for_web_access_with_timeout(client, START_TIMEOUT).await
}

async fn wait_for_web_access_with_timeout(
    client: &mut DaemonClient,
    timeout: Duration,
) -> Result<mj_client::web::WebViewerAccess> {
    tokio::time::timeout(timeout, async {
        loop {
            match client.web_access().await? {
                access @ mj_client::web::WebViewerAccess::Ready { .. } => return Ok(access),
                mj_client::web::WebViewerAccess::Starting => tokio::time::sleep(RETRY_DELAY).await,
                mj_client::web::WebViewerAccess::Failed { message, .. } => {
                    bail!("the web viewer failed to start: {message}");
                }
                mj_client::web::WebViewerAccess::Unavailable(reason) => {
                    bail!("the web viewer is unavailable: {reason}");
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
fn daemon_build(metadata: &DaemonMetadata, placement: &DaemonPlacement) -> Result<DaemonBuild> {
    let daemon =
        BuildIdentity::parse(&metadata.build_version).context("parse daemon build version")?;
    let client = this_build();
    // A daemon in a container runs the image's `mj`, whose file times are the
    // image's: the same version and revision is the same build.
    let order = if placement.container().is_some() && daemon.same_source(client) {
        DaemonBuildOrder::Same
    } else {
        compare_daemon_build(&daemon, client)
    };
    Ok(DaemonBuild {
        release: daemon.version().cmp_precedence(client.version()),
        order,
    })
}

/// The sentence naming both builds, followed by what this client does.
fn daemon_build_notice(metadata: &DaemonMetadata, decision: &str) -> String {
    let daemon = BuildIdentity::parse(&metadata.build_version)
        .map_or_else(|_| metadata.build_version.clone(), |build| build.describe());
    format!(
        "The running daemon is build {daemon}; this client is build {}. {decision}",
        this_build().describe()
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
    /// None runs, so one has to be started.
    Absent,
    ExitedContainer(DaemonMetadata),
    PendingContainer(ContainerDaemonRecord),
    Replaced,
    /// A recorded daemon still owns the store or has a live process.
    Unanswered(DaemonMetadata),
    /// It answers and this client may use it.
    Use(DaemonClient),
    /// It answers but has to be replaced first. `notice` is what to tell the
    /// person when the replacement starts.
    Replace {
        metadata: DaemonMetadata,
        placement: DaemonPlacement,
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

#[derive(Debug)]
enum DaemonRelease {
    StillOwned,
    Exited,
    Replaced,
    Unknown(String),
}

fn same_daemon_endpoint(current: &DaemonMetadata, previous: &DaemonMetadata) -> bool {
    current.address == previous.address && current.token == previous.token
}

fn matching_container_owner(metadata: &DaemonMetadata) -> Result<Option<ContainerDaemonRecord>> {
    Ok(read_container_owner()?.filter(|record| record.matches(metadata)))
}

fn missing_container_owner_reason(error: Option<anyhow::Error>) -> String {
    let detail = error.map_or_else(String::new, |error| format!(" ({error:#})"));
    format!(
        "daemon.json records a container daemon (pid: 0) without a matching daemon-container.json \
         owner record{detail}. Mjolnir cannot verify whether it exited. Verify that no container \
         daemon is running, then run `mj daemon stop --force` to clear the stale record."
    )
}

async fn probe_daemon_release(metadata: &DaemonMetadata) -> Result<DaemonRelease> {
    let previous = metadata.clone();
    let owner = mj_core::runtime::spawn_blocking(move || {
        if read_metadata_any().is_ok_and(|current| !same_daemon_endpoint(&current, &previous)) {
            return Err(DaemonRelease::Replaced);
        }
        if previous.pid == 0 {
            return match matching_container_owner(&previous) {
                Ok(Some(record)) => Ok(record),
                other => Err(DaemonRelease::Unknown(missing_container_owner_reason(
                    other.err(),
                ))),
            };
        }
        let status = match ControllerStoreGuard::try_acquire() {
            Ok(None) => Ok(true),
            // The daemon released the store and is exiting. Nothing is
            // signalled here, so a PID that still exists is waited for even
            // when its command line is already gone; the caller's timeout
            // bounds the wait.
            Ok(Some(_guard)) => Ok(host_pid(previous.pid) != HostPid::NotDaemon),
            Err(error) => Err(error),
        };
        Err(match status {
            Ok(true) => DaemonRelease::StillOwned,
            Ok(false) => DaemonRelease::Exited,
            Err(error) => DaemonRelease::Unknown(format!("{error:#}")),
        })
    })
    .await
    .context("inspect daemon ownership task failed")?;
    let record = match owner {
        Ok(record) => record,
        Err(release) => return Ok(release),
    };
    Ok(
        match container::probe_container_daemon(record.runtime, &record.container_id).await {
            container::ContainerDaemonStatus::Running => DaemonRelease::StillOwned,
            container::ContainerDaemonStatus::Exited { .. } => DaemonRelease::Exited,
            container::ContainerDaemonStatus::Unknown(reason) => DaemonRelease::Unknown(reason),
        },
    )
}

const UNREADABLE_CONTAINER_OWNER_RECOVERY: &str = "Verify that no container daemon is running, then run `mj daemon stop --force` to clear the unreadable daemon-container.json";

async fn unpublished_container() -> Result<ExistingDaemon> {
    Ok(mj_core::runtime::spawn_blocking(read_container_owner)
        .await
        .context("read container daemon ownership task failed")?
        .context(UNREADABLE_CONTAINER_OWNER_RECOVERY)?
        .map_or(ExistingDaemon::Absent, ExistingDaemon::PendingContainer))
}

async fn unanswered(metadata: DaemonMetadata) -> Result<ExistingDaemon> {
    match probe_daemon_release(&metadata).await? {
        DaemonRelease::Exited if metadata.pid == 0 => Ok(ExistingDaemon::ExitedContainer(metadata)),
        DaemonRelease::Exited => unpublished_container().await,
        DaemonRelease::Replaced => Ok(ExistingDaemon::Replaced),
        DaemonRelease::StillOwned => Ok(ExistingDaemon::Unanswered(metadata)),
        DaemonRelease::Unknown(reason) => Err(anyhow!(reason)),
    }
}

async fn decide_existing_daemon() -> Result<ExistingDaemon> {
    let metadata = mj_core::runtime::spawn_blocking(|| match read_metadata_any() {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if daemon_not_running(&error).is_some() => Ok(None),
        Err(error) => match ControllerStoreGuard::try_acquire()
            .context("check controller ownership after failing to read daemon.json")?
        {
            Some(_unowned) => Ok(None),
            None => {
                Err(error).context("the controller store is owned but daemon.json is unreadable")
            }
        },
    })
    .await
    .context("read daemon metadata task failed")??;
    let Some(metadata) = metadata else {
        return unpublished_container().await;
    };
    if let Err(error) = ensure_supported_daemon_protocol(&metadata) {
        return match unanswered(metadata).await? {
            ExistingDaemon::Unanswered(_) => Err(error),
            verdict => Ok(verdict),
        };
    }
    if metadata.protocol_version < PROTOCOL_VERSION {
        let placement = if metadata.pid == 0 {
            // A reachable pre-tracking container can name its engine via
            // Identity. Its endpoint alone cannot establish ownership on exit.
            daemon_identity_or_legacy(&metadata).await.placement
        } else {
            legacy_placement(metadata.pid)
        };
        if !matches!(
            placement,
            DaemonPlacement::Container {
                runtime: Some(_),
                ..
            }
        ) {
            match unanswered(metadata.clone()).await? {
                ExistingDaemon::Unanswered(_) => {}
                verdict => return Ok(verdict),
            }
        }
        let build = daemon_build(&metadata, &placement)?;
        ensure!(
            !build.daemon_is_newer(),
            "refusing to replace a newer daemon with this client: {}",
            daemon_build_notice(&metadata, "Run the daemon's build instead.")
        );
        return Ok(ExistingDaemon::Replace {
            metadata,
            placement,
            notice: None,
        });
    }
    let Ok(mut client) = DaemonClient::connect(metadata.clone()).await else {
        return unanswered(metadata).await;
    };
    // Discovery has no outer readiness clock. A silent endpoint must let
    // prepare_existing_daemon check ownership and update its wait notice.
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), ping_daemon(&mut client)).await,
        Ok(Ok(()))
    ) {
        return unanswered(metadata).await;
    }
    // This protocol reports its own placement. A failed discovery cannot safely
    // decide whether to use or replace it, and may have interrupted the connection.
    let identity = request_daemon_identity(&mut client)
        .await
        .context("read the running daemon's identity")?;
    let placement = daemon_placement(&metadata, Some(identity.placement));
    let build = daemon_build(&metadata, &placement)?;
    // A container daemon's store is in its volume; the host store says
    // nothing about it. The daemon reports its own migration failure.
    if placement.container().is_none()
        && let Err(error) = check_store_readiness().await
    {
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
            placement,
            notice: None,
        });
    }
    match build.order {
        DaemonBuildOrder::Same => {
            // Settings choose where the daemon runs; the next client of the
            // same build moves it there through the ordinary handoff, which
            // leaves workers running.
            let running = placement.container().map(str::to_owned);
            // A configuration this client cannot read leaves the daemon be.
            let configured = match load_daemon_config().await {
                Ok(daemon) => match resolve_placement(&daemon).await {
                    Ok(placement) => Some(placement.container().map(str::to_owned)),
                    Err(error) => {
                        tracing::warn!(
                            error = format!("{error:#}"),
                            "could not resolve where Settings run the daemon"
                        );
                        None
                    }
                },
                Err(_) => None,
            };
            if let Some(configured) = configured
                && configured != running
            {
                let notice = match &configured {
                    Some(container) => format!(
                        "Settings run the Mjolnir daemon in container {container}; moving it there."
                    ),
                    None => "Settings run the Mjolnir daemon on this computer; moving it out of \
                             its container."
                        .to_owned(),
                };
                return Ok(ExistingDaemon::Replace {
                    metadata,
                    placement,
                    notice: Some(notice),
                });
            }
        }
        DaemonBuildOrder::Older => {
            let notice = daemon_build_notice(
                &metadata,
                "This client's build is newer; replacing the daemon.",
            );
            return Ok(ExistingDaemon::Replace {
                metadata,
                placement,
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

enum PreparedDaemon {
    Ready(DaemonClient),
    Launch {
        prior_unpublished: Option<ContainerDaemonRecord>,
    },
}

/// Act on [`decide_existing_daemon`] while holding the startup lock
/// exclusively: use the running daemon, or replace it and report that none is
/// running yet. A recorded daemon that does not answer is waited for until it
/// answers or provably exits; starting another beside it could leave two
/// daemons serving one instance.
async fn prepare_existing_daemon(startup: &DaemonStartGuard) -> Result<PreparedDaemon> {
    let asked = Instant::now();
    let mut notice_at = asked + START_NOTICE_DELAY;
    loop {
        match decide_existing_daemon().await? {
            ExistingDaemon::Absent => {
                return Ok(PreparedDaemon::Launch {
                    prior_unpublished: None,
                });
            }
            ExistingDaemon::Use(client) => return Ok(PreparedDaemon::Ready(client)),
            ExistingDaemon::ExitedContainer(metadata) => clear_exited_daemon(&metadata)?,
            ExistingDaemon::PendingContainer(record) => {
                if matches!(
                    adopt_container_daemon(&record).await?,
                    ContainerAdoption::Exited
                ) {
                    return Ok(PreparedDaemon::Launch {
                        prior_unpublished: Some(record),
                    });
                }
            }
            ExistingDaemon::Replaced => {}
            ExistingDaemon::Replace {
                metadata,
                placement,
                notice,
            } => {
                match notice {
                    Some(notice) => startup_notice(notice),
                    None => tracing::info!(
                        daemon_protocol = metadata.protocol_version,
                        client_protocol = PROTOCOL_VERSION,
                        daemon_build = %metadata.build_version,
                        "the daemon is an older protocol or release, or its store needs a migration; replacing it"
                    ),
                }
                replace_daemon(startup, &metadata, &placement).await?;
            }
            ExistingDaemon::Unanswered(metadata) => {
                if Instant::now() >= notice_at {
                    let waited = asked.elapsed().as_secs();
                    tracing::info!(
                        pid = metadata.pid,
                        address = %metadata.address,
                        waited_s = waited,
                        "the recorded daemon has not answered and has not exited"
                    );
                    startup_notice(format!(
                        "The Mjolnir daemon at {} has not answered for {waited}s and is still \
                         running; waiting for it to answer or exit.",
                        metadata.address
                    ));
                    notice_at = Instant::now() + startup_notice_interval();
                }
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

fn pending_container_metadata(record: &ContainerDaemonRecord) -> DaemonMetadata {
    DaemonMetadata {
        protocol_version: record.protocol_version.unwrap_or(PROTOCOL_VERSION),
        pid: 0,
        address: record.address,
        token: record.token.clone(),
        started_at: record.started_at.clone(),
        build_version: String::new(),
        instance_id: String::new(),
        placement: DaemonPlacement::Container {
            name: record.container_name.clone(),
            runtime: Some(record.runtime),
        },
    }
}

async fn connect_ready_container(metadata: &DaemonMetadata) -> Result<DaemonClient> {
    let mut client = DaemonClient::connect(metadata.clone()).await?;
    ping_daemon(&mut client).await?;
    Ok(client)
}

async fn request_daemon_identity(client: &mut DaemonClient) -> Result<DaemonIdentity> {
    match client.request_single(DaemonAction::Identity).await? {
        DaemonReply::Identity(identity) => Ok(identity),
        reply => bail!("unexpected daemon identity reply {reply:?}"),
    }
}

async fn container_daemon_may_be_alive(runtime: DaemonRuntime, id: &str) -> bool {
    !matches!(
        container::probe_container_daemon(runtime, id).await,
        container::ContainerDaemonStatus::Exited { .. }
    )
}

enum ContainerAdoption {
    Published,
    Exited,
}

async fn adopt_container_daemon(record: &ContainerDaemonRecord) -> Result<ContainerAdoption> {
    match container::probe_container_daemon(record.runtime, &record.container_id).await {
        container::ContainerDaemonStatus::Exited { .. } => {
            return Ok(ContainerAdoption::Exited);
        }
        container::ContainerDaemonStatus::Unknown(reason) => bail!("{reason}"),
        container::ContainerDaemonStatus::Running => {}
    }

    let metadata = pending_container_metadata(record);
    let outcome = wait_for_ready_daemon(
        || container_daemon_may_be_alive(record.runtime, &record.container_id),
        || async {
            let mut client = connect_ready_container(&metadata).await?;
            request_daemon_identity(&mut client).await
        },
        |waited| {
            startup_notice(format!(
                "Mjolnir daemon container {} has been starting for {}s; still waiting.",
                record.container_name,
                waited.as_secs()
            ))
        },
    )
    .await;
    match outcome {
        StartupOutcome::Ready(identity) => {
            write_launcher_metadata(&DaemonMetadata {
                placement: daemon_placement(&metadata, Some(identity.placement)),
                build_version: identity.build_version,
                instance_id: identity.instance_id,
                ..metadata
            })?;
            Ok(ContainerAdoption::Published)
        }
        StartupOutcome::Exited => Ok(ContainerAdoption::Exited),
        StartupOutcome::StillStarting {
            last_error,
            probe_pending,
        } => {
            bail!(startup_wait_failure(
                &format!("container {}", record.container_name),
                last_error,
                probe_pending
            ))
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
    /// Whether the daemon runs this client's build, as the daemon reports it.
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

/// Stop this instance, including when the daemon is already absent. The
/// startup lock is held throughout, so a stop cannot call the daemon absent
/// while another client is still launching it. Returns whether a running
/// daemon was stopped.
pub(crate) async fn stop_instance(force: bool) -> Result<bool> {
    mj_core::config::ensure_may_control_store(&data_dir(), "stop the Mjolnir daemon")?;
    let startup = acquire_start_guard(data_dir().join("daemon-start.lock")).await?;
    let fallback = if force {
        StopFallback::Force
    } else {
        StopFallback::Graceful
    };
    let stopped = match read_metadata_any() {
        Ok(metadata) => stop_daemon(&startup, &metadata, fallback).await?,
        Err(error) if daemon_not_running(&error).is_some() => match read_container_owner() {
            Ok(Some(record)) => stop_pending_container(&startup, &record, force).await?,
            Ok(None) => false,
            Err(_) if force => {
                clear_unreadable_container_owner()?;
                true
            }
            Err(error) => return Err(error.context(UNREADABLE_CONTAINER_OWNER_RECOVERY)),
        },
        Err(error) => return Err(error),
    };
    retire_legacy_node();
    Ok(stopped)
}

fn clear_unreadable_container_owner() -> Result<()> {
    let path = container_owner_path();
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    eprintln!(
        "Cleared the unreadable container daemon record in {}.",
        path.display()
    );
    Ok(())
}

async fn stop_pending_container(
    startup: &DaemonStartGuard,
    record: &ContainerDaemonRecord,
    force: bool,
) -> Result<bool> {
    match container::probe_container_daemon(record.runtime, &record.container_id).await {
        container::ContainerDaemonStatus::Exited {
            container_running: false,
        } => {
            // A stopped or absent container cannot start a delayed exec.
            remove_container_owner_if_matching(record.address, &record.token)?;
            Ok(false)
        }
        container::ContainerDaemonStatus::Running
        | container::ContainerDaemonStatus::Exited {
            container_running: true,
        } if force => {
            container::stop(record.runtime, &record.container_id).await?;
            remove_container_owner_if_matching(record.address, &record.token)?;
            Ok(true)
        }
        container::ContainerDaemonStatus::Exited {
            container_running: true,
        } => {
            // Keep credentials while the container runs so a delayed exec remains
            // discoverable and any same-target retry reuses the same token.
            Ok(false)
        }
        container::ContainerDaemonStatus::Unknown(reason) => Err(anyhow!(reason)),
        container::ContainerDaemonStatus::Running => {
            stop_daemon(
                startup,
                &pending_container_metadata(record),
                StopFallback::Graceful,
            )
            .await
        }
    }
}

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
        if let Err(error) = read_metadata_any()
            && daemon_not_running(&error).is_some()
            && let Some(record) = read_container_owner()?
        {
            adopt_container_daemon(&record).await?;
        }
        if let Ok(metadata) = read_metadata_any() {
            // An explicit restart may replace a build it cannot order, but
            // never a newer one: that is a downgrade, which `mj daemon stop`
            // makes deliberate.
            let identity = daemon_identity_or_legacy(&metadata).await;
            ensure!(
                !daemon_build(&metadata, &identity.placement)?.daemon_is_newer(),
                "{}",
                daemon_build_notice(
                    &metadata,
                    "The daemon's build is newer, so it was not restarted. Run `mj daemon \
                     restart` from the daemon's build, or run `mj daemon stop` first to \
                     start this older build instead."
                )
            );
            ensure_container_owner_recorded(&startup, &metadata, &identity.placement).await?;
            // Keep restart's ability to stop an unresponsive tracked daemon.
            // Clearing an untracked container record requires explicit stop --force.
            stop_daemon(&startup, &metadata, StopFallback::Restart).await?;
        }
        let mut client = connect_or_start_holding(&startup).await?;
        let status = client.status().await?;
        let identity = client.identity().await?;
        let runs_this_build = daemon_runs_this_build(&identity)?;
        if runs_this_build != Some(false) || attempt == RESTART_ATTEMPTS {
            return restart_verdict(status.pid, &identity, runs_this_build);
        }
    }
    unreachable!("the final attempt always returns a verdict")
}

/// Turn one build-identity answer into the restart's result.
///
/// This is separate from the restart itself so the outcome can be exercised
/// without starting daemons. It is also the decision the command used to skip:
/// reporting success on a changed process id alone is what let a restart
/// announce a daemon running code the caller had just replaced.
fn restart_verdict(
    pid: u32,
    identity: &DaemonIdentity,
    runs_this_build: Option<bool>,
) -> Result<RestartedDaemon> {
    if runs_this_build == Some(false) {
        let daemon = BuildIdentity::parse(&identity.build_version)
            .map_or_else(|_| identity.build_version.clone(), |build| build.describe());
        bail!(
            "Mjolnir daemon {pid} runs build {daemon}, not this build ({}). \
             Another attached client started it. Close clients from the \
             previous build, then run `mj daemon restart` again.",
            this_build().describe()
        );
    }
    Ok(RestartedDaemon {
        pid,
        runs_this_build,
    })
}

/// Why waiting for a launched daemon stopped.
enum StartupOutcome<T> {
    /// It answered a request.
    Ready(T),
    /// The launched process is gone.
    Exited,
    /// It is still alive but has not answered within [`START_TIMEOUT`].
    StillStarting {
        last_error: Option<anyhow::Error>,
        probe_pending: bool,
    },
}

/// Wait for a launched daemon to answer a request.
///
/// A daemon that is still alive is still starting: initialization runs before
/// it publishes its endpoint, so a slow start is not a failure and the wait
/// continues. Only the process leaving, or the hard [`START_TIMEOUT`] bound,
/// ends the wait without a client. The clock is Tokio's, so tests can drive it.
async fn wait_for_ready_daemon<T, Alive, Probe, Waiting, Notice>(
    is_alive: impl Fn() -> Alive,
    mut probe: Probe,
    notice: Notice,
) -> StartupOutcome<T>
where
    Alive: Future<Output = bool>,
    Probe: FnMut() -> Waiting,
    Waiting: Future<Output = Result<T>>,
    Notice: FnOnce(Duration),
{
    let started = tokio::time::Instant::now();
    let mut last_error = None;
    let mut probe_pending = false;
    tokio::select! {
        biased;
        ready = async {
            loop {
                probe_pending = true;
                match probe().await {
                    Ok(ready) => break ready,
                    Err(error) => last_error = Some(error),
                }
                probe_pending = false;
                tokio::time::sleep(RETRY_DELAY).await;
            }
        } => return StartupOutcome::Ready(ready),
        _ = async {
            loop {
                if !is_alive().await {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        } => return StartupOutcome::Exited,
        _ = async {
            tokio::time::sleep_until(started + START_NOTICE_DELAY).await;
            notice(started.elapsed());
            std::future::pending::<()>().await;
        } => unreachable!("the startup notice does not end the wait"),
        _ = tokio::time::sleep_until(started + START_TIMEOUT) => {}
    }
    StartupOutcome::StillStarting {
        last_error,
        probe_pending,
    }
}

/// The daemon this client launched and where its log stood at launch.
#[derive(Debug, Clone)]
struct LaunchedDaemon {
    pid: u32,
    log_offset: u64,
    container: Option<container::ContainerLaunch>,
}

impl LaunchedDaemon {
    fn label(&self) -> String {
        match &self.container {
            Some(container) => format!("container {}", container.container_name),
            None => format!("(waiter PID {})", self.pid),
        }
    }

    async fn is_alive(&self) -> bool {
        match &self.container {
            Some(container) => {
                container_daemon_may_be_alive(container.runtime, &container.container_id).await
            }
            None => process_is_alive(self.pid),
        }
    }

    /// Include process diagnostics and detached stderr. Failures before logger
    /// initialization have only stderr; later failures also have a process log.
    async fn output_since_launch(&self, log_path: &Path) -> (PathBuf, String) {
        if let Some(container) = &self.container {
            let log = container::log_tail(
                container
                    .runtime
                    .engine()
                    .expect("a daemon container has an engine"),
                &container.container_id,
            )
            .await;
            return (
                PathBuf::from(format!("container {}", container.container_name)),
                log,
            );
        }
        let log_path = log_path.to_path_buf();
        let fallback_path = log_path.clone();
        let offset = self.log_offset;
        let pid = self.pid;
        let output = mj_core::runtime::spawn_blocking(move || -> Result<(PathBuf, String)> {
            let directory = log_path
                .parent()
                .context("daemon stderr path has no parent")?;
            let (diagnostics, exit) = launch_diagnostics(directory, pid)?;
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
            if let Some(exit) = exit {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&format_exit_reason(&exit));
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

fn launch_diagnostics(
    directory: &Path,
    pid: u32,
) -> Result<(Option<PathBuf>, Option<DaemonExitRecord>)> {
    use crate::logging::daemon_log_path;

    if let Some(path) = daemon_log_path(directory, pid)? {
        return Ok((Some(path), None));
    }
    // Failed children have already been reaped by the waiter. Their record
    // links the launch PID to the PID used in the daemon's log filename.
    if let Ok(entries) = fs::read_dir(daemon_exits_dir(directory)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
                && !entry.file_name().as_encoded_bytes().starts_with(b".")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
                && let Ok(record) = read_daemon_exit_record(&path)
                && record.waiter_pid == Some(pid)
            {
                return Ok((daemon_log_path(directory, record.pid)?, Some(record)));
            }
        }
    }
    // A launch that is still starting has a child process and no exit record.
    let parent = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::new(),
    );
    for process in system.processes().values() {
        if process.parent() == Some(parent)
            && let Some(path) = daemon_log_path(directory, process.pid().as_u32())?
        {
            return Ok((Some(path), None));
        }
    }
    Ok((None, None))
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

async fn maybe_replace_stale_development_daemon(_startup: &DaemonStartGuard) -> Result<()> {
    if std::env::var_os(DEV_RESTART_STALE_DAEMON_ENV).is_none() {
        return Ok(());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    DEVELOPMENT_DAEMON_REFRESH
        .get_or_try_init(|| async {
            let Ok(metadata) = read_metadata_any() else {
                return Ok(());
            };
            if ensure_supported_daemon_protocol(&metadata).is_err() {
                return Ok(());
            }
            // The variable makes this client authoritative over stale builds,
            // not over newer ones. Ordinary startup explains the kept daemon.
            let identity = daemon_identity_or_legacy(&metadata).await;
            if daemon_build(&metadata, &identity.placement)?.daemon_is_newer() {
                return Ok(());
            }
            let started: SystemTime = chrono::DateTime::parse_from_rfc3339(&metadata.started_at)
                .context("parse development daemon start time")?
                .into();
            let placement = identity.placement.clone();
            let stale = mj_core::runtime::spawn_blocking(move || -> Result<bool> {
                Ok(daemon_runs_this_build(&identity)? == Some(false)
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
            replace_daemon(_startup, &metadata, &placement).await
        })
        .await?;
    Ok(())
}

/// Automatic upgrades have no authority to cancel work, even when a daemon
/// is slow or temporarily unreachable. Explicit restart uses `stop_daemon`.
async fn replace_daemon(
    startup: &DaemonStartGuard,
    metadata: &DaemonMetadata,
    placement: &DaemonPlacement,
) -> Result<()> {
    mj_core::config::ensure_may_control_store(
        &data_dir(),
        &format!(
            "replace Mjolnir daemon {} (build {})",
            metadata.pid, metadata.build_version
        ),
    )?;
    ensure_config_loads(&mj_core::config::config_path(), "replaced")?;
    ensure_container_owner_recorded(startup, metadata, placement).await?;
    // The replacement daemon pins this build's worker sources before it can
    // serve; for a new build that is seconds of copying and hashing. Do it
    // while the old daemon drains, and wait for it before the launch so the
    // two never hash the same files at once.
    let warming = mj_core::runtime::spawn_blocking(|| {
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
    tracing::info!(
        pid = metadata.pid,
        build = %metadata.build_version,
        "asking the daemon to hand off to this build"
    );
    let outcome = ask_for_handoff(metadata).await?;
    tracing::info!(
        pid = metadata.pid,
        waited_ms = asked.elapsed().as_millis(),
        ?outcome,
        "daemon handoff admission finished"
    );
    match outcome {
        HandoffAskOutcome::Accepted => wait_for_daemon_release(metadata, STOP_TIMEOUT)
            .await
            .context("the daemon accepted the handoff but did not exit"),
        HandoffAskOutcome::Gone if metadata.pid == 0 => clear_exited_daemon(metadata),
        HandoffAskOutcome::Gone | HandoffAskOutcome::Replaced => Ok(()),
    }
}

#[derive(Debug)]
enum HandoffAskOutcome {
    Accepted,
    Gone,
    Replaced,
}

const HANDOFF_ASK_INTERVAL: Duration = Duration::from_millis(250);
const BLOCKER_PREWARM_LEAD: Duration = Duration::from_secs(1);

async fn ask_prepare_upgrade(metadata: &DaemonMetadata) -> Result<DaemonReply> {
    let mut client = DaemonClient::connect(metadata.clone()).await?;
    client.request_single(DaemonAction::PrepareUpgrade).await
}

async fn ask_for_handoff(metadata: &DaemonMetadata) -> Result<HandoffAskOutcome> {
    let ask = async {
        loop {
            match ask_prepare_upgrade(metadata).await {
                Ok(DaemonReply::Done) => return Ok(HandoffAskOutcome::Accepted),
                Ok(DaemonReply::UpgradePending) => {
                    tokio::time::sleep(HANDOFF_ASK_INTERVAL).await;
                }
                Ok(reply) => bail!("unexpected upgrade admission reply {reply:?}"),
                // PrepareUpgrade is idempotent on the daemon. A transient connect
                // or transport error may reflect concurrent teardown before the
                // store lock drops; keep retrying while `observe_owner` watches
                // for replacement or exit.
                Err(error) => {
                    tracing::debug!(%error, "automatic upgrade cannot establish safe handoff yet");
                    tokio::time::sleep(HANDOFF_ASK_INTERVAL).await;
                }
            }
        }
    };
    let observe_owner = async {
        loop {
            if let Some(outcome) = handoff_owner_change(metadata).await? {
                return Ok(outcome);
            }
            tokio::time::sleep(HANDOFF_ASK_INTERVAL).await;
        }
    };
    let notices = async {
        let mut blockers = PendingBlockers::default();
        let prewarm = START_NOTICE_DELAY.saturating_sub(BLOCKER_PREWARM_LEAD);
        tokio::time::sleep(prewarm).await;
        blockers.ensure_started(|| {
            let metadata = metadata.clone();
            async move { upgrade_blockers(&metadata).await }
        });
        tokio::time::sleep(START_NOTICE_DELAY - prewarm).await;
        loop {
            startup_notice(upgrade_wait_notice(
                blockers
                    .refresh(|| {
                        let metadata = metadata.clone();
                        async move { upgrade_blockers(&metadata).await }
                    })
                    .await,
            ));
            tokio::time::sleep(startup_notice_interval()).await;
        }
    };
    tokio::select! {
        biased;
        result = ask => result,
        result = observe_owner => result,
        _ = notices => unreachable!("the handoff notices do not end the wait"),
    }
}

async fn handoff_owner_change(metadata: &DaemonMetadata) -> Result<Option<HandoffAskOutcome>> {
    match probe_daemon_release(metadata).await? {
        DaemonRelease::Replaced => Ok(Some(HandoffAskOutcome::Replaced)),
        DaemonRelease::Exited => Ok(Some(HandoffAskOutcome::Gone)),
        DaemonRelease::StillOwned => Ok(None),
        DaemonRelease::Unknown(reason) => Err(anyhow!(reason)),
    }
}

// The caller holds the startup lock across the compare and removal.
fn clear_exited_daemon(metadata: &DaemonMetadata) -> Result<()> {
    remove_matching_metadata(metadata)?;
    if metadata.pid == 0 {
        remove_container_owner_if_matching(metadata.address, &metadata.token)?;
    }
    Ok(())
}

fn remove_matching_metadata(previous: &DaemonMetadata) -> Result<bool> {
    if !read_metadata_any().is_ok_and(|current| same_daemon_endpoint(&current, previous)) {
        return Ok(false);
    }
    let path = metadata_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

async fn wait_for_daemon_release(metadata: &DaemonMetadata, timeout: Duration) -> Result<()> {
    let exited = tokio::time::timeout(timeout, async {
        loop {
            match probe_daemon_release(metadata).await? {
                DaemonRelease::Replaced => return Ok(false),
                DaemonRelease::Exited => return Ok(true),
                DaemonRelease::Unknown(reason) => bail!("{reason}"),
                DaemonRelease::StillOwned => tokio::time::sleep(RETRY_DELAY).await,
            }
        }
    })
    .await
    .with_context(|| {
        format!(
            "could not verify release of the Mjolnir daemon at {} within {}s",
            metadata.address,
            timeout.as_secs()
        )
    })??;
    if exited && metadata.pid == 0 {
        clear_exited_daemon(metadata)?;
    }
    Ok(())
}

/// The line shown while an automatic upgrade waits for the old daemon.
fn upgrade_wait_notice(blockers: Option<&[String]>) -> String {
    match blockers {
        Some(blockers) => format!(
            "Mjolnir upgrade is waiting for: {}; existing sessions remain available.",
            blockers.join(", ")
        ),
        None => "Mjolnir upgrade is waiting for ongoing work; existing sessions remain available."
            .to_owned(),
    }
}

/// A background observation scoped to one wait for the startup lock or handoff.
#[derive(Default)]
struct PendingBlockers {
    task: Option<tokio::task::JoinHandle<Option<Vec<String>>>>,
    latest: Option<Vec<String>>,
}

impl PendingBlockers {
    fn ensure_started<F>(&mut self, query: impl FnOnce() -> F)
    where
        F: Future<Output = Option<Vec<String>>> + Send + 'static,
    {
        if self.task.is_none() {
            self.task = Some(mj_core::runtime::spawn(query()));
        }
    }

    async fn refresh<F>(&mut self, query: impl FnOnce() -> F) -> Option<&[String]>
    where
        F: Future<Output = Option<Vec<String>>> + Send + 'static,
    {
        if self
            .task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            let task = self.task.take().expect("finished blockers query");
            self.latest = match task.await {
                Ok(labels) => labels,
                Err(error) => {
                    tracing::warn!(%error, "daemon upgrade blockers query failed");
                    None
                }
            };
        }
        self.ensure_started(query);
        self.latest.as_deref()
    }
}

impl Drop for PendingBlockers {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Older daemons close the connection for an unknown action. With no labels,
/// the wait notice describes the handoff without naming any specific work.
async fn upgrade_blockers(metadata: &DaemonMetadata) -> Option<Vec<String>> {
    let mut client = DaemonClient::connect(metadata.clone()).await.ok()?;
    match client
        .request_single(DaemonAction::UpgradeBlockers)
        .await
        .ok()?
    {
        DaemonReply::UpgradeBlockers(labels) if !labels.is_empty() => Some(labels),
        _ => None,
    }
}

async fn ensure_container_owner_recorded(
    _startup: &DaemonStartGuard,
    metadata: &DaemonMetadata,
    placement: &DaemonPlacement,
) -> Result<()> {
    if metadata.pid != 0 {
        return Ok(());
    }
    let record = async {
        if matching_container_owner(metadata)?.is_some() {
            return Ok(());
        }
        let DaemonPlacement::Container {
            name,
            runtime: Some(runtime),
        } = placement
        else {
            bail!(missing_container_owner_reason(None));
        };
        let container_id = container::resolve_container_id(*runtime, name)
            .await?
            .with_context(|| format!("container {name} was not listed by the engine"))?;
        write_container_owner(&ContainerDaemonRecord {
            schema: CONTAINER_OWNER_SCHEMA,
            protocol_version: Some(metadata.protocol_version),
            runtime: *runtime,
            container_id,
            container_name: name.clone(),
            address: metadata.address,
            token: metadata.token.clone(),
            started_at: metadata.started_at.clone(),
        })
    };
    record.await.context(
        "The daemon was left running because its container owner record could not be established",
    )
}

#[derive(Clone, Copy)]
enum StopFallback {
    Graceful,
    Force,
    Restart,
}

async fn stop_daemon(
    startup: &DaemonStartGuard,
    metadata: &DaemonMetadata,
    fallback: StopFallback,
) -> Result<bool> {
    let mut answering = false;
    let attempt = match DaemonClient::connect(metadata.clone()).await {
        Ok(mut client) => {
            // Capture ownership before sending Stop to a legacy container.
            let identity = if metadata.pid == 0
                && !matching_container_owner(metadata).is_ok_and(|record| record.is_some())
            {
                tokio::time::timeout(Duration::from_secs(2), request_daemon_identity(&mut client))
                    .await
                    .context("time out reading the legacy container daemon's identity")
                    .and_then(|identity| identity.map(Some))
            } else {
                Ok(None)
            };
            match identity {
                Ok(identity) => {
                    if let Some(identity) = identity {
                        answering = true;
                        ensure_container_owner_recorded(startup, metadata, &identity.placement)
                            .await?;
                    }
                    ManagementClient::new(client).stop().await
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    let error = match attempt {
        Ok(()) => {
            wait_for_daemon_release(metadata, STOP_DRAIN_TIMEOUT)
                .await
                .context("the daemon accepted the stop but its release could not be verified")?;
            return Ok(true);
        }
        Err(error) => error,
    };
    match probe_daemon_release(metadata).await? {
        DaemonRelease::Exited => {
            clear_exited_daemon(metadata)?;
            Ok(false)
        }
        DaemonRelease::Replaced => bail!("The Mjolnir daemon was replaced while stopping it. Run the stop command again."),
        DaemonRelease::StillOwned if !matches!(fallback, StopFallback::Graceful) => {
            signal_daemon(metadata).await?;
            Ok(true)
        }
        DaemonRelease::StillOwned => Err(error).context(
            "The Mjolnir daemon still owns its store and did not acknowledge the stop. Run `mj daemon stop --force` to stop it",
        ),
        DaemonRelease::Unknown(reason) => {
            if matches!(fallback, StopFallback::Force)
                && metadata.pid == 0
                && !answering
                && clear_unreachable_container_record(metadata).await?
            {
                return Ok(true);
            }
            Err(error).context(reason)
        }
    }
}

/// The caller holds the startup lock and has explicitly forced legacy recovery.
async fn clear_unreachable_container_record(metadata: &DaemonMetadata) -> Result<bool> {
    let unreadable = match matching_container_owner(metadata) {
        Ok(Some(_)) => return Ok(false),
        Ok(None) => false,
        Err(_) => true,
    };
    if matches!(
        tokio::time::timeout(Duration::from_secs(2), connect_ready_container(metadata)).await,
        Ok(Ok(_))
    ) {
        return Ok(false);
    }
    ensure!(
        remove_matching_metadata(metadata)?,
        "The daemon.json record changed while stopping the Mjolnir daemon. Run the stop command again."
    );
    eprintln!(
        "Cleared the stale container daemon record in {}.",
        metadata_path().display()
    );
    if unreadable {
        clear_unreadable_container_owner()?;
    }
    Ok(true)
}

/// Stop the daemon `metadata` names after it did not answer a protocol
/// stop: SIGTERM on Unix, which every supported daemon handles as graceful
/// cancellation. Windows has no SIGTERM, and the daemon has no console to
/// receive Ctrl-Break, so it is terminated; its state is in the store and
/// its workers keep running, so the next daemon recovers it as after a crash.
async fn signal_daemon(metadata: &DaemonMetadata) -> Result<()> {
    if metadata.pid == 0 {
        let record = matching_container_owner(metadata)?.context(
            "the container daemon no longer has a matching daemon-container.json owner record",
        )?;
        // Forcing the container to stop also stops its workers.
        container::stop(record.runtime, &record.container_id).await?;
        return clear_exited_daemon(metadata);
    }
    if let DaemonPlacement::Container {
        name,
        runtime: Some(runtime),
    } = daemon_identity_or_legacy(metadata).await.placement
    {
        container::stop(runtime, &name).await?;
        return clear_exited_daemon(metadata);
    }
    #[cfg(unix)]
    if host_pid_is_daemon(metadata.pid)? {
        // SAFETY: the PID comes from owner-only daemon metadata and SIGTERM is
        // handled as graceful cancellation by every supported daemon.
        let result = unsafe { libc::kill(metadata.pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
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
            if host_pid_is_daemon(metadata.pid)? {
                return Err(error).context("open superseded Mjolnir daemon");
            }
        } else {
            struct Handle(windows_sys::Win32::Foundation::HANDLE);
            impl Drop for Handle {
                fn drop(&mut self) {
                    // SAFETY: the handle came from OpenProcess and is closed once.
                    unsafe { CloseHandle(self.0) };
                }
            }
            let handle = Handle(handle);
            if host_pid_is_daemon(metadata.pid)? {
                // SAFETY: the handle is open with PROCESS_TERMINATE.
                if unsafe { TerminateProcess(handle.0, 1) } == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("stop superseded Mjolnir daemon");
                }
            }
        }
    }
    wait_for_daemon_release(metadata, STOP_TIMEOUT).await
}

/// What runs at a daemon's recorded PID, judged by its command line so a
/// reused PID cannot stand in for the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPid {
    Daemon,
    /// Gone, or running something other than `mj daemon-run`.
    NotDaemon,
    /// The process exists but its command line cannot be read. A daemon in
    /// the middle of exiting looks like this on macOS and Windows: its
    /// arguments are gone before its PID is.
    Unreadable,
}

fn host_pid(pid: u32) -> HostPid {
    if !process_is_alive(pid) {
        return HostPid::NotDaemon;
    }
    let process_id = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    // The command line is read only when the refresh asks for it.
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[process_id]),
        true,
        sysinfo::ProcessRefreshKind::new().with_cmd(sysinfo::UpdateKind::Always),
    );
    let command = system.process(process_id).map(sysinfo::Process::cmd);
    if let Some(command) = command.filter(|command| !command.is_empty()) {
        return if command
            .get(1)
            .is_some_and(|argument| argument == "daemon-run")
        {
            HostPid::Daemon
        } else {
            HostPid::NotDaemon
        };
    }
    if process_is_alive(pid) {
        HostPid::Unreadable
    } else {
        HostPid::NotDaemon
    }
}

/// Whether the daemon still runs at `pid`, for a caller about to signal it.
/// An unreadable command line is inconclusive, so it is an error here.
fn host_pid_is_daemon(pid: u32) -> Result<bool> {
    match host_pid(pid) {
        HostPid::Daemon => Ok(true),
        HostPid::NotDaemon => Ok(false),
        HostPid::Unreadable => {
            bail!("could not read the command line for Mjolnir daemon PID {pid}")
        }
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

const ATTACHMENT_REFRESH_DISPLAY_TIMEOUT: Duration = Duration::from_secs(5);

/// Keep this client's presence fresh in whatever daemon is running.
///
/// This is a two-second timer, not a user action, so it only ever reconnects.
/// It deliberately does not start a daemon: a timer cannot express the intent
/// to start one, and when the client's own executable has been replaced on
/// disk the daemon it would start runs code the user has already discarded.
/// That is exactly how a stopped daemon used to come back on an old build,
/// beating the restart that had just stopped it. When no daemon answers, the
/// surface says so and offers its explicit restart instead.
pub fn maintain_attachment(client_id: String, cancellation: CancellationToken) -> Attachment {
    let (presence_tx, presence) = tokio::sync::watch::channel(DaemonPresence::Attached);
    let task = mj_core::runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    let observed = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = tokio::time::timeout(ATTACHMENT_REFRESH_DISPLAY_TIMEOUT, observe_attachment(&client_id)) => {
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

async fn observe_attachment(client_id: &str) -> Result<DaemonPresence> {
    if let Some(target) = upgraded_daemon_executable().await? {
        return Ok(DaemonPresence::Upgraded(target));
    }
    match connect_existing()
        .await?
        .request_single(DaemonAction::Attach {
            client_id: client_id.to_owned(),
        })
        .await?
    {
        DaemonReply::Done => Ok(DaemonPresence::Attached),
        DaemonReply::UpgradePending => Ok(DaemonPresence::Missing(
            "daemon upgrade handoff is in progress".into(),
        )),
        reply => bail!("unexpected attach reply {reply:?}"),
    }
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

pub(crate) async fn upgraded_daemon_executable() -> Result<Option<UpgradeTarget>> {
    if connector::DaemonConnector::shared().await?.is_ssh() {
        return Ok(None);
    }
    let Ok(metadata) = read_metadata_any() else {
        return Ok(None);
    };
    let identity = daemon_identity_or_legacy(&metadata).await;
    let newer = daemon_build(&metadata, &identity.placement)?
        .release
        .is_gt();
    if !newer && metadata.protocol_version <= PROTOCOL_VERSION {
        return Ok(None);
    }
    // A container daemon runs the image's own executable; there is no local
    // file this client could continue under. The next command starts the
    // client's own build in the ordinary way.
    if identity.placement.container().is_some() {
        return Ok(None);
    }
    let generation = if identity.instance_id.is_empty() {
        format!("{}:{}", metadata.pid, metadata.started_at)
    } else {
        identity.instance_id.clone()
    };
    // Do not loop if another installer replaced the executable again during handoff.
    if std::env::var("MJ_UPGRADE_DAEMON").ok().as_deref() == Some(&generation) {
        return Ok(None);
    }
    let pid = metadata.pid;
    let executable = mj_core::runtime::spawn_blocking(move || process_executable_path(pid))
        .await
        .context("inspect upgraded daemon executable task failed")?;
    Ok(executable
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

    async fn fake_daemon(pid: u32) -> (tokio::net::TcpListener, DaemonMetadata) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid,
            address: listener.local_addr().unwrap(),
            token: "lifecycle-test".into(),
            started_at: "test".into(),
            build_version: this_build().published(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        (listener, metadata)
    }

    async fn answer(
        stream: &mut tokio::net::TcpStream,
        request: RequestEnvelope,
        result: std::result::Result<DaemonReply, String>,
    ) {
        write_frame(
            stream,
            &ResponseEnvelope {
                protocol_version: request.protocol_version,
                request_id: request.request_id,
                result,
            },
        )
        .await
        .unwrap();
    }

    mod container_tests {
        use super::*;

        const CONTAINER_ID: &str = "test-container-id";
        const RUNTIME: DaemonRuntime = DaemonRuntime::Podman;

        fn fake_podman(child: &str, test: &str) -> bool {
            crate::test_support::rerun_in_isolated_child_with(child, test, |command, directory| {
                let tools = directory.join("bin");
                fs::create_dir(&tools).unwrap();
                mj_core::test_hooks::install_fake_command(
                    &tools,
                    "podman",
                    r#"
printf '%s\n' "$*" >> "$MJ_DATA_DIR/engine.calls"
state=$(cat "$MJ_DATA_DIR/engine.state")
case "$1" in
  image) printf 'test-image|%s\n' "$MJ_TEST_IMAGE_REVISION" ;;
  ps)
    if [ "$state" = "ps-error" ]; then
      echo 'simulated engine ps failure' >&2
      exit 125
    fi
    case "$*" in
      *--format={{.Names}}*) if [ "$state" = "inspect-error" ]; then echo predecessor; fi ;;
      *id=old-container-id*) ;;
      *id=test-container-id*) echo test-container-id ;;
      *'name=^/?'*)
        # Podman matches names without a leading slash; require `^/?` so a
        # Docker-only `^/` filter fails the suite.
        filter=$(printf '%s\n' "$*" | sed -n 's/.*name=\^\/?\([^$]*\)\$.*/\1/p' | sed 's/\[\.\]/./g')
        [ -n "$filter" ] || exit 64
        printf 'test-container-id\t%s\n' "$filter" ;;
      *) exit 64 ;;
    esac ;;
  inspect)
    [ "$2 $3" = "--format={{.State.Running}} test-container-id" ] || exit 64
    case "$state" in
      inspect-error) echo 'No such container (simulated engine failure)' >&2; exit 125 ;;
      stopped) echo false ;;
      *) echo true ;;
    esac ;;
  top)
    [ "$2 $3" = 'test-container-id args' ] || exit 64
    printf 'COMMAND\nsleep infinity\n'
    case "$state" in
      daemon) echo '/opt/mjolnir/mj daemon-run' ;;
      *) printf 'hel worker\ncodex app-server\n' ;;
    esac ;;
  start)
    [ "$2" = 'test-container-id' ] || exit 64
    echo worker > "$MJ_DATA_DIR/engine.state" ;;
  port) cat "$MJ_DATA_DIR/engine.endpoint" ;;
  exec)
    [ "$2" = '--detach' ] || exit 64
    [ -s "$MJ_DATA_DIR/daemon-container.json" ] || exit 65
    [ ! -e "$MJ_DATA_DIR/daemon.json" ] || exit 66
    printf '%s' "$MJ_DAEMON_TOKEN" > "$MJ_DATA_DIR/exec.token"
    echo daemon > "$MJ_DATA_DIR/engine.state" ;;
  stop)
    [ "$2" = 'test-container-id' ] || exit 64
    echo stopped > "$MJ_DATA_DIR/engine.state" ;;
  *) exit 64 ;;
esac
"#,
                );
                let paths = std::iter::once(tools)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
                    .collect::<Vec<_>>();
                let build = mj_core::worker_build::build_id();
                let (_, revision) = build.rsplit_once('+').expect("test build has a revision");
                command
                    .env("PATH", std::env::join_paths(paths).unwrap())
                    .env("MJ_TEST_IMAGE_REVISION", revision);
            })
        }

        fn engine_state(state: &str) {
            fs::write(data_dir().join("engine.state"), state).unwrap();
        }

        fn calls() -> String {
            fs::read_to_string(data_dir().join("engine.calls")).unwrap_or_default()
        }

        async fn configure_container() -> Box<container::ContainerSpec> {
            fs::write(
                mj_core::config::config_path(),
                "version = 1\n[daemon]\nruntime = \"podman\"\n",
            )
            .unwrap();
            let config = load_daemon_config().await.unwrap();
            let ConfiguredPlacement::Container(spec) = resolve_placement(&config).await.unwrap()
            else {
                panic!("expected fake engine placement");
            };
            spec
        }

        fn owner(
            metadata: &DaemonMetadata,
            container_id: &str,
            name: &str,
        ) -> ContainerDaemonRecord {
            ContainerDaemonRecord {
                schema: CONTAINER_OWNER_SCHEMA,
                protocol_version: Some(metadata.protocol_version),
                runtime: RUNTIME,
                container_id: container_id.into(),
                container_name: name.into(),
                address: metadata.address,
                token: metadata.token.clone(),
                started_at: metadata.started_at.clone(),
            }
        }

        fn set_endpoint(listener: &tokio::net::TcpListener) {
            fs::write(
                data_dir().join("engine.endpoint"),
                listener.local_addr().unwrap().to_string(),
            )
            .unwrap();
        }

        async fn serve_container_readiness(
            listener: &tokio::net::TcpListener,
            identity: Option<DaemonIdentity>,
        ) {
            let (mut stream, _) = listener.accept().await.unwrap();
            let ping: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(ping.action, DaemonAction::Ping));
            assert_eq!(ping.token, read_container_owner().unwrap().unwrap().token);
            answer(&mut stream, ping, Ok(DaemonReply::Pong)).await;
            if let Some(identity) = identity {
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(request.action, DaemonAction::Identity));
                answer(&mut stream, request, Ok(DaemonReply::Identity(identity))).await;
            }
        }

        // Hard-won: #1458: stale container metadata prevented relaunch after the daemon exited.
        #[tokio::test]
        async fn exited_container_daemons_relaunch_without_disrupting_workers() {
            if fake_podman(
                "MJ_TEST_RELAUNCH_CONTAINER",
                "daemon::tests::container_tests::exited_container_daemons_relaunch_without_disrupting_workers",
            ) {
                return;
            }
            engine_state("worker");
            let spec = configure_container().await;
            for (state, old_id) in [
                ("absent", "old-container-id"),
                ("stopped", CONTAINER_ID),
                ("worker", CONTAINER_ID),
            ] {
                engine_state(state);
                fs::write(data_dir().join("engine.calls"), "").unwrap();
                let (old, metadata) = fake_daemon(0).await;
                let (ready, _) = fake_daemon(0).await;
                drop(old);
                set_endpoint(&ready);
                write_launcher_metadata(&metadata).unwrap();
                write_container_owner(&owner(&metadata, old_id, &spec.name)).unwrap();
                assert!(matches!(
                    probe_daemon_release(&metadata).await.unwrap(),
                    DaemonRelease::Exited
                ));

                let startup = acquire_start_guard(data_dir().join("daemon-start.lock"))
                    .await
                    .unwrap();
                let (opened, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                    tokio::join!(
                        connect_or_start_holding(&startup),
                        serve_container_readiness(&ready, None),
                    )
                })
                .await
                .unwrap();
                opened.unwrap();
                let published = read_metadata_any().unwrap();
                let record = read_container_owner().unwrap().unwrap();
                assert_eq!(published.address, ready.local_addr().unwrap());
                assert_ne!(metadata.token, published.token);
                assert!(record.matches(&published));
                let calls = calls();
                assert_eq!(
                    calls
                        .lines()
                        .filter(|call| call.starts_with("exec --detach"))
                        .count(),
                    1,
                    "{calls}"
                );
                assert!(
                    !calls
                        .lines()
                        .any(|call| call.starts_with("stop ") || call.starts_with("rm ")),
                    "{calls}"
                );
                assert_eq!(
                    calls.contains("start test-container-id"),
                    state == "stopped",
                    "{calls}"
                );
                clear_exited_daemon(&published).unwrap();
            }
        }

        // Hard-won: #1568: a container's hung RPC was treated as released ownership.
        #[tokio::test]
        async fn silent_container_endpoint_retains_ownership_and_blocks_relaunch() {
            if fake_podman(
                "MJ_TEST_SILENT_CONTAINER",
                "daemon::tests::container_tests::silent_container_endpoint_retains_ownership_and_blocks_relaunch",
            ) {
                return;
            }
            engine_state("daemon");
            let spec = configure_container().await;
            let (_listener, metadata) = fake_daemon(0).await;
            write_launcher_metadata(&metadata).unwrap();
            write_container_owner(&owner(&metadata, CONTAINER_ID, &spec.name)).unwrap();
            assert!(matches!(
                probe_daemon_release(&metadata).await.unwrap(),
                DaemonRelease::StillOwned
            ));
            assert!(matches!(
                decide_existing_daemon().await.unwrap(),
                ExistingDaemon::Unanswered(_)
            ));
            let startup = acquire_start_guard(data_dir().join("daemon-start.lock"))
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(3), connect_or_start_holding(&startup))
                    .await
                    .is_err()
            );
            assert!(!calls().contains("exec --detach"));
            assert!(read_container_owner().unwrap().unwrap().matches(&metadata));
        }

        // Hard-won: #1458: a failed container inspect was treated as an idle predecessor.
        #[tokio::test]
        async fn engine_and_owner_record_errors_fail_closed() {
            if fake_podman(
                "MJ_TEST_CONTAINER_PROBE_FAILURE",
                "daemon::tests::container_tests::engine_and_owner_record_errors_fail_closed",
            ) {
                return;
            }
            engine_state("inspect-error");
            let spec = configure_container().await;
            let (listener, metadata) = fake_daemon(0).await;
            drop(listener);
            write_launcher_metadata(&metadata).unwrap();
            let record = owner(&metadata, CONTAINER_ID, &spec.name);
            write_container_owner(&record).unwrap();
            let DaemonRelease::Unknown(reason) = probe_daemon_release(&metadata).await.unwrap()
            else {
                panic!("inspect error must leave ownership unknown");
            };
            assert!(reason.contains("simulated engine failure"), "{reason}");

            write_container_owner(&ContainerDaemonRecord {
                token: "unbound".into(),
                ..record.clone()
            })
            .unwrap();
            let DaemonRelease::Unknown(reason) = probe_daemon_release(&metadata).await.unwrap()
            else {
                panic!("record for another token must leave ownership unknown");
            };
            assert!(
                reason.contains("without a matching daemon-container.json"),
                "{reason}"
            );

            fs::write(container_owner_path(), b"{").unwrap();
            let DaemonRelease::Unknown(reason) = probe_daemon_release(&metadata).await.unwrap()
            else {
                panic!("corrupt record must leave ownership unknown");
            };
            assert!(reason.contains("parse "), "{reason}");
            fs::remove_file(metadata_path()).unwrap();
            let Err(error) = decide_existing_daemon().await else {
                panic!("startup ignored an unreadable unpublished owner");
            };
            assert!(format!("{error:#}").contains("daemon-container.json"));

            fs::remove_file(container_owner_path()).unwrap();
            let startup = acquire_start_guard(data_dir().join("daemon-start.lock"))
                .await
                .unwrap();
            let Err(error) = connect_or_start_holding(&startup).await else {
                panic!("startup ignored the predecessor's inspect error");
            };
            assert!(format!("{error:#}").contains("simulated engine failure"));
            assert!(
                !calls()
                    .lines()
                    .any(|call| call.starts_with("rm ") || call.starts_with("exec "))
            );
        }

        // Hard-won: #1458: a launcher crash stranded an unpublished running daemon.
        #[tokio::test]
        async fn successor_adopts_a_container_after_the_launcher_loses_its_lock() {
            if fake_podman(
                "MJ_TEST_ADOPT_CONTAINER",
                "daemon::tests::container_tests::successor_adopts_a_container_after_the_launcher_loses_its_lock",
            ) {
                return;
            }
            engine_state("worker");
            let spec = configure_container().await;
            let (listener, _) = fake_daemon(0).await;
            set_endpoint(&listener);
            let lock = data_dir().join("daemon-start.lock");
            let launcher = acquire_start_guard(lock.clone()).await.unwrap();
            let launch = container::launch(&spec, None).await.unwrap();
            assert!(!metadata_path().exists());
            drop(launcher);
            assert!(matches!(
                decide_existing_daemon().await.unwrap(),
                ExistingDaemon::PendingContainer(record) if record.matches(&launch.metadata)
            ));
            assert!(!metadata_path().exists());
            fs::write(metadata_path(), b"{").unwrap();
            assert!(matches!(
                decide_existing_daemon().await.unwrap(),
                ExistingDaemon::PendingContainer(record) if record.matches(&launch.metadata)
            ));
            assert_eq!(fs::read(metadata_path()).unwrap(), b"{");
            fs::remove_file(metadata_path()).unwrap();

            let identity = DaemonIdentity {
                instance_id: "adopted".into(),
                build_version: "999.0.0".into(),
                placement: launch.metadata.placement.clone(),
            };
            let peer = async {
                assert!(!metadata_path().exists());
                serve_container_readiness(&listener, Some(identity.clone())).await;
                // The published record now goes back through build/placement discovery.
                serve_container_readiness(&listener, Some(identity)).await;
            };
            let (opened, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(connect_or_start_local(), peer)
            })
            .await
            .unwrap();
            opened.unwrap();
            let published = read_metadata_any().unwrap();
            assert!(same_daemon_endpoint(&published, &launch.metadata));
            assert_eq!(published.build_version, "999.0.0");
            assert_eq!(
                calls()
                    .lines()
                    .filter(|call| call.starts_with("exec --detach"))
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn retry_reuses_credentials_of_an_interrupted_container_exec() {
            if fake_podman(
                "MJ_TEST_RETRY_CONTAINER_EXEC",
                "daemon::tests::container_tests::retry_reuses_credentials_of_an_interrupted_container_exec",
            ) {
                return;
            }
            engine_state("worker");
            let spec = configure_container().await;
            let (listener, metadata) = fake_daemon(0).await;
            let prior = owner(&metadata, CONTAINER_ID, &spec.name);
            set_endpoint(&listener);
            write_container_owner(&prior).unwrap();
            assert!(!metadata_path().exists());
            assert_eq!(
                container::probe_container_daemon(RUNTIME, CONTAINER_ID).await,
                container::ContainerDaemonStatus::Exited {
                    container_running: true
                }
            );
            // Stopping an apparently exited pending launch while the container
            // still runs must keep its credentials so a delayed exec cannot
            // become orphaned or split from a retry.
            assert!(!stop_instance(false).await.unwrap());
            assert_eq!(read_container_owner().unwrap().as_ref(), Some(&prior));

            // Explicit --force must stop the running container to prevent a
            // delayed exec from starting after stop reports success.
            assert!(stop_instance(true).await.unwrap());
            assert!(calls().contains("stop test-container-id"));
            assert!(!container_owner_path().exists());

            // Restore the running container + pending owner for the retry check.
            engine_state("worker");
            write_container_owner(&prior).unwrap();

            let peer = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let ping: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(ping.action, DaemonAction::Ping));
                // The delayed first exec wins the store lock. Only its original
                // token authenticates, regardless of what the retry wrote.
                assert_eq!(ping.token, prior.token);
                assert_eq!(
                    fs::read_to_string(data_dir().join("exec.token")).unwrap(),
                    prior.token
                );
                assert_eq!(read_container_owner().unwrap().unwrap().token, prior.token);
                answer(&mut stream, ping, Ok(DaemonReply::Pong)).await;
            };
            let startup = acquire_start_guard(data_dir().join("daemon-start.lock"))
                .await
                .unwrap();
            let (connected, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(connect_or_start_holding(&startup), peer)
            })
            .await
            .unwrap();
            connected.unwrap();
            assert!(prior.matches(&read_metadata_any().unwrap()));
        }

        #[tokio::test]
        async fn failing_to_record_legacy_container_owner_leaves_daemon_running() {
            if fake_podman(
                "MJ_TEST_LEGACY_OWNER_REQUIRED",
                "daemon::tests::container_tests::failing_to_record_legacy_container_owner_leaves_daemon_running",
            ) {
                return;
            }
            engine_state("worker");
            let spec = configure_container().await;
            let (listener, metadata) = fake_daemon(0).await;
            let identity = DaemonIdentity {
                instance_id: "legacy-container".into(),
                build_version: metadata.build_version.clone(),
                placement: DaemonPlacement::Container {
                    name: spec.name,
                    runtime: Some(RUNTIME),
                },
            };
            write_launcher_metadata(&metadata).unwrap();
            engine_state("ps-error");
            let startup = acquire_start_guard(data_dir().join("daemon-start.lock"))
                .await
                .unwrap();
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                replace_daemon(&startup, &metadata, &identity.placement),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(format!("{error:#}").contains("simulated engine ps failure"));
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
            drop(startup);

            for force in [false, true] {
                let peer = async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                    assert!(matches!(request.action, DaemonAction::Identity));
                    answer(
                        &mut stream,
                        request,
                        Ok(DaemonReply::Identity(identity.clone())),
                    )
                    .await;
                    use tokio::io::AsyncReadExt;
                    assert_eq!(
                        stream.read(&mut [0]).await.unwrap(),
                        0,
                        "client sent Stop after seeding failed"
                    );
                };
                let (stopped, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(stop_instance(force), peer)
                })
                .await
                .unwrap();
                let error = format!("{:#}", stopped.unwrap_err());
                assert!(error.contains("left running"), "{error}");
                assert!(error.contains("simulated engine ps failure"), "{error}");
            }
            assert!(same_daemon_endpoint(
                &read_metadata_any().unwrap(),
                &metadata
            ));
            assert!(!container_owner_path().exists());
            assert!(!calls().lines().any(|call| call.starts_with("stop ")));
        }

        #[tokio::test]
        async fn force_stop_removes_unreadable_owner_but_preserves_valid_owner_on_engine_failure() {
            if fake_podman(
                "MJ_TEST_CLEAR_UNREADABLE_CONTAINER_OWNER",
                "daemon::tests::container_tests::force_stop_removes_unreadable_owner_but_preserves_valid_owner_on_engine_failure",
            ) {
                return;
            }
            let (listener, metadata) = fake_daemon(0).await;
            drop(listener);
            for published in [false, true] {
                fs::write(container_owner_path(), b"{").unwrap();
                if published {
                    write_launcher_metadata(&metadata).unwrap();
                }
                let Err(discovery) = decide_existing_daemon().await else {
                    panic!("discovery accepted an unreadable owner");
                };
                for error in [discovery, stop_instance(false).await.unwrap_err()] {
                    let error = format!("{error:#}");
                    assert!(error.contains("daemon-container.json"), "{error}");
                    assert!(error.contains("mj daemon stop --force"), "{error}");
                }
                assert_eq!(metadata_path().exists(), published);
                assert!(container_owner_path().exists());
                assert!(stop_instance(true).await.unwrap());
                assert!(!metadata_path().exists());
                assert!(!container_owner_path().exists());
                assert!(matches!(
                    decide_existing_daemon().await.unwrap(),
                    ExistingDaemon::Absent
                ));
            }

            engine_state("ps-error");
            write_launcher_metadata(&metadata).unwrap();
            let record = owner(&metadata, CONTAINER_ID, "daemon");
            write_container_owner(&record).unwrap();
            let error = format!("{:#}", stop_instance(true).await.unwrap_err());
            assert!(error.contains("simulated engine ps failure"), "{error}");
            assert!(record.matches(&read_metadata_any().unwrap()));
            assert_eq!(read_container_owner().unwrap(), Some(record));
            assert!(!calls().lines().any(|call| call.starts_with("stop ")));
        }

        // Hard-won: #1568: stop treated a container's closed socket as released ownership.
        #[tokio::test]
        async fn legacy_container_stop_refuses_silence_and_seeds_an_answering_owner() {
            if fake_podman(
                "MJ_TEST_LEGACY_CONTAINER_STOP",
                "daemon::tests::container_tests::legacy_container_stop_refuses_silence_and_seeds_an_answering_owner",
            ) {
                return;
            }
            let (listener, metadata) = fake_daemon(0).await;
            drop(listener);
            write_launcher_metadata(&metadata).unwrap();
            let Err(discovery) = decide_existing_daemon().await else {
                panic!("discovery allowed an untracked container");
            };
            for error in [
                discovery,
                ask_for_handoff(&metadata).await.unwrap_err(),
                stop_instance(false).await.unwrap_err(),
            ] {
                assert!(
                    format!("{error:#}").contains("mj daemon stop --force"),
                    "{error:#}"
                );
            }
            assert!(metadata_path().exists());
            assert!(stop_instance(true).await.unwrap());
            assert!(!metadata_path().exists());

            engine_state("daemon");
            let spec = configure_container().await;
            let (listener, metadata) = fake_daemon(0).await;
            write_launcher_metadata(&metadata).unwrap();
            let peer = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(request.action, DaemonAction::Identity));
                let identity = DaemonIdentity {
                    instance_id: "legacy-container".into(),
                    build_version: metadata.build_version.clone(),
                    placement: DaemonPlacement::Container {
                        name: spec.name,
                        runtime: Some(RUNTIME),
                    },
                };
                answer(&mut stream, request, Ok(DaemonReply::Identity(identity))).await;
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(request.action, DaemonAction::Stop));
                let record = read_container_owner().unwrap().unwrap();
                assert!(record.matches(&metadata));
                assert_eq!(record.container_id, CONTAINER_ID);
                answer(&mut stream, request, Ok(DaemonReply::Done)).await;
                drop(stream);
                drop(listener);
                // The endpoint goes away before the daemon process, as in
                // real teardown. Wait until the client observes that process.
                while !calls().contains("top test-container-id args") {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(metadata_path().exists());
                assert!(container_owner_path().exists());
                engine_state("worker");
            };
            let (stopped, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(stop_instance(true), peer)
            })
            .await
            .unwrap();
            assert!(stopped.unwrap());
            assert!(!calls().contains("stop test-container-id"));
            assert!(!metadata_path().exists());
            assert!(!container_owner_path().exists());
        }
    }

    // Hard-won: #1568: readiness dropped a slow Ping before it could return Pong.
    #[tokio::test]
    async fn startup_announces_while_a_ping_is_still_pending() {
        use std::cell::Cell;

        let (listener, metadata) = fake_daemon(std::process::id()).await;
        let announced = Cell::new(None);
        let readiness = wait_for_ready_daemon(
            || async { true },
            || async {
                let mut client = DaemonClient::connect(metadata.clone()).await?;
                ping_daemon(&mut client).await
            },
            |waited| announced.set(Some(waited)),
        );
        let peer = async {
            // Keep the clock unpaused during real loopback TCP connect and frame
            // exchange so Tokio's paused auto-advance cannot fire Connect's 1s
            // timeout while the kernel completes the handshake.
            let (mut stream, _) = listener.accept().await.unwrap();
            let ping: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(ping.action, DaemonAction::Ping));
            tokio::time::pause();
            tokio::time::sleep(START_NOTICE_DELAY + Duration::from_secs(1)).await;
            let announced_at = announced
                .get()
                .expect("startup notice while Ping is pending");
            assert!((START_NOTICE_DELAY..START_NOTICE_DELAY + RETRY_DELAY).contains(&announced_at));
            tokio::time::resume();
            answer(&mut stream, ping, Ok(DaemonReply::Pong)).await;
        };
        let (outcome, ()) = tokio::join!(readiness, peer);
        assert!(matches!(outcome, StartupOutcome::Ready(())));
    }

    // Hard-won: #1568: a slow blocker query held up startup lock retries.
    #[tokio::test(start_paused = true)]
    async fn startup_lock_retries_and_aborts_its_pending_blocker_query() {
        const CHILD: &str = "MJ_TEST_PENDING_STARTUP_BLOCKERS";
        const TEST: &str =
            "daemon::tests::startup_lock_retries_and_aborts_its_pending_blocker_query";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        let path = data_dir().join("daemon-start.lock");
        let held = acquire_start_guard(path.clone()).await.unwrap();
        let (listener, metadata) = fake_daemon(std::process::id()).await;
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let (route, mut notices) = StartupNoticeRoute::open();
        let waiting = mj_core::runtime::spawn(acquire_start_lock(path, false));
        let mut stream = tokio::time::timeout(START_NOTICE_DELAY + Duration::from_secs(1), async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(request.action, DaemonAction::UpgradeBlockers));
            stream
        })
        .await
        .unwrap();
        let notice = tokio::time::timeout(
            BLOCKER_PREWARM_LEAD + Duration::from_secs(1),
            notices.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(notice.contains("another client"), "{notice}");
        drop(held);

        // The peer never replies. Acquiring the lock must end the wait and
        // close the blocker connection before the old two-second RPC cap.
        tokio::time::timeout(Duration::from_secs(1), async {
            waiting.await.unwrap().unwrap();
            use tokio::io::AsyncReadExt;
            assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
        })
        .await
        .unwrap();
        drop(route);
    }

    // Hard-won: #1568: a post-ack timeout sent the client back to PrepareUpgrade.
    #[tokio::test(start_paused = true)]
    async fn handoff_release_timeout_does_not_ask_for_admission_again() {
        const CHILD: &str = "MJ_TEST_HANDOFF_RELEASE_TIMEOUT";
        const TEST: &str =
            "daemon::tests::handoff_release_timeout_does_not_ask_for_admission_again";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        let _held = ControllerStoreGuard::acquire().unwrap();
        let (listener, metadata) = fake_daemon(std::process::id()).await;
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let peer = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(request.action, DaemonAction::PrepareUpgrade));
            // A partially written replacement record does not authorize
            // moving on while the daemon still holds the store.
            fs::write(metadata_path(), b"{").unwrap();
            tokio::time::sleep(Duration::from_secs(6)).await;
            answer(&mut stream, request, Ok(DaemonReply::Done)).await;
            listener
        });
        let error = tokio::time::timeout(
            STOP_TIMEOUT + Duration::from_secs(10),
            hand_off_daemon(&metadata),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(format!("{error:#}").contains("accepted the handoff"));
        let listener = peer.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn handoff_waits_for_store_release_when_peer_drops_during_shutdown() {
        const CHILD: &str = "MJ_TEST_HANDOFF_PEER_DROP";
        const TEST: &str =
            "daemon::tests::handoff_waits_for_store_release_when_peer_drops_during_shutdown";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        let held = ControllerStoreGuard::acquire().unwrap();
        let (listener, metadata) = fake_daemon(std::process::id()).await;
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let peer = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(request.action, DaemonAction::PrepareUpgrade));
            // Drop socket and listener first, then release the store guard
            // shortly afterward, matching real teardown ordering.
            drop(stream);
            drop(listener);
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(held);
        });
        tokio::time::timeout(Duration::from_secs(5), hand_off_daemon(&metadata))
            .await
            .unwrap()
            .unwrap();
        peer.await.unwrap();
    }

    // Hard-won: #1568: a draining daemon left the keep-alive displaying Attached.
    #[tokio::test]
    async fn attachment_reports_upgrade_refusal_within_the_same_refresh() {
        const CHILD: &str = "MJ_TEST_ATTACHMENT_UPGRADE_REFUSAL";
        const TEST: &str =
            "daemon::tests::attachment_reports_upgrade_refusal_within_the_same_refresh";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        let (listener, metadata) = fake_daemon(std::process::id()).await;
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let cancellation = CancellationToken::new();
        let Attachment { task, mut presence } =
            maintain_attachment("display-client".into(), cancellation.clone());
        let serve = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(request.action, DaemonAction::Identity));
            let identity = test_daemon_identity(&metadata.build_version);
            answer(&mut stream, request, Ok(DaemonReply::Identity(identity))).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(
                matches!(&request.action, DaemonAction::Attach { client_id } if client_id == "display-client")
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(2100), presence.changed())
                    .await
                    .is_err()
            );
            assert_eq!(*presence.borrow(), DaemonPresence::Attached);
            answer(&mut stream, request, Ok(DaemonReply::UpgradePending)).await;
            presence.changed().await.unwrap();
            assert_eq!(
                *presence.borrow(),
                DaemonPresence::Missing("daemon upgrade handoff is in progress".into())
            );
        };
        tokio::time::timeout(Duration::from_secs(4), serve)
            .await
            .unwrap();
        cancellation.cancel();
        task.await.unwrap();
    }

    // Hard-won: #1568: a pid-zero record with an unavailable Identity was treated as a host daemon.
    #[tokio::test]
    async fn pid_zero_is_container_placement_when_identity_fails() {
        let (listener, mut metadata) = fake_daemon(0).await;
        metadata.protocol_version -= 1;
        let peer = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert!(matches!(request.action, DaemonAction::Identity));
            answer(&mut stream, request, Err("identity unavailable".into())).await;
        };
        let (identity, ()) = tokio::join!(daemon_identity_or_legacy(&metadata), peer);
        assert!(matches!(
            identity.placement,
            DaemonPlacement::Container { runtime: None, .. }
        ));
    }

    // Hard-won: #1568: startup ignored corruption even when the daemon owned the store.
    #[tokio::test]
    async fn a_corrupt_daemon_record_is_absent_only_with_a_free_host_store() {
        const CHILD: &str = "MJ_TEST_CORRUPT_DAEMON_RECORD";
        const TEST: &str =
            "daemon::tests::a_corrupt_daemon_record_is_absent_only_with_a_free_host_store";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        fs::write(metadata_path(), b"{").unwrap();
        let held = ControllerStoreGuard::acquire().unwrap();
        let Err(error) = decide_existing_daemon().await else {
            panic!("startup accepted a corrupt daemon.json while the host store was held");
        };
        assert!(format!("{error:#}").contains("daemon.json"), "{error:#}");
        drop(held);
        assert!(matches!(
            decide_existing_daemon().await.unwrap(),
            ExistingDaemon::Absent
        ));
    }

    // Hard-won: #1566: startup guessed a legacy identity after abandoning a supported daemon's request.
    #[tokio::test]
    async fn startup_propagates_identity_failure_instead_of_guessing_placement() {
        const CHILD: &str = "MJ_TEST_STARTUP_IDENTITY_FAILURE";
        const TEST: &str =
            "daemon::tests::startup_propagates_identity_failure_instead_of_guessing_placement";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        for build_version in [env!("CARGO_PKG_VERSION"), "0.0.1"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let metadata = DaemonMetadata {
                protocol_version: PROTOCOL_VERSION,
                pid: std::process::id(),
                address: listener.local_addr().unwrap(),
                token: "isolated-identity-failure".into(),
                started_at: "test".into(),
                build_version: build_version.into(),
                instance_id: String::new(),
                placement: DaemonPlacement::Host,
            };
            fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
            let server = mj_core::runtime::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let ping: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(ping.action, DaemonAction::Ping));
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: ping.protocol_version,
                        request_id: ping.request_id,
                        result: Ok(DaemonReply::Pong),
                    },
                )
                .await
                .unwrap();
                let identity: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(identity.action, DaemonAction::Identity));
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: identity.protocol_version,
                        request_id: identity.request_id,
                        result: Err("identity unavailable".into()),
                    },
                )
                .await
                .unwrap();
                use tokio::io::AsyncReadExt;
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
            });
            let error = match decide_existing_daemon().await {
                Err(error) => error,
                Ok(_) => panic!("startup used a daemon without its identity"),
            };
            let message = format!("{error:#}");
            assert!(message.contains("identity unavailable"), "{message}");
            server.await.unwrap();
        }
    }

    // Hard-won: #1566: guessed RPC deadlines blocked startup and same-protocol upgrades under load.
    #[tokio::test]
    async fn startup_waits_for_slow_identity_before_replacing_an_older_release() {
        const CHILD: &str = "MJ_TEST_SLOW_STARTUP_IDENTITY";
        const TEST: &str =
            "daemon::tests::startup_waits_for_slow_identity_before_replacing_an_older_release";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "isolated-slow-identity".into(),
            started_at: "test".into(),
            build_version: "0.0.1".into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let (received, mut waiting) = tokio::sync::mpsc::unbounded_channel();
        let server = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for is_identity in [false, true] {
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(if is_identity {
                    matches!(request.action, DaemonAction::Identity)
                } else {
                    matches!(request.action, DaemonAction::Ping)
                });
                if is_identity {
                    let (release, released) = tokio::sync::oneshot::channel();
                    received.send(release).unwrap();
                    released.await.unwrap();
                }
                let reply = if is_identity {
                    DaemonReply::Identity(DaemonIdentity {
                        instance_id: "isolated-slow-identity".into(),
                        build_version: "0.0.1".into(),
                        // Avoid a host store check: this peer represents a
                        // container daemon with its own volume and store.
                        placement: DaemonPlacement::Container {
                            name: "isolated-slow-identity".into(),
                            runtime: None,
                        },
                    })
                } else {
                    DaemonReply::Pong
                };
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id: request.request_id,
                        result: Ok(reply),
                    },
                )
                .await
                .unwrap();
            }
        });
        let decision = decide_existing_daemon();
        tokio::pin!(decision);
        let release = tokio::select! {
            result = &mut decision => panic!("startup finished before Identity replied: {}", result.is_ok()),
            release = waiting.recv() => release.unwrap(),
        };
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        // Poll after the old deadline, before releasing Identity's reply.
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(decision.as_mut().poll(cx).is_pending())
            })
            .await,
            "startup abandoned its pending Identity request"
        );
        release.send(()).unwrap();
        assert!(matches!(
            decision.await.unwrap(),
            ExistingDaemon::Replace { .. }
        ));
        server.await.unwrap();
    }

    // Hard-won: 413864386: the startup splash displayed frozen blocker ages for thirty seconds.
    #[tokio::test]
    async fn splash_refreshes_blockers_each_second_for_replacing_and_queued_clients() {
        const CHILD: &str = "MJ_TEST_LIVE_HANDOFF_NOTICES";
        const TEST: &str =
            "daemon::tests::splash_refreshes_blockers_each_second_for_replacing_and_queued_clients";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        // A live daemon owns the store; the handoff treats a free store as a
        // daemon that has gone.
        let _store = ControllerStoreGuard::acquire().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "isolated-notices".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let server = mj_core::runtime::spawn(async move {
            use std::io::{Error, ErrorKind};

            // Cancelling a waiter can close either its admission exchange or
            // its background blocker query while the fake daemon is replying.
            let disconnected = |error: &anyhow::Error| {
                error.downcast_ref::<Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        ErrorKind::UnexpectedEof
                            | ErrorKind::BrokenPipe
                            | ErrorKind::ConnectionReset
                    )
                })
            };
            let mut age = 0;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: RequestEnvelope = match read_frame(&mut stream).await {
                    Ok(request) => request,
                    Err(error) if disconnected(&error) => continue,
                    Err(error) => panic!("read fake daemon request: {error:#}"),
                };
                let reply = match request.action {
                    DaemonAction::Identity => DaemonReply::Identity(DaemonIdentity {
                        instance_id: "isolated-notices".into(),
                        build_version: env!("CARGO_PKG_VERSION").into(),
                        placement: DaemonPlacement::Host,
                    }),
                    DaemonAction::PrepareUpgrade => DaemonReply::UpgradePending,
                    DaemonAction::UpgradeBlockers => {
                        age += 1;
                        DaemonReply::UpgradeBlockers(vec![format!("session lifecycle ({age}s)")])
                    }
                    action => panic!("unexpected management action {action:?}"),
                };
                if let Err(error) = write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id: request.request_id,
                        result: Ok(reply),
                    },
                )
                .await
                {
                    assert!(disconnected(&error), "write fake daemon reply: {error:#}");
                }
            }
        });
        let (_route, mut notices) = StartupNoticeRoute::open();
        let path = data_dir().join("daemon-start.lock");
        let held = acquire_start_guard(path.clone()).await.unwrap();
        for queued in [false, true] {
            let metadata = metadata.clone();
            let path = path.clone();
            let waiting = mj_core::runtime::spawn(async move {
                if queued {
                    let _guard = acquire_shared_start_guard(path).await?;
                    Ok(())
                } else {
                    hand_off_daemon(&metadata).await
                }
            });
            let first = tokio::time::timeout(Duration::from_secs(15), notices.recv())
                .await
                .unwrap()
                .unwrap();
            let second = tokio::time::timeout(Duration::from_secs(3), notices.recv())
                .await
                .expect("the splash age must refresh without waiting thirty seconds")
                .unwrap();
            assert!(first.contains("session lifecycle ("), "{first}");
            assert!(second.contains("session lifecycle ("), "{second}");
            assert_ne!(first, second, "a fresh daemon age must reach the splash");
            if queued {
                assert!(first.contains("another client"), "{first}");
            }
            waiting.abort();
            assert!(waiting.await.unwrap_err().is_cancelled());
        }
        drop(held);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    async fn viewer_fixture(
        statuses: Vec<mj_client::web::WebViewerAccess>,
    ) -> (DaemonClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address: listener.local_addr().unwrap(),
            token: "test".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let task = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut statuses = statuses.into_iter();
            while let Ok(request) = read_frame::<RequestEnvelope>(&mut stream).await {
                assert!(matches!(request.action, DaemonAction::WebViewerAccess));
                let Some(access) = statuses.next() else {
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
                        result: Ok(DaemonReply::WebViewerAccess(access)),
                    },
                )
                .await
                .unwrap();
            }
        });
        (DaemonClient::connect(metadata).await.unwrap(), task)
    }

    #[tokio::test]
    async fn api_waits_for_the_viewer_without_a_retry_command() {
        let (mut client, task) = viewer_fixture(vec![
            mj_client::web::WebViewerAccess::Starting,
            mj_client::web::WebViewerAccess::Starting,
            mj_client::web::WebViewerAccess::Ready {
                viewer_url: "http://127.0.0.1:1234".into(),
                viewer_code: "test".into(),
                qr_login_url: None,
                fallback_reason: None,
                local_address: Some("127.0.0.1:1234".parse().unwrap()),
                certificate_sha256: None,
            },
        ])
        .await;
        assert!(matches!(
            wait_for_web_access(&mut client).await.unwrap(),
            mj_client::web::WebViewerAccess::Ready { viewer_url, .. }
                if viewer_url == "http://127.0.0.1:1234"
        ));
        drop(client);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn viewer_startup_reports_failure_and_bounds_a_stalled_start() {
        let (mut client, task) = viewer_fixture(vec![
            mj_client::web::WebViewerAccess::Starting,
            mj_client::web::WebViewerAccess::Failed {
                address: "127.0.0.1:1234".parse().unwrap(),
                message: "port unavailable".into(),
                port_conflict: false,
            },
        ])
        .await;
        assert!(
            wait_for_web_access(&mut client)
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
            wait_for_web_access_with_timeout(&mut client, timeout)
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(start.elapsed() >= timeout);
        drop(client);
        task.await.unwrap();
    }
    // Hard-won: #1524: a session-launched daemon inherited staged profile homes.
    #[cfg(unix)]
    #[test]
    fn host_daemon_environment_restores_login_and_preserves_instance() {
        const CHILD: &str = "MJ_TEST_DAEMON_ENVIRONMENT_CHILD";
        const TEST: &str =
            "daemon::tests::host_daemon_environment_restores_login_and_preserves_instance";
        if let Ok(mode) = std::env::var(CHILD) {
            let mut command = std::process::Command::new("/usr/bin/env");
            configure_host_daemon_environment(
                &mut command,
                host_daemon_login_environment().unwrap(),
            )
            .unwrap();
            let output = mj_core::subprocess::run_with_input(&mut command, b"").unwrap();
            assert!(output.status.success());
            let output = String::from_utf8(output.stdout).unwrap();
            let environment: std::collections::BTreeMap<_, _> = output
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            for name in ["MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE"] {
                let expected = if name == "MJ_INSTANCE" {
                    std::env::var(name).unwrap()
                } else {
                    std::path::absolute(std::env::var_os(name).unwrap())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned()
                };
                assert_eq!(environment.get(name).copied(), Some(expected.as_str()));
            }
            if mode == "session" {
                for name in [
                    "MJ_SESSION_ID",
                    "MJ_SESSION_BASH_ENV",
                    "MJ_PROFILE_CUSTOM",
                    "BASH_ENV",
                ] {
                    assert!(!environment.contains_key(name), "leaked {name}");
                }
                assert_ne!(environment.get("CODEX_HOME"), Some(&"/session/profile"));
                assert_ne!(environment.get("HOME"), Some(&"/session/home"));
                assert_ne!(environment.get("TMPDIR"), Some(&"/session/tmp"));
            } else {
                assert_eq!(environment.get("CODEX_HOME"), Some(&"/user/custom-codex"));
            }
            assert_eq!(
                command.get_current_dir(),
                Some(std::path::absolute(data_dir()).unwrap().as_path())
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("data")).unwrap();
        for mode in ["session", "ordinary"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST])
                .env(CHILD, mode)
                .current_dir(directory.path())
                .env("MJ_CONFIG_DIR", "config")
                .env("MJ_DATA_DIR", "data")
                .env("MJ_INSTANCE", "1524-daemon-env")
                .env_remove("MJ_SESSION_ID")
                .env_remove("MJ_SESSION_BASH_ENV")
                .env("CODEX_HOME", "/user/custom-codex");
            if mode == "session" {
                command
                    .env("MJ_SESSION_ID", "old-worker-session")
                    .env("MJ_SESSION_BASH_ENV", "/session/bash-env")
                    .env("MJ_PROFILE_CUSTOM", "profile-value")
                    .env("CODEX_HOME", "/session/profile")
                    .env("HOME", "/session/home")
                    .env("TMPDIR", "/session/tmp")
                    .env("BASH_ENV", "/session/bash-env");
            }
            let output = mj_core::subprocess::run_with_input(&mut command, b"").unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
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
        let mut waiter = mj_core::runtime::spawn(acquire_start_guard(path.clone()));
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
        let mut replacing = mj_core::runtime::spawn(acquire_start_guard(path.clone()));
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
    // Hard-won: ab8be9d0a: stop decided the daemon was absent while a launcher still held the startup lock.
    #[tokio::test]
    async fn stopping_an_absent_daemon_waits_for_a_launch_in_progress() {
        const CHILD: &str = "MJ_TEST_STOP_WAITS_FOR_DAEMON_LAUNCH";
        const TEST: &str =
            "daemon::tests::stopping_an_absent_daemon_waits_for_a_launch_in_progress";
        if crate::test_support::rerun_in_isolated_child(CHILD, TEST) {
            return;
        }
        let startup = acquire_start_guard(data_dir().join("daemon-start.lock"))
            .await
            .unwrap();
        assert!(!metadata_path().exists());
        assert!(
            tokio::time::timeout(Duration::from_millis(150), stop_instance(false))
                .await
                .is_err(),
            "stop must wait until the launcher finishes before deciding whether the daemon is absent"
        );
        drop(startup);
        assert!(!stop_instance(false).await.unwrap());
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
        let attachment = maintain_attachment("keep-alive-test".to_owned(), cancellation.clone());
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
    fn test_daemon_identity(build_version: &str) -> DaemonIdentity {
        DaemonIdentity {
            instance_id: "test-instance".to_owned(),
            build_version: build_version.to_owned(),
            placement: DaemonPlacement::Host,
        }
    }
    #[test]
    fn a_restart_onto_this_build_succeeds_and_says_so() {
        let verdict = restart_verdict(4242, &test_daemon_identity("2.0.0"), Some(true)).unwrap();
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
        let verdict = restart_verdict(7, &test_daemon_identity("2.0.0"), None).unwrap();
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
    fn a_restart_onto_another_build_fails_and_names_both_builds() {
        let other = format!("1.0.0+{}", "b".repeat(40));
        let error = restart_verdict(99, &test_daemon_identity(&other), Some(false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("99"), "{error}");
        assert!(error.contains("1.0.0+bbbbbbbb"), "{error}");
        assert!(error.contains("not this build"), "{error}");
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
            || async { true },
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
            container: None,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_launch_that_is_still_starting_uses_its_live_childs_log() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir(&logs).unwrap();
        let stderr = dir.path().join("daemon.log");
        fs::write(&stderr, "").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let diagnostics = logs.join(format!("mj-daemon-20260930T000000.000Z-{}.log", child.id()));
        fs::write(&diagnostics, "recovering store").unwrap();
        let launched = LaunchedDaemon {
            pid: std::process::id(),
            log_offset: 0,
            container: None,
        };
        let (path, output) = launched.output_since_launch(&stderr).await;
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(path, diagnostics);
        assert_eq!(output, "recovering store");
    }

    #[tokio::test]
    async fn a_failure_before_logging_starts_reports_detached_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let stderr = directory.path().join("daemon.log");
        fs::write(&stderr, "failed before logger initialization\n").unwrap();
        let launched = LaunchedDaemon {
            pid: 7,
            log_offset: 0,
            container: None,
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
            || async { attempts.get() < 3 },
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

    // Hard-won: #1568: a pending probe prevented the startup deadline from firing.
    #[tokio::test(start_paused = true)]
    async fn an_alive_daemon_that_never_answers_stops_at_the_bound() {
        let started = tokio::time::Instant::now();
        let mut attempts = 0;
        let outcome: StartupOutcome<()> = wait_for_ready_daemon(
            || async { true },
            || {
                attempts += 1;
                async move {
                    if attempts == 1 {
                        return Err(anyhow!("connection refused"));
                    }
                    std::future::pending().await
                }
            },
            |_| {},
        )
        .await;
        assert!(matches!(
            outcome,
            StartupOutcome::StillStarting { last_error: Some(error), probe_pending: true }
                if error.to_string() == "connection refused"
        ));
        assert_eq!(attempts, 2);
        assert!((START_TIMEOUT..START_TIMEOUT + RETRY_DELAY).contains(&started.elapsed()));
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

/// Machine endpoint discovery is always local to this launcher. In particular,
/// an SSH placement here must not recursively launch another SSH client.
pub async fn print_endpoint(existing: bool) -> Result<()> {
    mj_core::runtime::spawn_blocking(|| {
        let path = mj_core::config::config_path();
        let placement = mj_core::config::DaemonConfig::load_placement(&path)?;
        anyhow::ensure!(
            placement.runtime != mj_core::config::DaemonRuntime::Ssh,
            "{}: daemon endpoint cannot follow SSH placement; choose host, docker or podman on this machine",
            path.display()
        );
        Ok::<_, anyhow::Error>(())
    }).await.context("load endpoint placement task failed")??;
    let mut client = if existing {
        connect_existing()
            .await
            .context("no running daemon available for daemon endpoint --existing")?
    } else {
        connect_or_start().await?
    };
    let mut endpoint = client.endpoint().await?;
    endpoint.config_path = Some(mj_core::config::config_path());
    println!("{}", serde_json::to_string(&endpoint)?);
    Ok(())
}

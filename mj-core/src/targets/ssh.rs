use super::*;

#[cfg(unix)]
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

/// The connectivity probe `mj doctor` runs against an SSH target.
///
/// It reuses the provisioning argument order so the probe fails exactly where
/// a real session would, with two deliberate overrides prepended. OpenSSH
/// honours the first occurrence of an option, so these win over the
/// provisioning defaults: `BatchMode=yes` never prompts for a password, and
/// `StrictHostKeyChecking=yes` never accepts an unknown host key. Doctor
/// diagnoses; the user decides whether to trust a key.
pub fn ssh_connectivity_probe(ssh: &SshTarget) -> CommandSpec {
    let mut probe = ssh.clone();
    probe.ssh_args.splice(
        0..0,
        [
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
        ],
    );
    ssh_command(&probe, ["true"]).purpose("verify SSH connectivity")
}

pub fn ssh_command(
    ssh: &SshTarget,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> CommandSpec {
    ssh_command_owned(
        ssh,
        args.into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect(),
    )
}

pub fn ssh_command_owned(ssh: &SshTarget, remote_args: Vec<String>) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    push_connection_sharing_args(&mut args);
    args.push(ssh.destination.clone());
    args.push(join_remote_command(&remote_args));
    CommandSpec::new("ssh", args).ssh_destination(ssh.destination.clone())
}

/// Home-relative directory on an SSH host where files bound for a remote
/// container wait before the engine copies them in. Home-relative rather than
/// `~/`, because `ssh_command` quotes every argument while `scp` expands `~`.
pub const REMOTE_UPLOAD_STAGING: &str = ".cache/mjolnir/uploads";

/// Upload a local file or directory to the SSH host.
pub fn scp_upload(ssh: &SshTarget, source: &Path, remote: &str, recursive: bool) -> CommandSpec {
    let mut args = scp_args(ssh);
    if recursive {
        args.push("-r".into());
    }
    args.push(source.to_string_lossy().into_owned());
    args.push(format!("{}:{remote}", ssh.destination));
    scp_command(ssh, args)
}

/// Download a remote file from the SSH host.
pub fn scp_download(ssh: &SshTarget, remote: &str, local: &str) -> CommandSpec {
    let mut args = scp_args(ssh);
    args.push(format!("{}:{remote}", ssh.destination));
    args.push(local.into());
    scp_command(ssh, args)
}

/// The connection's `ssh` arguments rewritten for `scp`, which spells the port
/// option `-P`; to `scp`, `-p` means "preserve file times". The connection
/// sharing options follow, as they do for `ssh`.
fn scp_args(ssh: &SshTarget) -> Vec<String> {
    let mut args = ssh
        .ssh_args
        .iter()
        .map(|argument| {
            if argument == "-p" {
                "-P".to_owned()
            } else {
                argument.clone()
            }
        })
        .collect();
    push_connection_sharing_args(&mut args);
    args
}

fn scp_command(ssh: &SshTarget, args: Vec<String>) -> CommandSpec {
    // `scp` opens its own connection to the same host, so it competes for the
    // same pre-auth budget and is admitted and retried the same way.
    CommandSpec::new("scp", args).ssh_destination(ssh.destination.clone())
}

/// How long a shared master connection stays alive after its last channel
/// closes. The master is an `ssh` process that outlives the daemon by this
/// long, so it is kept short enough to be unsurprising and long enough to
/// cover a whole provision.
#[cfg(unix)]
const CONTROL_PERSIST: &str = "60";

/// Environment override that turns connection sharing off. Any of `0`, `off`,
/// `false`, or `no` disables it.
pub const CONTROL_MASTER_ENV: &str = "MJ_SSH_CONTROL_MASTER";

/// Longest `ControlPath` that still fits in a Unix socket address. `sun_path`
/// holds 108 bytes on Linux and 104 on macOS, minus the terminating NUL.
#[cfg(unix)]
const MAX_CONTROL_PATH: usize = 103;

/// `%C` expands to a hash of host, port, user, and local host: SHA-1 hex
/// today, and 64 characters is reserved for it so a longer one still fits.
#[cfg(unix)]
const CONTROL_PATH_FILE: &str = "%C";

#[cfg(unix)]
fn sharing_disabled(value: Option<&std::ffi::OsStr>) -> bool {
    let Some(value) = value else {
        return false;
    };
    matches!(
        value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// How a test pins connection sharing instead of letting it resolve from the
/// environment.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum SshSharingForTest {
    /// Behave as though the escape hatch were set.
    Disabled,
    /// Keep the control sockets in this directory.
    Directory(PathBuf),
}

static SHARING_OVERRIDE: Mutex<Option<SshSharingForTest>> = Mutex::new(None);

/// Pin connection sharing for a test, or restore the real resolution with
/// `None`. Tests must not depend on the developer's `$XDG_RUNTIME_DIR` or home
/// directory, so every test that inspects `ssh` arguments pins it. Not part of
/// the daemon's behaviour.
#[doc(hidden)]
pub fn set_ssh_connection_sharing_for_test(setting: Option<SshSharingForTest>) {
    *SHARING_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = setting;
}

#[cfg(unix)]
fn sharing_override() -> Option<SshSharingForTest> {
    SHARING_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Where the ControlMaster sockets live, or `None` when sharing is off.
///
/// `$XDG_RUNTIME_DIR` is preferred because it is short, per-user, and on
/// tmpfs; the data directory is the fallback. Neither is world-writable, and
/// the directory is created 0700 because `ssh` will not create it itself.
#[cfg(unix)]
fn control_socket_path() -> Option<PathBuf> {
    match sharing_override() {
        Some(SshSharingForTest::Disabled) => return None,
        Some(SshSharingForTest::Directory(dir)) => return prepare_control_path(dir),
        None => {}
    }
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        if sharing_disabled(std::env::var_os(CONTROL_MASTER_ENV).as_deref()) {
            return None;
        }
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) if !runtime.is_empty() => PathBuf::from(runtime).join("mjolnir"),
            _ => crate::config::data_dir().join("ssh"),
        };
        prepare_control_path(base)
    })
    .clone()
}

/// Create the socket directory 0700 and reject one whose sockets would not fit
/// in a Unix socket address. Failure means no sharing, never a failed command.
#[cfg(unix)]
fn prepare_control_path(dir: PathBuf) -> Option<PathBuf> {
    let socket = dir.join(CONTROL_PATH_FILE);
    // Measure the path ssh actually binds, with 64 characters reserved for
    // the `%C` expansion.
    let bound_len = socket.as_os_str().len() - CONTROL_PATH_FILE.len() + 64;
    if bound_len > MAX_CONTROL_PATH {
        tracing::debug!(
            directory = %dir.display(),
            "skipping SSH connection sharing: control socket path would be too long"
        );
        return None;
    }
    if let Err(error) = fs::create_dir_all(&dir) {
        tracing::debug!(
            directory = %dir.display(),
            %error,
            "skipping SSH connection sharing: control directory is unavailable"
        );
        return None;
    }
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
        tracing::debug!(
            directory = %dir.display(),
            %error,
            "skipping SSH connection sharing: cannot restrict control directory"
        );
        return None;
    }
    Some(socket)
}

/// Append the options that let this command create and keep a shared master
/// connection, to a partly built `ssh` or `scp` argument list.
///
/// Every Mjolnir invocation against one destination then rides one
/// authenticated connection instead of paying for its own handshake. Call this
/// after the caller's own `ssh_args` and before the destination: OpenSSH keeps
/// the first value it sees for an option, so a user who sets `ControlMaster`
/// or `ControlPath` in `extra_args` still wins.
///
/// Adds nothing on non-unix (Windows OpenSSH has no ControlMaster), when
/// `MJ_SSH_CONTROL_MASTER` disables it, or when the socket directory cannot be
/// prepared.
pub fn push_connection_sharing_args(args: &mut Vec<String>) {
    push_control_args(args, true);
}

/// Append the options that let this command *reuse* a shared master without
/// ever becoming one.
///
/// Use this for commands that carry deliberately impatient options, such as
/// the short `ConnectTimeout` and one-miss `ServerAlive` keepalive of a
/// validation probe or a Tab completion. Those settings belong to the one
/// command that asked for them. If such a command opened the master, the
/// master would enforce them for its whole lifetime and drop every later
/// multiplexed session -- an upload, a `podman run`, the worker bootstrap --
/// on a stall of a couple of seconds. With `ControlMaster=no` the command
/// joins an existing master when one is up and otherwise opens its own direct
/// connection, keeping its fail-fast options to itself.
pub fn push_connection_reuse_args(args: &mut Vec<String>) {
    push_control_args(args, false);
}

fn push_control_args(args: &mut Vec<String>, may_become_master: bool) {
    #[cfg(unix)]
    if let Some(socket) = control_socket_path() {
        args.extend([
            "-o".to_owned(),
            if may_become_master {
                "ControlMaster=auto".to_owned()
            } else {
                "ControlMaster=no".to_owned()
            },
            "-o".to_owned(),
            format!("ControlPath={}", socket.display()),
        ]);
        // Only a command that may open the master decides how long it lingers.
        if may_become_master {
            args.extend(["-o".to_owned(), format!("ControlPersist={CONTROL_PERSIST}")]);
        }
    }
    #[cfg(not(unix))]
    let _ = (args, may_become_master);
}

pub fn join_remote_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| posix_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Check whether a directory exists on the configured SSH host.
pub fn ssh_directory_exists(
    ssh: &SshTarget,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let command = ssh_validation_command(
        ssh,
        vec![
            "test".into(),
            "-d".into(),
            path.to_string_lossy().into_owned(),
        ],
        "validate remote directory",
    );
    let output = executor.execute(&command)?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        status => bail!(
            "remote directory check failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Verify that a bare-SSH project path exists and has a committed Git HEAD.
pub fn validate_bare_project_directory(
    ssh: &SshTarget,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_bare_project_path(path)?;
    if !ssh_directory_exists(ssh, path, executor)? {
        bail!(
            "remote project directory {} does not exist or is not a directory",
            path.display()
        );
    }
    let output = executor.execute(&ssh_validation_command(
        ssh,
        vec![
            "git".into(),
            "-C".into(),
            path.to_string_lossy().into_owned(),
            "rev-parse".into(),
            "--verify".into(),
            "HEAD".into(),
        ],
        "validate bare SSH Git project",
    ))?;
    if output.status != 0 {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!(
                "remote project directory {} has no valid Git HEAD",
                path.display()
            );
        }
        bail!(
            "remote project directory {} has no valid Git HEAD: {detail}",
            path.display()
        );
    }
    Ok(())
}

pub fn validate_bare_project_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        bail!("bare project directory must be an absolute safe path");
    }
    Ok(())
}

pub fn ssh_validation_command(
    ssh: &SshTarget,
    remote_args: Vec<String>,
    purpose: &'static str,
) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ServerAliveInterval=2".into(),
        "-o".into(),
        "ServerAliveCountMax=1".into(),
    ]);
    push_connection_reuse_args(&mut args);
    args.extend([ssh.destination.clone(), join_remote_command(&remote_args)]);
    CommandSpec::new("ssh", args)
        .ssh_destination(ssh.destination.clone())
        .purpose(purpose)
}

/// Wrap a value so a POSIX shell reads it as one literal argument. Used at the
/// SSH boundary here and when Hel rebuilds an agent's terminal command line
/// (`terminal::shell_line`).
pub fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn verify_locator(locator: &TargetLocator, session_id: &str) -> Result<()> {
    let expected_name = resource_name(session_id)?;
    match locator {
        TargetLocator::LocalBare { worker_root } => {
            let path = Path::new(worker_root);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
                || !path.ends_with(session_id)
            {
                bail!("refusing cleanup: invalid local bare worker root");
            }
        }
        TargetLocator::LocalPodman {
            container_id,
            borrowed_from,
            ..
        }
        | TargetLocator::LocalDocker {
            container_id,
            borrowed_from,
        }
        | TargetLocator::AppleContainer {
            container_id,
            borrowed_from,
        }
        | TargetLocator::SshPodman {
            container_id,
            borrowed_from,
            ..
        }
        | TargetLocator::SshDocker {
            container_id,
            borrowed_from,
            ..
        } => match borrowed_from {
            Some(owner) => {
                validate_session_id(owner)?;
                if owner == session_id {
                    bail!(
                        "refusing cleanup: a borrowed container cannot be owned by the borrowing session"
                    );
                }
                let owner_name = resource_name(owner)?;
                if container_id != &owner_name && !is_runtime_container_id(container_id) {
                    bail!(
                        "refusing cleanup: borrowed container locator is neither the owning session's generated name nor an immutable runtime ID"
                    );
                }
            }
            None => {
                if container_id != &expected_name && !is_runtime_container_id(container_id) {
                    bail!(
                        "refusing cleanup: container locator is neither the generated name nor an immutable runtime ID"
                    );
                }
            }
        },
        TargetLocator::AwsEc2 {
            instance_id,
            workspace,
            ..
        } => {
            if !valid_ec2_instance_id(instance_id) {
                bail!("refusing cleanup: invalid EC2 instance ID");
            }
            verify_session_workspace(workspace, session_id)?;
        }
        TargetLocator::SshBare {
            workspace,
            worker_id,
            ..
        } => match worker_id {
            Some(worker_id) => {
                validate_session_id(worker_id)?;
                if worker_id != session_id {
                    bail!("refusing cleanup: SSH worker identity does not match session ID");
                }
                validate_workspace_prefix(workspace)?;
            }
            None => verify_session_workspace(workspace, session_id)?,
        },
    }
    Ok(())
}

/// Whether this locator names a target another session owns: a sub-agent
/// child either borrowing its parent's container or running as its own worker
/// inside the parent's SSH workspace.
pub fn is_borrowed(locator: &TargetLocator) -> bool {
    match locator {
        TargetLocator::LocalPodman { borrowed_from, .. }
        | TargetLocator::LocalDocker { borrowed_from, .. }
        | TargetLocator::AppleContainer { borrowed_from, .. }
        | TargetLocator::SshPodman { borrowed_from, .. }
        | TargetLocator::SshDocker { borrowed_from, .. } => borrowed_from.is_some(),
        TargetLocator::SshBare { worker_id, .. } => worker_id.is_some(),
        TargetLocator::LocalBare { .. } | TargetLocator::AwsEc2 { .. } => false,
    }
}

pub fn verify_session_workspace(workspace: &str, session_id: &str) -> Result<()> {
    validate_workspace_prefix(workspace)?;
    let final_component = workspace.trim_end_matches('/').rsplit('/').next();
    if final_component != Some(session_id) {
        bail!("refusing cleanup: workspace does not end in the exact session ID");
    }
    Ok(())
}

pub fn validate_session_id(value: &str) -> Result<()> {
    if value.len() < 8
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("session ID must be 8-128 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

pub fn validate_relative_path(value: &str) -> Result<()> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        bail!("unsafe relative bundle path {value:?}");
    }
    Ok(())
}

pub fn validate_workspace_prefix(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "/"
        || value == "~"
        || value == "~/"
        || value.contains('\0')
        || value.split('/').any(|part| part == "..")
    {
        bail!("unsafe workspace path");
    }
    Ok(())
}

pub fn validate_container_template(template: &ContainerTemplate) -> Result<()> {
    if template.image.trim().is_empty() || template.image.starts_with('-') {
        bail!("invalid container image");
    }
    if template
        .extra_run_args
        .iter()
        .any(|arg| arg == "--name" || arg.starts_with("--name="))
    {
        bail!("container template may not override the generated name");
    }
    if template.extra_run_args.iter().any(|arg| {
        arg == "--label"
            || [SESSION_LABEL, MANAGED_LABEL, INSTANCE_LABEL]
                .iter()
                .any(|label| arg.starts_with(&format!("--label={label}=")))
    }) {
        bail!("container template may not override Mjolnir ownership labels");
    }
    Ok(())
}

pub fn validate_ssh(ssh: &SshTarget) -> Result<()> {
    if ssh.destination.trim().is_empty()
        || ssh.destination.starts_with('-')
        || ssh.destination.chars().any(char::is_whitespace)
    {
        bail!("invalid SSH destination");
    }
    Ok(())
}

pub fn validate_aws(aws: &AwsTemplate) -> Result<()> {
    validate_ssh(&aws.ssh)?;
    for (name, value) in [
        ("AWS profile", &aws.profile),
        ("AWS region", &aws.region),
        ("launch template", &aws.launch_template),
    ] {
        if value.is_empty()
            || value.starts_with('-')
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        {
            bail!("invalid {name}");
        }
    }
    Ok(())
}

pub fn validate_executable(value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_whitespace) {
        bail!("invalid executable name");
    }
    Ok(())
}

pub fn valid_ec2_instance_id(value: &str) -> bool {
    value
        .strip_prefix("i-")
        .is_some_and(|rest| rest.len() >= 8 && rest.chars().all(|c| c.is_ascii_hexdigit()))
}

pub fn is_runtime_container_id(value: &str) -> bool {
    value.len() >= 12 && value.len() <= 128 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// `ssh` reserves exit status 255 for its own transport failures; a remote
/// command never produces it, so the remote side provably never ran.
pub const SSH_TRANSPORT_EXIT_STATUS: i32 = 255;

/// Stderr fragments OpenSSH prints when the server hangs up before
/// authentication. `sshd`'s `MaxStartups` produces exactly these when it drops
/// an unauthenticated connection, and so does a server that is still starting.
const TRANSPORT_REJECTION_MARKERS: [&str; 4] = [
    "Connection closed by",
    "Connection reset by",
    "kex_exchange_identification",
    "Connection timed out during banner exchange",
];

/// Whether a finished `ssh` process was turned away by the transport rather
/// than by the remote command.
///
/// The remote command never started in this case, so the caller may retry the
/// whole invocation without worrying about repeating a side effect.
pub fn is_transport_rejection(status: i32, stderr: &str) -> bool {
    status == SSH_TRANSPORT_EXIT_STATUS
        && TRANSPORT_REJECTION_MARKERS
            .iter()
            .any(|marker| stderr.contains(marker))
}

/// Default number of `ssh` processes this daemon will have in flight against
/// one destination at a time.
///
/// `sshd` counts *unauthenticated* connections against `MaxStartups`, whose
/// stock value is `10:30:100`: from the eleventh concurrent pre-auth connection
/// it starts dropping them, and past a hundred it drops all of them. A daemon
/// that spawns one fresh `ssh` per operation reaches that during startup, so it
/// admits its own connections instead of letting the server refuse them.
const DEFAULT_MAX_CONCURRENT_SSH: usize = 6;

/// Environment override for [`DEFAULT_MAX_CONCURRENT_SSH`].
pub const MAX_CONCURRENT_SSH_ENV: &str = "MJ_SSH_MAX_CONCURRENT";

fn max_concurrent_ssh() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let Some(raw) = std::env::var_os(MAX_CONCURRENT_SSH_ENV) else {
            return DEFAULT_MAX_CONCURRENT_SSH;
        };
        match raw
            .to_str()
            .and_then(|value| value.trim().parse::<usize>().ok())
        {
            Some(limit) if limit > 0 => limit,
            _ => {
                tracing::warn!(
                    variable = MAX_CONCURRENT_SSH_ENV,
                    value = %raw.to_string_lossy(),
                    default = DEFAULT_MAX_CONCURRENT_SSH,
                    "ignoring invalid SSH concurrency limit"
                );
                DEFAULT_MAX_CONCURRENT_SSH
            }
        }
    })
}

/// A counting semaphore per SSH destination.
///
/// Deliberately built on `std::sync` rather than a runtime primitive: the
/// blocking process executors are called from plain threads as well as from
/// `spawn_blocking`, and both must share one gate.
struct DestinationGate {
    limit: usize,
    in_flight: Mutex<usize>,
    released: Condvar,
}

impl DestinationGate {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            in_flight: Mutex::new(0),
            released: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> SshPermit {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *in_flight >= self.limit {
            in_flight = self
                .released
                .wait(in_flight)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *in_flight += 1;
        drop(in_flight);
        SshPermit {
            gate: Arc::clone(self),
        }
    }
}

/// One admitted `ssh` connection. The slot is returned on drop.
pub struct SshPermit {
    gate: Arc<DestinationGate>,
}

impl std::fmt::Debug for SshPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SshPermit")
    }
}

impl Drop for SshPermit {
    fn drop(&mut self) {
        let mut in_flight = self
            .gate
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight = in_flight.saturating_sub(1);
        drop(in_flight);
        self.gate.released.notify_one();
    }
}

/// Process-wide admission control for outbound `ssh` connections.
pub struct SshAdmission;

impl SshAdmission {
    /// Block until this process may open another `ssh` connection to
    /// `destination`. The returned permit holds the slot until it is dropped.
    pub fn acquire(destination: &str) -> SshPermit {
        Self::gate(destination).acquire()
    }

    fn gate(destination: &str) -> Arc<DestinationGate> {
        static GATES: OnceLock<Mutex<BTreeMap<String, Arc<DestinationGate>>>> = OnceLock::new();
        let mut gates = GATES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            gates
                .entry(destination.to_owned())
                .or_insert_with(|| DestinationGate::new(max_concurrent_ssh())),
        )
    }
}

/// How many times a transport-rejected `ssh` invocation is tried in total.
pub const SSH_RETRY_ATTEMPTS: usize = 3;

/// Inclusive millisecond bounds the jittered delay is drawn from, indexed by
/// the number of attempts already made. `sshd` sheds load for as long as its
/// pre-auth queue stays full, so the second wait is a multiple of the first.
const SSH_RETRY_BACKOFF_MS: [(u64, u64); SSH_RETRY_ATTEMPTS - 1] = [(500, 2_000), (2_000, 4_000)];

/// Test override collapsing every retry delay to this many milliseconds.
/// `u64::MAX` means "no override".
static SSH_RETRY_BACKOFF_OVERRIDE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Shorten the retry backoff so tests can drive the retry path without
/// sleeping for seconds. Not part of the daemon's behaviour.
#[doc(hidden)]
pub fn set_ssh_retry_backoff_for_test(delay: Option<Duration>) {
    SSH_RETRY_BACKOFF_OVERRIDE_MS.store(
        delay.map_or(u64::MAX, |delay| delay.as_millis() as u64),
        Ordering::Relaxed,
    );
}

/// The jittered wait before retry number `attempts_made + 1`.
///
/// Jitter matters more than the mean here: every session's reconnect fails at
/// the same instant, so an unjittered schedule would simply re-send the whole
/// burst into the same full queue.
pub fn ssh_retry_delay(attempts_made: usize) -> Duration {
    let override_ms = SSH_RETRY_BACKOFF_OVERRIDE_MS.load(Ordering::Relaxed);
    if override_ms != u64::MAX {
        return Duration::from_millis(override_ms);
    }
    let (low, high) = SSH_RETRY_BACKOFF_MS
        .get(attempts_made.saturating_sub(1))
        .copied()
        .unwrap_or(*SSH_RETRY_BACKOFF_MS.last().expect("non-empty schedule"));
    let mut bytes = [0_u8; 8];
    // A failed draw only costs jitter, so fall back to the lower bound.
    let spread = if getrandom::fill(&mut bytes).is_ok() {
        u64::from_le_bytes(bytes) % (high - low + 1)
    } else {
        0
    };
    Duration::from_millis(low + spread)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BORROW_PARENT: &str = "0123456789abcdef0123456789abcdef";
    const BORROW_CHILD: &str = "fedcba9876543210fedcba9876543210";

    fn borrowed_podman(owner: &str) -> TargetLocator {
        TargetLocator::LocalPodman {
            container_id: crate::targets::resource_name(owner).unwrap(),
            workspace_storage: PodmanWorkspaceLocator::default(),
            borrowed_from: Some(owner.to_owned()),
        }
    }

    #[test]
    fn verify_locator_accepts_a_container_borrowed_from_its_owner() {
        verify_locator(&borrowed_podman(BORROW_PARENT), BORROW_CHILD)
            .expect("a child may borrow its parent's container");
    }

    #[test]
    fn verify_locator_rejects_a_container_borrowed_from_the_checking_session() {
        let error = verify_locator(&borrowed_podman(BORROW_PARENT), BORROW_PARENT)
            .expect_err("a session cannot borrow from itself");
        assert!(
            format!("{error:#}").contains("cannot be owned by the borrowing session"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn verify_locator_rejects_a_borrowed_container_naming_another_session() {
        let locator = TargetLocator::LocalPodman {
            container_id: crate::targets::resource_name(BORROW_CHILD).unwrap(),
            workspace_storage: PodmanWorkspaceLocator::default(),
            borrowed_from: Some(BORROW_PARENT.to_owned()),
        };
        let error = verify_locator(&locator, BORROW_CHILD)
            .expect_err("the container must belong to the recorded owner");
        assert!(
            format!("{error:#}").contains("borrowed container locator"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn worker_root_of_a_borrowed_container_is_the_childs_own_directory() {
        assert_eq!(
            crate::targets::worker_root(&borrowed_podman(BORROW_PARENT), BORROW_CHILD).unwrap(),
            format!("/var/lib/hel/workers/{BORROW_CHILD}")
        );
    }

    #[test]
    fn is_borrowed_distinguishes_borrowed_targets_from_owned_ones() {
        assert!(is_borrowed(&borrowed_podman(BORROW_PARENT)));
        assert!(is_borrowed(&TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "host".to_owned(),
                ssh_args: Vec::new(),
            },
            workspace: format!(".local/share/hel/workspaces/{BORROW_PARENT}"),
            worker_id: Some(BORROW_CHILD.to_owned()),
        }));
        assert!(!is_borrowed(&TargetLocator::LocalPodman {
            container_id: crate::targets::resource_name(BORROW_CHILD).unwrap(),
            workspace_storage: PodmanWorkspaceLocator::default(),
            borrowed_from: None,
        }));
    }

    #[test]
    fn an_owned_container_locator_serializes_without_a_borrowed_from_key() {
        let owned = TargetLocator::LocalDocker {
            container_id: crate::targets::resource_name(BORROW_CHILD).unwrap(),
            borrowed_from: None,
        };
        let serialized = serde_json::to_string(&owned).unwrap();
        assert!(
            !serialized.contains("borrowed_from"),
            "owned locators must stay byte-identical for older readers: {serialized}"
        );
        assert_eq!(
            serde_json::from_str::<TargetLocator>(&serialized).unwrap(),
            owned
        );

        let borrowed = borrowed_podman(BORROW_PARENT);
        let serialized = serde_json::to_string(&borrowed).unwrap();
        assert!(serialized.contains("borrowed_from"));
        assert_eq!(
            serde_json::from_str::<TargetLocator>(&serialized).unwrap(),
            borrowed
        );
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The connection-sharing override is process-wide, so the tests that set
    /// it take turns.
    #[cfg(unix)]
    static SHARING_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Records the commands it is handed and reports an empty success.
    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingExecutor {
        seen: std::cell::RefCell<Vec<CommandSpec>>,
    }

    #[cfg(unix)]
    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.seen.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[cfg(unix)]
    fn sharing_args(ssh: &SshTarget) -> Vec<String> {
        ssh_command(ssh, ["true"]).args
    }

    #[cfg(unix)]
    fn sharing_socket_dir() -> tempfile::TempDir {
        // macOS's default temporary path leaves too little room for SSH's hash.
        tempfile::tempdir_in("/tmp").expect("short control socket directory")
    }

    #[test]
    #[cfg(unix)]
    fn connection_sharing_follows_user_supplied_ssh_args() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let ssh = SshTarget {
            destination: "host".to_owned(),
            ssh_args: vec!["-o".to_owned(), "ControlMaster=no".to_owned()],
        };
        let args = sharing_args(&ssh);
        set_ssh_connection_sharing_for_test(None);

        let expected_path = format!("ControlPath={}/%C", socket_dir.path().display());
        assert_eq!(
            args,
            vec![
                "-o".to_owned(),
                "ControlMaster=no".to_owned(),
                "-o".to_owned(),
                "ControlMaster=auto".to_owned(),
                "-o".to_owned(),
                expected_path,
                "-o".to_owned(),
                format!("ControlPersist={CONTROL_PERSIST}"),
                "host".to_owned(),
                "'true'".to_owned(),
            ],
            "sharing options must come after the user's own args, which OpenSSH prefers"
        );
        assert_eq!(
            std::os::unix::fs::MetadataExt::mode(
                &fs::metadata(socket_dir.path()).expect("socket directory")
            ) & 0o777,
            0o700
        );
    }

    /// A command with a two-second keepalive must join a master, never open
    /// one: as the master it would impose that keepalive on every later
    /// session sharing the connection.
    #[test]
    #[cfg(unix)]
    fn fail_fast_commands_reuse_a_master_without_becoming_one() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let ssh = SshTarget {
            destination: "host".to_owned(),
            ssh_args: Vec::new(),
        };
        let validation = ssh_validation_command(&ssh, vec!["true".to_owned()], "test").args;
        let executor = RecordingExecutor::default();
        crate::path_completion::ssh_completions(
            &ssh,
            "/srv/pr",
            crate::path_completion::CompletionKind::Directories,
            &executor,
        )
        .expect("completion runs");
        let completion = executor.seen.borrow()[0].args.clone();
        set_ssh_connection_sharing_for_test(None);

        let control_path = format!("ControlPath={}/%C", socket_dir.path().display());
        for args in [&validation, &completion] {
            assert!(args.contains(&"ControlMaster=no".to_owned()), "{args:?}");
            assert!(args.contains(&control_path), "{args:?}");
            assert!(
                !args.iter().any(|arg| arg.starts_with("ControlPersist")),
                "a fail-fast command must not set how long a master lingers: {args:?}"
            );
            let master = args
                .iter()
                .position(|arg| arg == "ControlMaster=no")
                .expect("sharing options");
            let alive = args
                .iter()
                .position(|arg| arg == "ServerAliveCountMax=1")
                .expect("its own keepalive");
            assert!(alive < master, "{args:?}");
            assert!(
                master
                    < args
                        .iter()
                        .position(|arg| arg == "host")
                        .expect("destination"),
                "{args:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn connection_sharing_is_absent_when_turned_off() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let ssh = SshTarget {
            destination: "host".to_owned(),
            ssh_args: Vec::new(),
        };
        let args = sharing_args(&ssh);
        set_ssh_connection_sharing_for_test(None);
        assert_eq!(args, vec!["host".to_owned(), "'true'".to_owned()]);
    }

    #[test]
    #[cfg(unix)]
    fn a_control_path_that_cannot_fit_a_socket_address_is_skipped() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().expect("temp dir");
        let long = root.path().join("a".repeat(MAX_CONTROL_PATH));
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(long.clone())));
        let ssh = SshTarget {
            destination: "host".to_owned(),
            ssh_args: Vec::new(),
        };
        let args = sharing_args(&ssh);
        set_ssh_connection_sharing_for_test(None);
        assert_eq!(args, vec!["host".to_owned(), "'true'".to_owned()]);
        assert!(!long.exists(), "an unusable directory must not be created");
    }

    #[test]
    #[cfg(not(unix))]
    fn connection_sharing_is_unix_only() {
        let mut args = vec!["-o".to_owned(), "BatchMode=yes".to_owned()];
        push_connection_sharing_args(&mut args);
        assert_eq!(args, vec!["-o".to_owned(), "BatchMode=yes".to_owned()]);
    }

    #[test]
    #[cfg(unix)]
    fn the_escape_hatch_accepts_the_usual_off_spellings() {
        for value in ["0", "off", "FALSE", " no "] {
            assert!(
                sharing_disabled(Some(std::ffi::OsStr::new(value))),
                "{value:?} must disable connection sharing"
            );
        }
        for value in ["1", "auto", "", "yes"] {
            assert!(
                !sharing_disabled(Some(std::ffi::OsStr::new(value))),
                "{value:?} must leave connection sharing on"
            );
        }
        assert!(!sharing_disabled(None));
    }

    /// Against a real host: the first invocation must leave a master behind
    /// that `ssh -O check` finds. Set `MJ_E2E_SSH_HOST` to a reachable
    /// destination to run it.
    #[test]
    #[cfg(unix)]
    fn sharing_leaves_a_reusable_master_on_a_real_host() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(host) = std::env::var_os("MJ_E2E_SSH_HOST") else {
            return;
        };
        let host = host.to_string_lossy().into_owned();
        let socket_dir = sharing_socket_dir();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let ssh = SshTarget {
            destination: host.clone(),
            ssh_args: vec!["-o".to_owned(), "BatchMode=yes".to_owned()],
        };
        let spec = ssh_command(&ssh, ["true"]);
        set_ssh_connection_sharing_for_test(None);

        let first = std::process::Command::new(&spec.program)
            .args(&spec.args)
            .status()
            .expect("ssh must run");
        assert!(first.success(), "ssh {host} true failed");

        let control_path = format!("{}/%C", socket_dir.path().display());
        let check = std::process::Command::new("ssh")
            .args([
                "-O",
                "check",
                "-o",
                &format!("ControlPath={control_path}"),
                &host,
            ])
            .output()
            .expect("ssh -O check must run");
        let exit = std::process::Command::new("ssh")
            .args([
                "-O",
                "exit",
                "-o",
                &format!("ControlPath={control_path}"),
                &host,
            ])
            .output();
        assert!(
            check.status.success(),
            "no master survived the first connection: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        drop(exit);
    }

    /// `scp` spells the port `-P`; passing an `ssh` `-p` through would ask it
    /// to preserve file times and read the port as a file name. Every `scp`
    /// also opens a connection, so it is admitted like `ssh`.
    #[test]
    #[cfg(unix)]
    fn scp_translates_the_ssh_port_option_and_is_tagged_with_its_destination() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let ssh = SshTarget {
            destination: "build@10.0.0.1".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        };

        let upload = scp_upload(&ssh, Path::new("/tmp/local"), "remote/path", true);
        let download = scp_download(&ssh, "remote/archive.zip", "/tmp/local.zip");
        set_ssh_connection_sharing_for_test(None);

        assert_eq!(
            upload.args,
            [
                "-P",
                "2222",
                "-r",
                "/tmp/local",
                "build@10.0.0.1:remote/path"
            ]
        );
        assert_eq!(
            download.args,
            [
                "-P",
                "2222",
                "build@10.0.0.1:remote/archive.zip",
                "/tmp/local.zip"
            ]
        );
        for command in [upload, download] {
            assert_eq!(command.program, "scp");
            assert_eq!(command.ssh_destination.as_deref(), Some("build@10.0.0.1"));
        }
    }

    #[test]
    fn transport_rejection_matches_only_sshd_hangups() {
        let cases: [(i32, &str, bool); 7] = [
            (255, "Connection closed by 192.168.1.77 port 22", true),
            (
                255,
                "kex_exchange_identification: read: Connection reset by peer",
                true,
            ),
            (255, "ssh: Connection reset by 10.0.0.1 port 22", true),
            (255, "Connection timed out during banner exchange", true),
            (255, "Permission denied (publickey).", false),
            (
                255,
                "ssh: connect to host h port 22: Connection refused",
                false,
            ),
            (1, "Connection closed by 192.168.1.77 port 22", false),
        ];
        for (status, stderr, expected) in cases {
            assert_eq!(
                is_transport_rejection(status, stderr),
                expected,
                "status {status} stderr {stderr:?}"
            );
        }
    }

    #[test]
    fn admission_never_admits_more_than_the_limit() {
        let gate = DestinationGate::new(2);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..12)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let permit = gate.acquire();
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::yield_now();
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        drop(permit);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("admission worker must not panic");
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "admission let {} connections run against a 2-permit gate",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn admission_blocks_once_every_permit_is_held() {
        let gate = DestinationGate::new(2);
        let first = gate.acquire();
        let second = gate.acquire();
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let permit = gate.acquire();
                drop(permit);
            })
        };
        // The third acquire has nothing to take until a permit comes back.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished());
        drop(first);
        waiter
            .join()
            .expect("waiter must be admitted once a permit frees");
        drop(second);
    }
}

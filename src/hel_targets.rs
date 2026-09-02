//! Declarative execution plans for Hel session targets.
//!
//! Plans deliberately contain argv vectors instead of local shell strings.  A
//! shell is used only at the SSH boundary, where OpenSSH necessarily sends a
//! command string; every remotely supplied argument is POSIX-quoted there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hel_config::ImagePullPolicy;

pub const SESSION_LABEL: &str = "dev.mj.session";
pub const MANAGED_LABEL: &str = "dev.mj.managed";
pub const SESSION_TAG: &str = "dev.mj.session";
pub const MANAGED_TAG: &str = "dev.mj.managed";
pub const CONTAINER_WORKSPACE: &str = "/workspace";
pub const PODMAN_DOCUMENTATION_PATH: &str = "docs/PODMAN.md";
pub const DOCKER_DOCUMENTATION_PATH: &str = "docs/DOCKER.md";

// `mj doctor` prints a self-contained setup page that quotes these two pages in
// full. They are embedded here, beside the paths that name them, because this
// crate's `include` list is what carries `docs/` into the published package;
// the controller crate that renders the page cannot reach outside its own
// directory.
/// The rootless Podman postconditions page, verbatim.
pub const PODMAN_DOCUMENTATION: &str = include_str!("../docs/PODMAN.md");
/// The Docker postconditions page, verbatim.
pub const DOCKER_DOCUMENTATION: &str = include_str!("../docs/DOCKER.md");

const PODMAN_MINIMUM_MAJOR_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedResourceKind {
    Container,
    Ec2Instance,
}

/// Build command-line fragments that identify resources Hel owns for a session.
fn managed_resource_identity_args(kind: ManagedResourceKind, session_id: &str) -> Vec<String> {
    match kind {
        ManagedResourceKind::Container => vec![
            "--label".to_owned(),
            format!("{SESSION_LABEL}={session_id}"),
            "--label".to_owned(),
            format!("{MANAGED_LABEL}=true"),
        ],
        ManagedResourceKind::Ec2Instance => vec![
            "--tag-specifications".to_owned(),
            format!(
                "ResourceType=instance,Tags=[{{Key={SESSION_TAG},Value={session_id}}},{{Key={MANAGED_TAG},Value=true}}]"
            ),
        ],
    }
}

/// The launch phase a command belongs to, reported as launch progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProvisionStage {
    Provisioning,
    Booting,
    Cloning,
    Syncing,
    Restoring,
    Starting,
    Compacting,
}

impl ProvisionStage {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Provisioning => "Provision",
            Self::Booting => "Boot",
            Self::Cloning => "Clone",
            Self::Syncing => "Sync",
            Self::Restoring => "Restore",
            Self::Starting => "Start",
            Self::Compacting => "Compact",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SensitiveCommandInput(Vec<u8>);

impl std::fmt::Debug for SensitiveCommandInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub purpose: String,
    #[serde(default)]
    pub stage: Option<ProvisionStage>,
    /// Commands that share this marker and appear consecutively in a plan's
    /// command list may run concurrently under
    /// [`CommandPlan::execute_concurrent`]. Commands without a marker, or
    /// whose neighbors do not share it, keep running strictly in plan order.
    #[serde(default)]
    pub parallel_group: Option<u32>,
    /// Whether this command brings the session's target into existence. Every
    /// command after it in a provisioning plan runs against a target that
    /// already exists, so a later failure owes that target's teardown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub creates_target: bool,
    /// Input that must reach the child without becoming part of its arguments,
    /// environment, serialized plan, or debug representation.
    #[serde(skip)]
    sensitive_stdin: Option<SensitiveCommandInput>,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            purpose: String::new(),
            stage: None,
            parallel_group: None,
            creates_target: false,
            sensitive_stdin: None,
        }
    }

    pub fn purpose(mut self, purpose: impl Into<String>) -> Self {
        self.purpose = purpose.into();
        self
    }

    pub fn stage(mut self, stage: ProvisionStage) -> Self {
        self.stage = Some(stage);
        self
    }

    /// Mark this command as eligible to run concurrently with its
    /// plan-adjacent siblings that share the same group.
    pub fn parallel_group(mut self, group: u32) -> Self {
        self.parallel_group = Some(group);
        self
    }

    /// Mark this command as the one that creates the session's target.
    pub fn creates_target(mut self) -> Self {
        self.creates_target = true;
        self
    }

    /// Feed private file content through the shared concurrent pipe handler.
    /// The bytes stay out of argv, environments, serialization, and Debug.
    pub fn with_sensitive_stdin(mut self, input: Vec<u8>) -> Self {
        self.sensitive_stdin = Some(SensitiveCommandInput(input));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionResourceUsage {
    pub cpu_percent: Option<u8>,
    pub memory_current_bytes: u64,
    pub memory_limit_bytes: Option<u64>,
    pub swap_current_bytes: Option<u64>,
    pub swap_limit_bytes: Option<u64>,
    pub writable_disk_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionResourceProbe {
    pub memory: CommandSpec,
    pub disk: Option<CommandSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentCapacityKind {
    Host,
    AwsFleet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentCapacityTarget {
    pub id: String,
    pub host: String,
    pub target_ids: Vec<String>,
    pub kind: DeploymentCapacityKind,
    pub local: bool,
    /// Alternative commands for a host, or one command per live AWS instance.
    pub probes: Vec<CommandSpec>,
    /// Prevents a partial AWS fleet sample when one live instance cannot be probed yet.
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentCapacityUsage {
    pub cpu_percent: Option<u8>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub logical_cores: u64,
    pub disk_total_bytes: Option<u64>,
}

/// An additional directory made available to one session.
///
/// Containers use isolated mounts. Remote targets may instead receive a
/// controller-packed snapshot at the destination while retaining this shared
/// persisted shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdditionalMount {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// Attach the source read-only instead of behind the container runtime's
    /// copy-on-write overlay. Defaults to false so archives and records written
    /// before the option existed keep the overlay they were provisioned with.
    #[serde(default)]
    pub read_only: bool,
}

/// Why a filesystem cannot host a container target's copy-on-write overlay,
/// or `None` when it can. Unknown types are allowed: the overlay is the better
/// mount and only a filesystem known to break it is downgraded.
///
/// The names are those `stat -f -c %T` reports, matched case-insensitively.
pub fn overlay_unsupported_filesystem(filesystem: &str) -> Option<&'static str> {
    let name = filesystem.trim().to_ascii_lowercase();
    // FUSE reports the backing driver as `fuse.sshfs`, `fuse.s3fs`, and so on.
    if name == "fuse" || name == "fuseblk" || name.starts_with("fuse.") {
        return Some("FUSE filesystem");
    }
    match name.as_str() {
        "nfs" | "nfs4" | "cifs" | "smb2" | "smb3" | "9p" | "v9fs" | "virtiofs" | "ceph"
        | "lustre" | "afs" | "glusterfs" | "ocfs2" | "gfs" | "gfs2" => Some("network filesystem"),
        "msdos" | "vfat" | "fat" | "exfat" | "ntfs" | "ntfs3" => Some("no POSIX metadata"),
        "overlayfs" => Some("overlay stacking limit"),
        _ => None,
    }
}

/// Filesystem type of each directory, probed on the host that runs the
/// container engine. `ssh` names that host for a remote Podman target; `None`
/// probes this machine.
///
/// The reply is positional, so the whole batch fails unless `stat` answered for
/// every directory in order.
pub fn probe_filesystem_types(
    ssh: Option<&SshTarget>,
    paths: &[PathBuf],
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "stat".to_owned(),
        "-f".to_owned(),
        "-c".to_owned(),
        "%T".to_owned(),
        "--".to_owned(),
    ];
    args.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
    let host = match ssh {
        Some(ssh) => PodmanHost::Ssh(ssh),
        None => PodmanHost::Local,
    };
    let output = executor.execute(&host.command_owned(args, "probe mount source filesystem"))?;
    if output.status != 0 {
        bail!(
            "filesystem probe failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let types = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect::<Vec<_>>();
    if types.len() != paths.len() {
        bail!(
            "filesystem probe named {} filesystems for {} directories",
            types.len(),
            paths.len()
        );
    }
    Ok(types)
}

pub fn validate_additional_mounts(mounts: &[AdditionalMount]) -> Result<()> {
    let mut destinations = BTreeSet::new();
    for mount in mounts {
        if !mount.source.is_absolute() || mount.source.as_os_str().is_empty() {
            bail!("additional mount source must be an absolute directory path");
        }
        if !mount.destination.is_absolute()
            || mount.destination.as_os_str().is_empty()
            || mount
                .destination
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            bail!("additional mount destination must be a safe absolute container path");
        }
        if !destinations.insert(mount.destination.clone()) {
            bail!(
                "additional mount destination {:?} is configured more than once",
                mount.destination
            );
        }
    }
    Ok(())
}

/// Choose the editable default destination for an additional host directory.
pub fn default_mount_destination(source: &Path, existing: &[AdditionalMount]) -> PathBuf {
    let basename = source
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("mount"));
    let base = PathBuf::from("/mnt").join(basename);
    if !existing.iter().any(|mount| mount.destination == base) {
        return base;
    }
    for number in 2.. {
        let candidate =
            PathBuf::from("/mnt").join(format!("{}-{number}", basename.to_string_lossy()));
        if !existing.iter().any(|mount| mount.destination == candidate) {
            return candidate;
        }
    }
    unreachable!("a finite mount list always has an unused numbered destination")
}

/// Complete an on-disk directory path without spawning a shell.
pub fn local_directory_completions(prefix: &str) -> Vec<String> {
    let (directory, fragment) = match prefix.rsplit_once('/') {
        Some((directory, fragment)) => (format!("{directory}/"), fragment),
        None => (String::new(), prefix),
    };
    let lookup = if directory.is_empty() {
        "."
    } else {
        &directory
    };
    let entries = match fs::read_dir(lookup) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = lookup, %error, "path completion directory could not be read");
            return Vec::new();
        }
    };
    let mut matches = entries
        .filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(path = lookup, %error, "path completion directory entry could not be read");
                    return None;
                }
            };
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(name) => name,
                None => {
                    tracing::debug!(path = %entry.path().display(), "path completion skipped a non-UTF-8 directory entry");
                    return None;
                }
            };
            (name.starts_with(fragment) && entry.path().is_dir())
                .then(|| format!("{directory}{name}/"))
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    matches
}

/// Return the single match or the extra shared path prefix that Tab can add.
pub fn path_completion(prefix: &str, candidates: &[String]) -> Option<String> {
    let first = candidates.first()?;
    if candidates.len() == 1 {
        return Some(first.clone());
    }
    let common = candidates
        .iter()
        .skip(1)
        .fold(first.clone(), |common, next| {
            common
                .chars()
                .zip(next.chars())
                .take_while(|(left, right)| left == right)
                .map(|(character, _)| character)
                .collect()
        });
    (common.len() > prefix.len() && common.starts_with(prefix)).then_some(common)
}

pub trait CommandExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput>;

    /// Whether the operation supervising this executor has requested
    /// cancellation. Test executors and ordinary process execution are not
    /// cancellable unless they opt in.
    fn cancellation_requested(&self) -> bool {
        false
    }

    /// Report entry into a lifecycle stage. Callers that cover more than one
    /// command should hold a [`ProvisionStageGuard`] for the whole operation
    /// so concurrent stages remain visible between subprocesses.
    fn stage_started(&self, _stage: ProvisionStage) {}

    /// Report exit from a lifecycle stage previously passed to
    /// [`Self::stage_started`].
    fn stage_finished(&self, _stage: ProvisionStage) {}

    /// Report a decision an operation made on the user's behalf. This is not a
    /// failure: the work continues, and the user is told what changed.
    fn notify_notice(&self, _notice: &str) {}

    fn execute_with_stdin(
        &self,
        _command: &CommandSpec,
        _input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        bail!("this command executor does not support streamed stdin")
    }
}

/// A scoped lifecycle-stage report for controller-side work or a sequence of
/// commands. Dropping the guard reports completion even when the work returns
/// early with an error.
pub struct ProvisionStageGuard<'a, E: CommandExecutor + ?Sized> {
    executor: &'a E,
    stage: ProvisionStage,
}

impl<'a, E: CommandExecutor + ?Sized> ProvisionStageGuard<'a, E> {
    pub fn new(executor: &'a E, stage: ProvisionStage) -> Self {
        executor.stage_started(stage);
        Self { executor, stage }
    }
}

impl<E: CommandExecutor + ?Sized> Drop for ProvisionStageGuard<'_, E> {
    fn drop(&mut self) {
        self.executor.stage_finished(self.stage);
    }
}

pub struct ProcessExecutor;

/// One debug line per finished target command, so a slow launch or resume
/// phase can be attributed from logs instead of re-profiled by hand.
fn trace_command_duration(command: &CommandSpec, started: Instant, status: i32) {
    tracing::debug!(
        purpose = command.purpose.as_str(),
        program = command.program.as_str(),
        status,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "target command finished"
    );
}

impl CommandExecutor for ProcessExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if let Some(input) = &command.sensitive_stdin {
            let mut input = std::io::Cursor::new(input.0.as_slice());
            return self.execute_with_stdin(command, &mut input);
        }
        let started = Instant::now();
        let output = Command::new(&command.program)
            .args(&command.args)
            .envs(&command.env)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
        let status = output.status.code().unwrap_or(-1);
        trace_command_duration(command, started, status);
        Ok(CommandOutput {
            status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        let mut process = Command::new(&command.program);
        process.args(&command.args).envs(&command.env);
        // Plain process execution is not cancellable, so the transfer only
        // ends when the child does.
        stream_command_with_stdin(process, command, input, &|| false)
    }
}

/// Streams `input` into a freshly spawned child and collects its output.
///
/// Both executors share this one implementation because the pipe edge cases
/// below are easy to get subtly wrong in a second copy.
///
/// `is_cancelled` reports whether the supervising operation wants the transfer
/// abandoned; [`ProcessExecutor`] passes a check that is never true, which also
/// makes the kill path below unreachable for it.
fn stream_command_with_stdin(
    mut process: Command,
    command: &CommandSpec,
    input: &mut (dyn Read + Send),
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<CommandOutput> {
    let started = Instant::now();
    if is_cancelled() {
        bail!("operation cancelled");
    }
    let mut child = process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
    let stdin = child
        .stdin
        .take()
        .context("streamed command stdin missing")?;
    let mut stdout = child
        .stdout
        .take()
        .context("streamed command stdout missing")?;
    let mut stderr = child
        .stderr
        .take()
        .context("streamed command stderr missing")?;
    // Reader threads keep the child's output pipes drained; a child that fills
    // one while nobody reads would block instead of exiting.
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::copy(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::copy(&mut stderr, &mut bytes).map(|_| bytes)
    });
    let process_result = std::thread::scope(|scope| -> Result<_> {
        // Pipe writes can block forever when a remote helper stops reading.
        // Keep the writer off the supervising thread so cancellation can kill
        // the process group and thereby close the blocked pipe.
        let input_writer = scope.spawn(move || -> Result<()> {
            // Owning `stdin` here is what closes the pipe's write end once the
            // transfer finishes. A child that reads to EOF, such as
            // `mj worker export-checkpoint --spec -`, never exits while any
            // copy of the write end is still open.
            let mut stdin = stdin;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                // Checking before each chunk makes large checkpoint copies
                // cooperatively cancellable without changing the executor
                // interface.
                if is_cancelled() {
                    bail!("operation cancelled");
                }
                let count = input.read(&mut buffer).context("read command input")?;
                if count == 0 {
                    break;
                }
                stdin
                    .write_all(&buffer[..count])
                    .context("stream command input")?;
            }
            stdin.flush().context("flush command input")
        });
        let status = loop {
            if is_cancelled() {
                terminate_cancellable_child(&mut child);
                if let Err(error) = input_writer.join() {
                    tracing::warn!(
                        purpose = command.purpose.as_str(),
                        "streamed command input writer panicked while cancelling: {error:?}"
                    );
                }
                bail!("operation cancelled while {}", command.purpose);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    terminate_cancellable_child(&mut child);
                    if let Err(join_error) = input_writer.join() {
                        tracing::warn!(
                            purpose = command.purpose.as_str(),
                            "streamed command input writer panicked while waiting: {join_error:?}"
                        );
                    }
                    return Err(error).with_context(|| format!("wait for {}", command.purpose));
                }
            }
        };
        let input_result = input_writer
            .join()
            .map_err(|_| anyhow::anyhow!("streamed command input writer panicked"))?;
        Ok((status, input_result))
    });
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("streamed command stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("streamed command stderr reader panicked"))??;
    let (status, input_result) = process_result?;
    if status.success() {
        // A child that exited first explains the failure through its own
        // status and stderr; the broken pipe that exit caused would only hide
        // it. A successful child must not hide an input error.
        input_result?;
    }
    let status = status.code().unwrap_or(-1);
    trace_command_duration(command, started, status);
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

#[derive(Clone)]
pub struct CancellableProcessExecutor {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl CancellableProcessExecutor {
    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            deadline: None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Some(Instant::now() + timeout),
        }
    }

    /// Bounds an existing flag-based executor with a deadline, so a wedged
    /// child becomes a reported failure instead of running forever.
    pub fn with_deadline(mut self, timeout: Duration) -> Self {
        self.deadline = Some(Instant::now() + timeout);
        self
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("operation cancelled");
        }
        Ok(())
    }
}

fn cancellable_command(command: &CommandSpec) -> Command {
    let mut process = Command::new(&command.program);
    process.args(&command.args).envs(&command.env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }
    process
}

fn terminate_cancellable_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    // The child owns a fresh process group, so descendants such as an SSH or
    // shell helper cannot keep its output pipes open after cancellation. A
    // group that is already gone is the wanted outcome, not a failure, so the
    // shared helper decides what deserves a warning.
    if let Err(error) =
        crate::hel_subprocess::signal_process_group(child.id() as i32, libc::SIGKILL)
    {
        tracing::warn!(pid = child.id(), %error, "could not terminate cancelled command process group");
    }
    #[cfg(not(unix))]
    if let Err(error) = child.kill() {
        tracing::warn!(pid = child.id(), %error, "could not terminate cancelled command");
    }
    if let Err(error) = child.wait() {
        tracing::warn!(pid = child.id(), %error, "could not reap cancelled command");
    }
}

impl CommandExecutor for CancellableProcessExecutor {
    fn cancellation_requested(&self) -> bool {
        self.is_cancelled()
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if let Some(input) = &command.sensitive_stdin {
            let mut input = std::io::Cursor::new(input.0.as_slice());
            return self.execute_with_stdin(command, &mut input);
        }
        let started = Instant::now();
        self.check_cancelled()?;
        let mut child = cancellable_command(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
        let mut stdout = child.stdout.take().context("command stdout missing")?;
        let mut stderr = child.stderr.take().context("command stderr missing")?;
        let stdout_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            std::io::copy(&mut stdout, &mut bytes).map(|_| bytes)
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            std::io::copy(&mut stderr, &mut bytes).map(|_| bytes)
        });
        let status = loop {
            if self.is_cancelled() {
                terminate_cancellable_child(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                bail!("operation cancelled while {}", command.purpose);
            }
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("wait for {}", command.purpose))?
            {
                break status;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let stdout = stdout_reader
            .join()
            .map_err(|_| anyhow::anyhow!("command stdout reader panicked"))??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| anyhow::anyhow!("command stderr reader panicked"))??;
        let status = status.code().unwrap_or(-1);
        trace_command_duration(command, started, status);
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
        })
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        // The child runs in its own process group so cancellation can kill the
        // whole group, which is what releases a writer blocked on a full pipe.
        stream_command_with_stdin(cancellable_command(command), command, input, &|| {
            self.is_cancelled()
        })
    }
}

/// Runs every command with its own deadline.
///
/// [`CancellableProcessExecutor::with_timeout`] bounds a whole operation from a
/// single shared deadline, which suits one provisioning run. Prerequisite
/// probes are different: each one is expected to answer quickly, and a wedged
/// socket or blackholed network must not stall the probes that follow it. A
/// timeout here names the probe that hung, so the caller can report it the same
/// way it reports any other probe failure.
#[derive(Debug, Clone, Copy)]
pub struct BoundedProcessExecutor {
    timeout: Duration,
}

impl BoundedProcessExecutor {
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl CommandExecutor for BoundedProcessExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let executor = CancellableProcessExecutor::with_timeout(self.timeout);
        executor.execute(command).map_err(|error| {
            if executor.is_cancelled() {
                anyhow::anyhow!(
                    "`{}` did not answer within {} seconds while trying to {}",
                    command.program,
                    self.timeout.as_secs(),
                    command.purpose
                )
            } else {
                error
            }
        })
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        CancellableProcessExecutor::with_timeout(self.timeout).execute_with_stdin(command, input)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanPreflight {
    pub version: String,
    /// Non-fatal host configuration problems that can make sessions fragile.
    pub warnings: Vec<PodmanPreflightWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanPreflightWarning {
    pub detail: String,
    pub remediation: String,
}

impl PodmanPreflightWarning {
    pub fn notice(&self) -> String {
        format!("{} {}", self.detail, self.remediation)
    }
}

/// Where the Podman prerequisite probes run.
///
/// The same postconditions apply locally and over SSH; only the command
/// wrapping and the wording of a failure differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PodmanHost<'a> {
    Local,
    Ssh(&'a SshTarget),
}

impl PodmanHost<'_> {
    /// Sentence opener for every failure raised by these probes.
    fn failure(self) -> String {
        match self {
            Self::Local => "Podman preflight failed".to_owned(),
            Self::Ssh(ssh) => format!("Remote Podman preflight failed on {}", ssh.destination),
        }
    }

    /// Prefix that says where a remediation must be applied.
    fn remediation_scope(self) -> String {
        match self {
            Self::Local => String::new(),
            Self::Ssh(ssh) => format!("On {}: ", ssh.destination),
        }
    }

    fn command(self, args: &[&str], purpose: &'static str) -> CommandSpec {
        self.command_owned(args.iter().map(|arg| (*arg).to_owned()).collect(), purpose)
    }

    fn command_owned(self, args: Vec<String>, purpose: &'static str) -> CommandSpec {
        match self {
            Self::Local => {
                CommandSpec::new(args[0].clone(), args[1..].iter().cloned()).purpose(purpose)
            }
            Self::Ssh(ssh) => ssh_validation_command(ssh, args, purpose),
        }
        .stage(ProvisionStage::Provisioning)
    }
}

/// Verify the fast local preconditions for Hel's rootless Podman target.
///
/// This intentionally never pulls an image. Image availability is verified by
/// `mj setup`'s smoke test and by the subsequent target creation command.
pub fn verify_local_podman(executor: &impl CommandExecutor) -> Result<PodmanPreflight> {
    verify_podman(PodmanHost::Local, executor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerPreflight {
    pub version: String,
}

/// Verify that the Docker CLI can reach a Linux Docker daemon.
///
/// Image and OverlayFS support are exercised by the setup/doctor smoke test;
/// this fast probe runs before every launch and never pulls an image.
pub fn verify_local_docker(executor: &impl CommandExecutor) -> Result<DockerPreflight> {
    let command = CommandSpec::new(
        "docker",
        ["version", "--format", "{{.Server.Version}} {{.Server.Os}}"],
    )
    .purpose("check Docker daemon")
    .stage(ProvisionStage::Provisioning);
    let output = executor
        .execute(&command)
        .context("Docker preflight failed: run `docker info` as the user running Mjolnir")?;
    ensure!(
        output.status == 0,
        "Docker preflight failed: `docker version` exited with status {}: {}. Run `docker info` as the user running Mjolnir. See {DOCKER_DOCUMENTATION_PATH}.",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let reported = String::from_utf8_lossy(&output.stdout);
    let mut fields = reported.split_whitespace();
    let version = fields.next().unwrap_or_default();
    let os = fields.next().unwrap_or_default();
    ensure!(
        !version.is_empty() && os == "linux",
        "Docker preflight failed: expected a Linux Docker daemon, got {:?}. See {DOCKER_DOCUMENTATION_PATH}.",
        reported.trim()
    );
    Ok(DockerPreflight {
        version: version.to_owned(),
    })
}

/// Verify the same rootless Podman preconditions on an SSH host.
///
/// The probes run through the noninteractive SSH options, so an unreachable
/// host fails fast instead of blocking doctor or session preflight.
pub fn verify_ssh_podman(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Result<PodmanPreflight> {
    let host = PodmanHost::Ssh(ssh);
    validate_ssh(ssh).map_err(|error| {
        anyhow::anyhow!(
            "{}: the configured SSH destination is unusable ({error}). Set a valid `host` (and optional `user`) for this ssh-podman target. See {PODMAN_DOCUMENTATION_PATH}.",
            host.failure()
        )
    })?;
    let mut preflight = verify_podman(host, executor)?;
    if let Some(warning) = ssh_podman_linger_warning(ssh, executor) {
        preflight.warnings.push(warning);
    }
    Ok(preflight)
}

fn verify_podman(host: PodmanHost<'_>, executor: &impl CommandExecutor) -> Result<PodmanPreflight> {
    let version = execute_podman_preflight(
        executor,
        host,
        &["podman", "--version"],
        "check Podman version",
        "Postcondition `podman --version` succeeds with Podman 4.0.0 or newer",
        "Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`.",
    )?;
    let version = parse_podman_version(host, &version.stdout)?;

    let rootless = execute_podman_preflight(
        executor,
        host,
        &["podman", "info", "--format", "{{.Host.Security.Rootless}}"],
        "check rootless Podman mode",
        "Postcondition `podman info --format '{{.Host.Security.Rootless}}'` prints `true`",
        "Run Mjolnir as the ordinary user without `sudo`; if a remote Podman connection is configured, unset `CONTAINER_HOST` or select the rootless local connection.",
    )?;
    let rootless_output = String::from_utf8_lossy(&rootless.stdout);
    if rootless_output.trim() != "true" {
        bail!(
            "{}: Postcondition `podman info --format '{{{{.Host.Security.Rootless}}}}'` prints `true` returned {:?}. {}Run Mjolnir as the ordinary user without `sudo`; if a remote Podman connection is configured, unset `CONTAINER_HOST` or select the rootless local connection. See {PODMAN_DOCUMENTATION_PATH}.",
            host.failure(),
            rootless_output.trim(),
            host.remediation_scope(),
        );
    }

    let uid_map = execute_podman_preflight(
        executor,
        host,
        &["podman", "unshare", "cat", "/proc/self/uid_map"],
        "check rootless Podman UID map",
        "Postcondition `podman unshare cat /proc/self/uid_map` maps container UIDs 0 and 1",
        "Install UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora), then add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"` and start a fresh login session.",
    )?;
    if !valid_rootless_uid_map(&uid_map.stdout) {
        bail!(
            "{}: Postcondition `podman unshare cat /proc/self/uid_map` maps container UIDs 0 and 1 was not met. {}Add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"`, verify `/etc/subuid` and `/etc/subgid`, then log out and back in. See {PODMAN_DOCUMENTATION_PATH}.",
            host.failure(),
            host.remediation_scope(),
        );
    }

    Ok(PodmanPreflight {
        version,
        warnings: Vec::new(),
    })
}

/// Report either an explicitly unsafe systemd setting or an unavailable
/// durability check. Neither condition makes an otherwise usable target fail.
fn ssh_podman_linger_warning(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Option<PodmanPreflightWarning> {
    let command = PodmanHost::Ssh(ssh).command(
        &[
            "sh",
            "-c",
            "loginctl show-user \"$(id -u)\" --property=Linger --value",
        ],
        "check remote user lingering",
    );
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            return Some(linger_unavailable_warning(
                ssh,
                format!("the probe could not run: {error}"),
            ));
        }
    };
    let linger = String::from_utf8_lossy(&output.stdout);
    match (output.status, linger.trim().to_ascii_lowercase().as_str()) {
        (0, "yes") => None,
        (0, "no") => Some(PodmanPreflightWarning {
            detail: format!(
                "Remote user lingering is disabled on {}; SSH-Podman sessions may be terminated when the last SSH connection closes.",
                ssh.destination
            ),
            remediation: format!(
                "On {}, run `sudo loginctl enable-linger \"$(id -un)\"`.",
                ssh.destination
            ),
        }),
        (status, _) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let reason = if status == 127 || stderr.contains("loginctl: not found") {
                "`loginctl` was not found; this host may not use systemd".to_owned()
            } else if status != 0 {
                format!("`loginctl` exited with status {status}: {stderr}")
            } else {
                format!("`loginctl` returned an unrecognized Linger value {linger:?}")
            };
            Some(linger_unavailable_warning(ssh, reason))
        }
    }
}

fn linger_unavailable_warning(ssh: &SshTarget, reason: String) -> PodmanPreflightWarning {
    PodmanPreflightWarning {
        detail: format!(
            "Remote user-manager durability check is unavailable on {} because {reason}. Mjolnir cannot verify whether rootless Podman sessions survive logout.",
            ssh.destination
        ),
        remediation: format!(
            "Configure {}'s service manager to keep the user and rootless Podman services running after logout; if it uses systemd, make `loginctl` available and enable lingering.",
            ssh.destination
        ),
    }
}

fn execute_podman_preflight(
    executor: &impl CommandExecutor,
    host: PodmanHost<'_>,
    args: &[&str],
    purpose: &'static str,
    postcondition: &str,
    remediation: &str,
) -> Result<CommandOutput> {
    let command = host.command(args, purpose);
    let failure = host.failure();
    let scope = host.remediation_scope();
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => match ssh_transport_failure(host, &error.to_string()) {
            Some(message) => bail!("{message}"),
            None => bail!(
                "{failure}: {postcondition}. {scope}{remediation} See {PODMAN_DOCUMENTATION_PATH}. Underlying error: {error}"
            ),
        },
    };
    if output.status == SSH_TRANSPORT_EXIT_STATUS
        && let Some(message) =
            ssh_transport_failure(host, String::from_utf8_lossy(&output.stderr).trim())
    {
        bail!("{message}");
    }
    if output.status != 0 {
        bail!(
            "{failure}: {postcondition}. {scope}{remediation} See {PODMAN_DOCUMENTATION_PATH}. Podman reported: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// `ssh` reserves exit status 255 for its own connection failures; the Podman
/// probes never produce it. Reporting that case separately keeps an
/// unreachable host from being mistaken for a broken Podman installation.
const SSH_TRANSPORT_EXIT_STATUS: i32 = 255;

fn ssh_transport_failure(host: PodmanHost<'_>, reported: &str) -> Option<String> {
    let PodmanHost::Ssh(ssh) = host else {
        return None;
    };
    let destination = &ssh.destination;
    Some(format!(
        "{}: SSH could not run the probes on {destination}. Verify that `ssh {destination}` succeeds noninteractively from this host. See {PODMAN_DOCUMENTATION_PATH}. ssh reported: {reported}",
        host.failure()
    ))
}

fn parse_podman_version(host: PodmanHost<'_>, stdout: &[u8]) -> Result<String> {
    let failure = host.failure();
    let scope = host.remediation_scope();
    let version = String::from_utf8_lossy(stdout).trim().to_owned();
    let Some(candidate) = version
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))
    else {
        bail!(
            "{failure}: Postcondition `podman --version` succeeds with Podman 4.0.0 or newer returned {version:?}. {scope}Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`. See {PODMAN_DOCUMENTATION_PATH}."
        );
    };
    let Some(major) = candidate
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
    else {
        bail!(
            "{failure}: Postcondition `podman --version` succeeds with Podman 4.0.0 or newer returned {version:?}. {scope}Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`. See {PODMAN_DOCUMENTATION_PATH}."
        );
    };
    if major < PODMAN_MINIMUM_MAJOR_VERSION {
        bail!(
            "{failure}: Postcondition `podman --version` succeeds with Podman 4.0.0 or newer was not met (found {candidate}). {scope}Upgrade Podman to 4.0.0 or newer: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`. See {PODMAN_DOCUMENTATION_PATH}."
        );
    }
    Ok(candidate.to_owned())
}

fn valid_rootless_uid_map(stdout: &[u8]) -> bool {
    let mappings = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse::<u64>().ok()?,
                fields.next()?.parse::<u64>().ok()?,
                fields.next()?.parse::<u64>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    [0, 1].into_iter().all(|container_id| {
        mappings.iter().any(|(inside, _outside, length)| {
            inside
                .checked_add(*length)
                .is_some_and(|end| *inside <= container_id && container_id < end)
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPlan {
    pub description: String,
    pub commands: Vec<CommandSpec>,
}

impl CommandPlan {
    /// Supply one container environment value without placing it in the
    /// Podman/SSH argument vector. The target launcher reads the value from
    /// stdin, exports it, and asks the container engine to inherit it by name.
    pub fn provide_target_environment_secret(
        &mut self,
        target: &TargetTemplate,
        name: &str,
        value: &str,
    ) -> Result<()> {
        ensure!(
            !name.is_empty()
                && name.bytes().enumerate().all(|(index, byte)| byte == b'_'
                    || byte.is_ascii_alphabetic()
                    || (index > 0 && byte.is_ascii_digit())),
            "invalid secret environment variable name"
        );
        ensure!(
            !value.as_bytes().contains(&b'\n') && !value.as_bytes().contains(&b'\r'),
            "secret environment value cannot contain a newline"
        );
        let command = self
            .commands
            .iter_mut()
            .find(|command| command.creates_target)
            .context("provisioning plan has no target creation command")?;
        let read_and_export = format!("IFS= read -r {name} || exit 1; export {name};");
        match target {
            TargetTemplate::LocalPodman(_)
            | TargetTemplate::LocalDocker(_)
            | TargetTemplate::AppleContainer(_) => {
                let program = std::mem::replace(&mut command.program, "sh".to_owned());
                let args = std::mem::take(&mut command.args);
                command.args = vec![
                    "-c".to_owned(),
                    format!("{read_and_export} exec \"$@\""),
                    "mj-secret-env".to_owned(),
                    program,
                ];
                command.args.extend(args);
            }
            TargetTemplate::SshPodman { .. } => {
                let remote = command
                    .args
                    .last_mut()
                    .context("remote Podman command has no SSH command argument")?;
                *remote = format!("{read_and_export} exec {remote}");
            }
            TargetTemplate::LocalBare
            | TargetTemplate::AwsEc2(_)
            | TargetTemplate::SshBare { .. } => {
                bail!("target does not support inherited container environment")
            }
        }
        let mut input = value.as_bytes().to_vec();
        input.push(b'\n');
        command.sensitive_stdin = Some(SensitiveCommandInput(input));
        Ok(())
    }

    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<Vec<CommandOutput>> {
        let mut outputs = Vec::with_capacity(self.commands.len());
        for command in &self.commands {
            let output = executor.execute(command)?;
            if output.status != 0 {
                bail!(
                    "{} failed with status {}: {}",
                    command.purpose,
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            outputs.push(output);
        }
        Ok(outputs)
    }

    /// Execute the plan the same way [`Self::execute`] does, except that
    /// commands sharing a [`CommandSpec::parallel_group`] marker and
    /// appearing consecutively in `commands` run concurrently as one batch.
    ///
    /// A batch starts only once every earlier command has succeeded, and a
    /// batch that fails reports the first failure in plan order regardless
    /// of which command finished first — the same fail-fast contract
    /// [`Self::execute`] provides between individual commands. This method
    /// requires a `Sync` executor because a batch shares it across threads;
    /// [`Self::execute`] keeps working with non-`Sync` executors such as
    /// test fakes built on `RefCell`.
    pub fn execute_concurrent(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Vec<CommandOutput>> {
        let mut outputs = Vec::with_capacity(self.commands.len());
        let mut index = 0;
        while index < self.commands.len() {
            let group = self.commands[index].parallel_group;
            let mut end = index + 1;
            if group.is_some() {
                while end < self.commands.len() && self.commands[end].parallel_group == group {
                    end += 1;
                }
            }
            let batch = &self.commands[index..end];
            if let [command] = batch {
                outputs.push(checked_command_output(command, executor.execute(command)?)?);
            } else {
                let results: Vec<Result<CommandOutput>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = batch
                        .iter()
                        .map(|command| scope.spawn(|| executor.execute(command)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|handle| match handle.join() {
                            Ok(result) => result,
                            Err(panic) => Err(anyhow::anyhow!(
                                "concurrent command thread panicked: {}",
                                command_thread_panic_message(panic.as_ref())
                            )),
                        })
                        .collect()
                });
                for (command, result) in batch.iter().zip(results) {
                    outputs.push(checked_command_output(command, result?)?);
                }
            }
            index = end;
        }
        Ok(outputs)
    }

    /// Split the plan around the command that creates the session's target:
    /// the commands through that one, then the commands that run against a
    /// target which already exists.
    ///
    /// A plan that creates nothing — an existing project directory, say —
    /// splits into nothing, so a caller never arms a teardown for a target it
    /// did not bring into existence.
    pub fn split_at_target_creation(&self) -> Option<(Self, Self)> {
        let created = self
            .commands
            .iter()
            .position(|command| command.creates_target)?;
        let (creation, remainder) = self.commands.split_at(created + 1);
        Some((
            Self {
                description: self.description.clone(),
                commands: creation.to_vec(),
            },
            Self {
                description: self.description.clone(),
                commands: remainder.to_vec(),
            },
        ))
    }
}

/// Fail the same way [`CommandPlan::execute`] does for a non-zero exit
/// status; kept as a shared helper so [`CommandPlan::execute_concurrent`]
/// reports identical error text.
fn checked_command_output(command: &CommandSpec, output: CommandOutput) -> Result<CommandOutput> {
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

/// Describe a spawned command thread's panic payload for error context.
pub fn command_thread_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySpec {
    /// Clone URL for network-backed repositories. `None` creates an empty
    /// repository which a verified local snapshot restores later.
    pub url: Option<String>,
    pub destination: String,
    pub git_ref: Option<String>,
    /// Read-only bare repository mounted into the target for Git object reuse.
    /// A missing or unusable reference is only an optimization miss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBundleSpec {
    pub primary: String,
    pub repositories: Vec<RepositorySpec>,
}

impl ProjectBundleSpec {
    pub fn validate(&self) -> Result<()> {
        validate_relative_path(&self.primary)?;
        if self.repositories.is_empty() {
            bail!("a project bundle must contain at least one repository");
        }
        let mut destinations = std::collections::BTreeSet::new();
        for repository in &self.repositories {
            validate_relative_path(&repository.destination)?;
            if repository
                .url
                .as_deref()
                .is_some_and(|url| url.trim().is_empty() || url.starts_with('-'))
            {
                bail!("invalid repository URL");
            }
            if !destinations.insert(&repository.destination) {
                bail!(
                    "duplicate repository destination {}",
                    repository.destination
                );
            }
        }
        if !destinations.contains(&self.primary) {
            bail!("primary repository is not present in the bundle");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerTemplate {
    pub image: String,
    #[serde(default)]
    pub pull_policy: ImagePullPolicy,
    #[serde(default)]
    pub extra_run_args: Vec<String>,
}

impl ImagePullPolicy {
    fn resolve(self, image: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        if image_is_digest_pinned(image) {
            Self::Missing
        } else if image_is_remote(image) && image_uses_latest_tag(image) {
            Self::Newer
        } else {
            Self::Missing
        }
    }

    fn podman_value(self, image: &str) -> &'static str {
        match self.resolve(image) {
            Self::Always => "always",
            Self::Newer => "newer",
            Self::Missing => "missing",
            Self::Never => "never",
            Self::Auto => unreachable!("auto pull policy must resolve"),
        }
    }
}

fn image_is_digest_pinned(image: &str) -> bool {
    image
        .rsplit_once('@')
        .is_some_and(|(_, digest)| !digest.is_empty())
}

fn image_is_remote(image: &str) -> bool {
    !image.starts_with("localhost/") && !image.starts_with("local/")
}

fn image_uses_latest_tag(image: &str) -> bool {
    let name = image.split_once('@').map_or(image, |(name, _)| name);
    let final_component = name.rsplit('/').next().unwrap_or(name);
    !final_component.contains(':') || final_component.ends_with(":latest")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub destination: String,
    #[serde(default)]
    pub ssh_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsTemplate {
    pub profile: String,
    pub region: String,
    pub launch_template: String,
    pub launch_template_version: Option<String>,
    pub instance_type: Option<String>,
    pub ssh: SshTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetTemplate {
    LocalBare,
    LocalPodman(ContainerTemplate),
    LocalDocker(ContainerTemplate),
    AppleContainer(ContainerTemplate),
    AwsEc2(AwsTemplate),
    SshBare {
        ssh: SshTarget,
        #[serde(default = "default_ssh_prefix")]
        workspace_prefix: String,
    },
    SshPodman {
        ssh: SshTarget,
        container: ContainerTemplate,
    },
}

fn default_ssh_prefix() -> String {
    ".local/share/hel/workspaces".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetLocator {
    LocalBare {
        worker_root: String,
    },
    LocalPodman {
        container_id: String,
    },
    LocalDocker {
        container_id: String,
    },
    AppleContainer {
        container_id: String,
    },
    AwsEc2 {
        profile: String,
        region: String,
        instance_id: String,
        ssh: SshTarget,
        workspace: String,
    },
    SshBare {
        ssh: SshTarget,
        workspace: String,
    },
    SshPodman {
        ssh: SshTarget,
        container_id: String,
    },
}

/// Commands and identity needed to bring a stopped managed target back online.
/// Only runtimes whose stopped resources retain their durable files provide
/// one; callers leave every other target kind alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRecoveryPlan {
    pub exists: CommandSpec,
    pub inspect: CommandSpec,
    pub start: CommandSpec,
    pub session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRecoveryOutcome {
    NotRequired,
    Missing,
    AlreadyRunning,
    Started,
}

pub fn resource_name(session_id: &str) -> Result<String> {
    validate_session_id(session_id)?;
    let readable: String = session_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(12)
        .map(|character| character.to_ascii_lowercase())
        .collect();
    let digest = Sha256::digest(session_id.as_bytes());
    Ok(format!(
        "mj-{readable}-{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2]
    ))
}

pub fn workspace_for(template: &TargetTemplate, session_id: &str) -> Result<String> {
    validate_session_id(session_id)?;
    match template {
        TargetTemplate::LocalBare => bail!("local bare projects use their selected directory"),
        TargetTemplate::LocalPodman(_)
        | TargetTemplate::LocalDocker(_)
        | TargetTemplate::AppleContainer(_)
        | TargetTemplate::SshPodman { .. } => Ok(CONTAINER_WORKSPACE.to_owned()),
        TargetTemplate::AwsEc2(_) => Ok(format!(".local/share/hel/workspaces/{session_id}")),
        TargetTemplate::SshBare {
            workspace_prefix, ..
        } => {
            validate_workspace_prefix(workspace_prefix)?;
            // Interpret a leading "~/" as home-relative. Remote commands are
            // single-quoted, so a literal tilde would name a directory called
            // "~"; a relative path resolves against the login home for ssh
            // and scp alike.
            let prefix = workspace_prefix
                .strip_prefix("~/")
                .unwrap_or(workspace_prefix);
            Ok(format!("{}/{session_id}", prefix.trim_end_matches('/')))
        }
    }
}

/// Create the initial resource. AWS address discovery and all SSH bootstrap
/// happen after parsing the `run-instances` response and constructing a locator.
pub fn provision_plan(
    template: &TargetTemplate,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
) -> Result<CommandPlan> {
    bundle.validate()?;
    if !additional_mounts.is_empty()
        && !matches!(
            template,
            TargetTemplate::LocalPodman(_)
                | TargetTemplate::LocalDocker(_)
                | TargetTemplate::AppleContainer(_)
                | TargetTemplate::SshPodman { .. }
        )
    {
        bail!("additional mounts require a container-backed target");
    }
    let name = resource_name(session_id)?;
    let mut commands = Vec::new();
    match template {
        TargetTemplate::LocalBare => {
            bail!("local bare projects must use the existing-project provisioning path")
        }
        TargetTemplate::LocalPodman(container) => {
            validate_container_template(container)?;
            commands.push(container_run(
                "podman",
                container,
                &name,
                session_id,
                additional_mounts,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "podman",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                container_exec("podman", &name, args)
            }));
        }
        TargetTemplate::LocalDocker(container) => {
            validate_container_template(container)?;
            commands.push(docker_container_run(
                container,
                &name,
                session_id,
                additional_mounts,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "docker",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                container_exec("docker", &name, args)
            }));
        }
        TargetTemplate::AppleContainer(container) => {
            validate_container_template(container)?;
            commands.push(
                CommandSpec::new("container", ["system", "status"])
                    .purpose("check Apple container service")
                    .stage(ProvisionStage::Provisioning),
            );
            commands.extend(apple_image_prepare_commands(container));
            commands.push(container_run(
                "container",
                container,
                &name,
                session_id,
                additional_mounts,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "container",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                container_exec("container", &name, args)
            }));
        }
        TargetTemplate::AwsEc2(aws) => {
            validate_aws(aws)?;
            let launch_key = if aws.launch_template.starts_with("lt-") {
                "LaunchTemplateId"
            } else {
                "LaunchTemplateName"
            };
            let mut launch = format!("{launch_key}={}", aws.launch_template);
            if let Some(version) = &aws.launch_template_version {
                launch.push_str(",Version=");
                launch.push_str(version);
            }
            let mut args = vec![
                "--profile".to_owned(),
                aws.profile.clone(),
                "--region".to_owned(),
                aws.region.clone(),
                "ec2".to_owned(),
                "run-instances".to_owned(),
                "--launch-template".to_owned(),
                launch,
            ];
            if let Some(instance_type) = &aws.instance_type {
                args.extend(["--instance-type".to_owned(), instance_type.clone()]);
            }
            args.extend(managed_resource_identity_args(
                ManagedResourceKind::Ec2Instance,
                session_id,
            ));
            args.extend(["--output".to_owned(), "json".to_owned()]);
            commands.push(
                CommandSpec::new("aws", args)
                    .purpose("launch EC2 session instance")
                    .stage(ProvisionStage::Provisioning)
                    .creates_target(),
            );
        }
        TargetTemplate::SshBare {
            ssh,
            workspace_prefix: _,
        } => {
            validate_ssh(ssh)?;
            let workspace = workspace_for(template, session_id)?;
            commands.push(
                ssh_command(ssh, ["mkdir", "-p", &workspace])
                    .purpose("create SSH session workspace")
                    .stage(ProvisionStage::Provisioning)
                    .creates_target(),
            );
            commands.extend(install_git_plan(ExecutionBoundary::Ssh(ssh)).commands);
            commands.extend(clone_commands(bundle, &workspace, |args| {
                ssh_command_owned(ssh, args)
            }));
        }
        TargetTemplate::SshPodman { ssh, container } => {
            validate_ssh(ssh)?;
            validate_container_template(container)?;
            let mut run = vec!["podman".to_owned()];
            run.extend(container_run_args(
                "podman",
                container,
                &name,
                session_id,
                additional_mounts,
            )?);
            commands.push(
                ssh_command_owned(ssh, run)
                    .purpose("start remote Podman container")
                    .stage(ProvisionStage::Provisioning)
                    .creates_target(),
            );
            commands.extend(
                install_git_plan(ExecutionBoundary::SshPodman {
                    ssh,
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                let mut remote = vec!["podman".to_owned(), "exec".to_owned(), name.clone()];
                remote.extend(args);
                ssh_command_owned(ssh, remote)
            }));
        }
    }
    Ok(CommandPlan {
        description: format!("provision Mjolnir session {session_id}"),
        commands,
    })
}

/// Build the no-op infrastructure plan for an existing bare project.
/// The wizard validates the project for early feedback; worker/ACP startup is
/// authoritative if it changes before launch. Worker state is installed later
/// under the dedicated worker and profile roots, not under a cloned workspace.
pub fn provision_bare_project_plan(
    template: &TargetTemplate,
    session_id: &str,
    project_directory: &str,
) -> Result<CommandPlan> {
    let project = std::path::Path::new(project_directory);
    validate_bare_project_path(project)?;
    match template {
        TargetTemplate::LocalBare => {}
        TargetTemplate::SshBare { ssh, .. } => {
            validate_ssh(ssh)?;
            workspace_for(template, session_id)?;
        }
        _ => bail!("raw project directories require a bare target"),
    }
    Ok(CommandPlan {
        description: format!("provision Mjolnir session {session_id}"),
        commands: Vec::new(),
    })
}

/// Create the short-lived local container used to verify a setup target.
///
/// This deliberately shares the same argv construction as session targets so
/// setup catches an unusable image or runtime before the first session exists.
pub fn setup_smoke_plan(template: &TargetTemplate, smoke_id: &str) -> Result<CommandPlan> {
    let name = resource_name(smoke_id)?;
    let (engine, container, boundary) = match template {
        TargetTemplate::LocalPodman(container) => ("podman", container, ExecutionBoundary::Direct),
        TargetTemplate::LocalDocker(container) => ("docker", container, ExecutionBoundary::Direct),
        TargetTemplate::AppleContainer(container) => {
            ("container", container, ExecutionBoundary::Direct)
        }
        TargetTemplate::SshPodman { ssh, container } => {
            validate_ssh(ssh)?;
            ("podman", container, ExecutionBoundary::Ssh(ssh))
        }
        _ => bail!("setup smoke tests require a local container or ssh-podman target"),
    };
    validate_container_template(container)?;

    let mut run = vec![engine.to_owned()];
    run.extend(container_run_args(engine, container, &name, smoke_id, &[])?);
    let exec = vec![
        engine.to_owned(),
        "exec".to_owned(),
        "-i".to_owned(),
        name.clone(),
        "true".to_owned(),
    ];
    let remove = vec![
        engine.to_owned(),
        "rm".to_owned(),
        "--force".to_owned(),
        name,
    ];

    Ok(CommandPlan {
        description: format!("smoke test Mjolnir setup target {smoke_id}"),
        commands: vec![
            at_boundary(boundary, run).purpose("create disposable setup container"),
            at_boundary(boundary, exec).purpose("execute setup smoke command"),
            at_boundary(boundary, remove).purpose("remove disposable setup container"),
        ],
    })
}

/// Run the disposable setup smoke test and always attempt container cleanup
/// after a successful create step.
pub fn run_setup_smoke_test(
    template: &TargetTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    if let TargetTemplate::LocalDocker(container) = template {
        return run_docker_overlay_smoke_test(container, smoke_id, executor);
    }
    let plan = setup_smoke_plan(template, smoke_id)?;
    execute_checked(executor, &plan.commands[0])?;
    let smoke_result = execute_checked(executor, &plan.commands[1]);
    let cleanup_result = execute_checked(executor, &plan.commands[2]);
    smoke_result?;
    cleanup_result
}

fn run_docker_overlay_smoke_test(
    container: &ContainerTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_container_template(container)?;
    let lower = tempfile::Builder::new()
        .prefix("mj-docker-overlay-smoke-")
        .tempdir()
        .context("create Docker OverlayFS smoke directory")?;
    let original = lower.path().join("original.txt");
    let added = lower.path().join("container-created.txt");
    fs::write(&original, b"lower\n").context("write Docker OverlayFS smoke source")?;
    let name = resource_name(smoke_id)?;
    let mount = AdditionalMount {
        source: lower.path().to_path_buf(),
        destination: PathBuf::from("/mnt/hel-overlay-smoke"),
        read_only: false,
    };
    let create = docker_container_run(container, &name, smoke_id, &[mount])?
        .purpose("create disposable Docker OverlayFS smoke container");
    let probe = container_exec(
        "docker",
        &name,
        [
            "sh",
            "-c",
            "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && printf 'changed\\n' >/mnt/hel-overlay-smoke/original.txt && printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt",
        ],
    )
    .purpose("verify Docker OverlayFS copy-on-write attachment");
    let cleanup = close_plan(&TargetLocator::LocalDocker { container_id: name }, smoke_id)?
        .commands
        .into_iter()
        .next()
        .context("Docker OverlayFS smoke cleanup plan is empty")?;

    execute_checked(executor, &create)?;
    let smoke_result = execute_checked(executor, &probe);
    let cleanup_result = execute_checked(executor, &cleanup);
    smoke_result?;
    cleanup_result?;
    ensure!(
        fs::read(&original).context("read Docker OverlayFS smoke source after container write")?
            == b"lower\n",
        "Docker OverlayFS smoke test changed its lower source"
    );
    ensure!(
        !added.exists(),
        "Docker OverlayFS smoke test created a file in its lower source"
    );
    Ok(())
}

fn execute_checked(executor: &impl CommandExecutor, command: &CommandSpec) -> Result<()> {
    let output = executor.execute(command)?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Clone/bootstrap commands for AWS once the exact instance ID and address are known.
pub fn provision_on_locator_plan(
    locator: &TargetLocator,
    session_id: &str,
    bundle: &ProjectBundleSpec,
) -> Result<CommandPlan> {
    bundle.validate()?;
    verify_locator(locator, session_id)?;
    let TargetLocator::AwsEc2 { ssh, workspace, .. } = locator else {
        bail!("post-launch provisioning is only required for AWS");
    };
    let mut commands = vec![
        ssh_command(ssh, ["mkdir", "-p", workspace])
            .purpose("create EC2 session workspace")
            .stage(ProvisionStage::Cloning),
    ];
    commands.extend(install_git_plan(ExecutionBoundary::Ssh(ssh)).commands);
    commands.extend(clone_commands(bundle, workspace, |args| {
        ssh_command_owned(ssh, args)
    }));
    Ok(CommandPlan {
        description: format!("initialize EC2 session {session_id}"),
        commands,
    })
}

pub fn reconnect_plan(locator: &TargetLocator, session_id: &str) -> Result<CommandPlan> {
    verify_locator(locator, session_id)?;
    let root = worker_root(locator, session_id)?;
    let binary = format!("{root}/hel");
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            CommandSpec::new(binary, ["worker", "proxy", "--root", root.as_str()])
        }
        TargetLocator::LocalPodman { container_id } => container_exec(
            "podman",
            container_id,
            [&binary, "worker", "proxy", "--root", &root],
        ),
        TargetLocator::LocalDocker { container_id } => container_exec(
            "docker",
            container_id,
            [&binary, "worker", "proxy", "--root", &root],
        ),
        TargetLocator::AppleContainer { container_id } => container_exec(
            "container",
            container_id,
            [&binary, "worker", "proxy", "--root", &root],
        ),
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command(ssh, [&binary, "worker", "proxy", "--root", &root])
        }
        TargetLocator::SshPodman { ssh, container_id } => ssh_command(
            ssh,
            [
                "podman",
                "exec",
                "-i",
                container_id,
                &binary,
                "worker",
                "proxy",
                "--root",
                &root,
            ],
        ),
    }
    .purpose("connect to Mjolnir worker")
    .stage(ProvisionStage::Starting);
    Ok(CommandPlan {
        description: format!("reconnect Mjolnir session {session_id}"),
        commands: vec![command],
    })
}

/// Describe safe recovery for a container that belongs to an active
/// session. The inspect command is deliberately separate from `exec`: a host
/// crash can leave the container present but stopped, where `exec` cannot
/// distinguish that state from other transport failures.
pub fn target_recovery_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<TargetRecoveryPlan>> {
    verify_locator(locator, session_id)?;
    let (exists, inspect, start) = match locator {
        TargetLocator::LocalPodman { container_id } => (
            CommandSpec::new("podman", ["container", "exists", container_id])
                .purpose("check for Mjolnir session container"),
            CommandSpec::new("podman", ["container", "inspect", container_id])
                .purpose("inspect Mjolnir session container"),
            CommandSpec::new("podman", ["start", container_id])
                .purpose("start stopped Mjolnir session container"),
        ),
        TargetLocator::LocalDocker { container_id } => (
            CommandSpec::new(
                "sh",
                [
                    "-c",
                    "docker container inspect \"$1\" >/dev/null 2>&1 && exit 0; docker info >/dev/null 2>&1 && exit 1; exit 125",
                    "mj-docker-exists",
                    container_id,
                ],
            )
            .purpose("check for Mjolnir Docker session container"),
            CommandSpec::new("docker", ["container", "inspect", container_id])
                .purpose("inspect Mjolnir Docker session container"),
            CommandSpec::new("docker", ["start", container_id])
                .purpose("start stopped Mjolnir Docker session container"),
        ),
        TargetLocator::SshPodman { ssh, container_id } => (
            ssh_command(ssh, ["podman", "container", "exists", container_id])
                .purpose("check for remote Mjolnir session container"),
            ssh_command(ssh, ["podman", "container", "inspect", container_id])
                .purpose("inspect remote Mjolnir session container"),
            ssh_command(ssh, ["podman", "start", container_id])
                .purpose("start stopped remote Mjolnir session container"),
        ),
        TargetLocator::LocalBare { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::AwsEc2 { .. }
        | TargetLocator::SshBare { .. } => return Ok(None),
    };
    Ok(Some(TargetRecoveryPlan {
        exists,
        inspect,
        start,
        session_id: session_id.to_owned(),
    }))
}

/// Start a confirmed stopped container target and verify it reached `running`.
/// Missing or foreign containers, transport failures, and transitional states
/// fail without running the start command.
pub fn ensure_recovery_target_running(
    executor: &impl CommandExecutor,
    plan: Option<&TargetRecoveryPlan>,
) -> Result<TargetRecoveryOutcome> {
    let Some(plan) = plan else {
        return Ok(TargetRecoveryOutcome::NotRequired);
    };
    let existence = executor
        .execute(&plan.exists)
        .context("check whether container session target exists")?;
    match existence.status {
        0 => {}
        // `podman container exists` deliberately reserves 1 for absence and
        // uses 125 for invocation or storage failures. SSH preserves the
        // remote exit status, so this contract also covers remote Podman.
        1 => return Ok(TargetRecoveryOutcome::Missing),
        _ => {
            checked_command_output(&plan.exists, existence)
                .context("check whether container session target exists")?;
            unreachable!("a successful checked command has status zero");
        }
    }
    let status = inspect_recovery_target(executor, plan)?;
    match status.as_str() {
        "running" => Ok(TargetRecoveryOutcome::AlreadyRunning),
        "created" | "initialized" | "stopped" | "exited" => {
            let output = executor.execute(&plan.start)?;
            checked_command_output(&plan.start, output)
                .context("start confirmed stopped container session target")?;
            let after = inspect_recovery_target(executor, plan)
                .context("verify container session target after starting it")?;
            ensure!(
                after == "running",
                "container session target reported {after:?} after start"
            );
            Ok(TargetRecoveryOutcome::Started)
        }
        "paused" | "removing" | "stopping" | "unknown" => {
            bail!("refusing to start container session target in {status:?} state")
        }
        _ => bail!("container session target reported unexpected state {status:?}"),
    }
}

fn inspect_recovery_target(
    executor: &impl CommandExecutor,
    plan: &TargetRecoveryPlan,
) -> Result<String> {
    let output = executor.execute(&plan.inspect)?;
    let output = checked_command_output(&plan.inspect, output)
        .context("inspect container session target for recovery")?;
    let values: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("parse container target inspection")?;
    ensure!(
        values.len() == 1,
        "container inspection returned {} targets instead of one",
        values.len()
    );
    let target = &values[0];
    let labels = target
        .pointer("/Config/Labels")
        .and_then(serde_json::Value::as_object)
        .context("container session target has no ownership labels")?;
    ensure!(
        labels
            .get(MANAGED_LABEL)
            .and_then(serde_json::Value::as_str)
            == Some("true"),
        "refusing to start a container target Mjolnir does not own"
    );
    ensure!(
        labels
            .get(SESSION_LABEL)
            .and_then(serde_json::Value::as_str)
            == Some(plan.session_id.as_str()),
        "refusing to start a container target owned by another session"
    );
    target
        .pointer("/State/Status")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("container session target inspection has no state")
}

/// Run the target-side half of the local Git bridge over the same trusted
/// execution boundary Hel uses for worker control.
pub fn git_bridge_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    let root = worker_root(locator, session_id)?;
    let binary = format!("{root}/hel");
    command_on_locator(
        locator,
        session_id,
        vec![
            binary,
            "worker".into(),
            "git-bridge".into(),
            "--root".into(),
            root,
        ],
        "bridge local Git repositories",
    )
}

/// Wrap an argv vector for execution at a provisioned session target.
pub fn command_on_locator(
    locator: &TargetLocator,
    session_id: &str,
    args: Vec<String>,
    purpose: impl Into<String>,
) -> Result<CommandSpec> {
    verify_locator(locator, session_id)?;
    if args.is_empty() {
        bail!("target command must not be empty");
    }
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            let mut args = args.into_iter();
            let program = args.next().expect("checked non-empty target command");
            CommandSpec::new(program, args)
        }
        TargetLocator::LocalPodman { container_id } => container_exec("podman", container_id, args),
        TargetLocator::LocalDocker { container_id } => container_exec("docker", container_id, args),
        TargetLocator::AppleContainer { container_id } => {
            container_exec("container", container_id, args)
        }
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command_owned(ssh, args)
        }
        TargetLocator::SshPodman { ssh, container_id } => {
            let mut remote = vec![
                "podman".to_owned(),
                "exec".to_owned(),
                "-i".to_owned(),
                container_id.to_owned(),
            ];
            remote.extend(args);
            ssh_command_owned(ssh, remote)
        }
    };
    Ok(command.purpose(purpose))
}

const CGROUP_RESOURCE_USAGE_SCRIPT: &str = r#"
for file in memory.current memory.max memory.swap.current memory.swap.max; do
    path="/sys/fs/cgroup/$file"
    if [ -r "$path" ]; then
        printf "%s=%s\n" "$file" "$(cat "$path")"
    fi
done
if [ -r /sys/fs/cgroup/cpu.stat ]; then
    before=$(awk '/^usage_usec / { print $2 }' /sys/fs/cgroup/cpu.stat)
    sleep 0.25
    after=$(awk '/^usage_usec / { print $2 }' /sys/fs/cgroup/cpu.stat)
    set -- $(cat /sys/fs/cgroup/cpu.max 2>/dev/null || printf 'max 100000')
    if [ "$1" = max ]; then
        cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')
    else
        cores=$(awk -v quota="$1" -v period="$2" 'BEGIN { print quota / period }')
    fi
    awk -v used="$((after - before))" -v cores="$cores" \
        'BEGIN { if (cores > 0) printf "cpu.percent=%.0f\n", used / 250000 / cores * 100 }'
fi
"#;

const HOST_RESOURCE_USAGE_SCRIPT: &str = r#"
memory_proc_root=${1:-/proc}
read_cpu() { awk '/^cpu / { total=0; for (i=2; i<=NF; i++) total += $i; print total, $5 + $6 }' /proc/stat; }
set -- $(read_cpu); total_before=$1; idle_before=$2
sleep 0.25
set -- $(read_cpu); total_after=$1; idle_after=$2
awk -v total="$((total_after - total_before))" -v idle="$((idle_after - idle_before))" \
    'BEGIN { if (total > 0) printf "cpu.percent=%.0f\n", (total - idle) * 100 / total }'
arc_size=0
arc_min=0
arcstats="$memory_proc_root/spl/kstat/zfs/arcstats"
if [ -r "$arcstats" ]; then
    set -- $(awk '
        $1 == "c_min" { arc_min = $3 }
        $1 == "size" { arc_size = $3 }
        END { printf "%.0f %.0f\n", arc_size, arc_min }
    ' "$arcstats")
    arc_size=$1
    arc_min=$2
fi
awk -v arc_size="$arc_size" -v arc_min="$arc_min" '
    /^MemTotal:/ { memory_total = $2 }
    /^MemAvailable:/ { memory_available = $2 }
    /^SwapTotal:/ { swap_total = $2 }
    /^SwapFree:/ { swap_free = $2 }
    END {
        memory_total *= 1024
        memory_available *= 1024
        # Like btop, count ARC above its minimum size as reclaimable cache.
        if (arc_size > arc_min) memory_available += arc_size - arc_min
        if (memory_available > memory_total) memory_available = memory_total
        printf "memory.current=%.0f\n", memory_total - memory_available
        printf "memory.max=%.0f\n", memory_total
        printf "memory.swap.current=%.0f\n", (swap_total - swap_free) * 1024
        printf "memory.swap.max=%.0f\n", swap_total * 1024
    }
' "$memory_proc_root/meminfo"
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
"#;

const AWS_ALLOCATED_CAPACITY_SCRIPT: &str = r#"
awk '/^MemTotal:/ { printf "memory.total=%.0f\n", $2 * 1024 }' /proc/meminfo
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
df -B1 -P -- "$1" | awk 'NR == 2 { print "disk.total=" $2 }'
"#;

// `du` is run on its own so a path it cannot measure fails the probe instead of
// being silently dropped from the total: a session that reports less disk than
// it uses is worse than one that reports none. Its stderr is deliberately left
// attached, so the caller's failure message names the path that could not be
// read.
const AWS_SESSION_DISK_USAGE_SCRIPT: &str = r#"
usage=$(du -sk "$@") || exit 1
printf '%s\n' "$usage" | awk '{ total += $1 * 1024 } END { print total + 0 }'
"#;

pub fn resource_probe(locator: &TargetLocator, session_id: &str) -> Result<SessionResourceProbe> {
    verify_locator(locator, session_id)?;
    let (memory, disk) = match locator {
        TargetLocator::LocalPodman { container_id } => (
            container_exec(
                "podman",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample local Podman container resources"),
            Some(
                CommandSpec::new(
                    "podman",
                    [
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample local Podman container writable disk"),
            ),
        ),
        TargetLocator::LocalDocker { container_id } => (
            container_exec(
                "docker",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample local Docker container resources"),
            Some(
                CommandSpec::new(
                    "docker",
                    [
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample local Docker container writable disk"),
            ),
        ),
        TargetLocator::SshPodman { ssh, container_id } => (
            ssh_command(
                ssh,
                [
                    "podman",
                    "exec",
                    container_id,
                    "sh",
                    "-c",
                    CGROUP_RESOURCE_USAGE_SCRIPT,
                ],
            )
            .purpose("sample remote Podman container resources"),
            Some(
                ssh_command(
                    ssh,
                    [
                        "podman",
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample remote Podman container writable disk"),
            ),
        ),
        TargetLocator::AwsEc2 { ssh, workspace, .. } => {
            let worker_root = worker_root(locator, session_id)?;
            let profile_root = format!(".local/share/hel/profiles/{session_id}");
            (
                ssh_command(ssh, ["sh", "-c", HOST_RESOURCE_USAGE_SCRIPT])
                    .purpose("sample EC2 session resources"),
                Some(
                    ssh_command(
                        ssh,
                        [
                            "sh",
                            "-c",
                            AWS_SESSION_DISK_USAGE_SCRIPT,
                            "sh",
                            workspace.as_str(),
                            worker_root.as_str(),
                            profile_root.as_str(),
                        ],
                    )
                    .purpose("sample EC2 session disk"),
                ),
            )
        }
        TargetLocator::AppleContainer { container_id } => (
            container_exec(
                "container",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample Apple container resources"),
            None,
        ),
        TargetLocator::LocalBare { .. } | TargetLocator::SshBare { .. } => {
            bail!("resource sampling is unsupported for this target")
        }
    };
    Ok(SessionResourceProbe { memory, disk })
}

pub fn parse_resource_usage(
    memory_output: &[u8],
    disk_output: Option<&[u8]>,
) -> Result<SessionResourceUsage> {
    let mut values = BTreeMap::new();
    let memory_text = String::from_utf8_lossy(memory_output);
    for line in memory_text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(name, value.trim());
    }

    let memory_current_bytes = parse_cgroup_counter(
        values
            .get("memory.current")
            .context("resource probe did not expose memory.current")?,
    )?
    .context("resource probe reported memory.current as unlimited")?;
    let memory_limit_bytes = values
        .get("memory.max")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let swap_current_bytes = values
        .get("memory.swap.current")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let swap_limit_bytes = values
        .get("memory.swap.max")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let writable_disk_bytes = disk_output.map(parse_disk_usage).transpose()?;
    let cpu_percent = values
        .get("cpu.percent")
        .map(|value| parse_percent(value))
        .transpose()?;

    Ok(SessionResourceUsage {
        cpu_percent,
        memory_current_bytes,
        memory_limit_bytes,
        swap_current_bytes,
        swap_limit_bytes,
        writable_disk_bytes,
    })
}

/// Read the single byte count every writable-disk probe answers with.
///
/// A probe that ran and answered something else measured nothing, which must be
/// reported as a failure rather than silently becoming "disk usage unknown":
/// only a probe that was never run leaves the value unknown.
fn parse_disk_usage(output: &[u8]) -> Result<u64> {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    text.parse()
        .with_context(|| format!("disk usage probe answered {text:?} instead of a byte count"))
}

pub fn ssh_host_capacity_command(ssh: &SshTarget) -> CommandSpec {
    ssh_command(ssh, ["sh", "-c", HOST_RESOURCE_USAGE_SCRIPT])
        .purpose("sample deployment host capacity")
}

pub fn aws_allocated_capacity_command(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandSpec> {
    let TargetLocator::AwsEc2 { workspace, .. } = locator else {
        bail!("AWS allocated-capacity probes require an EC2 locator");
    };
    command_on_locator(
        locator,
        session_id,
        vec![
            "sh".into(),
            "-c".into(),
            AWS_ALLOCATED_CAPACITY_SCRIPT.into(),
            "sh".into(),
            workspace.clone(),
        ],
        "sample EC2 allocated capacity",
    )
}

pub fn parse_host_capacity(output: &[u8]) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let total = parse_required_u64(&values, "memory.max")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: Some(parse_percent(required_value(&values, "cpu.percent")?)?),
        memory_used_bytes: parse_required_u64(&values, "memory.current")?,
        memory_total_bytes: total,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: None,
    })
}

pub fn parse_aws_allocated_capacity(output: &[u8]) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let memory_total_bytes = parse_required_u64(&values, "memory.total")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: Some(parse_required_u64(&values, "disk.total")?),
    })
}

fn parse_key_values(output: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.trim().to_owned()))
        .collect()
}

fn required_value<'a>(values: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("capacity probe did not expose {key}"))
}

fn parse_required_u64(values: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    required_value(values, key)?
        .parse()
        .with_context(|| format!("capacity probe reported invalid {key}"))
}

fn parse_percent(value: &str) -> Result<u8> {
    let value: f64 = value
        .parse()
        .with_context(|| format!("invalid percentage {value:?}"))?;
    if !value.is_finite() {
        bail!("invalid percentage {value:?}");
    }
    Ok(value.round().clamp(0.0, 100.0) as u8)
}

fn parse_cgroup_counter(value: &str) -> Result<Option<u64>> {
    if value == "max" {
        return Ok(None);
    }
    Ok(Some(value.parse().with_context(|| {
        format!("invalid memory counter {value:?}")
    })?))
}

pub fn worker_root(locator: &TargetLocator, session_id: &str) -> Result<String> {
    verify_locator(locator, session_id)?;
    Ok(match locator {
        TargetLocator::LocalBare { worker_root } => worker_root.clone(),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. } => format!("/var/lib/hel/workers/{session_id}"),
        TargetLocator::AwsEc2 { .. } | TargetLocator::SshBare { .. } => {
            format!(".local/share/hel/workers/{session_id}")
        }
    })
}

/// POSIX shell helpers that identify the daemon for one exact worker root.
/// The match is assembled at run time so the script's own command line cannot
/// select itself, and `worker proxy` command lines cannot match either.
fn worker_daemon_identity_script(worker_root: &str) -> String {
    format!(
        r#"hel_root={root}
hel_match="hel worker run --root $hel_root"
hel_match_home="hel worker run --root $HOME/$hel_root"
hel_ps() {{
    ps -ww "$@" 2>/dev/null || ps "$@" 2>/dev/null
}}
hel_is_worker() {{
    hel_args=$(hel_ps -o args= -p "$1") || return 1
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) return 0 ;;
    esac
    return 1
}}
hel_recorded_worker() {{
    [ -f "$hel_root/{pid_file}" ] || return 1
    hel_pid=$(cat "$hel_root/{pid_file}" 2>/dev/null)
    case "$hel_pid" in
        '' | *[!0-9]*) return 1 ;;
    esac
    hel_is_worker "$hel_pid" || return 1
    printf '%s\n' "$hel_pid"
}}"#,
        root = posix_quote(worker_root),
        pid_file = crate::hel_worker::WORKER_PID_FILE,
    )
}

/// Report whether the exact session worker is alive without signaling it.
/// A successful probe prints one stable token; transport or shell failures
/// stay distinguishable from a confirmed absent worker.
pub fn worker_daemon_liveness_script(worker_root: &str) -> String {
    let mut script = worker_daemon_identity_script(worker_root);
    script.push_str(
        r#"
hel_report_worker_state() {
    if [ -S "$hel_root/control.sock" ]; then
        printf 'alive\n'
    else
        printf 'starting\n'
    fi
}
if hel_recorded_worker >/dev/null; then
    hel_report_worker_state
    exit 0
fi
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_report_worker_state; exit 0 ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
printf 'dead\n'
"#,
    );
    script
}

/// Stop the detached worker daemon rooted at `worker_root`.
///
/// The daemon leads its own process group, so the signal goes to the group
/// first to take the agent down with it. Shells disagree about how to write a
/// negative PID (`dash` rejects `--`), hence the two forms before the
/// single-process fallback for daemons predating the group leadership.
pub fn stop_worker_daemon_script(worker_root: &str) -> String {
    let mut script = worker_daemon_identity_script(worker_root);
    script.push_str(
        r#"
hel_signal() {
    kill -"$1" -- "-$2" 2>/dev/null && return 0
    kill -"$1" "-$2" 2>/dev/null && return 0
    kill -"$1" "$2" 2>/dev/null
}
hel_stop() {
    hel_signal TERM "$1" || return 0
    hel_waited=0
    while [ "$hel_waited" -lt 2 ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
    kill -0 "$1" 2>/dev/null || return 0
    hel_signal KILL "$1" || true
    hel_waited=0
    while [ "$hel_waited" -lt 3 ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
}
if hel_pid=$(hel_recorded_worker); then
    hel_stop "$hel_pid"
fi
hel_ps -eo pid=,args= | while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_stop "$hel_pid" ;;
    esac
done
hel_left=0
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_left=1 ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
if [ "$hel_left" -ne 0 ]; then
    echo "worker still running after stop: $hel_root" >&2
    exit 1
fi
"#,
    );
    script
}

/// Stop a leaked worker and delete the durable relay state under its root.
///
/// A resume seeds fresh relay state into the same root a closed session used.
/// Leftover state wins over that seed at startup, so it has to go, and
/// whatever might still be writing it has to go first. Container and instance
/// targets are rebuilt from scratch on resume, so they need nothing here.
pub fn clear_relay_state_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<CommandSpec>> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let script = format!(
        "{}\nrm -rf -- {} {}\n",
        stop_worker_daemon_script(&session_worker_root),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            crate::hel_worker::RELAY_STATE_FILE
        )),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            crate::hel_worker::RELAY_JOURNAL_DIR
        )),
    );
    Ok(match locator {
        TargetLocator::LocalBare { .. } => Some(
            CommandSpec::new("sh", ["-c", script.as_str()])
                .purpose("stop a leaked local Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::SshBare { ssh, .. } => Some(
            ssh_command(ssh, ["sh", "-c", script.as_str()])
                .purpose("stop a leaked remote Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::AwsEc2 { .. } => None,
    })
}

pub fn close_plan(locator: &TargetLocator, session_id: &str) -> Result<CommandPlan> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let session_profile_home = format!(".local/share/hel/profiles/{session_id}");
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            // The daemon dies before its root does: a survivor's next durable
            // write would recreate the directory this command removes.
            let script = format!(
                "{}\nrm -rf -- {}\n",
                stop_worker_daemon_script(&session_worker_root),
                posix_quote(&session_worker_root),
            );
            CommandSpec::new("sh", ["-c", script.as_str()]).purpose(
                "stop the local Mjolnir worker and remove exact local Mjolnir worker state",
            )
        }
        TargetLocator::LocalPodman { container_id } => {
            let script = "status=0; podman rm --force --ignore \"$1\" || status=$?; rm -rf -- \"$HOME/.cache/mjolnir/git/sessions/$2\"; exit \"$status\"";
            CommandSpec::new("sh", ["-c", script, "mj-close", container_id, session_id])
                .purpose("remove local Podman session container and Git cache snapshot")
        }
        TargetLocator::LocalDocker { container_id } => {
            let script = r#"status=0
if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$1" 2>/dev/null); then
    if [ "$identity" = "true|$2" ]; then
        docker rm --force "$1" || status=$?
    else
        echo 'refusing to remove a Docker container Mjolnir does not own for this session' >&2
        status=2
    fi
elif ! docker info >/dev/null 2>&1; then
    echo 'could not determine whether the Docker session container exists' >&2
    status=1
fi
if [ "$status" -eq 0 ]; then
    volumes=$(docker volume ls --quiet --filter "label=dev.mj.managed=true" --filter "label=dev.mj.session=$2") || status=$?
    if [ "$status" -eq 0 ]; then
        for volume in $volumes; do docker volume rm --force "$volume" || status=$?; done
    fi
fi
rm -rf -- "$HOME/.cache/mjolnir/git/sessions/$2"
if [ "$status" -eq 0 ]; then
    root="$HOME/.cache/mjolnir/docker-overlays/$1"
    if [ "$(cat "$root/.hel-session" 2>/dev/null || true)" = "$2" ]; then
        case $1 in hel-*) rm -rf -- "$root" ;; *) status=2 ;; esac
    fi
fi
exit "$status""#;
            CommandSpec::new("sh", ["-c", script, "mj-close", container_id, session_id])
                .purpose("remove local Docker session container, overlay volumes, and cache state")
        }
        TargetLocator::AppleContainer { container_id } => {
            let script = "status=0; container rm --force \"$1\" || status=$?; rm -rf -- \"$HOME/.cache/mjolnir/git/sessions/$2\"; exit \"$status\"";
            CommandSpec::new("sh", ["-c", script, "mj-close", container_id, session_id])
                .purpose("remove Apple session container and Git cache snapshot")
        }
        TargetLocator::AwsEc2 {
            profile,
            region,
            instance_id,
            ..
        } => {
            // EC2 TerminateInstances is explicitly idempotent, including a
            // repeated request for an already-terminated instance.
            CommandSpec::new(
                "aws",
                [
                    "--profile",
                    profile,
                    "--region",
                    region,
                    "ec2",
                    "terminate-instances",
                    "--instance-ids",
                    instance_id,
                ],
            )
            .purpose("terminate exact EC2 session instance")
        }
        TargetLocator::SshBare { ssh, workspace } => {
            // Same ordering constraint as the local bare target: stop the
            // daemon before deleting the root it keeps writing to.
            let script = format!(
                "{}\nrm -rf -- {} {} {}\n",
                stop_worker_daemon_script(&session_worker_root),
                posix_quote(workspace),
                posix_quote(&session_worker_root),
                posix_quote(&session_profile_home),
            );
            ssh_command(ssh, ["sh", "-c", script.as_str()]).purpose(
                "stop the remote Mjolnir worker and remove exact SSH session workspace and runtime state",
            )
        }
        TargetLocator::SshPodman { ssh, container_id } => {
            let script = "status=0; podman rm --force --ignore \"$1\" || status=$?; rm -rf -- \"$HOME/.cache/mjolnir/git/sessions/$2\"; exit \"$status\"";
            ssh_command(
                ssh,
                ["sh", "-c", script, "mj-close", container_id, session_id],
            )
            .purpose("remove exact remote Podman session container and Git cache snapshot")
        }
    };
    Ok(CommandPlan {
        description: format!("close Mjolnir session {session_id}"),
        commands: vec![command],
    })
}

/// Confirm that a container is absent after its exact delete command failed.
/// Other target deletion commands are already idempotent: filesystem removal
/// uses `rm -rf`, Podman uses `--ignore`, and EC2 termination is an idempotent
/// API operation. Apple lists exact container IDs; Docker checks both the exact
/// container name and exact session-labeled volumes while distinguishing an
/// unavailable daemon from absence.
pub fn cleanup_target_is_confirmed_absent(
    locator: &TargetLocator,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    verify_locator(locator, session_id)?;
    let (command, is_docker) = match locator {
        TargetLocator::AppleContainer { .. } => (
            CommandSpec::new("container", ["list", "--all", "--quiet"])
                .purpose("confirm exact Apple session container is absent"),
            false,
        ),
        TargetLocator::LocalDocker { container_id } => (
            CommandSpec::new(
                "sh",
                [
                    "-c",
                    "if docker container inspect \"$1\" >/dev/null 2>&1; then exit 1; fi; docker info >/dev/null 2>&1 || exit 2; test -z \"$(docker volume ls --quiet --filter label=dev.mj.managed=true --filter label=dev.mj.session=$2)\"",
                    "hel-confirm-absent",
                    container_id,
                    session_id,
                ],
            )
            .purpose("confirm exact Docker session resources are absent"),
            true,
        ),
        _ => return Ok(false),
    };
    let output = executor.execute(&command)?;
    if is_docker {
        return match output.status {
            0 => Ok(true),
            1 => Ok(false),
            _ => bail!(
                "{} failed with status {}: {}",
                command.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        };
    }
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let listed = String::from_utf8(output.stdout).context("decode Apple container list")?;
    let TargetLocator::AppleContainer { container_id } = locator else {
        unreachable!("engine selected from locator")
    };
    Ok(!listed.lines().any(|id| id.trim() == container_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionBoundary<'a> {
    Direct,
    Container {
        engine: &'a str,
        container_id: &'a str,
    },
    Ssh(&'a SshTarget),
    SshPodman {
        ssh: &'a SshTarget,
        container_id: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessProbe<'a> {
    pub executable: &'a str,
    pub version_args: &'a [&'a str],
    pub bridge_executable: Option<&'a str>,
}

/// Compatibility is intentionally interpreted by the controller. A successful
/// probe permits an image-baked tool to be reused; a missing/incompatible tool
/// causes the controller to upload/install its release-owned copy.
pub fn bootstrap_probe_plan(
    boundary: ExecutionBoundary<'_>,
    harness: HarnessProbe<'_>,
) -> Result<CommandPlan> {
    validate_executable(harness.executable)?;
    let mut commands = vec![
        at_boundary(
            boundary,
            std::iter::once(harness.executable)
                .chain(harness.version_args.iter().copied())
                .map(str::to_owned)
                .collect(),
        )
        .purpose("probe harness version"),
    ];
    if let Some(bridge) = harness.bridge_executable {
        validate_executable(bridge)?;
        commands.push(
            at_boundary(boundary, vec![bridge.to_owned(), "--version".to_owned()])
                .purpose("probe ACP bridge version"),
        );
    }
    commands.push(
        at_boundary(boundary, vec!["git".to_owned(), "--version".to_owned()]).purpose("probe Git"),
    );
    Ok(CommandPlan {
        description: "probe reusable target tools".to_owned(),
        commands,
    })
}

/// Thin Linux Git bootstrap. Managed containers also receive GitHub CLI and
/// its HTTPS credential helper so an injected `GH_TOKEN` works before clone.
pub fn install_git_plan(boundary: ExecutionBoundary<'_>) -> CommandPlan {
    let managed_container = matches!(
        boundary,
        ExecutionBoundary::Container { .. } | ExecutionBoundary::SshPodman { .. }
    );
    let script = if managed_container {
        "set -eu; if ! command -v git >/dev/null 2>&1 || ! command -v gh >/dev/null 2>&1; then SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'Git and GitHub CLI installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y git gh ca-certificates curl; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git gh ca-certificates curl; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git gh ca-certificates curl; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache git github-cli ca-certificates curl; else echo 'Unsupported package manager; install Git and GitHub CLI in the image' >&2; exit 1; fi; fi; git config --global credential.https://github.com.helper '!gh auth git-credential'; git config --global credential.https://gist.github.com.helper '!gh auth git-credential'"
    } else {
        "set -eu; if command -v git >/dev/null 2>&1; then exit 0; fi; SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'Git installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y git ca-certificates curl; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git ca-certificates curl; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git ca-certificates curl; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache git ca-certificates curl; else echo 'Unsupported package manager; install Git manually' >&2; exit 1; fi"
    };
    CommandPlan {
        description: "install missing Git".to_owned(),
        commands: vec![
            at_boundary(
                boundary,
                vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            )
            .purpose("install Git")
            .stage(ProvisionStage::Cloning),
        ],
    }
}

/// Shared [`CommandSpec::parallel_group`] marker for one bundle's per-repository
/// clone/init commands. Every `clone_commands` call builds its own
/// [`CommandPlan`], so a single fixed marker never mixes batches across plans.
const BUNDLE_REPOSITORIES_PARALLEL_GROUP: u32 = 1;

fn clone_commands(
    bundle: &ProjectBundleSpec,
    workspace: &str,
    wrap: impl Fn(Vec<String>) -> CommandSpec,
) -> Vec<CommandSpec> {
    let mut commands = vec![
        wrap(vec![
            "mkdir".to_owned(),
            "-p".to_owned(),
            workspace.to_owned(),
        ])
        .purpose("create bundle workspace")
        .stage(ProvisionStage::Cloning),
    ];
    for repository in &bundle.repositories {
        let destination = format!("{workspace}/{}", repository.destination);
        let Some(url) = &repository.url else {
            commands.push(
                wrap(vec!["git".into(), "init".into(), "--".into(), destination])
                    .purpose(format!("initialize {}", repository.destination))
                    .stage(ProvisionStage::Cloning)
                    .parallel_group(BUNDLE_REPOSITORIES_PARALLEL_GROUP),
            );
            continue;
        };
        let mut args = vec!["git".to_owned(), "clone".to_owned()];
        if let Some(reference) = &repository.reference {
            args.extend(["--reference-if-able".to_owned(), reference.clone()]);
        }
        if let Some(git_ref) = &repository.git_ref {
            args.extend(["--branch".to_owned(), git_ref.clone()]);
        }
        args.push("--".to_owned());
        args.push(url.clone());
        args.push(destination);
        commands.push(
            wrap(args)
                .purpose(format!("clone {}", repository.destination))
                .stage(ProvisionStage::Cloning)
                .parallel_group(BUNDLE_REPOSITORIES_PARALLEL_GROUP),
        );
    }
    commands
}

fn container_run(
    engine: &str,
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
) -> Result<CommandSpec> {
    Ok(CommandSpec::new(
        engine,
        container_run_args(engine, template, name, session_id, additional_mounts)?,
    )
    .purpose("start session container")
    .stage(ProvisionStage::Provisioning)
    .creates_target())
}

const DOCKER_OVERLAY_RUN_SCRIPT: &str = r#"set -eu
session=$1
container=$2
shift 2
root="$HOME/.cache/mjolnir/docker-overlays/$container"
marker="$root/.hel-session"
volumes=
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        released=true
        if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
            if [ "$identity" = "true|$session" ]; then
                docker rm --force "$container" >/dev/null 2>&1 || released=false
            else
                released=false
            fi
        elif ! docker info >/dev/null 2>&1; then
            released=false
        fi
        if [ "$released" = true ]; then
            for volume in $volumes; do
                docker volume rm --force "$volume" >/dev/null 2>&1 || released=false
            done
        fi
        if [ "$released" = true ] && [ "$(cat "$marker" 2>/dev/null || true)" = "$session" ]; then
            case $container in hel-*) rm -rf -- "$root" ;; esac
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
mkdir -p -- "$root"
if [ -e "$marker" ]; then
    [ "$(cat "$marker")" = "$session" ] || {
        echo "refusing foreign Docker overlay directory $root" >&2
        exit 1
    }
elif [ -n "$(find "$root" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    echo "refusing non-empty Docker overlay directory $root" >&2
    exit 1
else
    printf '%s\n' "$session" >"$marker"
fi
while [ "$1" != -- ]; do
    ordinal=$1
    source=$2
    volume=$3
    shift 3
    upper="$root/$ordinal/upper"
    work="$root/$ordinal/work"
    mkdir -p -- "$upper" "$work"
    if ! docker volume inspect "$volume" >/dev/null 2>&1; then
        docker volume create \
            --driver local \
            --label "dev.mj.managed=true" \
            --label "dev.mj.session=$session" \
            --opt type=overlay \
            --opt device=overlay \
            --opt "o=lowerdir=$source,upperdir=$upper,workdir=$work" \
            "$volume" >/dev/null
    fi
    identity=$(docker volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume")
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign Docker volume $volume" >&2
        exit 1
    }
    volumes="$volumes $volume"
done
shift
"$@"
"#;

fn docker_overlay_volume_name(container_name: &str, ordinal: usize) -> String {
    format!("{container_name}-mount-{ordinal}")
}

fn docker_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
) -> Result<CommandSpec> {
    let run_args = container_run_args("docker", template, name, session_id, additional_mounts)?;
    let writable = additional_mounts
        .iter()
        .enumerate()
        .filter(|(_, mount)| !mount.read_only)
        .collect::<Vec<_>>();
    if writable.is_empty() {
        return container_run("docker", template, name, session_id, additional_mounts);
    }
    let mut args = vec![
        "-c".to_owned(),
        DOCKER_OVERLAY_RUN_SCRIPT.to_owned(),
        "hel-docker-run".to_owned(),
        session_id.to_owned(),
        name.to_owned(),
    ];
    for (ordinal, mount) in writable {
        args.extend([
            ordinal.to_string(),
            mount.source.to_string_lossy().into_owned(),
            docker_overlay_volume_name(name, ordinal),
        ]);
    }
    args.extend(["--".to_owned(), "docker".to_owned()]);
    args.extend(run_args);
    Ok(CommandSpec::new("sh", args)
        .purpose("start Docker session container with isolated attachments")
        .stage(ProvisionStage::Provisioning)
        .creates_target())
}

fn container_run_args(
    engine: &str,
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
) -> Result<Vec<String>> {
    validate_additional_mounts(additional_mounts)?;
    let mut args = vec!["run".to_owned()];
    if engine == "podman" {
        let pull_policy = template.pull_policy.resolve(&template.image);
        if pull_policy != ImagePullPolicy::Missing {
            args.push(format!(
                "--pull={}",
                template.pull_policy.podman_value(&template.image)
            ));
        }
        // PID 1 is `sleep infinity`, which reaps nothing, so every exec that
        // outlives its parent leaves a zombie behind. Apple's `container`
        // engine is left alone: its support for the flag is unverified.
        args.push("--init".to_owned());
    } else if engine == "docker" {
        let pull = match template.pull_policy.resolve(&template.image) {
            ImagePullPolicy::Always | ImagePullPolicy::Newer => "always",
            ImagePullPolicy::Missing => "missing",
            ImagePullPolicy::Never => "never",
            ImagePullPolicy::Auto => unreachable!("auto pull policy must resolve"),
        };
        args.push(format!("--pull={pull}"));
        args.push("--init".to_owned());
    }
    args.extend(["--detach".to_owned(), "--name".to_owned(), name.to_owned()]);
    args.extend(managed_resource_identity_args(
        ManagedResourceKind::Container,
        session_id,
    ));
    args.extend(template.extra_run_args.clone());
    for (ordinal, mount) in additional_mounts.iter().enumerate() {
        let source = mount.source.to_string_lossy();
        let destination = mount.destination.to_string_lossy();
        match engine {
            "podman" => {
                let mode = if mount.read_only { "ro" } else { "O" };
                args.extend([
                    "--volume".to_owned(),
                    format!("{source}:{destination}:{mode}"),
                ]);
            }
            "docker" => {
                let source = if mount.read_only {
                    source.into_owned()
                } else {
                    docker_overlay_volume_name(name, ordinal)
                };
                let suffix = if mount.read_only { ":ro" } else { "" };
                args.extend([
                    "--volume".to_owned(),
                    format!("{source}:{destination}{suffix}"),
                ]);
            }
            "container" => args.extend([
                "--mount".to_owned(),
                format!("type=bind,source={source},target={destination},readonly"),
            ]),
            _ => bail!("additional mounts are unsupported for container engine {engine:?}"),
        }
    }
    args.extend([
        template.image.clone(),
        "sleep".to_owned(),
        "infinity".to_owned(),
    ]);
    Ok(args)
}

fn apple_image_prepare_commands(template: &ContainerTemplate) -> Vec<CommandSpec> {
    let command = match template.pull_policy.resolve(&template.image) {
        ImagePullPolicy::Always | ImagePullPolicy::Newer => {
            CommandSpec::new("container", ["image", "pull", template.image.as_str()])
                .purpose(format!("refresh container image {}", template.image))
        }
        ImagePullPolicy::Never => {
            CommandSpec::new("container", ["image", "inspect", template.image.as_str()])
                .purpose(format!("find pinned container image {}", template.image))
        }
        ImagePullPolicy::Missing => return Vec::new(),
        ImagePullPolicy::Auto => unreachable!("auto pull policy must resolve"),
    };
    vec![command.stage(ProvisionStage::Provisioning)]
}

fn container_exec(
    engine: &str,
    container_id: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut command_args = vec!["exec".to_owned(), "-i".to_owned(), container_id.to_owned()];
    command_args.extend(args.into_iter().map(Into::into));
    CommandSpec::new(engine, command_args)
}

fn at_boundary(boundary: ExecutionBoundary<'_>, args: Vec<String>) -> CommandSpec {
    match boundary {
        ExecutionBoundary::Direct => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        ExecutionBoundary::Container {
            engine,
            container_id,
        } => container_exec(engine, container_id, args),
        ExecutionBoundary::Ssh(ssh) => ssh_command_owned(ssh, args),
        ExecutionBoundary::SshPodman { ssh, container_id } => {
            let mut remote = vec![
                "podman".to_owned(),
                "exec".to_owned(),
                "-i".to_owned(),
                container_id.to_owned(),
            ];
            remote.extend(args);
            ssh_command_owned(ssh, remote)
        }
    }
}

mod ssh;

pub use ssh::posix_quote;
pub use ssh::{
    join_remote_command, ssh_connectivity_probe, ssh_directory_completions, ssh_directory_exists,
    validate_bare_project_directory,
};
use ssh::{
    ssh_command, ssh_command_owned, ssh_validation_command, validate_aws,
    validate_bare_project_path, validate_container_template, validate_executable,
    validate_relative_path, validate_session_id, validate_ssh, validate_workspace_prefix,
    verify_locator,
};

#[cfg(test)]
mod tests;

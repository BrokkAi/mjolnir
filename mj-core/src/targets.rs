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

use crate::config::{HarnessKind, ImagePullPolicy};

pub const SESSION_LABEL: &str = "dev.mj.session";
pub const MANAGED_LABEL: &str = "dev.mj.managed";
pub const SESSION_TAG: &str = "dev.mj.session";
pub const MANAGED_TAG: &str = "dev.mj.managed";
pub const CONTAINER_WORKSPACE: &str = "/workspace";

/// The launch phase a command belongs to, reported as launch progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProvisionStage {
    Provisioning,
    Booting,
    Cloning,
    Syncing,
    Restoring,
    Starting,
    Installing(HarnessKind),
    Compacting,
    RecoveryCopy,
    Verifying,
    Closing,
    StoppingTarget,
    RemovingContainer,
    RemovingStorage,
    CleaningCache,
}

impl ProvisionStage {
    pub fn label(self) -> String {
        match self {
            Self::Provisioning => "Provision".into(),
            Self::Booting => "Boot".into(),
            Self::Cloning => "Clone".into(),
            Self::Syncing => "Sync".into(),
            Self::Restoring => "Restore".into(),
            Self::Starting => "Start".into(),
            Self::Installing(harness) => format!("Installing {}", harness.display_name()),
            Self::Compacting => "Compact".into(),
            Self::RecoveryCopy => "Recovery copy".into(),
            Self::Verifying => "Verify".into(),
            Self::Closing => "Close".into(),
            Self::StoppingTarget => "Stop target".into(),
            Self::RemovingContainer => "Remove container".into(),
            Self::RemovingStorage => "Remove container storage".into(),
            Self::CleaningCache => "Clean cache".into(),
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
    /// Replace ambient process variables with the explicitly supplied environment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_env: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
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
            clear_env: false,
            cwd: None,
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

/// Container destinations cannot use the controller or login user's home.
pub fn validate_mount_destination(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| part == std::path::Component::ParentDir),
        "additional mount destination must be a safe absolute container path; ~ is not supported"
    );
    Ok(())
}

pub fn validate_additional_mounts(mounts: &[AdditionalMount]) -> Result<()> {
    let mut destinations = BTreeSet::new();
    for mount in mounts {
        if !mount.source.is_absolute() || mount.source.as_os_str().is_empty() {
            bail!("additional mount source must be an absolute directory path");
        }
        validate_mount_destination(&mount.destination)?;
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
pub fn trace_command_duration(command: &CommandSpec, started: Instant, status: i32) {
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
        let output = configured_command(command)
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
        let process = configured_command(command);
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

fn configured_command(command: &CommandSpec) -> Command {
    let mut process = Command::new(&command.program);
    if command.clear_env {
        process.env_clear();
    }
    if let Some(cwd) = &command.cwd {
        process.current_dir(cwd);
    }
    process.args(&command.args).envs(&command.env);
    process
}

fn cancellable_command(command: &CommandSpec) -> Command {
    let mut process = configured_command(command);
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
    if let Err(error) = crate::subprocess::signal_process_group(child.id() as i32, libc::SIGKILL) {
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
        let mut status = None;
        let status = loop {
            if self.is_cancelled() {
                terminate_cancellable_child(&mut child);
                for (stream, reader) in [("stdout", stdout_reader), ("stderr", stderr_reader)] {
                    match reader.join() {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(stream, %error, "cancelled command reader failed")
                        }
                        Err(_) => tracing::warn!(stream, "cancelled command reader panicked"),
                    }
                }
                bail!("operation cancelled while {}", command.purpose);
            }
            if status.is_none() {
                status = child
                    .try_wait()
                    .with_context(|| format!("wait for {}", command.purpose))?;
            }
            // Descendants can retain these pipes after the shell exits. Keep
            // enforcing the deadline until both readers have actually finished.
            if let Some(status) = status
                && stdout_reader.is_finished()
                && stderr_reader.is_finished()
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
            TargetTemplate::SshPodman { .. } | TargetTemplate::SshDocker { .. } => {
                let remote = command
                    .args
                    .last_mut()
                    .context("remote container command has no SSH command argument")?;
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
pub fn checked_command_output(
    command: &CommandSpec,
    output: CommandOutput,
) -> Result<CommandOutput> {
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
    /// Network clone URL. Managed workspaces require a configured remote.
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_urls: Vec<String>,
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
            ensure!(
                repository
                    .url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty() && !url.starts_with('-')),
                "isolated repositories require a network Git remote; configure a remote or use a raw local session"
            );
            crate::remote_git::validate_network_url(
                repository.url.as_deref().expect("checked above"),
            )?;
            for push_url in &repository.push_urls {
                crate::remote_git::validate_network_url(push_url)?;
            }
            ensure!(
                repository.git_ref.is_none(),
                "git_ref is no longer supported; remove it to start from the remote's default branch"
            );
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PodmanWorkspaceStorage {
    PodmanVolume,
    HostHelper {
        root: String,
        helper: Vec<String>,
    },
    #[default]
    ContainerLayer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerTemplate {
    pub image: String,
    #[serde(default)]
    pub pull_policy: ImagePullPolicy,
    #[serde(default)]
    pub extra_run_args: Vec<String>,
    #[serde(default)]
    pub workspace_storage: PodmanWorkspaceStorage,
}

impl ImagePullPolicy {
    /// How fresh this target wants its image, with `Auto` read from the image
    /// reference. This is the freshness the background refresher acts on.
    pub fn resolve(self, image: &str) -> Self {
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

    /// How fresh a launch insists on being. `Auto` never pulls here: the daemon
    /// refreshes remote `:latest` images on its own schedule, so a session
    /// starts from the cached image instead of blocking a launch on a
    /// multi-gigabyte download. An explicit policy still means what it says.
    pub fn at_launch(self, image: &str) -> Self {
        if self == Self::Auto {
            Self::Missing
        } else {
            self.resolve(image)
        }
    }

    /// Podman's spelling of an already-resolved policy.
    pub fn podman_value(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Newer => "newer",
            Self::Missing => "missing",
            Self::Never => "never",
            Self::Auto => unreachable!("auto pull policy must resolve"),
        }
    }
}

/// Where a background image refresh runs.
///
/// The SSH form wraps commands the way provisioning does rather than the way
/// the preflight probes do: a pull runs for minutes, and the probes' two-second
/// keepalive would drop the connection underneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageHost {
    LocalPodman,
    LocalDocker,
    SshPodman(SshTarget),
    SshDocker(SshTarget),
}

impl ImageHost {
    const fn engine(&self) -> &'static str {
        match self {
            Self::LocalPodman | Self::SshPodman(_) => "podman",
            Self::LocalDocker | Self::SshDocker(_) => "docker",
        }
    }

    /// How this host is named in a log line.
    pub fn label(&self) -> String {
        match self {
            Self::LocalPodman => "local podman".to_owned(),
            Self::LocalDocker => "local docker".to_owned(),
            Self::SshPodman(ssh) => format!("podman on {}", ssh.destination),
            Self::SshDocker(ssh) => format!("docker on {}", ssh.destination),
        }
    }

    fn command(&self, args: Vec<String>, purpose: String) -> CommandSpec {
        match self {
            Self::LocalPodman | Self::LocalDocker => {
                CommandSpec::new(args[0].clone(), args[1..].iter().cloned())
            }
            Self::SshPodman(ssh) | Self::SshDocker(ssh) => ssh_command_owned(ssh, args),
        }
        .purpose(purpose)
    }
}

/// The commands that keep one host's copy of one image current, away from any
/// session launch. They run in this order, and only for that host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRefresh {
    pub host: ImageHost,
    pub image: String,
    pub platform: Option<String>,
    /// Reads the cached image id, so a pull that changed nothing stays quiet.
    /// Run before and after the pull.
    pub image_id: CommandSpec,
    pub pull: CommandSpec,
    /// Dangling images only. Both engines keep an image a container still uses.
    pub prune: CommandSpec,
}

/// The background refresh for one configured container target, or `None` when
/// the target's pull policy is satisfied by whatever the host already has.
pub fn image_refresh(
    host: ImageHost,
    image: &str,
    platform: Option<&str>,
    pull_policy: ImagePullPolicy,
) -> Option<ImageRefresh> {
    if !matches!(
        pull_policy.resolve(image),
        ImagePullPolicy::Always | ImagePullPolicy::Newer
    ) {
        return None;
    }
    let engine = host.engine();
    let image_id = host.command(
        vec![
            engine.to_owned(),
            "image".to_owned(),
            "inspect".to_owned(),
            "--format".to_owned(),
            "{{.Id}}".to_owned(),
            image.to_owned(),
        ],
        format!("read the cached id of container image {image}"),
    );
    let mut pull_args = vec![engine.to_owned(), "pull".to_owned()];
    if let Some(platform) = platform {
        pull_args.push(format!("--platform={platform}"));
    }
    pull_args.push(image.to_owned());
    let pull = host.command(pull_args, format!("refresh container image {image}"));
    let prune = host.command(
        vec![
            engine.to_owned(),
            "image".to_owned(),
            "prune".to_owned(),
            "-f".to_owned(),
        ],
        "remove dangling container images".to_owned(),
    );
    Some(ImageRefresh {
        host,
        image: image.to_owned(),
        platform: platform.map(str::to_owned),
        image_id,
        pull,
        prune,
    })
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
    SshDocker {
        ssh: SshTarget,
        container: ContainerTemplate,
    },
}

fn default_ssh_prefix() -> String {
    ".local/share/hel/workspaces".to_owned()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PodmanWorkspaceLocator {
    #[default]
    ContainerLayer,
    Volume {
        name: String,
    },
    HostPath {
        path: String,
        helper: Vec<String>,
        resource: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetLocator {
    LocalBare {
        worker_root: String,
    },
    LocalPodman {
        container_id: String,
        #[serde(default)]
        workspace_storage: PodmanWorkspaceLocator,
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
        #[serde(default)]
        workspace_storage: PodmanWorkspaceLocator,
    },
    SshDocker {
        ssh: SshTarget,
        container_id: String,
    },
}

impl TargetTemplate {
    pub const fn container_engine(&self) -> Option<&'static str> {
        match self {
            Self::LocalPodman(_) | Self::SshPodman { .. } => Some("podman"),
            Self::LocalDocker(_) | Self::SshDocker { .. } => Some("docker"),
            Self::AppleContainer(_) => Some("container"),
            _ => None,
        }
    }
}

impl TargetLocator {
    pub const fn container_engine(&self) -> Option<&'static str> {
        match self {
            Self::LocalPodman { .. } | Self::SshPodman { .. } => Some("podman"),
            Self::LocalDocker { .. } | Self::SshDocker { .. } => Some("docker"),
            Self::AppleContainer { .. } => Some("container"),
            _ => None,
        }
    }
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

pub fn podman_workspace_locator(
    template: &ContainerTemplate,
    session_id: &str,
) -> Result<PodmanWorkspaceLocator> {
    let resource = format!("{}-workspace", resource_name(session_id)?);
    match &template.workspace_storage {
        PodmanWorkspaceStorage::PodmanVolume => {
            Ok(PodmanWorkspaceLocator::Volume { name: resource })
        }
        PodmanWorkspaceStorage::HostHelper { root, helper } => {
            let root = Path::new(root);
            ensure!(
                root.is_absolute(),
                "Podman workspace storage root must be absolute"
            );
            ensure!(
                !helper.is_empty() && helper.iter().all(|argument| !argument.is_empty()),
                "Podman workspace storage helper must contain non-empty arguments"
            );
            Ok(PodmanWorkspaceLocator::HostPath {
                path: root.join(&resource).to_string_lossy().into_owned(),
                helper: helper.clone(),
                resource,
            })
        }
        PodmanWorkspaceStorage::ContainerLayer => Ok(PodmanWorkspaceLocator::ContainerLayer),
    }
}

pub fn workspace_for(template: &TargetTemplate, session_id: &str) -> Result<String> {
    validate_session_id(session_id)?;
    match template {
        TargetTemplate::LocalBare => bail!("local bare projects use their selected directory"),
        TargetTemplate::LocalPodman(_)
        | TargetTemplate::LocalDocker(_)
        | TargetTemplate::AppleContainer(_)
        | TargetTemplate::SshPodman { .. }
        | TargetTemplate::SshDocker { .. } => Ok(CONTAINER_WORKSPACE.to_owned()),
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
        TargetLocator::LocalPodman { container_id, .. } => {
            container_exec("podman", container_id, args)
        }
        TargetLocator::LocalDocker { container_id } => container_exec("docker", container_id, args),
        TargetLocator::AppleContainer { container_id } => {
            container_exec("container", container_id, args)
        }
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command_owned(ssh, args)
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker { ssh, container_id } => {
            let mut remote = vec![
                locator
                    .container_engine()
                    .expect("remote container")
                    .to_owned(),
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
pub fn worker_root(locator: &TargetLocator, session_id: &str) -> Result<String> {
    verify_locator(locator, session_id)?;
    Ok(match locator {
        TargetLocator::LocalBare { worker_root } => worker_root.clone(),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. } => format!("/var/lib/hel/workers/{session_id}"),
        TargetLocator::AwsEc2 { .. } | TargetLocator::SshBare { .. } => {
            format!(".local/share/hel/workers/{session_id}")
        }
    })
}
mod ssh;
pub use ssh::*;

pub fn container_exec(
    engine: &str,
    container_id: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut command_args = vec!["exec".to_owned(), "-i".to_owned(), container_id.to_owned()];
    command_args.extend(args.into_iter().map(Into::into));
    CommandSpec::new(engine, command_args)
}

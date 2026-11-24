//! Declarative execution plans for Hel session targets.
//!
//! Plans deliberately contain argv vectors instead of local shell strings.  A
//! shell is used only at the SSH boundary, where OpenSSH necessarily sends a
//! command string; every remotely supplied argument is POSIX-quoted there.

use std::collections::{BTreeMap, BTreeSet};
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

mod diagnostics;
pub use diagnostics::{BlockingOperation, BlockingOperationSnapshot, active_blocking_operations};
mod capacity_report;
pub mod storage;
pub use capacity_report::*;

pub const SESSION_LABEL: &str = "dev.mj.session";
pub const MANAGED_LABEL: &str = "dev.mj.managed";
pub const SESSION_TAG: &str = "dev.mj.session";
pub const MANAGED_TAG: &str = "dev.mj.managed";
/// Which Mjolnir instance (named `--instance` or data-directory fingerprint)
/// created a worker; see `config::instance_identity`.
pub const INSTANCE_LABEL: &str = "dev.mj.instance";
pub const INSTANCE_TAG: &str = "dev.mj.instance";
/// The shared in-container workspace every container session used before
/// per-session workspaces existed. New sessions record a path under it; see
/// `container_workspace_root`.
pub const CONTAINER_WORKSPACE: &str = "/workspace";

/// Whether this machine runs Mjolnir workers itself. Windows is a controller
/// only: its sessions run on Linux, in a container or on an SSH host, so it
/// has no local bare target and no local worker to probe a profile with.
pub const HOST_RUNS_WORKERS: bool = cfg!(unix);

/// The launch phase a command belongs to, reported as launch progress.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProvisionStage {
    /// Waiting for a background download of this target's image to finish,
    /// rather than starting a second download of the same image.
    PullingImage,
    /// Queued behind another task pulling this image. This is waiting for a
    /// turn, not work, so it never counts as a stall.
    WaitingForImagePull,
    WaitingForContainerUpgrade,
    /// Queued behind another task holding the worker binary store for this
    /// target. This is waiting for a turn, not work.
    WaitingForWorkerStore,
    /// Queued behind another task opening the shared SSH connection to this
    /// host. This is waiting for a turn, not work.
    WaitingForSshConnection,
    /// Queued behind other children starting in the same container. This is
    /// waiting for a turn, not work, so it never counts as a stall.
    WaitingForStartSlot,
    Provisioning,
    Booting,
    Cloning,
    Syncing,
    Restoring,
    Starting,
    Installing(HarnessKind),
    PreparingHarness {
        harness: HarnessKind,
        step: String,
        since_ms: i64,
    },
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
    pub fn label(&self) -> String {
        match self {
            Self::PullingImage => "Pull image".into(),
            Self::WaitingForImagePull => "Waiting for another image pull".into(),
            Self::WaitingForContainerUpgrade => {
                "Container upgrade pending; waiting for accepted work".into()
            }
            Self::WaitingForWorkerStore => "Waiting for shared target content".into(),
            Self::WaitingForSshConnection => "Waiting for an SSH connection to open".into(),
            Self::WaitingForStartSlot => "Waiting for a start slot".into(),
            Self::Provisioning => "Provision".into(),
            Self::Booting => "Boot".into(),
            Self::Cloning => "Clone".into(),
            Self::Syncing => "Sync".into(),
            Self::Restoring => "Restore".into(),
            Self::Starting => "Start".into(),
            Self::Installing(harness) => format!("Installing {}", harness.display_name()),
            Self::PreparingHarness { harness, step, .. } => {
                format!("Preparing {}: {step}", harness.display_name())
            }
            Self::Compacting => "Compact".into(),
            Self::RecoveryCopy => "Recovery copy".into(),
            Self::Verifying => "Verify".into(),
            Self::Closing => "Shut down agent".into(),
            Self::StoppingTarget => "Stop target".into(),
            Self::RemovingContainer => "Remove container".into(),
            Self::RemovingStorage => "Remove container storage".into(),
            Self::CleaningCache => "Clean cache".into(),
        }
    }

    /// Whether time in this stage stays off a stall clock. Queue time is not
    /// progress: the stage waits for a turn behind other work, and admission
    /// ends it. Harness preparation reports its own progress and failure
    /// (the native startup deadline is likewise suspended during it), and one
    /// step, such as a download, can be slow without being stuck.
    pub fn pauses_stall_clock(&self) -> bool {
        matches!(
            self,
            Self::WaitingForImagePull
                | Self::WaitingForContainerUpgrade
                | Self::WaitingForWorkerStore
                | Self::WaitingForSshConnection
                | Self::WaitingForStartSlot
                | Self::PreparingHarness { .. }
        )
    }

    /// The worker owns the start time for harness preparation. Other stages
    /// start when the controller observes them.
    pub fn started_at_epoch_seconds(&self) -> Option<u64> {
        match self {
            Self::PreparingHarness { since_ms, .. } => {
                Some(u64::try_from((*since_ms).max(0) / 1_000).unwrap_or_default())
            }
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SensitiveCommandInput(Arc<Vec<u8>>);

impl std::fmt::Debug for SensitiveCommandInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

type StreamReplyObserver = dyn Fn(Option<&[u8]>) + Send + Sync;

/// A streamed command's stdout replies, after SSH framing has been removed.
/// End of an accepted stream is reported as `None` so input awaiting a reply
/// can finish even when the remote command exits early.
#[derive(Clone)]
struct StreamReply(Arc<StreamReplyObserver>);

impl PartialEq for StreamReply {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for StreamReply {}

impl std::fmt::Debug for StreamReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<stream reply observer>")
    }
}

#[derive(Clone)]
struct CommandObserver(Arc<dyn Fn(CommandActivity) + Send + Sync>);

impl CommandObserver {
    fn report(&self, activity: CommandActivity) {
        (self.0)(activity);
    }
}

/// Events from the process executor during one streamed command.
pub enum CommandActivity {
    StdinStarted,
    StdinBytes(usize),
    StdinFinished,
    StageStarted(ProvisionStage),
    StageFinished(ProvisionStage),
}

impl std::fmt::Debug for CommandObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CommandObserver")
    }
}

impl PartialEq for CommandObserver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CommandObserver {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    /// Override `argv[0]` when the child's login mode is encoded there rather
    /// than in an argument. This is executor-local and is not serialized.
    #[cfg(unix)]
    #[serde(skip)]
    pub(crate) arg0: Option<String>,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Ambient variables to remove before the child starts. Used for a probe
    /// that must not see the daemon's inherited credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_remove: Vec<String>,
    /// Replace ambient process variables with the explicitly supplied environment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_env: bool,
    /// Output carries credentials and must not be inspected by diagnostic
    /// observers. Executor-local, never persisted in command plans.
    #[serde(skip)]
    pub sensitive_output: bool,
    /// Log non-empty stderr at `WARN` even when the command exits 0.
    /// Executor-local, never persisted in command plans.
    #[serde(skip)]
    pub warn_on_stderr: bool,
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
    /// This command deliberately leaves a process running after it exits: it
    /// starts a detached worker, whose lifetime belongs to the session rather
    /// than to the launch. The executor still gives the command its own
    /// process group, and cancellation still signals that group, but the
    /// successful completion of a launch must not kill what it launched.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detaches: bool,
    /// Let child processes finish if the supervisor cancels this command. The
    /// caller must ensure children can complete without the command process.
    /// This is executor-local behavior and is not part of serialized plans.
    #[serde(skip)]
    preserve_children_on_cancel: bool,
    /// The SSH destination this command opens a connection to, when it does.
    /// Tagged commands retry a server refusal before authentication rather
    /// than reporting it as a command failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_destination: Option<String>,
    /// The shared SSH connection this command runs on as one session, when it
    /// does. Just before spawning, the executor leases a session on one of the
    /// connection's masters with [`SshSessions::lease`] and puts the options
    /// that bind the command to that master in front of its arguments (see
    /// [`CommandSpec::open_ssh_session`]). The arguments stored here never
    /// contain them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_session: Option<SshSessionRequest>,
    /// The session is a fail-fast probe: it is counted on a shard but never
    /// opens a master (see [`CommandSpec::ssh_probe_session`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ssh_session_probe: bool,
    /// The remote command line starts by printing [`SSH_START_MARKER`], so the
    /// reader of this command's stdout strips it and the command's start is
    /// known from it. Set only by [`CommandSpec::open_ssh_session`], on the
    /// command it returns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) ssh_start_marker: bool,
    /// Input that must reach the child without becoming part of its arguments,
    /// environment, serialized plan, or debug representation.
    #[serde(skip)]
    sensitive_stdin: Option<SensitiveCommandInput>,
    /// Keep the stdin pipe open after sending the initial input. On cancellation
    /// close it first so a cooperating command can clean up before being killed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_stdin_open: bool,
    #[serde(skip)]
    stream_reply: Option<StreamReply>,
    #[serde(skip)]
    observer: Option<CommandObserver>,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            #[cfg(unix)]
            arg0: None,
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            clear_env: false,
            sensitive_output: false,
            warn_on_stderr: false,
            cwd: None,
            purpose: String::new(),
            stage: None,
            parallel_group: None,
            creates_target: false,
            detaches: false,
            preserve_children_on_cancel: false,
            ssh_destination: None,
            ssh_session: None,
            ssh_session_probe: false,
            ssh_start_marker: false,
            sensitive_stdin: None,
            keep_stdin_open: false,
            stream_reply: None,
            observer: None,
        }
    }

    /// Observe streamed stdin and any stages reported while opening SSH.
    /// Callbacks may run on the writer thread and are local to this execution.
    pub fn observe(mut self, report: impl Fn(CommandActivity) + Send + Sync + 'static) -> Self {
        self.observer = Some(CommandObserver(Arc::new(report)));
        self
    }

    /// Start the program with a specific `argv[0]`.
    ///
    /// POSIX shells use `-l`, but C shells recognize login mode from a leading
    /// dash in `argv[0]` (for example, `-tcsh`). SSH does the same when it
    /// starts an account's login shell.
    #[cfg(unix)]
    pub fn arg0(mut self, arg0: impl Into<String>) -> Self {
        self.arg0 = Some(arg0.into());
        self
    }

    pub fn with_stream_reply(
        mut self,
        observer: impl Fn(Option<&[u8]>) + Send + Sync + 'static,
    ) -> Self {
        self.stream_reply = Some(StreamReply(Arc::new(observer)));
        self
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

    /// Log non-empty stderr at `WARN` when this command succeeds.
    pub fn warn_on_stderr(mut self) -> Self {
        self.warn_on_stderr = true;
        self
    }

    /// Record that this command opens an `ssh` connection to `destination`,
    /// which is the host (or `user@host`) argument, never an option value.
    pub fn ssh_destination(mut self, destination: impl Into<String>) -> Self {
        self.ssh_destination = Some(destination.into());
        self
    }

    /// Record that this command is an `ssh` or `scp` invocation that runs as
    /// one session on `ssh`'s shared connection. It also opens a connection
    /// to that destination when sharing is off, so a server's refusal of it
    /// is retried the same way.
    pub fn ssh_session(mut self, ssh: &SshTarget, class: SshTrafficClass) -> Self {
        self.ssh_destination = Some(ssh.destination.clone());
        self.ssh_session = Some(SshSessionRequest {
            target: ssh.clone(),
            class,
        });
        self
    }

    /// Record that this command is a fail-fast `ssh` probe, such as a target
    /// validation, that joins `ssh`'s shared connection without ever opening
    /// a master. Its short timeouts and keepalives belong to the probe alone:
    /// a master opened by it would impose them on every later session. The
    /// probe still uses one of the master's sessions while it runs, so the
    /// executor counts it in the session ledger like any other session.
    pub fn ssh_probe_session(mut self, ssh: &SshTarget) -> Self {
        self = self.ssh_session(ssh, SshTrafficClass::Control);
        self.ssh_session_probe = true;
        self
    }

    /// Lease the SSH session this command asks for and return the command as
    /// it must be spawned, with the lease that must outlive the child.
    ///
    /// An `ssh` command's remote line is prefixed to print [`SSH_START_MARKER`]
    /// first. The reader of its stdout must strip it and report the start to
    /// the lease's [`StartGate`] (see [`SessionCommand::gate`]); the executors
    /// and `spawn_session_proxy` do. A command without a session
    /// request is returned unchanged. `executor` runs the master check and,
    /// when needed, the opener. `walk` is the command's walk across the
    /// connection's shards: a command that retries after a refusal passes the
    /// same walk to each attempt (see [`SshRefusalRetry::walk`]).
    pub fn open_ssh_session(
        &self,
        executor: &dyn CommandExecutor,
        walk: &mut SshWalk,
    ) -> Result<SessionCommand<'_>> {
        self.lease_ssh_session(executor, walk, true)
    }

    /// [`Self::open_ssh_session`] for a command whose `ssh` is run by another
    /// program that reads its stdout as its own protocol, such as the shell
    /// wrapper Move gives `rsync --rsh`: no marker is added, and nothing
    /// reports the session's start, so it counts only as a reservation.
    pub fn open_ssh_session_without_marker(
        &self,
        executor: &dyn CommandExecutor,
        walk: &mut SshWalk,
    ) -> Result<SessionCommand<'_>> {
        self.lease_ssh_session(executor, walk, false)
    }

    fn lease_ssh_session(
        &self,
        executor: &dyn CommandExecutor,
        walk: &mut SshWalk,
        marker: bool,
    ) -> Result<SessionCommand<'_>> {
        let Some(request) = &self.ssh_session else {
            return Ok(SessionCommand {
                command: std::borrow::Cow::Borrowed(self),
                lease: None,
            });
        };
        let ssh = &request.target;
        let _waiting = BlockingOperation::start(
            &format!(
                "lease SSH connection to {} for {}",
                ssh.destination, self.purpose
            ),
            "ssh",
        );
        let lease = if self.ssh_session_probe {
            SshSessions::lease_probe(ssh, walk)
        } else {
            let reporting = SshStageReporter {
                executor,
                observer: self.observer.as_ref(),
            };
            SshSessions::lease(ssh, request.class, &reporting, walk)?
        };
        let mut command = self.clone();
        command.ssh_session = None;
        command.ssh_session_probe = false;
        command.args = session_command_args(&self.program, &self.args, ssh, &lease);
        if self.keep_stdin_open && self.program == "ssh" {
            ssh::configure_stdin_lifetime(&mut command.args);
        }
        // The remote command line is the last argument of every `ssh` session
        // the builders in `targets/ssh.rs` make; `scp` has none.
        if marker
            && self.program == "ssh"
            && let Some(line) = command.args.last_mut()
        {
            *line = with_start_marker(line);
            command.ssh_start_marker = true;
        }
        Ok(SessionCommand {
            command: std::borrow::Cow::Owned(command),
            lease: Some(lease),
        })
    }

    /// Retain the stdin pipe as a lifetime signal after any initial input.
    pub fn keep_stdin_open(mut self) -> Self {
        self.keep_stdin_open = true;
        self
    }

    /// Whether the command's stdout starts with [`SSH_START_MARKER`].
    pub fn expects_ssh_start_marker(&self) -> bool {
        self.ssh_start_marker
    }

    /// Mark this command as the one that creates the session's target.
    pub fn creates_target(mut self) -> Self {
        self.creates_target = true;
        self
    }

    /// Keep child processes running if the supervising executor stops waiting.
    pub fn preserve_children_on_cancel(mut self) -> Self {
        self.preserve_children_on_cancel = true;
        self
    }

    /// Feed private file content through the shared concurrent pipe handler.
    /// The bytes stay out of argv, environments, serialization, and Debug.
    pub fn with_sensitive_stdin(mut self, input: Vec<u8>) -> Self {
        self.sensitive_stdin = Some(SensitiveCommandInput(Arc::new(input)));
        self
    }

    /// The command-owned input, for executors and test fakes.
    pub fn sensitive_stdin(&self) -> Option<&[u8]> {
        self.sensitive_stdin
            .as_ref()
            .map(|input| input.0.as_slice())
    }
}

/// A stand-in `ssh` for tests: a script on `PATH` that refuses the first
/// attempts as a plan says and otherwise runs the remote command line in a
/// shell, as the remote side would, so the start marker is printed by the
/// real line.
#[cfg(all(test, unix))]
pub(crate) mod fake_ssh {
    use super::*;
    use std::fs;

    static FAST_RETRIES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) struct FastRetries(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl FastRetries {
        pub(crate) fn new() -> Self {
            let guard = FAST_RETRIES_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            set_ssh_retry_backoff_for_test(Some(Duration::from_millis(5)));
            Self(guard)
        }
    }

    impl Drop for FastRetries {
        fn drop(&mut self) {
            set_ssh_retry_backoff_for_test(None);
            set_ssh_persistent_refusal_for_test(None);
        }
    }

    pub(crate) struct FakeSsh {
        directory: tempfile::TempDir,
    }

    impl FakeSsh {
        /// `plan` has one word per attempt: `refuse-session`, `refuse-auth`,
        /// `refuse-session-late-stderr` (closes stdout first and prints the
        /// refusal to stderr moments later), `warn-then-run` (prints an ssh
        /// warning to stderr before the session starts, then runs), or
        /// `run-then-255` (runs the command, then fails like a dropped
        /// connection). Attempts past the plan just run.
        pub(crate) fn new(plan: &[&str]) -> Self {
            let directory = tempfile::tempdir().expect("fake ssh directory");
            let script = r#"#!/bin/sh
dir=$(dirname "$0")
count=$(cat "$dir/attempts" 2>/dev/null || echo 0)
echo $((count + 1)) > "$dir/attempts"
for last; do :; done
case "$(sed -n "$((count + 1))p" "$dir/plan")" in
  refuse-session)
    echo 'mux_client_request_session: session request failed: Session open refused by peer' >&2
    exit 255 ;;
  refuse-session-late-stderr)
    exec >&-
    sleep 0.3
    echo 'mux_client_request_session: session request failed: Session open refused by peer' >&2
    exit 255 ;;
  refuse-auth)
    echo 'kex_exchange_identification: Connection closed by 10.0.0.1 port 22' >&2
    exit 255 ;;
  warn-then-run)
    echo "Warning: Permanently added 'fake-host' to the list of known hosts." >&2
    sleep 0.3
    exec sh -c "$last" ;;
  run-then-255)
    sh -c "$last"
    echo 'Read from remote host fake-host: Connection reset by peer' >&2
    exit 255 ;;
  *) exec sh -c "$last" ;;
esac
"#;
            let path = directory.path().join("ssh");
            fs::write(&path, script).unwrap();
            fs::write(directory.path().join("plan"), plan.join("\n")).unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            Self { directory }
        }

        /// An `ssh` session command for `remote` on the fake host.
        pub(crate) fn command(&self, remote: &[&str]) -> CommandSpec {
            let ssh = SshTarget {
                destination: "fake-host".to_owned(),
                ssh_args: Vec::new(),
            };
            let mut command = ssh_command(SshTrafficClass::Control, &ssh, remote)
                .purpose("run through the fake ssh");
            let path = std::env::var("PATH").unwrap_or_default();
            command.env.insert(
                "PATH".to_owned(),
                format!("{}:{path}", self.directory.path().display()),
            );
            command
        }

        pub(crate) fn attempts(&self) -> u32 {
            fs::read_to_string(self.directory.path().join("attempts"))
                .map(|text| text.trim().parse().expect("attempt count"))
                .unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod podman_exec_tests {
    use super::*;

    #[test]
    fn podman_exec_keeps_the_container_configured_user() {
        assert_eq!(
            CommandSpec::new("podman", ["exec", "--detach", "session", "true"]).args,
            ["exec", "--detach", "session", "true"]
        );
        assert_eq!(
            CommandSpec::new("podman", ["exec", "--user", "1000:1000", "session", "id"]).args,
            ["exec", "--user", "1000:1000", "session", "id"]
        );
        assert_eq!(
            CommandSpec::new("podman", ["exec", "session", "tool", "--user", "42"]).args,
            ["exec", "session", "tool", "--user", "42"]
        );
        assert_eq!(
            CommandSpec::new("docker", ["exec", "session", "id"]).args,
            ["exec", "session", "id"]
        );
    }
}

/// A command ready to spawn, together with the SSH session lease it runs on.
/// Keep this value alive until the child has exited: dropping it frees the
/// session slot.
#[derive(Debug)]
pub struct SessionCommand<'a> {
    command: std::borrow::Cow<'a, CommandSpec>,
    lease: Option<SshSessionLease>,
}

impl SessionCommand<'_> {
    pub fn command(&self) -> &CommandSpec {
        &self.command
    }

    pub fn lease(&self) -> Option<&SshSessionLease> {
        self.lease.as_ref()
    }

    /// Where the session stands, when its command prints the start marker:
    /// the reader of its stdout reports into it.
    pub fn gate(&self) -> Option<&Arc<StartGate>> {
        self.command
            .ssh_start_marker
            .then(|| self.lease.as_ref().map(SshSessionLease::gate))
            .flatten()
    }

    pub fn into_parts(self) -> (CommandSpec, Option<SshSessionLease>) {
        (self.command.into_owned(), self.lease)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// The local host has none: sysinfo reads its CPU and memory, and its
    /// storage is measured over `local_storage_paths`.
    pub probes: Vec<CommandSpec>,
    /// The paths on this machine whose filesystems local targets write to.
    /// Empty for every other host, whose probes carry their own paths.
    pub local_storage_paths: Vec<String>,
    pub storage_warning: Option<String>,
    /// Prevents a partial AWS fleet sample when one live instance cannot be probed yet.
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMemoryUsage {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentCapacityUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ec2_instance_type: Option<String>,
    /// Physical RAM when the measured allocation budget is a separate pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_memory: Option<HostMemoryUsage>,
    /// Whether `memory_total_bytes` is a pool set aside for sessions at boot
    /// (a Nitro host's sandbox memory) rather than the host's RAM. A pool is
    /// already net of the host's own needs, so admission applies no reserve
    /// to it.
    pub memory_pool: bool,
    pub cpu_percent: Option<u8>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub logical_cores: u64,
    pub disk_total_bytes: Option<u64>,
    /// Free space on the filesystems Mjolnir writes to, per machine probed:
    /// one for a host, one per instance for a fleet.
    pub storage: Vec<storage::HostStorageSample>,
}

impl DeploymentCapacityUsage {
    pub fn host_memory(&self) -> HostMemoryUsage {
        self.physical_memory.unwrap_or(HostMemoryUsage {
            used_bytes: self.memory_used_bytes,
            total_bytes: self.memory_total_bytes,
        })
    }
}

/// An additional directory made available to one session.
///
/// Containers use isolated mounts. Remote targets may instead receive a
/// controller-packed snapshot at the destination while retaining this shared
/// persisted shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "AdditionalMountRepr", into = "AdditionalMountRepr")]
pub struct AdditionalMount {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub access: MountAccess,
}

/// How a container sees an attached directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAccess {
    /// The container can read the directory but not change it.
    Ro,
    /// The container writes to a private copy-on-write overlay; the host
    /// directory never changes.
    Cow,
    /// The container writes straight through to the host directory.
    Rw,
}

impl MountAccess {
    pub const ALL: [Self; 3] = [Self::Ro, Self::Cow, Self::Rw];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Cow => "cow",
            Self::Rw => "rw",
        }
    }

    /// The mode this attachment takes where the host filesystem cannot carry
    /// a copy-on-write overlay. Read-only is the substitute, because the
    /// alternative would let the container write through to the host
    /// directory the user asked to keep unchanged.
    ///
    /// This is the one rule for an unavailable overlay: the wizard offers the
    /// same modes [`Self::offered`] lists, and the runtime downgrades a stored
    /// mount the same way.
    pub fn without_overlay(self) -> Self {
        match self {
            Self::Cow => Self::Ro,
            kept => kept,
        }
    }

    /// The modes an attachment may be given, given whether the host filesystem
    /// can carry the copy-on-write overlay.
    pub fn offered(overlay_available: bool) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|access| overlay_available || access.without_overlay() == *access)
            .collect()
    }
}

/// The persisted shape. Read-only and copy-on-write mounts keep the original
/// `read_only` boolean alone, so archives and API payloads that older builds
/// read stay unchanged; only read-write adds `access`, which older builds
/// cannot represent.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdditionalMountRepr {
    source: PathBuf,
    destination: PathBuf,
    #[serde(default)]
    read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<MountAccess>,
}

impl From<AdditionalMountRepr> for AdditionalMount {
    fn from(repr: AdditionalMountRepr) -> Self {
        let access = repr.access.unwrap_or(if repr.read_only {
            MountAccess::Ro
        } else {
            MountAccess::Cow
        });
        Self {
            source: repr.source,
            destination: repr.destination,
            access,
        }
    }
}

impl From<AdditionalMount> for AdditionalMountRepr {
    fn from(mount: AdditionalMount) -> Self {
        Self {
            source: mount.source,
            destination: mount.destination,
            read_only: mount.access == MountAccess::Ro,
            access: (mount.access == MountAccess::Rw).then_some(MountAccess::Rw),
        }
    }
}

/// Why a filesystem cannot host a container target's copy-on-write overlay,
/// or `None` when it can. Unknown types are allowed: the overlay is the better
/// mount and only a filesystem known to break it is downgraded.
///
/// The names are those Linux `stat -f -c %T` and macOS `mount` report,
/// matched case-insensitively.
pub fn overlay_unsupported_filesystem(filesystem: &str) -> Option<&'static str> {
    let name = filesystem.trim().to_ascii_lowercase();
    // FUSE reports the backing driver as `fuse.sshfs`, `fuse.s3fs`, and so on;
    // macFUSE reports itself.
    if name == "fuse"
        || name == "fuseblk"
        || name.starts_with("fuse.")
        || matches!(name.as_str(), "macfuse" | "osxfuse")
    {
        return Some("FUSE filesystem");
    }
    match name.as_str() {
        "nfs" | "nfs4" | "cifs" | "smb2" | "smb3" | "9p" | "v9fs" | "virtiofs" | "ceph"
        | "lustre" | "afs" | "glusterfs" | "ocfs2" | "gfs" | "gfs2" | "smbfs" | "afpfs"
        | "webdav" => Some("network filesystem"),
        "msdos" | "vfat" | "fat" | "exfat" | "ntfs" | "ntfs3" => Some("no POSIX metadata"),
        "overlayfs" => Some("overlay stacking limit"),
        _ => None,
    }
}

/// Container destinations cannot use the controller or login user's home.
pub fn validate_mount_destination(path: &Path) -> Result<()> {
    ensure!(
        crate::target_path::is_absolute(path)
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
        if !crate::target_path::is_absolute_on_host_or_target(&mount.source)
            || mount.source.as_os_str().is_empty()
        {
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
    // A container path: POSIX separators on a Windows controller too.
    let mnt = Path::new("/mnt");
    let base = crate::target_path::join(mnt, Path::new(basename));
    if !existing.iter().any(|mount| mount.destination == base) {
        return base;
    }
    for number in 2.. {
        let candidate = crate::target_path::join(
            mnt,
            Path::new(&format!("{}-{number}", basename.to_string_lossy())),
        );
        if !existing.iter().any(|mount| mount.destination == candidate) {
            return candidate;
        }
    }
    unreachable!("a finite mount list always has an unused numbered destination")
}

pub trait CommandExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput>;

    /// Bound cleanup attempts while preserving supervisor cancellation. Durable
    /// owners retry interrupted cleanup under the next lifecycle or daemon.
    fn execute_cleanup(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.execute(command)
    }

    /// Bound one cleanup attempt by `timeout`. Executors with shared
    /// cancellation continue to honor it, and executor-specific hard limits
    /// may cap the requested timeout. Executors without per-call deadlines
    /// retain their normal cleanup behavior.
    fn execute_cleanup_with_timeout(
        &self,
        command: &CommandSpec,
        _timeout: Duration,
    ) -> Result<CommandOutput> {
        self.execute_cleanup(command)
    }

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

    /// Stop workers sharing the environment before its owner quiesces.
    fn stop_borrowed_workers_before_quiesce(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    /// The Move source has been sealed; its destination belongs exclusively
    /// to the lifecycle until queue admission completes.
    fn reserve_move_destination(&self) {}

    /// Yield daemon admission for interruptible work recoverable after handoff.
    fn begin_resumable_work(&self) -> Result<()> {
        Ok(())
    }
    /// Release daemon admission, if this operation holds it and the next
    /// daemon restarts the operation from a durable record. Returns whether
    /// admission was released; the caller then owes
    /// [`Self::end_resumable_work`]. For a wait that can take minutes, such as
    /// shared target content, so a handoff does not wait for it.
    fn yield_admission_if_resumable(&self) -> bool {
        false
    }
    /// The daemon is yielding a durable operation to its replacement.
    fn shutdown_interrupted(&self) -> bool {
        false
    }
    /// Reacquire named control admission before leaving resumable work.
    fn end_resumable_work(&self, _label: &'static str) -> Result<()> {
        Ok(())
    }

    fn execute_with_stdin(
        &self,
        _command: &CommandSpec,
        _input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        bail!("this command executor does not support streamed stdin")
    }

    /// Run `command` with this executor's cancellation but without its
    /// deadline, for work a timeout named for another step must not cover.
    /// An executor without a deadline runs it as `execute` does; one with a
    /// deadline drops it and keeps its cancellation. Required, so every
    /// executor states which it is.
    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput>;

    /// [`Self::execute_without_deadline`] for a command that reads `input`.
    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput>;

    /// [`Self::cancellation_requested`] as the no-deadline methods see it: a
    /// deadline that has passed is not a cancellation.
    fn cancellation_requested_without_deadline(&self) -> bool;

    /// Exclude this scope's wall time from the operation deadline. Required:
    /// wrappers forward to their owner; executors with per-command deadlines
    /// or no deadline return an empty guard. Cancellation remains active.
    fn pause_deadline(&self) -> DeadlinePause;
}

/// Forward SSH admission stages through the command's owner while the process
/// executor retains cancellation and executes the actual master commands.
struct SshStageReporter<'a> {
    executor: &'a dyn CommandExecutor,
    observer: Option<&'a CommandObserver>,
}

impl CommandExecutor for SshStageReporter<'_> {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.executor.execute(command)
    }

    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.executor.execute_without_deadline(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        self.executor
            .execute_with_stdin_without_deadline(command, input)
    }

    fn cancellation_requested(&self) -> bool {
        self.executor.cancellation_requested()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.executor.cancellation_requested_without_deadline()
    }

    fn pause_deadline(&self) -> DeadlinePause {
        self.executor.pause_deadline()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        if let Some(observer) = self.observer {
            observer.report(CommandActivity::StageStarted(stage));
        } else {
            self.executor.stage_started(stage);
        }
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        if let Some(observer) = self.observer {
            observer.report(CommandActivity::StageFinished(stage));
        } else {
            self.executor.stage_finished(stage);
        }
    }
}

/// An executor whose commands run without the wrapped executor's deadline but
/// still stop when its supervisor cancels. Staging work that precedes a
/// timeout-bound command runs on this, so the timeout covers only the command
/// it names. Its lifetime also pauses that deadline, including gate waits and
/// other staging work that runs no command.
pub struct WithoutDeadline<'a, E: ?Sized> {
    executor: &'a E,
    _pause: DeadlinePause,
}

impl<'a, E: CommandExecutor + ?Sized> WithoutDeadline<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self {
            executor,
            _pause: executor.pause_deadline(),
        }
    }
}

impl<E: CommandExecutor + ?Sized> CommandExecutor for WithoutDeadline<'_, E> {
    fn pause_deadline(&self) -> DeadlinePause {
        self.executor.pause_deadline()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.executor.cancellation_requested_without_deadline()
    }

    fn execute_without_deadline(
        &self,
        command: &crate::targets::CommandSpec,
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &crate::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.executor.execute_without_deadline(command)
    }

    fn execute_cleanup(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.executor.execute_cleanup(command)
    }

    fn execute_cleanup_with_timeout(
        &self,
        command: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        self.executor.execute_cleanup_with_timeout(command, timeout)
    }

    // The wrapper's whole purpose is to ignore the wrapped deadline, so a
    // passed deadline must not read as a cancellation here either.
    fn cancellation_requested(&self) -> bool {
        self.executor.cancellation_requested_without_deadline()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        self.executor.stage_started(stage);
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        self.executor.stage_finished(stage);
    }

    fn notify_notice(&self, notice: &str) {
        self.executor.notify_notice(notice);
    }

    fn begin_resumable_work(&self) -> Result<()> {
        self.executor.begin_resumable_work()
    }

    fn yield_admission_if_resumable(&self) -> bool {
        self.executor.yield_admission_if_resumable()
    }

    fn shutdown_interrupted(&self) -> bool {
        self.executor.shutdown_interrupted()
    }

    fn end_resumable_work(&self, label: &'static str) -> Result<()> {
        self.executor.end_resumable_work(label)
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        self.executor
            .execute_with_stdin_without_deadline(command, input)
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
        executor.stage_started(stage.clone());
        Self { executor, stage }
    }
}

impl<E: CommandExecutor + ?Sized> Drop for ProvisionStageGuard<'_, E> {
    fn drop(&mut self) {
        self.executor.stage_finished(self.stage.clone());
    }
}

pub struct ProcessExecutor;

/// Run one `ssh` invocation, retrying it
/// when the server turned it away before the remote command started.
///
/// A refusal means the remote command never started (no byte of its stdout,
/// not even the start marker, ever arrived), so re-running the whole
/// invocation cannot repeat a side effect, whether or not the command
/// streams stdin: nothing of the caller's input is read before the start.
/// [`SshRefusalRetry`] decides each wait and when to stop: a refusal that
/// persists for its time bound fails the command with an error naming the
/// causes, for every caller; the caller's cancellation or deadline ends the
/// retries sooner.
///
/// A command that asks for a shared-connection session leases it on each
/// attempt, and the lease tells the ledger what the refusal means
/// ([`SshSessionLease::note_refusal`]). A session turned away by the transport
/// usually means its master died, so the retry checks and reopens the master.
/// A session refused for `MaxSessions` moves to the next shard of the
/// command's walk at once: the walk is the state change that ends that loop,
/// so it needs no wait and spends no time.
///
/// `run` receives the leased command and the gate its stdout reader must
/// report into (`None` when the command prints no start marker).
///
/// Commands that are not tagged with a destination run untouched.
fn with_ssh_refusal_retry(
    command: &CommandSpec,
    executor: &dyn CommandExecutor,
    is_cancelled: Option<&dyn Fn() -> bool>,
    mut run: impl FnMut(&CommandSpec, Option<&Arc<StartGate>>) -> Result<CommandOutput>,
) -> Result<CommandOutput> {
    let Some(destination) = command.ssh_destination.as_deref() else {
        return run(command, None);
    };
    let mut retry = SshRefusalRetry::for_command(command);
    loop {
        let session = command.open_ssh_session(executor, retry.walk())?;
        let output = run(session.command(), session.gate())?;
        let refusal = note_session_refusal(&session, &output);
        drop(session);
        let Some((refusal, walks_on)) = refusal else {
            return Ok(output);
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        let delay = retry
            .next(refusal, walks_on, destination, &command.purpose, &stderr)
            .with_context(|| format!("{} failed", command.purpose))?;
        if !sleep_unless_cancelled(delay, is_cancelled.unwrap_or(&|| false)) {
            bail!("operation cancelled while {}", command.purpose);
        }
    }
}

/// Classify a finished command's refusal by the server and tell the session's
/// lease what it means. Returns the refusal and whether retrying at once can
/// land elsewhere (see [`SshSessionLease::note_refusal`]). A command that
/// printed the start marker was not refused, whatever its exit status.
fn note_session_refusal(
    session: &SessionCommand<'_>,
    output: &CommandOutput,
) -> Option<(SshRefusal, bool)> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `scp` exits with a status of its own (normally 1) while relaying the
    // refusal of the `ssh` it runs, and carries no start marker, so only that
    // stderr can tell. Copying again is safe, as with Move's rsync.
    let is_scp = session.command().program == "scp";
    let Some(lease) = session.lease() else {
        let refusal = if is_scp {
            ssh_refusal_in_stderr(&stderr)
        } else {
            ssh_refusal(output.status, &stderr)
        };
        return refusal.map(|refusal| (refusal, false));
    };
    let refusal = if is_scp {
        if output.status == 0 {
            return None;
        }
        ssh_refusal_in_stderr(&stderr)?
    } else {
        lease.gate().refusal(output.status, &stderr)?
    };
    Some((refusal, lease.note_refusal(refusal)))
}

/// Wait out `delay`, giving up early if the supervising operation is
/// cancelled. Returns whether the wait completed.
pub fn sleep_unless_cancelled(delay: Duration, is_cancelled: &dyn Fn() -> bool) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if is_cancelled() {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

/// Every process executor finishes a command here: one debug line, so a slow
/// launch or resume phase can be attributed from logs instead of re-profiled
/// by hand, and one look at a failure for a full disk on the target.
fn finished_command(
    command: &CommandSpec,
    started: Instant,
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> CommandOutput {
    tracing::debug!(
        purpose = command.purpose.as_str(),
        program = command.program.as_str(),
        status,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "target command finished"
    );
    let output = CommandOutput {
        status,
        stdout,
        stderr,
    };
    if !command.sensitive_output {
        storage::observe_command_output(command, &output);
    }
    output
}

impl ProcessExecutor {
    /// One attempt, with no retry of its own.
    fn run_once(
        &self,
        command: &CommandSpec,
        gate: Option<&Arc<StartGate>>,
    ) -> Result<CommandOutput> {
        // One path for both executors: the child gets its own process group,
        // output is drained for a bounded time after it exits, and leftover
        // descendants are killed. With no cancellation it only ends when the
        // child does.
        CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false))).run_once(command, gate)
    }
}

// Both process executors wait for their child, and for the SSH session lease
// before it, through `off_async_worker`: a caller several calls below an `async fn`
// must not hold one of the runtime's async worker threads for the length of a
// target command.
impl CommandExecutor for ProcessExecutor {
    fn pause_deadline(&self) -> DeadlinePause {
        DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &crate::targets::CommandSpec,
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &crate::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        crate::runtime::off_async_worker(|| {
            let _running = BlockingOperation::command(command);
            with_ssh_refusal_retry(command, self, None, |command, gate| {
                self.run_once(command, gate)
            })
        })
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        crate::runtime::off_async_worker(|| {
            let _running = BlockingOperation::command(command);
            // The stream is not read until the session has started, so a
            // refused session is retried with the same stream. Plain process
            // execution is not cancellable, so a transfer only ends when the
            // child does.
            with_ssh_refusal_retry(command, self, None, |command, gate| {
                stream_command_with_stdin(
                    cancellable_command(command),
                    command,
                    &mut *input,
                    &|| false,
                    None,
                    gate,
                    None,
                )
            })
        })
    }
}

/// Observe bytes as a supervised command drains them. The callback may run
/// concurrently for stdout and stderr, and must finish without blocking.
pub type CommandOutputObserver = Arc<dyn Fn(&[u8]) + Send + Sync>;

struct ObservedStdin<'a>(&'a CommandObserver);

impl Drop for ObservedStdin<'_> {
    fn drop(&mut self) {
        self.0.report(CommandActivity::StdinFinished);
    }
}

struct ProgressWriter<'a> {
    stdin: std::process::ChildStdin,
    observer: Option<&'a CommandObserver>,
}

impl Write for ProgressWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(observer) = self.observer else {
            return self.stdin.write(bytes);
        };
        // A blocking pipe write can wait for the whole request. Use smaller
        // writes on observed streams so backpressure exposes progress before
        // a full input buffer has drained.
        let written = self.stdin.write(&bytes[..bytes.len().min(4096)])?;
        if written != 0 {
            observer.report(CommandActivity::StdinBytes(written));
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stdin.flush()
    }
}

// Give a cooperating remote command time to stop its group and clean up
// after SSH forwards EOF, even when the transport itself is unresponsive.
const REMOTE_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Streams `input` into a freshly spawned child and collects its output.
///
/// Both executors share this one implementation because the pipe edge cases
/// below are easy to get subtly wrong in a second copy.
///
/// `is_cancelled` reports whether the supervising operation wants the transfer
/// abandoned; [`ProcessExecutor`] passes a check that is never true, which also
/// makes the kill path below unreachable for it.
///
/// With a `gate`, the command prints a start marker that is stripped from the
/// collected stdout, and `input` is not read, nor anything written to the
/// child, until the marker arrives. A session the server refused therefore
/// leaves `input` untouched for the caller to retry with.
fn stream_command_with_stdin(
    mut process: Command,
    command: &CommandSpec,
    input: &mut (dyn Read + Send),
    is_cancelled: &(dyn Fn() -> bool + Sync),
    abort_stdin: Option<&(dyn Fn() + Sync)>,
    gate: Option<&Arc<StartGate>>,
    observer: Option<CommandOutputObserver>,
) -> Result<CommandOutput> {
    let started = Instant::now();
    if is_cancelled() {
        bail!("operation cancelled");
    }
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::subprocess::spawn_child(&mut process)
        .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
    let group =
        (!command.detaches).then(|| crate::subprocess::ProcessGroupGuard::new(Some(child.id())));
    let exit_watch = crate::subprocess::ExitWatch::new(&child);
    // Each timed wait says whether the child may have exited, so nothing
    // polls the pidfd twice; the first pass checks once.
    let mut maybe_exited = true;
    let stdin = child
        .stdin
        .take()
        .context("streamed command stdin missing")?;
    let stdout = child
        .stdout
        .take()
        .context("streamed command stdout missing")?;
    let stderr = child
        .stderr
        .take()
        .context("streamed command stderr missing")?;
    // Reader threads keep the child's output pipes drained; a child that fills
    // one while nobody reads would block instead of exiting.
    let reply = command.stream_reply.as_ref();
    let reply_observer = reply.cloned().map(|reply| {
        let observer = observer.clone();
        Arc::new(move |bytes: &[u8]| {
            if !bytes.is_empty() {
                (reply.0)(Some(bytes));
            }
            if let Some(observer) = &observer {
                observer(bytes);
            }
        }) as CommandOutputObserver
    });
    let mut reply_ended = false;
    let mut end_reply = || {
        if !reply_ended
            && let Some(reply) = reply
            && gate.is_none_or(|gate| gate.phase() == crate::targets::ssh::StartPhase::Started)
        {
            reply_ended = true;
            (reply.0)(None);
        }
    };
    let mut stdout_reader = PipeCollector::spawn_observed(
        stdout,
        gate.cloned(),
        None,
        reply_observer.or_else(|| observer.clone()),
    );
    let mut stderr_reader = PipeCollector::spawn_observed(stderr, None, None, observer);
    let process_result = std::thread::scope(|scope| -> Result<_> {
        // When `input.read` fails, keep `stdin` open until the supervisor has
        // terminated the child process group. Otherwise dropping `stdin` would
        // deliver a clean EOF and let a length-less child (such as chunked
        // `hel worker write-file`) publish a truncated file.
        let input_failed = Arc::new(AtomicBool::new(false));
        let child_done = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let input_failed_writer = input_failed.clone();
        let child_done_writer = child_done.clone();
        let wait_for_child_done = move || {
            let (lock, cvar) = &*child_done_writer;
            let done = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(
                cvar.wait_while(done, |done| !*done)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        };
        // Pipe writes can block forever when a remote helper stops reading.
        // Keep the writer off the supervising thread so cancellation can kill
        // the process group and thereby close the blocked pipe.
        let input_writer = scope.spawn(move || -> Result<bool> {
            if let Some(gate) = gate {
                match gate.wait_blocking(is_cancelled) {
                    StartWait::Started => {}
                    // The session ended without starting. Nothing was read,
                    // and the exit status says why.
                    StartWait::Ended => return Ok(false),
                    StartWait::Cancelled => bail!("operation cancelled"),
                }
            }
            // The command stage owns SSH startup and post-EOF work. Start
            // the transfer clock once the writer can send and close stdin
            // before dropping the progress guard.
            let progress = command.observer.as_ref().map(|observer| {
                observer.report(CommandActivity::StdinStarted);
                ObservedStdin(observer)
            });
            let mut stdin = ProgressWriter {
                stdin,
                observer: command.observer.as_ref(),
            };
            // Owning `stdin` here is what closes the pipe's write end once the
            // transfer finishes. A child that reads to EOF, such as
            // `mj worker export-checkpoint --spec -`, never exits while any
            // copy of the write end is still open.
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                // Checking before each chunk makes large checkpoint copies
                // cooperatively cancellable without changing the executor
                // interface.
                if is_cancelled() {
                    bail!("operation cancelled");
                }
                let count = match input.read(&mut buffer) {
                    Ok(count) => count,
                    Err(error) => {
                        input_failed_writer.store(true, Ordering::Release);
                        wait_for_child_done();
                        return Err(error).context("read command input");
                    }
                };
                if count == 0 {
                    break;
                }
                stdin
                    .write_all(&buffer[..count])
                    .context("stream command input")?;
            }
            stdin.flush().context("flush command input")?;
            if command.keep_stdin_open {
                drop(progress);
                wait_for_child_done();
            }
            Ok(true)
        });
        struct ChildDoneGuard<'a>(&'a (std::sync::Mutex<bool>, std::sync::Condvar));
        impl Drop for ChildDoneGuard<'_> {
            fn drop(&mut self) {
                let mut done = self
                    .0
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *done = true;
                self.0.1.notify_all();
            }
        }
        let child_done_guard = ChildDoneGuard(&child_done);
        let mark_child_done = || {
            let mut done = child_done
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *done = true;
            child_done.1.notify_all();
        };
        let mut status = None;
        let mut exited_at = None;
        let mut group_killed = false;
        let status = loop {
            if status.is_none() && (stdout_reader.failed() || stderr_reader.failed()) {
                terminate_cancellable_child(&mut child, false);
                status = child
                    .try_wait()
                    .context("reap command after pipe failure")?;
            }
            if is_cancelled() {
                let close_stdin_first = command.keep_stdin_open
                    && gate.is_none_or(|gate| gate.phase() == ssh::StartPhase::Started);
                if !close_stdin_first {
                    terminate_cancellable_child(&mut child, false);
                }
                mark_child_done();
                if let Some(abort) = abort_stdin {
                    abort();
                }
                if close_stdin_first {
                    let deadline = Instant::now() + REMOTE_CANCEL_GRACE;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {}
                            Err(error) => {
                                tracing::warn!(%error, purpose = command.purpose, "wait for command after closing stdin");
                                terminate_cancellable_child(&mut child, false);
                                break;
                            }
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            terminate_cancellable_child(&mut child, false);
                            break;
                        }
                        exit_watch.wait(remaining.min(Duration::from_millis(25)));
                    }
                }
                end_reply();
                if let Err(error) = input_writer.join() {
                    tracing::warn!(
                        purpose = command.purpose.as_str(),
                        "streamed command input writer panicked while cancelling: {error:?}"
                    );
                }
                bail!("operation cancelled while {}", command.purpose);
            }
            if status.is_none() && input_failed.load(Ordering::Acquire) {
                terminate_cancellable_child(&mut child, false);
                mark_child_done();
                end_reply();
                let input_result = input_writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("streamed command input writer panicked"))?;
                input_result?;
                bail!("streamed command input failed while {}", command.purpose);
            }
            if stdout_reader.is_finished() {
                end_reply();
            }
            match if status.is_some() || !maybe_exited {
                Ok(status)
            } else {
                child.try_wait()
            } {
                Ok(observed) => status = observed,
                Err(error) => {
                    terminate_cancellable_child(&mut child, false);
                    mark_child_done();
                    if let Some(abort) = abort_stdin {
                        abort();
                    }
                    end_reply();
                    if let Err(join_error) = input_writer.join() {
                        tracing::warn!(
                            purpose = command.purpose.as_str(),
                            "streamed command input writer panicked while waiting: {join_error:?}"
                        );
                    }
                    return Err(error).with_context(|| format!("wait for {}", command.purpose));
                }
            }
            // The command completes when its leader exits. Descendants can
            // retain the pipes, so output is collected for a bounded time;
            // then the owned group is terminated, which also releases a
            // writer blocked on a pipe a descendant holds.
            if let Some(status) = status {
                mark_child_done();
                let exited_at = *exited_at.get_or_insert_with(Instant::now);
                let drained = (stdout_reader.is_finished() && stderr_reader.is_finished())
                    || exited_at.elapsed() >= IO_DRAIN_TIMEOUT;
                if drained && !input_writer.is_finished() && !group_killed {
                    end_reply();
                    group_killed = true;
                    if let Some(group) = &group {
                        group.kill();
                    }
                    // Unblock an input reader once stdin streaming has actually
                    // started; a refused SSH attempt before the start marker
                    // must leave `input` untouched for retry.
                    if gate
                        .is_none_or(|gate| gate.phase() == crate::targets::ssh::StartPhase::Started)
                        && let Some(abort) = abort_stdin
                    {
                        abort();
                    }
                }
                if drained && input_writer.is_finished() {
                    break status;
                }
            }
            // A reaped child's pidfd stays readable; sleep while draining.
            maybe_exited = if status.is_some() {
                std::thread::sleep(Duration::from_millis(25));
                true
            } else {
                exit_watch.wait(Duration::from_millis(25))
            };
        };
        drop(child_done_guard);
        let input_result = input_writer
            .join()
            .map_err(|_| anyhow::anyhow!("streamed command input writer panicked"))?;
        Ok((status, input_result))
    });
    let deadline = Instant::now();
    let stdout = stdout_reader.finish("stdout", deadline)?;
    let stderr = stderr_reader.finish("stderr", deadline)?;
    let (status, input_result) = process_result?;
    drop(group);
    if status.success() {
        // A child that exited first explains the failure through its own
        // status and stderr; the broken pipe that exit caused would only hide
        // it. A successful child must not hide an input error.
        let delivered = input_result?;
        ensure!(
            delivered,
            "{} exited successfully without reporting that its command started, so its input was never sent",
            command.purpose
        );
    }
    let status = status.code().unwrap_or(-1);
    Ok(finished_command(command, started, status, stdout, stderr))
}

#[derive(Clone)]
pub struct CancellableProcessExecutor {
    cancelled: Arc<AtomicBool>,
    token: Option<tokio_util::sync::CancellationToken>,
    #[cfg(unix)]
    foreground_terminal: bool,
    deadline: Option<Arc<std::sync::Mutex<ProcessDeadline>>>,
}

/// One shared clock owns both deadline checks and nested pause accounting.
struct ProcessDeadline {
    expires: Instant,
    timeout: Duration,
    paused: Option<(Instant, usize)>,
}

impl ProcessDeadline {
    fn new(timeout: Duration) -> Arc<std::sync::Mutex<Self>> {
        Arc::new(std::sync::Mutex::new(Self {
            expires: Instant::now() + timeout,
            timeout,
            paused: None,
        }))
    }

    fn expired(&self) -> bool {
        self.paused.map_or_else(Instant::now, |(start, _)| start) >= self.expires
    }
}

/// Excludes a scope from its owner's deadline, including work between commands.
/// Nested or overlapping scopes count their union rather than adding time twice.
#[must_use]
pub struct DeadlinePause(Option<Arc<std::sync::Mutex<ProcessDeadline>>>);

impl DeadlinePause {
    /// Executors with no operation-wide deadline have nothing to pause.
    pub fn none() -> Self {
        Self(None)
    }

    fn new(deadline: Option<Arc<std::sync::Mutex<ProcessDeadline>>>) -> Self {
        if let Some(deadline) = &deadline {
            let mut deadline = deadline.lock().unwrap();
            match &mut deadline.paused {
                Some((_, depth)) => *depth += 1,
                paused @ None => *paused = Some((Instant::now(), 1)),
            }
        }
        Self(deadline)
    }
}

impl Drop for DeadlinePause {
    fn drop(&mut self) {
        if let Some(deadline) = &self.0 {
            let mut deadline = deadline.lock().unwrap();
            let (start, depth) = deadline.paused.as_mut().expect("a held deadline pause");
            *depth -= 1;
            if *depth == 0 {
                let elapsed = start.elapsed();
                deadline.expires += elapsed;
                deadline.paused = None;
            }
        }
    }
}

/// Cancels synchronous executor work when its supervising async wait is dropped.
pub struct ProcessCancellationGuard(Arc<AtomicBool>);

impl Drop for ProcessCancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl CancellableProcessExecutor {
    /// Feed input, close stdin at EOF, and observe both output streams while
    /// retaining the executor's process-group, cancellation and SSH ownership.
    pub fn execute_with_stdin_observed(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
        observer: Option<CommandOutputObserver>,
    ) -> Result<CommandOutput> {
        self.execute_with_stdin_inner(command, input, None, observer)
    }

    /// Feed input from a blocking source that can be woken via `abort_stdin`
    /// when cancellation starts or the child exits after stdin streaming has
    /// started.
    pub fn execute_with_abortable_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
        abort_stdin: &(dyn Fn() + Sync),
    ) -> Result<CommandOutput> {
        self.execute_with_stdin_inner(command, input, Some(abort_stdin), None)
    }

    fn execute_with_stdin_inner(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
        abort_stdin: Option<&(dyn Fn() + Sync)>,
        observer: Option<CommandOutputObserver>,
    ) -> Result<CommandOutput> {
        self.describe_cancellation(
            command,
            crate::runtime::off_async_worker(|| {
                let _running = BlockingOperation::command(command);
                // The stream is not read until the session has started, so a
                // refused session is retried with the same stream. The child
                // runs in its own process group so cancellation can kill the
                // whole group, which is what releases a writer blocked on a
                // full pipe.
                with_ssh_refusal_retry(
                    command,
                    self,
                    Some(&|| self.is_cancelled()),
                    |command, gate| {
                        stream_command_with_stdin(
                            cancellable_command(command),
                            command,
                            &mut *input,
                            &|| self.is_cancelled(),
                            abort_stdin,
                            gate,
                            observer.clone(),
                        )
                    },
                )
            }),
        )
    }

    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub fn cancel_on_drop(&self) -> ProcessCancellationGuard {
        ProcessCancellationGuard(self.cancelled.clone())
    }

    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            token: None,
            #[cfg(unix)]
            foreground_terminal: false,
            deadline: None,
        }
    }

    /// Use the supervising login's cancellation for blocking preparation.
    /// Interactive account shells need foreground terminal ownership even
    /// though their output is captured and cancellation owns their group.
    #[cfg(unix)]
    pub fn with_foreground_terminal(mut self) -> Self {
        self.foreground_terminal = true;
        self
    }

    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.token = Some(token);
        self
    }

    pub fn is_cancelled(&self) -> bool {
        self.token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
            || self.cancelled.load(Ordering::Acquire)
            || self
                .deadline
                .as_ref()
                .is_some_and(|deadline| deadline.lock().unwrap().expired())
    }

    /// Whether the supervisor, not the deadline, has asked to stop.
    pub fn is_cancelled_without_deadline(&self) -> bool {
        self.token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
            || self.cancelled.load(Ordering::Acquire)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            token: None,
            #[cfg(unix)]
            foreground_terminal: false,
            deadline: Some(ProcessDeadline::new(timeout)),
        }
    }

    /// Bounds an existing flag-based executor with a deadline, so a wedged
    /// child becomes a reported failure instead of running forever.
    pub fn with_deadline(mut self, timeout: Duration) -> Self {
        self.deadline = Some(ProcessDeadline::new(timeout));
        self
    }

    fn cancellation_message(&self, purpose: &str) -> String {
        if !self.cancelled.load(Ordering::Acquire)
            && !self
                .token
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
            && let Some(deadline) = &self.deadline
            && let deadline = deadline.lock().unwrap()
            && deadline.expired()
        {
            return format!(
                "timed out after {} s while {purpose}",
                deadline.timeout.as_secs_f64()
            );
        }
        format!("operation cancelled while {purpose}")
    }

    fn check_cancelled(&self, purpose: &str) -> Result<()> {
        if self.is_cancelled() {
            bail!("{}", self.cancellation_message(purpose));
        }
        Ok(())
    }

    fn describe_cancellation<T>(&self, command: &CommandSpec, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            if self.is_cancelled()
                && error
                    .chain()
                    .any(|cause| cause.to_string().starts_with("operation cancelled"))
            {
                anyhow::anyhow!(self.cancellation_message(&command.purpose))
            } else {
                error
            }
        })
    }
}

fn configured_command(command: &CommandSpec) -> Command {
    let mut process = Command::new(&command.program);
    #[cfg(unix)]
    if let Some(arg0) = &command.arg0 {
        use std::os::unix::process::CommandExt as _;
        process.arg0(arg0);
    }
    if command.clear_env {
        process.env_clear();
    }
    process.envs(&command.env);
    for name in &command.env_remove {
        process.env_remove(name);
    }
    if let Some(cwd) = &command.cwd {
        process.current_dir(cwd);
    }
    process.args(&command.args);
    process
}

fn cancellable_command(command: &CommandSpec) -> Command {
    #[cfg(unix)]
    let mut process = configured_command(command);
    #[cfg(not(unix))]
    let process = configured_command(command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }
    process
}

/// How long a command's output may keep arriving after its leader exits.
/// A backgrounded descendant inherits the output pipes and can hold them open
/// for its whole life, although the command it was started by is finished.
/// The same bound as Codex's `IO_DRAIN_TIMEOUT_MS`. The owned process group is
/// terminated once this drain ends.
const IO_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// A child pipe that can be waited on with a timeout, so its reader can be
/// stopped while a descendant still holds the write end.
trait PollablePipe: Read + Send + 'static {
    fn readable(&self, timeout: Duration) -> bool;
}

macro_rules! pollable_pipe {
    ($pipe:ty) => {
        impl PollablePipe for $pipe {
            #[cfg(unix)]
            fn readable(&self, timeout: Duration) -> bool {
                use std::os::fd::AsRawFd as _;
                let mut poll = libc::pollfd {
                    fd: self.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
                // SAFETY: `poll` is one initialized pollfd for an open descriptor.
                unsafe { libc::poll(&raw mut poll, 1, millis) > 0 }
            }

            // Without polling, a read blocks until the pipe closes.
            #[cfg(not(unix))]
            fn readable(&self, _timeout: Duration) -> bool {
                true
            }
        }
    };
}

pollable_pipe!(std::process::ChildStdout);
pollable_pipe!(std::process::ChildStderr);

/// Collects one child pipe on its own thread. Stopping it drops the thread's
/// end of the pipe, so a descendant that keeps the pipe open cannot hold the
/// reader past the drain.
struct PipeCollector {
    bytes: Arc<std::sync::Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    task: PipeTask,
}

enum PipeTask {
    Running(std::thread::JoinHandle<std::io::Result<()>>),
    Finished(std::io::Result<()>),
}

impl PipeCollector {
    fn spawn(pipe: impl PollablePipe) -> Self {
        Self::spawn_scanning(pipe, None)
    }

    /// Collect a session's stdout. With a `gate` the start marker is removed
    /// from the front, the first byte reports the session's start, and the end
    /// of the pipe reports that it ended; the gate settles however the thread
    /// stops.
    fn spawn_scanning(pipe: impl PollablePipe, gate: Option<Arc<StartGate>>) -> Self {
        Self::spawn_output(pipe, gate, None)
    }

    fn spawn_output(
        pipe: impl PollablePipe,
        gate: Option<Arc<StartGate>>,
        output: Option<std::fs::File>,
    ) -> Self {
        Self::spawn_observed(pipe, gate, output, None)
    }

    fn spawn_observed(
        mut pipe: impl PollablePipe,
        gate: Option<Arc<StartGate>>,
        mut output: Option<std::fs::File>,
        observer: Option<CommandOutputObserver>,
    ) -> Self {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (collected, stopped) = (bytes.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut scan = gate.as_ref().map(|_| StartScan::new());
            let mut chunk = [0_u8; 8192];
            let mut passed = Vec::new();
            let result = (|| {
                while !stopped.load(Ordering::Acquire) {
                    if !pipe.readable(Duration::from_millis(25)) {
                        continue;
                    }
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => {
                            let mut collected = collected
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            match (&mut scan, &gate) {
                                (Some(scan), Some(gate)) => {
                                    passed.clear();
                                    if scan.push(&chunk[..count], &mut passed) {
                                        gate.mark_started();
                                    }
                                    if let Some(observer) = &observer {
                                        observer(&passed);
                                    }
                                    if let Some(file) = &mut output {
                                        file.write_all(&passed)?;
                                    } else {
                                        collected.extend_from_slice(&passed);
                                    }
                                }
                                _ => {
                                    if let Some(observer) = &observer {
                                        observer(&chunk[..count]);
                                    }
                                    if let Some(file) = &mut output {
                                        file.write_all(&chunk[..count])?;
                                    } else {
                                        collected.extend_from_slice(&chunk[..count]);
                                    }
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            })();
            let result = result.and_then(|()| {
                if let Some(scan) = &mut scan {
                    passed.clear();
                    scan.finish(&mut passed);
                    if let Some(observer) = &observer {
                        observer(&passed);
                    }
                    if let Some(file) = &mut output {
                        file.write_all(&passed)?;
                    } else {
                        collected
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .extend_from_slice(&passed);
                    }
                }
                Ok(())
            });
            if let Some(gate) = &gate {
                gate.mark_ended();
            }
            result
        });
        Self {
            bytes,
            stop,
            task: PipeTask::Running(thread),
        }
    }

    fn is_finished(&self) -> bool {
        match &self.task {
            PipeTask::Running(thread) => thread.is_finished(),
            PipeTask::Finished(_) => true,
        }
    }

    fn join(&mut self) {
        if let PipeTask::Running(thread) =
            std::mem::replace(&mut self.task, PipeTask::Finished(Ok(())))
        {
            self.task =
                PipeTask::Finished(thread.join().unwrap_or_else(|_| {
                    Err(std::io::Error::other("command pipe reader panicked"))
                }));
        }
    }

    /// A finished reader owns its one result. The supervisor observes failure
    /// before waiting for a child that may otherwise keep working forever.
    fn failed(&mut self) -> bool {
        if matches!(&self.task, PipeTask::Running(thread) if thread.is_finished()) {
            self.join();
        }
        matches!(&self.task, PipeTask::Finished(Err(_)))
    }

    /// Waits for end of stream until `deadline`, then stops the reader and
    /// returns everything it collected.
    fn finish(mut self, stream: &str, deadline: Instant) -> Result<Vec<u8>> {
        while !self.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.stop.store(true, Ordering::Release);
        if matches!(self.task, PipeTask::Running(_)) {
            self.join();
        }
        let PipeTask::Finished(result) =
            std::mem::replace(&mut self.task, PipeTask::Finished(Ok(())))
        else {
            unreachable!()
        };
        result.with_context(|| format!("read command {stream}"))?;
        let mut bytes = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(std::mem::take(&mut bytes))
    }
}

impl Drop for PipeCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn terminate_cancellable_child(child: &mut std::process::Child, preserve_children: bool) {
    #[cfg(unix)]
    if preserve_children {
        if let Err(error) = child.kill()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(pid = child.id(), %error, "could not terminate cancelled command");
        }
    } else if let Err(error) =
        crate::subprocess::signal_process_group(child.id() as i32, libc::SIGKILL)
    {
        // A group that is already gone is the wanted outcome, not a failure.
        tracing::warn!(pid = child.id(), %error, "could not terminate cancelled command process group");
    }
    #[cfg(not(unix))]
    {
        let _ = preserve_children;
        if let Err(error) = child.kill() {
            tracing::warn!(pid = child.id(), %error, "could not terminate cancelled command");
        }
    }
    if let Err(error) = child.wait() {
        tracing::warn!(pid = child.id(), %error, "could not reap cancelled command");
    }
}

impl CancellableProcessExecutor {
    /// One attempt, with no retry of its own. A `gate` makes the attempt read
    /// the start marker from stdout and report the session's start.
    fn run_once(
        &self,
        command: &CommandSpec,
        gate: Option<&Arc<StartGate>>,
    ) -> Result<CommandOutput> {
        self.run_once_output(command, gate, None)
    }

    /// Stream stdout into a file, retaining the shared cancellation, process-group,
    /// and SSH refusal/start-marker semantics. Returned stdout is empty.
    pub fn execute_to_file(
        &self,
        command: &CommandSpec,
        output: &std::fs::File,
    ) -> Result<CommandOutput> {
        use std::io::{Seek, SeekFrom};
        ensure!(
            !command.keep_stdin_open && command.sensitive_stdin.is_none(),
            "file-output commands cannot carry buffered or retained stdin"
        );
        crate::runtime::off_async_worker(|| {
            let _running = BlockingOperation::command(command);
            self.describe_cancellation(
                command,
                with_ssh_refusal_retry(
                    command,
                    self,
                    Some(&|| self.is_cancelled()),
                    |command, gate| {
                        let mut file = output.try_clone().context("clone command output file")?;
                        file.set_len(0).context("reset command output file")?;
                        file.seek(SeekFrom::Start(0))
                            .context("rewind command output file")?;
                        self.run_once_output(command, gate, Some(file))
                    },
                ),
            )
        })
    }

    fn run_once_output(
        &self,
        command: &CommandSpec,
        gate: Option<&Arc<StartGate>>,
        output: Option<std::fs::File>,
    ) -> Result<CommandOutput> {
        let mut process = cancellable_command(command);
        #[cfg(unix)]
        let _foreground = if self.foreground_terminal {
            crate::subprocess::ForegroundGroup::prepare(&mut process)?
        } else {
            None
        };
        if command.sensitive_stdin.is_some() || command.keep_stdin_open {
            let mut input = std::io::Cursor::new(command.sensitive_stdin().unwrap_or_default());
            // Owned bytes, so each attempt gets its own reader.
            return stream_command_with_stdin(
                process,
                command,
                &mut input,
                &|| self.is_cancelled(),
                None,
                gate,
                None,
            );
        }
        let started = Instant::now();
        self.check_cancelled(&command.purpose)?;
        process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::subprocess::spawn_child(&mut process)
            .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
        let group = (!command.detaches && !command.preserve_children_on_cancel)
            .then(|| crate::subprocess::ProcessGroupGuard::new(Some(child.id())));
        let exit_watch = crate::subprocess::ExitWatch::new(&child);
        // Each timed wait says whether the child may have exited, so nothing
        // polls the pidfd twice; the first pass checks once.
        let mut maybe_exited = true;
        let stdout = child.stdout.take().context("command stdout missing")?;
        let stderr = child.stderr.take().context("command stderr missing")?;
        let mut stdout_reader = PipeCollector::spawn_output(stdout, gate.cloned(), output);
        let mut stderr_reader = PipeCollector::spawn(stderr);
        let mut status = None;
        let mut exited_at = None;
        let status = loop {
            if status.is_none() && (stdout_reader.failed() || stderr_reader.failed()) {
                terminate_cancellable_child(&mut child, command.preserve_children_on_cancel);
                status = child
                    .try_wait()
                    .context("reap command after pipe failure")?;
            }
            if self.is_cancelled() {
                terminate_cancellable_child(&mut child, command.preserve_children_on_cancel);
                let deadline = Instant::now() + IO_DRAIN_TIMEOUT;
                for (stream, reader) in [("stdout", stdout_reader), ("stderr", stderr_reader)] {
                    if let Err(error) = reader.finish(stream, deadline) {
                        tracing::warn!(stream, %error, "cancelled command reader failed");
                    }
                }
                bail!("{}", self.cancellation_message(&command.purpose));
            }
            if status.is_none() && maybe_exited {
                status = child
                    .try_wait()
                    .with_context(|| format!("wait for {}", command.purpose))?;
            }
            // The command completes when its leader exits. Descendants can
            // retain the pipes, so output is collected for a bounded time.
            if let Some(status) = status {
                let exited_at = *exited_at.get_or_insert_with(Instant::now);
                if (stdout_reader.is_finished() && stderr_reader.is_finished())
                    || exited_at.elapsed() >= IO_DRAIN_TIMEOUT
                {
                    break status;
                }
            }
            // A reaped child's pidfd stays readable; sleep while draining.
            maybe_exited = if status.is_some() {
                std::thread::sleep(Duration::from_millis(25));
                true
            } else {
                exit_watch.wait(Duration::from_millis(25))
            };
        };
        let deadline = Instant::now();
        let stdout = stdout_reader.finish("stdout", deadline)?;
        let stderr = stderr_reader.finish("stderr", deadline)?;
        let status = status.code().unwrap_or(-1);
        drop(group);
        Ok(finished_command(command, started, status, stdout, stderr))
    }
}

impl CommandExecutor for CancellableProcessExecutor {
    fn pause_deadline(&self) -> DeadlinePause {
        DeadlinePause::new(self.deadline.clone())
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.is_cancelled_without_deadline()
    }

    fn execute_cleanup(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.clone()
            .with_deadline(Duration::from_secs(15))
            .execute(command)
    }

    fn execute_cleanup_with_timeout(
        &self,
        command: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        self.clone().with_deadline(timeout).execute(command)
    }

    fn cancellation_requested(&self) -> bool {
        self.is_cancelled()
    }

    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
        Self {
            deadline: None,
            ..self.clone()
        }
        .execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        Self {
            deadline: None,
            ..self.clone()
        }
        .execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        crate::runtime::off_async_worker(|| {
            let _running = BlockingOperation::command(command);
            self.describe_cancellation(
                command,
                with_ssh_refusal_retry(
                    command,
                    self,
                    Some(&|| self.is_cancelled()),
                    |command, gate| self.run_once(command, gate),
                ),
            )
        })
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        self.execute_with_stdin_observed(command, input, None)
    }
}

/// A command supervised by [`BoundedProcessExecutor`] did not finish before
/// its deadline. Callers downcast to this to tell a hung probe apart from a
/// command that could not be started at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTimedOut {
    pub program: String,
    pub purpose: String,
    pub timeout: Duration,
}

impl std::fmt::Display for CommandTimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "`{}` did not answer within {} seconds while trying to {}",
            self.program,
            self.timeout.as_secs(),
            self.purpose
        )
    }
}

impl std::error::Error for CommandTimedOut {}

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
    fn pause_deadline(&self) -> DeadlinePause {
        DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &crate::targets::CommandSpec,
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        // This executor's only bound is its deadline and it has no
        // cancellation, so without the deadline it is a plain process.
        ProcessExecutor.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &crate::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        ProcessExecutor.execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let executor = CancellableProcessExecutor::with_timeout(self.timeout);
        executor.execute(command).map_err(|error| {
            if executor.is_cancelled() {
                anyhow::Error::new(CommandTimedOut {
                    program: command.program.clone(),
                    purpose: command.purpose.clone(),
                    timeout: self.timeout,
                })
            } else {
                error
            }
        })
    }

    fn execute_cleanup_with_timeout(
        &self,
        command: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        // This executor's configured timeout is a hard per-command cap; the
        // call-specific cleanup timeout can only make the attempt shorter.
        CancellableProcessExecutor::with_timeout(self.timeout.min(timeout)).execute(command)
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

/// Environment inherited through a shell must be an assignment identifier.
pub fn validate_inherited_environment_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.bytes().enumerate().all(|(index, byte)| byte == b'_'
                || byte.is_ascii_alphabetic()
                || (index > 0 && byte.is_ascii_digit())),
        "invalid inherited environment variable name"
    );
    Ok(())
}

/// One argv-only operation in an ordered shell batch.
#[derive(Debug, Clone)]
pub struct CommandStep {
    argv: Vec<String>,
    purpose: String,
    accepted_statuses: Vec<i32>,
    cleanup: bool,
    best_effort: bool,
}

impl CommandStep {
    pub fn new(
        purpose: impl Into<String>,
        argv: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            purpose: purpose.into(),
            accepted_statuses: vec![0],
            cleanup: false,
            best_effort: false,
        }
    }

    pub fn accepted_statuses(mut self, statuses: impl IntoIterator<Item = i32>) -> Self {
        self.accepted_statuses = statuses.into_iter().collect();
        self
    }

    /// Run on shell exit, including when an earlier operation failed.
    pub fn cleanup(mut self) -> Self {
        self.cleanup = true;
        self
    }

    /// Report a cleanup failure to the caller without failing the batch.
    /// Callers must log [`step_script_warnings`] even when execution failed.
    pub fn best_effort_cleanup(mut self) -> Self {
        self.cleanup = true;
        self.best_effort = true;
        self
    }
}

/// Build one `sh -c` argv. Local decisions and transport selection stay with
/// the caller; only these already-decided operations run in the script.
pub fn step_script(steps: impl IntoIterator<Item = CommandStep>) -> Vec<String> {
    let steps: Vec<_> = steps.into_iter().collect();
    let mut script = String::new();
    for (index, step) in steps.iter().enumerate() {
        let argv = step
            .argv
            .iter()
            .map(|arg| posix_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let statuses = step
            .accepted_statuses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|");
        let purpose = serde_json::to_string(&step.purpose).expect("serialize step purpose");
        let marker = if step.best_effort {
            "MJ_STEP_WARNING"
        } else {
            "MJ_STEP_FAILED"
        };
        let returned = if step.best_effort {
            "0"
        } else {
            "\"$mj_status\""
        };
        script.push_str(&format!(
            "mj_step_{index}() {{\n( {argv} )\nmj_status=$?\ncase $mj_status in {statuses}) return 0;; esac\nprintf '\\n{marker} {index} %s %s\\n' \"$mj_status\" {} >&2\nreturn {returned}\n}}\n",
            posix_quote(&purpose),
        ));
    }
    if steps.iter().any(|step| step.cleanup) {
        script.push_str("mj_cleanup() {\nmj_original=$?\ntrap - EXIT HUP INT TERM\n");
        for (index, step) in steps.iter().enumerate() {
            if step.cleanup {
                script.push_str(&format!("mj_step_{index}\nmj_result=$?\nif [ \"$mj_original\" -eq 0 ]; then mj_original=$mj_result; fi\n"));
            }
        }
        script.push_str("exit \"$mj_original\"\n}\ntrap mj_cleanup EXIT\ntrap 'exit 129' HUP\ntrap 'exit 130' INT\ntrap 'exit 143' TERM\n");
    }
    for (index, step) in steps.iter().enumerate() {
        if !step.cleanup {
            script.push_str(&format!("mj_step_{index} || exit $?\n"));
        }
    }
    script.push_str("exit 0\n");
    vec!["sh".into(), "-c".into(), script, "mj-steps".into()]
}

/// Decode the first failed step, preserving its stderr and original purpose.
/// The marker includes JSON-escaped purpose text so transports and serialized
/// plans need no extra metadata, including for purposes containing newlines.
pub fn step_script_error(output: &CommandOutput) -> Option<String> {
    if output.status == 0 {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut failure = None;
    let mut detail = Vec::new();
    for line in stderr.lines() {
        let parsed = parse_step_marker(line, "MJ_STEP_FAILED ");
        if let Some(parsed) = parsed {
            failure.get_or_insert(parsed);
        } else if parse_step_marker(line, "MJ_STEP_WARNING ").is_none() {
            detail.push(line);
        }
    }
    failure.map(|(status, purpose)| {
        format!(
            "{purpose} failed with status {status}: {}",
            detail.join("\n").trim()
        )
    })
}

/// Failures of cleanup steps marked best-effort. Always inspect these before
/// checking the command result, so an earlier failure cannot hide cleanup errors.
pub fn step_script_warnings(output: &CommandOutput) -> Vec<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .filter(|line| !line.starts_with("MJ_STEP_"))
        .collect::<Vec<_>>()
        .join("\n");
    stderr
        .lines()
        .filter_map(|line| {
            let (status, purpose) = parse_step_marker(line, "MJ_STEP_WARNING ")?;
            Some(format!(
                "{purpose} failed with status {status}: {}",
                detail.trim()
            ))
        })
        .collect()
}

fn parse_step_marker(line: &str, prefix: &str) -> Option<(i32, String)> {
    let mut fields = line.strip_prefix(prefix)?.splitn(3, ' ');
    let _index = fields.next()?.parse::<usize>().ok()?;
    let status = fields.next()?.parse::<i32>().ok()?;
    let purpose = serde_json::from_str::<String>(fields.next()?).ok()?;
    Some((status, purpose))
}

impl CommandPlan {
    /// Supply one container environment value without placing it in the
    /// Podman/SSH argument vector. See [`Self::provide_target_environment_secrets`].
    pub fn provide_target_environment_secret(
        &mut self,
        target: &TargetTemplate,
        name: &str,
        value: &str,
    ) -> Result<()> {
        self.provide_target_environment_secrets(target, &[(name.to_owned(), value.to_owned())])
    }

    /// Supply container environment values without placing them in the
    /// Podman/SSH argument vector. The target launcher reads one value per
    /// line from stdin, exports each, and the container engine is asked to
    /// inherit it by name (`--env NAME`).
    pub fn provide_target_environment_secrets(
        &mut self,
        target: &TargetTemplate,
        secrets: &[(String, String)],
    ) -> Result<()> {
        ensure!(
            !secrets.is_empty(),
            "no secret environment values to supply"
        );
        let mut read_and_export = String::new();
        let mut input = Vec::new();
        for (index, (name, value)) in secrets.iter().enumerate() {
            validate_inherited_environment_name(name)?;
            ensure!(
                secrets[..index].iter().all(|(earlier, _)| earlier != name),
                "secret environment variable {name} is supplied twice"
            );
            ensure!(
                !value.as_bytes().contains(&b'\n') && !value.as_bytes().contains(&b'\r'),
                "secret environment value cannot contain a newline"
            );
            read_and_export.push_str(&format!("IFS= read -r {name} || exit 1; export {name}; "));
            input.extend_from_slice(value.as_bytes());
            input.push(b'\n');
        }
        let command = self
            .commands
            .iter_mut()
            .find(|command| command.creates_target)
            .context("provisioning plan has no target creation command")?;
        match target {
            TargetTemplate::LocalPodman(_)
            | TargetTemplate::LocalDocker(_)
            | TargetTemplate::AppleContainer(_) => {
                let program = std::mem::replace(&mut command.program, "sh".to_owned());
                let args = std::mem::take(&mut command.args);
                command.args = vec![
                    "-c".to_owned(),
                    format!("{read_and_export}exec \"$@\""),
                    "mj-secret-env".to_owned(),
                    program,
                ];
                command.args.extend(args);
            }
            TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. }
            | TargetTemplate::SshAppleContainer { .. }
            | TargetTemplate::SshNitro { .. } => {
                let remote = command
                    .args
                    .last_mut()
                    .context("remote container command has no SSH command argument")?;
                *remote = format!("{read_and_export}exec {remote}");
            }
            TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => {
                bail!("target does not support inherited container environment")
            }
        }
        command.sensitive_stdin = Some(SensitiveCommandInput(Arc::new(input)));
        Ok(())
    }

    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<Vec<CommandOutput>> {
        let mut outputs = Vec::with_capacity(self.commands.len());
        for command in &self.commands {
            let output = executor.execute(command)?;
            outputs.push(checked_command_output(command, output)?);
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

/// Emit a `WARN` log if a command succeeded with non-empty stderr and opted in.
pub fn log_command_warnings(command: &CommandSpec, output: &CommandOutput) {
    if command.warn_on_stderr && !command.sensitive_output {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let warning = stderr.trim();
        if !warning.is_empty() {
            tracing::warn!(
                purpose = %command.purpose,
                stderr = %warning,
                "command succeeded with warnings"
            );
        }
    }
}

/// Fail the same way [`CommandPlan::execute`] does for a non-zero exit
/// status; kept as a shared helper so [`CommandPlan::execute_concurrent`]
/// reports identical error text.
pub fn checked_command_output(
    command: &CommandSpec,
    output: CommandOutput,
) -> Result<CommandOutput> {
    if let Some(error) = step_script_error(&output) {
        bail!("{error}");
    }
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    log_command_warnings(command, &output);
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

/// Internal namespace used to carry an exact checkout selection from a host
/// snapshot into an ordinary clone without borrowing the snapshot's objects.
pub const HOST_SNAPSHOT_EXACT_REFS: &str = "refs/mj/host-snapshot-exact";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySpec {
    /// Network clone URL. Managed workspaces require a configured remote.
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_urls: Vec<String>,
    pub destination: String,
    pub git_ref: Option<String>,
    /// Read-only bare repository mounted into the target for Git object reuse.
    /// For a host-only source the snapshot is required and serves the clone.
    /// With `history_cutoff`, it is instead a Git bundle in the target that
    /// holds only the cutoff history, and the checkout fetches from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Start the checkout with these commits and their ancestors only. It is
    /// created by `git init` and a fetch by object ID instead of a clone, so
    /// it has no `origin` remote and no other refs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_cutoff: Option<HistoryCutoff>,
}

/// The commits a checkout created without later history may hold, with their
/// ancestors. See [`RepositorySpec::history_cutoff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCutoff {
    /// The commit the checkout starts at, detached.
    pub head: String,
    /// The diff base when it is not `head`. The checkout keeps it reachable
    /// as `refs/mj/base`, because it need not be an ancestor of `head`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

impl HistoryCutoff {
    /// The ref the checkout keeps the base at, which is also the base's name
    /// in a clone-cache cutoff bundle.
    pub const BASE_REF: &str = "refs/mj/base";
    /// The head's name in a clone-cache cutoff bundle.
    pub const BUNDLE_HEAD_REF: &str = "refs/mj/head";

    /// `head`, then the base when there is one.
    pub fn commits(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.head.as_str()).chain(self.base.as_deref())
    }

    pub fn validate(&self) -> Result<()> {
        for commit in self.commits() {
            ensure!(
                crate::remote_git::is_object_id(commit)
                    && !commit.bytes().any(|byte| byte.is_ascii_uppercase()),
                "history cutoff commit {commit:?} is not a lowercase full object ID"
            );
        }
        ensure!(
            self.base.as_deref() != Some(self.head.as_str()),
            "history cutoff base repeats its head"
        );
        Ok(())
    }
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
            if let Some(cutoff) = &repository.history_cutoff {
                cutoff.validate()?;
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
    /// Per-target mbx build cache overrides, carried from the user
    /// configuration so cache resolution can read them off a runtime target.
    #[serde(default)]
    pub build_cache: Option<crate::config::TargetBuildCache>,
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

    /// What this policy does to `image`, in the words a settings screen can
    /// show. `Auto` is derived rather than an alias, so it names the launch
    /// behavior and the background refresh it implies, from the same image
    /// reading `resolve` uses.
    pub fn describe(self, image: &str) -> &'static str {
        match self {
            Self::Always => "Pull every launch",
            Self::Newer => "Pull when the registry is newer",
            Self::Missing => "Pull only if missing",
            Self::Never => "Never pull",
            Self::Auto => match self.resolve(image) {
                Self::Newer => "Pull if missing at launch; refresh :latest in background",
                _ => "Pull if missing",
            },
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
    AppleContainer,
    SshPodman(SshTarget),
    SshDocker(SshTarget),
    SshAppleContainer(SshTarget),
    /// The `mj-worker microvm` engine on an EC2 host; see [`MICROVM_ENGINE`].
    SshNitro(SshTarget),
}

/// The name of the Nitro Sandbox host engine, `mj-worker microvm`. It
/// implements the subset of Podman's verbs, flags and output formats that
/// Mjolnir drives over SSH, so most call sites treat it like `podman`.
pub const MICROVM_ENGINE: &str = "microvm";

/// Where the host engine's worker binary lives on an SSH host, relative to
/// the login home. The daemon installs it there before the first engine call
/// (see the controller's `ensure_microvm_engine`); this is the only place
/// that names the path.
pub const MICROVM_ENGINE_WORKER: &str = ".local/share/hel/microvm/hel";

/// The argument vector that runs `engine` on its host, up to the verb.
///
/// Podman, Docker and Apple's `container` are one program. The microVM engine
/// is the installed worker run as root. `sudo` needs an absolute path and
/// ssh quotes every argument, so the login shell cannot expand `$HOME`; the
/// inner `sh` does it before `sudo` starts. Everything after this prefix
/// reaches the engine unchanged, including stdin, stdout and the exit status.
pub fn engine_argv(engine: &str) -> Vec<String> {
    engine_argv_with_env(engine, &[])
}

/// [`engine_argv`] for a command whose engine must inherit the environment
/// variables `names` from the ssh command's environment. `sudo` resets the
/// environment, so the microVM engine's prefix preserves exactly these; a
/// value on the command line would be logged by `sudo` and visible in `ps`.
/// Other engines inherit the whole environment and ignore `names`.
pub fn engine_argv_with_env(engine: &str, names: &[String]) -> Vec<String> {
    if engine == MICROVM_ENGINE {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!(
                r#"exec {} "$@""#,
                engine_shell_words_with_env(engine, names)
            ),
            "mj-microvm".to_owned(),
        ]
    } else {
        vec![engine.to_owned()]
    }
}

/// `engine` as the words of a shell command line, for a script that runs it:
/// `exec $(engine_shell_words) volume rm "$volume"`. The words contain `$HOME`
/// for the microVM engine, so they belong inside a script that does not quote
/// them away, never in an argument that ssh quotes.
pub fn engine_shell_words(engine: &str) -> String {
    engine_shell_words_with_env(engine, &[])
}

/// [`engine_shell_words`] preserving the environment variables `names`
/// through `sudo`; see [`engine_argv_with_env`].
pub fn engine_shell_words_with_env(engine: &str, names: &[String]) -> String {
    if engine == MICROVM_ENGINE {
        let preserve = if names.is_empty() {
            String::new()
        } else {
            format!("--preserve-env={} ", names.join(","))
        };
        format!(r#"sudo -n {preserve}"$HOME/{MICROVM_ENGINE_WORKER}" {MICROVM_ENGINE}"#)
    } else {
        engine.to_owned()
    }
}

/// `args` with a leading engine name expanded to the command that runs it.
pub fn with_engine_argv(mut args: Vec<String>) -> Vec<String> {
    if args.first().is_some_and(|first| first == MICROVM_ENGINE) {
        let verbs = args.split_off(1);
        let mut argv = engine_argv(MICROVM_ENGINE);
        argv.extend(verbs);
        argv
    } else {
        args
    }
}

/// `engine` followed by `args`, as one command.
pub fn engine_command(
    engine: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut argv = engine_argv(engine);
    argv.extend(args.into_iter().map(Into::into));
    let program = argv.remove(0);
    CommandSpec::new(program, argv)
}

/// `engine` followed by `args`, run on the SSH host.
pub fn ssh_engine_command(
    class: SshTrafficClass,
    ssh: &SshTarget,
    engine: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut argv = engine_argv(engine);
    argv.extend(args.into_iter().map(Into::into));
    ssh_command_owned(class, ssh, argv)
}

impl ImageHost {
    pub const fn engine(&self) -> &'static str {
        match self {
            Self::LocalPodman | Self::SshPodman(_) => "podman",
            Self::LocalDocker | Self::SshDocker(_) => "docker",
            Self::AppleContainer | Self::SshAppleContainer(_) => "container",
            Self::SshNitro(_) => MICROVM_ENGINE,
        }
    }

    /// How this host is named in a log line.
    pub fn label(&self) -> String {
        match self {
            Self::LocalPodman => "local podman".to_owned(),
            Self::LocalDocker => "local docker".to_owned(),
            Self::AppleContainer => "apple container".to_owned(),
            Self::SshPodman(ssh) => format!("podman on {}", ssh.destination),
            Self::SshDocker(ssh) => format!("docker on {}", ssh.destination),
            Self::SshAppleContainer(ssh) => format!("apple container on {}", ssh.destination),
            Self::SshNitro(ssh) => format!("nitro sandboxes on {}", ssh.destination),
        }
    }

    /// Run `args` (an engine command line, starting with the engine's name)
    /// on this host.
    pub fn command(&self, args: Vec<String>, purpose: String) -> CommandSpec {
        match self {
            Self::LocalPodman | Self::LocalDocker | Self::AppleContainer => {
                CommandSpec::new(args[0].clone(), args[1..].iter().cloned())
            }
            Self::SshPodman(ssh)
            | Self::SshDocker(ssh)
            | Self::SshAppleContainer(ssh)
            | Self::SshNitro(ssh) => {
                let mut argv = engine_argv(&args[0]);
                argv.extend(args.into_iter().skip(1));
                ssh_command_owned(SshTrafficClass::Control, ssh, argv)
            }
        }
        .purpose(purpose)
    }
}

/// When a background refresh downloads an image.
///
/// A host that already has the image is the common case, and most targets
/// only need the copy to exist. Ordering matters: merging two targets that
/// share an image takes the larger value, so one demanding target upgrades
/// the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefreshWhen {
    /// Pull only when the host has no copy (missing, or auto on a fixed tag).
    WhenAbsent,
    /// Pull every tick (always, or newer / auto on a moving tag).
    Always,
}

/// The commands that keep one host's copy of one image current, away from any
/// session launch. They run in this order, and only for that host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRefresh {
    pub host: ImageHost,
    pub image: String,
    pub platform: Option<String>,
    /// Whether this image is downloaded only when the host lacks it, or on
    /// every refresh.
    pub when: RefreshWhen,
    /// Reads the cached image id, so a pull that changed nothing stays quiet.
    /// Run before and after the pull.
    pub image_id: CommandSpec,
    pub pull: CommandSpec,
    /// Dangling images and orphaned content only. Keep tagged images ready
    /// for future sessions; `--all` would delete those preloaded images.
    pub prune: CommandSpec,
}

/// The background refresh for one configured container target, or `None` when
/// the target asked never to download the image in the background.
///
/// `never` is the only opt-out. Every other policy at least wants the host to
/// have a copy, so the daemon downloads a missing image once; `always` and
/// `newer` keep the hourly pull they always had.
pub fn image_refresh(
    host: ImageHost,
    image: &str,
    platform: Option<&str>,
    pull_policy: ImagePullPolicy,
) -> Option<ImageRefresh> {
    let when = match pull_policy.resolve(image) {
        ImagePullPolicy::Always | ImagePullPolicy::Newer => RefreshWhen::Always,
        ImagePullPolicy::Missing => RefreshWhen::WhenAbsent,
        ImagePullPolicy::Never => return None,
        ImagePullPolicy::Auto => unreachable!("auto pull policy must resolve"),
    };
    let engine = host.engine();
    // Apple's `container` CLI has no `--format` on `image inspect`: the exit
    // status is what says the image is present, and the printed description
    // stands in for the id when comparing before and after a pull.
    let apple = matches!(
        host,
        ImageHost::AppleContainer | ImageHost::SshAppleContainer(_)
    );
    let mut image_id_args = vec![engine.to_owned(), "image".to_owned(), "inspect".to_owned()];
    if !apple {
        image_id_args.push("--format".to_owned());
        image_id_args.push("{{.Id}}".to_owned());
    }
    image_id_args.push(image.to_owned());
    let image_id = host.command(
        image_id_args,
        format!("read the cached id of container image {image}"),
    );
    let mut pull_args = vec![engine.to_owned()];
    if apple {
        pull_args.push("image".to_owned());
    }
    pull_args.push("pull".to_owned());
    // Apple's engine runs only native images, so it takes no platform.
    if let Some(platform) = platform.filter(|_| !apple) {
        pull_args.push(format!("--platform={platform}"));
    }
    pull_args.push(image.to_owned());
    let pull = host.command(pull_args, format!("refresh container image {image}"));
    let mut prune_args = vec![engine.to_owned(), "image".to_owned(), "prune".to_owned()];
    // Apple prunes without prompting and does not accept Docker/Podman's -f.
    if !apple {
        prune_args.push("-f".to_owned());
    }
    let prune = host.command(prune_args, "remove dangling container images".to_owned());
    Some(ImageRefresh {
        host,
        image: image.to_owned(),
        platform: platform.map(str::to_owned),
        when,
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetTemplate {
    LocalBare,
    LocalPodman(ContainerTemplate),
    LocalDocker(ContainerTemplate),
    AppleContainer(ContainerTemplate),
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
    SshAppleContainer {
        ssh: SshTarget,
        container: ContainerTemplate,
    },
    SshNitro {
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
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    LocalDocker {
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    AppleContainer {
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
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
        /// A borrowed-target child has its own worker identity in the parent's workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_id: Option<String>,
    },
    SshPodman {
        ssh: SshTarget,
        container_id: String,
        #[serde(default)]
        workspace_storage: PodmanWorkspaceLocator,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    SshDocker {
        ssh: SshTarget,
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    SshAppleContainer {
        ssh: SshTarget,
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    SshNitro {
        ssh: SshTarget,
        container_id: String,
        /// The session that owns the sandbox when this locator is a
        /// sub-agent child borrowing its parent's sandbox; `None` when the
        /// session owns the sandbox itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
}

impl TargetTemplate {
    pub const fn container_engine(&self) -> Option<&'static str> {
        match self {
            Self::LocalPodman(_) | Self::SshPodman { .. } => Some("podman"),
            Self::LocalDocker(_) | Self::SshDocker { .. } => Some("docker"),
            Self::AppleContainer(_) | Self::SshAppleContainer { .. } => Some("container"),
            Self::SshNitro { .. } => Some(MICROVM_ENGINE),
            _ => None,
        }
    }

    /// The host that downloads this target's image, with the container
    /// settings that name the image. `None` for targets that run no image.
    pub fn image_host(&self) -> Option<(ImageHost, &ContainerTemplate)> {
        match self {
            Self::LocalPodman(container) => Some((ImageHost::LocalPodman, container)),
            Self::LocalDocker(container) => Some((ImageHost::LocalDocker, container)),
            Self::AppleContainer(container) => Some((ImageHost::AppleContainer, container)),
            Self::SshPodman { ssh, container } => {
                Some((ImageHost::SshPodman(ssh.clone()), container))
            }
            Self::SshDocker { ssh, container } => {
                Some((ImageHost::SshDocker(ssh.clone()), container))
            }
            Self::SshAppleContainer { ssh, container } => {
                Some((ImageHost::SshAppleContainer(ssh.clone()), container))
            }
            Self::SshNitro { ssh, container } => {
                Some((ImageHost::SshNitro(ssh.clone()), container))
            }
            Self::LocalBare | Self::SshBare { .. } => None,
        }
    }
}

/// The engine and host holding a container, borrowed from its locator.
#[derive(Clone, Copy)]
pub struct ContainerEndpoint<'a> {
    pub engine: &'static str,
    pub ssh: Option<&'a SshTarget>,
    pub container_id: &'a str,
}

impl ContainerEndpoint<'_> {
    /// Run a command on the engine's host.
    pub fn host_command(self, class: SshTrafficClass, command: CommandSpec) -> CommandSpec {
        match self.ssh {
            Some(ssh) => command_over_ssh(class, command, ssh),
            None => command,
        }
    }
}

/// Move a command to the remote host without losing its input or lifecycle metadata.
pub fn command_over_ssh(
    class: SshTrafficClass,
    mut command: CommandSpec,
    ssh: &SshTarget,
) -> CommandSpec {
    let remote = std::iter::once(command.program)
        .chain(command.args)
        .collect();
    let wrapped = ssh_command_owned(class, ssh, remote);
    command.program = wrapped.program;
    command.args = wrapped.args;
    // The wrapped command now opens an SSH session, so it is admitted and
    // placed on a shared connection like any other.
    command.ssh_destination = wrapped.ssh_destination;
    command.ssh_session = wrapped.ssh_session;
    command
}

impl TargetLocator {
    pub fn container_endpoint(&self) -> Option<ContainerEndpoint<'_>> {
        let (ssh, container_id) = match self {
            Self::LocalPodman { container_id, .. }
            | Self::LocalDocker { container_id, .. }
            | Self::AppleContainer { container_id, .. } => (None, container_id),
            Self::SshPodman {
                ssh, container_id, ..
            }
            | Self::SshDocker {
                ssh, container_id, ..
            }
            | Self::SshAppleContainer {
                ssh, container_id, ..
            }
            | Self::SshNitro {
                ssh, container_id, ..
            } => (Some(ssh), container_id),
            _ => return None,
        };
        Some(ContainerEndpoint {
            engine: self.container_engine().expect("container locator"),
            ssh,
            container_id,
        })
    }

    /// Container placement includes borrowed workers: each can survive its owner moving.
    pub fn cpu_accounting(&self) -> crate::cpu_usage::CpuAccountingMode {
        if self.container_engine().is_some() {
            crate::cpu_usage::CpuAccountingMode::Container
        } else {
            crate::cpu_usage::CpuAccountingMode::ProcessTree
        }
    }

    /// The target kind spelling shared with [`crate::config::TargetTemplate`].
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::LocalBare { .. } => "local-bare",
            Self::LocalPodman { .. } => "local-podman",
            Self::LocalDocker { .. } => "local-docker",
            Self::AppleContainer { .. } => "apple-container",
            Self::AwsEc2 { .. } => "aws-ec2",
            Self::SshBare { .. } => "ssh-bare",
            Self::SshPodman { .. } => "ssh-podman",
            Self::SshDocker { .. } => "ssh-docker",
            Self::SshAppleContainer { .. } => "ssh-apple-container",
            Self::SshNitro { .. } => "ssh-nitro",
        }
    }

    pub const fn container_engine(&self) -> Option<&'static str> {
        match self {
            Self::LocalPodman { .. } | Self::SshPodman { .. } => Some("podman"),
            Self::LocalDocker { .. } | Self::SshDocker { .. } => Some("docker"),
            Self::AppleContainer { .. } | Self::SshAppleContainer { .. } => Some("container"),
            Self::SshNitro { .. } => Some(MICROVM_ENGINE),
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

/// Resource generations let a Move retain its source while provisioning the destination.
pub fn move_resource_name(session_id: &str, operation_id: &str) -> Result<String> {
    let digest = Sha256::digest(operation_id.as_bytes());
    Ok(format!(
        "{}-move-{}",
        resource_name(session_id)?,
        crate::hex::lower_hex(&digest[..8])
    ))
}

pub fn resource_name_belongs_to(name: &str, session_id: &str) -> Result<bool> {
    let base = resource_name(session_id)?;
    Ok(name == base
        || name
            .strip_prefix(&format!("{base}-move-"))
            .is_some_and(|suffix| {
                suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            }))
}

pub fn podman_workspace_locator(
    template: &ContainerTemplate,
    session_id: &str,
) -> Result<PodmanWorkspaceLocator> {
    podman_workspace_locator_named(template, &resource_name(session_id)?)
}

pub fn podman_workspace_locator_named(
    template: &ContainerTemplate,
    name: &str,
) -> Result<PodmanWorkspaceLocator> {
    let resource = format!("{name}-workspace");
    match &template.workspace_storage {
        PodmanWorkspaceStorage::PodmanVolume => {
            Ok(PodmanWorkspaceLocator::Volume { name: resource })
        }
        PodmanWorkspaceStorage::HostHelper { root, helper } => {
            let root = Path::new(root);
            ensure!(
                crate::target_path::is_absolute(root),
                "Podman workspace storage root must be absolute"
            );
            ensure!(
                !helper.is_empty() && helper.iter().all(|argument| !argument.is_empty()),
                "Podman workspace storage helper must contain non-empty arguments"
            );
            Ok(PodmanWorkspaceLocator::HostPath {
                path: crate::target_path::text(&crate::target_path::join(
                    root,
                    Path::new(&resource),
                )),
                helper: helper.clone(),
                resource,
            })
        }
        PodmanWorkspaceStorage::ContainerLayer => Ok(PodmanWorkspaceLocator::ContainerLayer),
    }
}

/// The in-container workspace root a session's repositories live under.
///
/// `recorded` is the session record's `container_workspace`. A session whose
/// container predates per-session workspaces has none and keeps the legacy
/// shared `/workspace`; every session created since records
/// `/workspace/<session id>`, so a build cache shared by every container on a
/// host never sees two checkouts of one project at the same absolute path.
pub fn container_workspace_root(recorded: Option<&Path>) -> String {
    recorded.map_or_else(|| CONTAINER_WORKSPACE.to_owned(), crate::target_path::text)
}

/// The per-session container workspace recorded for a session created now.
pub fn new_container_workspace(session_id: &str) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    Ok(crate::target_path::join(
        Path::new(CONTAINER_WORKSPACE),
        Path::new(session_id),
    ))
}

/// The workspace directory an EC2 session owns in its login home.
///
/// EC2 instances are provisioned per session, so the path is decided by the
/// session id alone and is the same whether it is read from a target template
/// or rebuilt from a stored locator.
pub fn aws_workspace(session_id: &str) -> String {
    format!(".local/share/hel/workspaces/{session_id}")
}

pub fn workspace_for(template: &TargetTemplate, session_id: &str) -> Result<String> {
    validate_session_id(session_id)?;
    match template {
        TargetTemplate::LocalBare => bail!("local bare projects use their selected directory"),
        // A container workspace is per session and recorded on the session
        // record, so it is read with `container_workspace_root` instead.
        TargetTemplate::LocalPodman(_)
        | TargetTemplate::LocalDocker(_)
        | TargetTemplate::AppleContainer(_)
        | TargetTemplate::SshPodman { .. }
        | TargetTemplate::SshDocker { .. }
        | TargetTemplate::SshAppleContainer { .. }
        | TargetTemplate::SshNitro { .. } => {
            bail!("container targets use the session's recorded container workspace")
        }
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
    class: SshTrafficClass,
    locator: &TargetLocator,
    session_id: &str,
    args: Vec<String>,
    purpose: impl Into<String>,
) -> Result<CommandSpec> {
    verify_locator(locator, session_id)?;
    if args.is_empty() {
        bail!("target command must not be empty");
    }
    Ok(locator_command(class, locator, args).purpose(purpose))
}

/// Wrap a non-empty argv vector for execution at a target, without checking
/// that the locator belongs to a particular session. Every per-target command
/// is built here, so each target kind is spelled out once.
pub fn locator_command(
    class: SshTrafficClass,
    locator: &TargetLocator,
    args: Vec<String>,
) -> CommandSpec {
    match locator {
        TargetLocator::LocalBare { .. } => {
            let mut args = args.into_iter();
            let program = args.next().expect("target command must not be empty");
            CommandSpec::new(program, args)
        }
        TargetLocator::LocalPodman { container_id, .. }
        | TargetLocator::LocalDocker { container_id, .. }
        | TargetLocator::AppleContainer { container_id, .. } => container_exec(
            locator.container_engine().expect("local container"),
            container_id,
            args,
        ),
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command_owned(class, ssh, args)
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker {
            ssh, container_id, ..
        }
        | TargetLocator::SshAppleContainer {
            ssh, container_id, ..
        }
        | TargetLocator::SshNitro {
            ssh, container_id, ..
        } => {
            let remote = drained_container_exec(
                locator.container_engine().expect("remote container"),
                container_id,
                args,
            );
            let mut argv = vec![remote.program];
            argv.extend(remote.args);
            ssh_command_owned(class, ssh, argv)
        }
    }
}
pub fn worker_root(locator: &TargetLocator, session_id: &str) -> Result<String> {
    verify_locator(locator, session_id)?;
    Ok(match locator {
        TargetLocator::LocalBare { worker_root } => worker_root.clone(),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. }
        | TargetLocator::SshAppleContainer { .. }
        | TargetLocator::SshNitro { .. } => {
            format!("/var/lib/hel/workers/{session_id}")
        }
        TargetLocator::AwsEc2 { .. } => format!(".local/share/hel/workers/{session_id}"),
        TargetLocator::SshBare { worker_id, .. } => format!(
            ".local/share/hel/workers/{}",
            worker_id.as_deref().unwrap_or(session_id)
        ),
    })
}
mod convert;
pub use convert::{
    RecordedTarget, StoredTarget, TargetConversionError, locator_kind_name,
    locator_needs_connection, ssh_args_with_identity,
};

mod ssh;
pub use ssh::*;

pub fn container_exec(
    engine: &str,
    container_id: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut command_args = vec!["exec".to_owned(), "-i".to_owned(), container_id.to_owned()];
    command_args.extend(args.into_iter().map(Into::into));
    engine_command(engine, command_args)
}

/// A container exec run on a remote host over SSH, through the output drain
/// below. Only SSH targets need it: there the drain runs on the remote host
/// and outlives a cancelled local ssh, whereas a local target's cancellation
/// kills the whole process group, drain included, and a local controller
/// (Windows) may have no POSIX shell to run it.
pub fn drained_container_exec(
    engine: &str,
    container_id: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let exec = container_exec(engine, container_id, args);
    let mut wrapped = vec![
        "-c".to_owned(),
        CONTAINER_EXEC_DRAIN.to_owned(),
        "exec-drain".to_owned(),
        exec.program,
    ];
    wrapped.extend(exec.args);
    CommandSpec::new("sh", wrapped)
}

// Podman's attached client removes its exec record only on normal exit. A broken
// SSH output pipe kills it with SIGPIPE and leaks that record. The host-side
// POSIX drain preserved both streams and status and prevented leaks in all 40
// cancellation trials on morannon (2026-11-07). Keep draining after EPIPE.
// An infinite command retains its temporary status directory until it ends.
const CONTAINER_EXEC_DRAIN: &str = r#"status_dir=$(mktemp -d) || exit 125
trap 'rm -rf "$status_dir"' 0
drain() {
    trap '' PIPE
    cat
    cat >/dev/null
}
{
    {
        "$@"
        printf '%s\n' "$?" >"$status_dir/status"
    } 2>&1 1>&3 | drain >&2
} 3>&1 | drain
if [ -r "$status_dir/status" ]; then
    read -r status <"$status_dir/status"
    exit "$status"
fi
exit 125
"#;

#[cfg(all(test, unix))]
mod executor_tests {
    #[test]
    #[cfg(unix)]
    fn step_script_reports_partial_failure_and_runs_cleanup() {
        use super::*;
        let directory = tempfile::tempdir().unwrap();
        let cleaned = directory.path().join("cleanup 'with spaces");
        let skipped = directory.path().join("must not run");
        let argv = step_script([
            CommandStep::new("absent Git key", ["sh", "-c", "exit 5"]).accepted_statuses([0, 5]),
            CommandStep::new(
                "failing middle step",
                ["sh", "-c", "echo precise-error >&2; exit 7"],
            ),
            CommandStep::new("later operation", ["touch", skipped.to_str().unwrap()]),
            CommandStep::new("remove staging", ["touch", cleaned.to_str().unwrap()]).cleanup(),
            CommandStep::new(
                "optional cleanup",
                ["sh", "-c", "echo cleanup-error >&2; exit 9"],
            )
            .best_effort_cleanup(),
        ]);
        let command = CommandSpec::new(&argv[0], argv[1..].iter().cloned()).purpose("batch");
        let output = ProcessExecutor.execute(&command).unwrap();
        assert_eq!(output.status, 7);
        let warnings = step_script_warnings(&output);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("optional cleanup failed with status 9"));
        assert!(warnings[0].contains("cleanup-error"));
        let error = checked_command_output(&command, output)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("failing middle step failed with status 7: precise-error"),
            "{error}"
        );
        assert!(cleaned.exists());
        assert!(!skipped.exists());

        // Cleanup failure remains visible even after successful publication.
        let argv = step_script([CommandStep::new(
            "optional discard",
            ["sh", "-c", "echo discard-error >&2; exit 9"],
        )
        .best_effort_cleanup()]);
        let command = CommandSpec::new(&argv[0], argv[1..].iter().cloned());
        let output = ProcessExecutor.execute(&command).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            step_script_warnings(&output),
            ["optional discard failed with status 9: discard-error"]
        );
    }

    use std::fs;

    use super::*;

    // Hard-won: leaked exec records: a broken SSH pipe killed the attached client.
    #[test]
    fn container_exec_drain_preserves_streams_stdin_and_status() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("podman");
        // Never exec a file this multi-threaded test just wrote (ETXTBSY).
        crate::test_hooks::install_fake_command(
            temp.path(),
            "podman",
            "shift 3\ncat\nprintf 'stderr bytes\\n' >&2\nexit \"$1\"\n",
        );
        for status in [0, 37, 23, 255] {
            let plain = CommandSpec::new(
                fake.to_str().unwrap(),
                ["exec", "-i", "ctr", &status.to_string()],
            );
            let wrapped =
                drained_container_exec(fake.to_str().unwrap(), "ctr", [status.to_string()]);
            let input = vec![b'x'; 204800];
            let expected = ProcessExecutor
                .execute_with_stdin(&plain, &mut std::io::Cursor::new(&input))
                .unwrap();
            let actual = ProcessExecutor
                .execute_with_stdin(&wrapped, &mut std::io::Cursor::new(&input))
                .unwrap();
            assert_eq!(actual.stdout, expected.stdout);
            assert_eq!(actual.stderr, expected.stderr);
            assert_eq!(actual.status, expected.status);
            assert_eq!(actual.stdout.len(), input.len());
            let mut argv = vec![wrapped.program.clone()];
            argv.extend(wrapped.args.clone());
            let marked = CommandSpec::new(
                "sh",
                [
                    "-c".to_owned(),
                    with_start_marker(&remote_command_line(&argv)),
                ],
            );
            let marked = ProcessExecutor
                .execute_with_stdin(&marked, &mut std::io::Cursor::new(&input))
                .unwrap();
            assert_eq!(
                marked.stdout,
                [SSH_START_MARKER.as_bytes(), actual.stdout.as_slice()].concat()
            );
            assert_eq!(marked.stderr, actual.stderr);
            assert_eq!(marked.status, actual.status);
        }
    }

    // Hard-won: leaked exec records: output cancellation must let the client exit normally.
    // Hard-won: 1e324ede0: BSD head rejects zero-byte readers in this fixture.
    #[test]
    fn container_exec_drain_survives_closed_downstream_pipes() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("podman");
        crate::test_hooks::install_fake_command(
            temp.path(),
            "podman",
            "set -e\nshift 3\nhead -c 204800 /dev/zero\nhead -c 204800 /dev/zero >&2\nprintf done > \"$1\"\n",
        );
        let marker = temp.path().join("normal-exit");
        let wrapped =
            drained_container_exec(fake.to_str().unwrap(), "ctr", [marker.to_str().unwrap()]);
        let mut argv = vec![wrapped.program];
        argv.extend(wrapped.args);
        let line = format!(
            "{{ {} 2>&1 1>&3 | true; }} 3>&1 | true",
            remote_command_line(&argv)
        );
        let result = ProcessExecutor
            .execute(&CommandSpec::new("sh", ["-c", &line]))
            .unwrap();
        assert_eq!(result.status, 0);
        assert_eq!(fs::read_to_string(marker).unwrap(), "done");
    }

    /// The retry backoff override is process-wide, so tests that shorten it
    /// take turns: one test restoring the real delay mid-run would stretch
    /// another's retries past its deadline.
    use fake_ssh::FastRetries;

    #[cfg(target_os = "linux")]
    #[test]
    fn successful_commands_stop_descendants_that_closed_their_pipes() {
        for streamed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("descendant");
            let command = CommandSpec::new("sh", [
                "-c".to_owned(),
                "cat >/dev/null; head -c 131072 /dev/zero; sleep 60 </dev/null >/dev/null 2>&1 & echo $! > \"$1\"".to_owned(),
                "owned-descendant".to_owned(), pid_file.display().to_string(),
            ]);
            let executor = CancellableProcessExecutor::with_timeout(Duration::from_secs(5));
            let output = if streamed {
                executor
                    .execute_with_stdin(&command, &mut std::io::Cursor::new(vec![b'x'; 256 * 1024]))
            } else {
                executor.execute(&command)
            }
            .unwrap();
            assert_eq!(output.stdout.len(), 131072);
            let pid: i32 = fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = fs::read_to_string(format!("/proc/{pid}/stat")).ok();
                if state.as_ref().is_none_or(|state| {
                    state
                        .rsplit_once(") ")
                        .is_some_and(|(_, fields)| fields.starts_with('Z'))
                }) {
                    break;
                }
                if Instant::now() >= deadline {
                    // Do not leave the fixture behind when the regression fails.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    panic!(
                        "successful command left descendant {pid} running (streamed={streamed})"
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// B-2 at the executor: a backgrounded descendant that holds the output
    /// pipes must not hold the completed command past the drain.
    // Hard-won: c86b2123: A target command stayed Running until a background descendant closed inherited pipes.
    #[cfg(target_os = "linux")]
    #[test]
    fn commands_complete_at_leader_exit_when_a_descendant_holds_the_pipes() {
        for streamed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("descendant");
            let command = CommandSpec::new(
                "sh",
                [
                    "-c".to_owned(),
                    "sleep 300 & echo $! > \"$1\"; echo hi; exit 3".to_owned(),
                    "held-pipes".to_owned(),
                    pid_file.display().to_string(),
                ],
            );
            let executor = CancellableProcessExecutor::with_timeout(Duration::from_secs(30));
            let started = Instant::now();
            let output = if streamed {
                executor.execute_with_stdin(&command, &mut std::io::Cursor::new(b"input".to_vec()))
            } else {
                executor.execute(&command)
            }
            .unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "streamed={streamed} took {:?}",
                started.elapsed()
            );
            assert_eq!(output.status, 3);
            assert_eq!(output.stdout, b"hi\n");
            let pid: i32 = fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = fs::read_to_string(format!("/proc/{pid}/stat")).ok();
                if state.as_ref().is_none_or(|state| {
                    state
                        .rsplit_once(") ")
                        .is_some_and(|(_, fields)| fields.starts_with('Z'))
                }) {
                    break;
                }
                if Instant::now() >= deadline {
                    // SAFETY: cleans up this test's own fixture.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    panic!("completed command left descendant {pid} (streamed={streamed})");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn streamed_deadline_survives_leader_exit_and_inherited_pipes() {
        let command = CommandSpec::new(
            "sh",
            [
                "-c",
                "head -c 131072 /dev/zero; cat >/dev/null; (trap '' TERM; sleep 60) & exit 0",
            ],
        );
        let mut input = std::io::Cursor::new(vec![b'x'; 256 * 1024]);
        let started = Instant::now();
        let error = CancellableProcessExecutor::with_timeout(Duration::from_millis(300))
            .execute_with_stdin(&command, &mut input)
            .unwrap_err();
        assert!(
            error.to_string().contains("timed out after 0.3 s while"),
            "{error:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn executor_deadlines_name_the_duration_and_purpose_without_relabelling_cancellation() {
        let command = CommandSpec::new("sh", ["-c", "sleep 30"]).purpose("stage a worker");
        let error = CancellableProcessExecutor::with_timeout(Duration::from_millis(50))
            .execute(&command)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "timed out after 0.05 s while stage a worker"
        );
        let cancelled = Arc::new(AtomicBool::new(true));
        let error = CancellableProcessExecutor::new(cancelled)
            .with_deadline(Duration::ZERO)
            .execute(&command)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "operation cancelled while stage a worker"
        );
    }

    #[test]
    fn cleanup_timeout_replaces_an_expired_deadline_and_keeps_shared_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("completed");
        let command = CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                "printf ran > \"$1\"".to_owned(),
                "cleanup-deadline".to_owned(),
                marker.display().to_string(),
            ],
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let executor =
            CancellableProcessExecutor::new(cancelled.clone()).with_deadline(Duration::ZERO);

        let output = executor
            .execute_cleanup_with_timeout(&command, Duration::from_secs(5))
            .unwrap();

        assert_eq!(output.status, 0);
        assert_eq!(fs::read_to_string(marker).unwrap(), "ran");

        cancelled.store(true, Ordering::Release);
        let error = executor
            .execute_cleanup_with_timeout(&command, Duration::from_secs(5))
            .unwrap_err();
        assert!(
            error.to_string().contains("operation cancelled"),
            "{error:#}"
        );
    }

    /// A stand-in for `ssh` that is refused by the server on its first call and
    /// connects on the next, the way a host at its `MaxStartups` ceiling
    /// behaves once the daemon's burst drains.
    ///
    /// The count is written to a scratch file and renamed into place, so a
    /// deadline that kills the script mid-write never leaves an empty counter.
    fn flaky_ssh_script(directory: &Path, refusals: u32) -> CommandSpec {
        let counter = directory.join("attempts");
        let script = format!(
            "count=$(cat {counter} 2>/dev/null || echo 0)\n\
             echo $((count + 1)) > {counter}.tmp && mv {counter}.tmp {counter}\n\
             if [ \"$count\" -lt {refusals} ]; then\n\
             echo 'kex_exchange_identification: Connection closed by 10.0.0.1 port 22' >&2\n\
             exit 255\n\
             fi\n\
             echo connected\n",
            counter = counter.display(),
            refusals = refusals
        );
        CommandSpec::new("sh", ["-c".to_owned(), script])
            .ssh_destination("build@10.0.0.1")
            .purpose("run the flaky SSH fixture")
    }

    fn attempts(directory: &Path) -> u32 {
        fs::read_to_string(directory.join("attempts"))
            .expect("the fixture records its attempts")
            .trim()
            .parse()
            .expect("attempt count is a number")
    }

    /// An executor that models `ssh` masters and the sessions on them. The
    /// first session finds its master dead (it was killed after the ledger
    /// last checked it), which `ssh` reports the way a session guarded with
    /// `ProxyCommand=false` does.
    #[derive(Default)]
    struct MasterKilledOnce {
        running: std::cell::RefCell<BTreeSet<String>>,
        openers: std::cell::Cell<usize>,
        sessions: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl CommandExecutor for MasterKilledOnce {
        fn pause_deadline(&self) -> DeadlinePause {
            DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &crate::targets::CommandSpec,
        ) -> anyhow::Result<crate::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &crate::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<crate::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let reply = |status: i32, stderr: &str| CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            };
            if command.ssh_session.is_some() {
                // A session. The first one arrives just after its master died.
                return with_ssh_refusal_retry(command, self, None, |spawned, _gate| {
                    self.sessions.borrow_mut().push(spawned.args.clone());
                    if self.sessions.borrow().len() == 1 {
                        self.running.borrow_mut().clear();
                        return Ok(reply(255, "Connection closed by UNKNOWN port 65535"));
                    }
                    Ok(reply(0, ""))
                });
            }
            // The first session on a master the ledger just opened.
            if command.args.contains(&"ProxyCommand=false".to_owned()) {
                return Ok(reply(0, ""));
            }
            let socket = command
                .args
                .iter()
                .find_map(|arg| arg.strip_prefix("ControlPath="))
                .expect("a master command names its socket")
                .to_owned();
            if command.args.windows(2).any(|pair| pair == ["-O", "check"]) {
                return Ok(reply(
                    if self.running.borrow().contains(&socket) {
                        0
                    } else {
                        255
                    },
                    "",
                ));
            }
            assert!(command.args.contains(&"ControlMaster=yes".to_owned()));
            self.openers.set(self.openers.get() + 1);
            self.running.borrow_mut().insert(socket);
            Ok(reply(0, ""))
        }
    }

    #[test]
    fn a_session_whose_master_died_is_retried_on_a_reopened_master() {
        let _guard = ssh::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fast_retries = FastRetries::new();
        let socket_dir = tempfile::tempdir_in("/tmp").expect("short socket directory");
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let ssh = SshTarget {
            destination: "master-killed-once-host".to_owned(),
            ssh_args: Vec::new(),
        };
        let executor = MasterKilledOnce::default();
        let output = executor.execute(&ssh_command(SshTrafficClass::Control, &ssh, ["true"]));
        set_ssh_connection_sharing_for_test(None);

        assert_eq!(output.expect("the retry succeeds").status, 0);
        let sessions = executor.sessions.borrow();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            executor.openers.get(),
            2,
            "the retry reopens the master instead of trusting the earlier check"
        );
        for args in sessions.iter() {
            assert_eq!(args[..6][5], "ProxyCommand=false", "{args:?}");
        }
    }

    // Hard-won: e6ed54ed: sshd MaxStartups rejected concurrent session handshakes and users saw dead workers and failed closes.
    #[test]
    fn a_transport_rejected_ssh_command_is_retried_once_and_then_succeeds() {
        let _fast_retries = FastRetries::new();
        let directory = tempfile::tempdir().expect("temp dir");
        let command = flaky_ssh_script(directory.path(), 1);

        let output = ProcessExecutor
            .execute(&command)
            .expect("the retry must reach the successful attempt");

        assert_eq!(output.status, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "connected");
        assert_eq!(attempts(directory.path()), 2);
    }

    // Hard-won: e6ed54ed: sshd MaxStartups rejected remote operations during daemon startup and they were falsely reported as failed.
    #[test]
    fn the_cancellable_executor_also_retries_a_transport_rejection() {
        let _fast_retries = FastRetries::new();
        let directory = tempfile::tempdir().expect("temp dir");
        let command = flaky_ssh_script(directory.path(), 1);

        let output = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
            .execute(&command)
            .expect("the retry must reach the successful attempt");

        assert_eq!(output.status, 0);
        assert_eq!(attempts(directory.path()), 2);
    }

    /// A host at its `MaxStartups` ceiling can keep dropping connections for
    /// longer than any fixed budget. The command never ran, so a caller that
    /// can be cancelled keeps coming back until it gets in.
    #[test]
    fn a_cancellable_caller_retries_pre_authentication_refusals_until_it_gets_in() {
        let _fast_retries = FastRetries::new();
        let directory = tempfile::tempdir().expect("temp dir");
        let command = flaky_ssh_script(directory.path(), 7);

        let output = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
            .execute(&command)
            .expect("the seventh attempt gets in");

        assert_eq!(output.status, 0);
        assert_eq!(attempts(directory.path()), 8);
    }

    /// A permanent block (a firewall or gateway that always closes the
    /// connection) looks like `MaxStartups`. The retries end at the caller's
    /// deadline when it has one, and after the persistent-refusal bound for
    /// every caller, cancellable or not, with an error that names the host and
    /// the possible causes.
    // Hard-won: 5cb64e019: pre-authentication refusals were retried until cancellation, so a cancellable caller with no deadline (Move, session creation) retried a permanent block forever.
    #[test]
    fn pre_authentication_retries_end_at_the_deadline_or_the_time_bound() {
        let _fast_retries = FastRetries::new();
        let directory = tempfile::tempdir().expect("temp dir");
        let command = flaky_ssh_script(directory.path(), u32::MAX);
        let error = CancellableProcessExecutor::with_timeout(Duration::from_secs(2))
            .execute(&command)
            .expect_err("a server that never lets the command in ends at the deadline");
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
        assert!(attempts(directory.path()) > 3);

        set_ssh_persistent_refusal_for_test(Some(Duration::from_millis(400)));
        for cancellable in [true, false] {
            let directory = tempfile::tempdir().expect("temp dir");
            let command = flaky_ssh_script(directory.path(), u32::MAX);
            let started = Instant::now();
            let error = if cancellable {
                CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false))).execute(&command)
            } else {
                ProcessExecutor.execute(&command)
            }
            .expect_err("a server that never lets the command in fails after the bound");
            let message = format!("{error:#}");
            let elapsed = started.elapsed();
            assert!(elapsed < Duration::from_secs(10), "{message}");
            for needle in ["build@10.0.0.1", "MaxStartups", "firewall", "kex_exchange"] {
                assert!(message.contains(needle), "{needle} missing: {message}");
            }
            // Each attempt spawns several processes, so how many fit in the
            // bound depends on the machine. Check that the caller retried and
            // that the bound, not an attempt count, ended the retries.
            assert!(
                attempts(directory.path()) >= 2,
                "cancellable: {cancellable}, retried within the bound"
            );
            assert!(
                elapsed >= Duration::from_millis(400),
                "cancellable: {cancellable}, stopped before the bound after {elapsed:?}"
            );
        }
    }

    /// A fail-fast probe (target validation, `mj doctor`) fails within its own
    /// connect timeout when the server closes every connection before
    /// authentication, instead of retrying for the persistent-refusal bound
    /// of a minute like a command that has to get in.
    // Hard-won: ca6580092: probes retried pre-authentication refusals for the full 60 s bound.
    #[test]
    fn a_probe_gives_a_refusal_no_more_time_than_its_connect_timeout() {
        let _fast_retries = FastRetries::new();
        let directory = tempfile::tempdir().expect("temp dir");
        let mut command = flaky_ssh_script(directory.path(), u32::MAX);
        command.ssh_session_probe = true;
        let started = Instant::now();
        let error = ProcessExecutor
            .execute(&command)
            .expect_err("a server that always closes fails the probe");
        let elapsed = started.elapsed();
        let message = format!("{error:#}");
        assert!(message.contains("build@10.0.0.1"), "{message}");
        assert!(message.contains("firewall"), "{message}");
        assert!(
            elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(15),
            "{elapsed:?}"
        );
        assert!(attempts(directory.path()) > 3, "retried within the bound");
    }

    /// Pins both defects of an exit-255 rule without a start proof: a command
    /// that ran and printed its marker, then lost its connection (exit 255
    /// with "Connection reset" on stderr), was retried as a transport
    /// rejection and so could run twice.
    // Hard-won: 5cb64e019: exit 255 after the command started was retried as a refusal.
    #[test]
    fn a_session_that_printed_the_marker_is_never_retried_even_at_exit_255() {
        let _guard = ssh::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fast_retries = FastRetries::new();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        for cancellable in [false, true] {
            let fake = fake_ssh::FakeSsh::new(&["run-then-255"]);
            let command = fake.command(&["echo", "ran"]);
            let output = if cancellable {
                CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false))).execute(&command)
            } else {
                ProcessExecutor.execute(&command)
            }
            .expect("ssh runs");
            assert_eq!(output.status, 255);
            assert_eq!(output.stdout, b"ran\n", "the marker is stripped");
            assert_eq!(fake.attempts(), 1, "cancellable: {cancellable}");
        }
        set_ssh_connection_sharing_for_test(None);
    }

    /// A streamed command the server refuses is retried like any other, and
    /// the whole input (more than a pipe buffers) arrives once, on the attempt
    /// that started: nothing is read from the caller's stream before that.
    // Hard-won: 5cb64e019: streamed commands returned the refusal instead of retrying.
    #[test]
    fn a_refused_streamed_command_is_retried_and_receives_all_its_input_once() {
        let _guard = ssh::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fast_retries = FastRetries::new();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let payload = vec![b'x'; 300 * 1024];
        for cancellable in [false, true] {
            let fake = fake_ssh::FakeSsh::new(&["refuse-session", "refuse-auth"]);
            let activity = Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = activity.clone();
            let command = fake.command(&["wc", "-c"]).observe(move |event| {
                observed.lock().unwrap().push(event);
            });
            let mut input = std::io::Cursor::new(payload.clone());
            let output = if cancellable {
                CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
                    .execute_with_stdin(&command, &mut input)
            } else {
                ProcessExecutor.execute_with_stdin(&command, &mut input)
            }
            .expect("the third attempt starts");
            assert_eq!(
                output.status,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                payload.len().to_string()
            );
            assert_eq!(fake.attempts(), 3, "cancellable: {cancellable}");
            let activity = activity.lock().unwrap();
            let [
                CommandActivity::StdinStarted,
                writes @ ..,
                CommandActivity::StdinFinished,
            ] = &activity[..]
            else {
                panic!("only the admitted attempt should report stdin activity");
            };
            let bytes: usize = writes
                .iter()
                .map(|event| match event {
                    CommandActivity::StdinBytes(count) => *count,
                    _ => panic!("unexpected activity inside the admitted attempt"),
                })
                .sum();
            assert!(writes.len() > 1);
            assert_eq!(bytes, payload.len());
        }
        set_ssh_connection_sharing_for_test(None);
    }

    // Hard-won: #1583: a command still running after EOF was reported as a stalled stdin stream.
    #[test]
    fn streamed_stdin_finishes_before_the_command_exits() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        let (finished, watch) = std::sync::mpsc::channel();
        let command =
            CommandSpec::new("sh", ["-c", "cat >/dev/null; sleep 600"]).observe(move |event| {
                if matches!(event, CommandActivity::StdinFinished)
                    && let Err(error) = finished.send(())
                {
                    tracing::debug!(%error, "test stopped waiting for stdin completion");
                }
            });
        let child = std::thread::spawn(move || {
            let mut input = std::io::Cursor::new(vec![b'x'; 128 * 1024]);
            executor.execute_with_stdin(&command, &mut input)
        });
        let ended = watch.recv_timeout(Duration::from_secs(10));
        let pending = !child.is_finished();
        cancelled.store(true, Ordering::Release);
        let outcome = child.join().unwrap();
        ended.expect("stdin completion should arrive while the command is still running");
        assert!(pending);
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("cancelled"), "{error}");
    }

    #[test]
    fn file_output_preserves_large_ssh_stdout_across_refusals_and_cancels() {
        let _guard = ssh::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fast_retries = FastRetries::new();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let fake = fake_ssh::FakeSsh::new(&["refuse-session", "refuse-auth"]);
        let command = fake.command(&[
            "sh",
            "-c",
            "head -c 307200 /dev/zero; printf diagnostic >&2",
        ]);
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"old output that must be truncated")
            .unwrap();
        let output = CancellableProcessExecutor::new(Default::default())
            .execute_to_file(&command, &file)
            .unwrap();
        assert_eq!(output.status, 0);
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"diagnostic");
        assert_eq!(file.metadata().unwrap().len(), 307200);
        assert_eq!(fake.attempts(), 3);
        use std::io::{Seek as _, SeekFrom};
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut received = Vec::new();
        file.read_to_end(&mut received).unwrap();
        assert_eq!(received, vec![0; 307200]);
        set_ssh_connection_sharing_for_test(None);

        let token = tokio_util::sync::CancellationToken::new();
        let executor =
            CancellableProcessExecutor::new(Default::default()).with_cancellation(token.clone());
        let file = tempfile::tempfile().unwrap();
        let probe = file.try_clone().unwrap();
        let pending = std::thread::spawn(move || {
            executor.execute_to_file(
                &CommandSpec::new("sh", ["-c", "head -c 307200 /dev/zero; sleep 600"]),
                &file,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while probe.metadata().unwrap().len() < 307200 {
            assert!(
                Instant::now() < deadline,
                "child did not produce its output"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        token.cancel();
        assert!(
            pending
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }

    // Hard-won: #1544: a failed output spool was observed only after the child exited.
    #[test]
    fn file_output_failure_terminates_a_child_that_keeps_running() {
        const CHILD: &str = "MJ_TEST_FETCH_WRITE_FAILURE";
        if std::env::var_os(CHILD).is_some() {
            // Restrict only this test subprocess's regular-file writes. Pipe
            // writes are unaffected, so the producer can still fill stdout.
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                let limit = libc::rlimit {
                    rlim_cur: 128 * 1024,
                    rlim_max: 128 * 1024,
                };
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
            let file = tempfile::tempfile().unwrap();
            let error = CancellableProcessExecutor::new(Default::default())
                .execute_to_file(
                    &CommandSpec::new("sh", ["-c", "head -c 1048576 /dev/zero; sleep 600"]),
                    &file,
                )
                .unwrap_err();
            assert!(
                error.chain().any(|cause| cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.raw_os_error() == Some(libc::EFBIG))),
                "{error:#}"
            );
            return;
        }
        let mut command = CommandSpec::new(
            std::env::current_exe().unwrap().to_string_lossy(),
            [
                "--exact",
                "targets::executor_tests::file_output_failure_terminates_a_child_that_keeps_running",
                "--nocapture",
            ],
        );
        command.env.insert(CHILD.into(), "1".into());
        let output = CancellableProcessExecutor::with_timeout(Duration::from_secs(10))
            .execute(&command)
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `scp` exits 1, not ssh's 255, while relaying the refusal of the `ssh` it
    /// runs; the copy is still retried instead of failing.
    // Hard-won: af9848339: scp's own exit status hid an SSH session refusal from the retry
    #[test]
    fn an_scp_that_relays_a_refused_session_is_retried() {
        let _guard = ssh::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fast_retries = FastRetries::new();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let directory = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
dir=$(dirname "$0")
count=$(cat "$dir/attempts" 2>/dev/null || echo 0)
echo $((count + 1)) > "$dir/attempts"
if [ "$count" -eq 0 ]; then
  echo 'mux_client_request_session: session request failed: Session open refused by peer' >&2
  echo 'scp: Connection closed' >&2
  exit 1
fi
exit 0
"#;
        crate::test_hooks::install_fake_command(directory.path(), "scp", script);
        let ssh = SshTarget {
            destination: "fake-host".to_owned(),
            ssh_args: Vec::new(),
        };
        let mut command = ssh::scp_download(&ssh, "remote-file", "local-file");
        let path = std::env::var("PATH").unwrap_or_default();
        command.env.insert(
            "PATH".to_owned(),
            format!("{}:{path}", directory.path().display()),
        );

        let output = ProcessExecutor.execute(&command).expect("scp runs");

        assert_eq!(output.status, 0);
        let attempts = std::fs::read_to_string(directory.path().join("attempts")).unwrap();
        assert_eq!(attempts.trim(), "2");
        set_ssh_connection_sharing_for_test(None);
    }

    /// A caller's sensitive stdin is part of the command, so each attempt
    /// sends it afresh.
    #[test]
    fn sensitive_stdin_is_sent_again_on_each_retry() {
        let _guard = ssh::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fast_retries = FastRetries::new();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let fake = fake_ssh::FakeSsh::new(&["refuse-session"]);
        let command = fake
            .command(&["cat"])
            .with_sensitive_stdin(b"secret\n".to_vec());
        let output = ProcessExecutor.execute(&command).expect("ssh runs");
        assert_eq!(output.stdout, b"secret\n");
        assert_eq!(fake.attempts(), 2);
        set_ssh_connection_sharing_for_test(None);
    }

    // Hard-won: #1671: closing SSH stdin before the mirror finishes cancels it.
    #[cfg(unix)]
    #[test]
    fn keep_stdin_open_allows_normal_completion() {
        let command = CommandSpec::new(
            "sh",
            [
                "-c",
                r#"
                exec 3<&0
                (while IFS= read -r _unused; do :; done; printf early-eof) <&3 &
                watcher=$!
                dd if=/dev/zero bs=1024 count=128 2>/dev/null
                sleep 0.1
                kill "$watcher" || exit 1
                wait "$watcher" 2>/dev/null || :
            "#,
            ],
        )
        .keep_stdin_open();
        let cancellable =
            CancellableProcessExecutor::new(Arc::default()).with_deadline(Duration::from_secs(5));
        for executor in [&ProcessExecutor as &dyn CommandExecutor, &cancellable] {
            let output = executor.execute(&command).unwrap();
            assert_eq!(output.status, 0);
            assert_eq!(output.stdout.len(), 128 * 1024);
        }
    }

    // Hard-won: #1671: killing the local SSH client first orphaned remote Git.
    #[cfg(unix)]
    #[test]
    fn keep_stdin_open_cancel_waits_for_eof_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let cleaned = directory.path().join("cleaned");
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let command = CommandSpec::new(
            "sh",
            [
                "-c",
                r#"
                sleep 60 &
                worker=$!
                trap 'wait "$worker" 2>/dev/null || :; printf "%s" "$worker" > "$1"; exit 0' TERM
                exec 3<&0
                (cat >/dev/null; kill -s TERM 0) <&3 &
                printf ready
                wait "$worker"
            "#,
                "sh",
                cleaned.to_str().unwrap(),
            ],
        )
        .keep_stdin_open()
        .with_stream_reply(move |bytes| {
            if bytes.is_some() {
                flag.store(true, Ordering::Release);
            }
        });
        let error = CancellableProcessExecutor::new(cancelled.clone())
            .with_deadline(Duration::from_secs(5))
            .execute(&command)
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(cancelled.load(Ordering::Acquire));
        let pid = std::fs::read_to_string(cleaned).unwrap();
        assert!(process_gone(pid.parse().unwrap()));
    }

    #[cfg(unix)]
    fn process_gone(pid: libc::pid_t) -> bool {
        // SAFETY: signal zero checks existence without sending a signal.
        (unsafe { libc::kill(pid, 0) == -1 })
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    #[cfg(unix)]
    #[test]
    fn keep_stdin_open_cancel_reaps_a_child_that_ignores_eof() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let command = CommandSpec::new(
            "sh",
            [
                "-c",
                r#"printf '%s' "$$" > "$1"; printf ready; exec sleep 60"#,
                "sh",
                pid_file.to_str().unwrap(),
            ],
        )
        .keep_stdin_open()
        .with_stream_reply(move |bytes| {
            if bytes.is_some() {
                flag.store(true, Ordering::Release);
            }
        });
        let started = Instant::now();
        let result = CancellableProcessExecutor::new(cancelled.clone())
            .with_deadline(Duration::from_secs(5))
            .execute(&command);
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(started.elapsed() >= REMOTE_CANCEL_GRACE);
        let pid = std::fs::read_to_string(pid_file).unwrap();
        assert!(process_gone(pid.parse().unwrap()));
    }

    /// A mid-stream stdin read error must kill the child before closing stdin so
    /// a length-less writer never publishes a truncated file on EOF.
    #[cfg(unix)]
    #[test]
    fn streamed_stdin_read_error_terminates_child_before_eof() {
        struct FailAfter {
            remaining: usize,
        }
        impl Read for FailAfter {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "simulated upstream disconnect",
                    ));
                }
                let n = buf.len().min(self.remaining);
                buf[..n].fill(b'x');
                self.remaining -= n;
                Ok(n)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let published = dir.path().join("published.bin");
        // Child reads stdin to EOF and only creates `published.bin` if it sees EOF.
        let script = format!("cat >/dev/null && printf done > '{}'", published.display());
        let command = CommandSpec::new("sh", vec!["-c".into(), script]);
        let mut reader = FailAfter {
            remaining: 128 * 1024,
        };
        let executor = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)));
        let result = executor.execute_with_stdin(&command, &mut reader);
        assert!(result.is_err(), "expected stdin read error to fail command");
        assert!(
            !published.exists(),
            "child must be killed before seeing EOF on stdin read error"
        );
    }

    #[cfg(unix)]
    #[test]
    fn abortable_stdin_unblocks_on_early_child_exit_and_cancellation() {
        struct BlockingReader {
            aborted: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
            entered: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }
        impl Read for BlockingReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                {
                    let mut started = self.entered.0.lock().unwrap();
                    *started = true;
                    self.entered.1.notify_all();
                }
                let mut aborted = self.aborted.0.lock().unwrap();
                while !*aborted {
                    aborted = self.aborted.1.wait(aborted).unwrap();
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "stdin aborted",
                ))
            }
        }

        // 1. Child exits non-zero before reading stdin while reader blocks.
        let aborted = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let entered = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut reader = BlockingReader {
            aborted: aborted.clone(),
            entered,
        };
        let abort_fn = || {
            let mut flag = aborted.0.lock().unwrap();
            *flag = true;
            aborted.1.notify_all();
        };
        let command = CommandSpec::new("sh", ["-c", "echo already-exists >&2; exit 7"]);
        let executor = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)));
        let output = executor
            .execute_with_abortable_stdin(&command, &mut reader, &abort_fn)
            .expect("non-zero child exit returns CommandOutput without hanging");
        assert_eq!(output.status, 7);
        assert!(String::from_utf8_lossy(&output.stderr).contains("already-exists"));

        // 2. Cancellation fires while child is waiting on stdin and reader blocks.
        let aborted = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let entered = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let entered_cancel = entered.clone();
        let cancel_trigger = cancelled.clone();
        let canceller = std::thread::spawn(move || {
            let mut started = entered_cancel.0.lock().unwrap();
            while !*started {
                started = entered_cancel.1.wait(started).unwrap();
            }
            cancel_trigger.store(true, Ordering::Release);
        });
        let mut reader = BlockingReader {
            aborted: aborted.clone(),
            entered,
        };
        let abort_fn = || {
            let mut flag = aborted.0.lock().unwrap();
            *flag = true;
            aborted.1.notify_all();
        };
        let command = CommandSpec::new("sh", ["-c", "cat >/dev/null"]);
        let executor = CancellableProcessExecutor::new(cancelled);
        let result = executor.execute_with_abortable_stdin(&command, &mut reader, &abort_fn);
        canceller.join().unwrap();
        assert!(result.is_err(), "cancellation must return error promptly");
    }
}

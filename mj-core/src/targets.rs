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
pub mod storage;

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
            Self::WaitingForWorkerStore => "Waiting for the worker binary store".into(),
            Self::WaitingForSshConnection => "Waiting for an SSH connection to open".into(),
            Self::WaitingForStartSlot => "Waiting for a container start slot".into(),
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
struct SensitiveCommandInput(Vec<u8>);

impl std::fmt::Debug for SensitiveCommandInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

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
    pub ssh_session: Option<SshTarget>,
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
    /// The command works on the daemon's own data (its checkpoint archives,
    /// its store), so it runs where the daemon runs rather than on the node.
    /// Executor-local, never serialized.
    #[serde(skip)]
    in_daemon: bool,
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
            in_daemon: false,
        }
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
    pub fn ssh_session(mut self, ssh: &SshTarget) -> Self {
        self.ssh_destination = Some(ssh.destination.clone());
        self.ssh_session = Some(ssh.clone());
        self
    }

    /// Record that this command is a fail-fast `ssh` probe, such as a target
    /// validation, that joins `ssh`'s shared connection without ever opening
    /// a master. Its short timeouts and keepalives belong to the probe alone:
    /// a master opened by it would impose them on every later session. The
    /// probe still uses one of the master's sessions while it runs, so the
    /// executor counts it in the session ledger like any other session.
    pub fn ssh_probe_session(mut self, ssh: &SshTarget) -> Self {
        self = self.ssh_session(ssh);
        self.ssh_session_probe = true;
        self
    }

    /// Lease the SSH session this command asks for and return the command as
    /// it must be spawned, with the lease that must outlive the child.
    ///
    /// An `ssh` command's remote line is prefixed to print [`SSH_START_MARKER`]
    /// first. The reader of its stdout must strip it and report the start to
    /// the lease's [`StartGate`] (see [`SessionCommand::gate`]); the executors,
    /// the node and `spawn_session_proxy` do. A command without a session
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
        let Some(ssh) = &self.ssh_session else {
            return Ok(SessionCommand {
                command: std::borrow::Cow::Borrowed(self),
                lease: None,
            });
        };
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
            SshSessions::lease(ssh, executor, walk)?
        };
        let mut command = self.clone();
        command.ssh_session = None;
        command.ssh_session_probe = false;
        command.args = session_command_args(&self.program, &self.args, ssh, &lease);
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
        self.sensitive_stdin = Some(SensitiveCommandInput(input));
        self
    }

    /// Run where the daemon runs: the command reads or writes the daemon's own
    /// data, which only the daemon can reach.
    pub fn in_daemon(mut self) -> Self {
        self.in_daemon = true;
        self
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
            let mut command = ssh_command(&ssh, remote).purpose("run through the fake ssh");
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

/// A [`CommandSpec`] as it travels to a node, carrying the executor-local
/// fields that its own serialization leaves out. Only the node protocol uses
/// this form; plans and stores keep the redacted one.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRun {
    pub spec: CommandSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg0: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive_output: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preserve_children_on_cancel: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitive_stdin: Option<Vec<u8>>,
    /// The caller streams stdin after the description; otherwise the child's
    /// stdin is empty (or `sensitive_stdin`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub streamed_stdin: bool,
}

impl std::fmt::Debug for NodeRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NodeRun")
            .field("spec", &self.spec)
            .field("streamed_stdin", &self.streamed_stdin)
            .finish_non_exhaustive()
    }
}

impl CommandSpec {
    /// The command with every field a node needs to run it exactly as this
    /// process would.
    pub fn to_node_run(&self, streamed_stdin: bool) -> NodeRun {
        NodeRun {
            spec: self.clone(),
            #[cfg(unix)]
            arg0: self.arg0.clone(),
            #[cfg(not(unix))]
            arg0: None,
            sensitive_output: self.sensitive_output,
            preserve_children_on_cancel: self.preserve_children_on_cancel,
            sensitive_stdin: self.sensitive_stdin.as_ref().map(|input| input.0.clone()),
            streamed_stdin,
        }
    }
}

impl NodeRun {
    pub fn into_spec(self) -> CommandSpec {
        let mut spec = self.spec;
        #[cfg(unix)]
        {
            spec.arg0 = self.arg0;
        }
        spec.sensitive_output = self.sensitive_output;
        spec.preserve_children_on_cancel = self.preserve_children_on_cancel;
        spec.sensitive_stdin = self.sensitive_stdin.map(SensitiveCommandInput);
        spec
    }
}

/// Where this process's commands run. In the daemon that is its node, the
/// process on the computer holding the user's files; everywhere else it is
/// unset and commands run here. Installed once at daemon startup, so every
/// executor in the daemon makes the same choice and none runs a command
/// locally behind the node's back.
pub trait CommandHost: Send + Sync {
    /// Run `command` to completion with `input` as its stdin (or none),
    /// abandoning it once `is_cancelled` turns true. Errors keep the text the
    /// executing side gave them, so "operation cancelled" stays recognisable.
    fn run(
        &self,
        command: &CommandSpec,
        input: Option<&mut (dyn Read + Send)>,
        is_cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<CommandOutput>;

    /// Start `command` as a live stream: stdin, stdout and stderr flow while
    /// it runs. Long-lived proxies run this way.
    fn open_stream<'a>(
        &'a self,
        command: &CommandSpec,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<crate::node::NodeExec>> + Send + 'a>,
    >;
}

static COMMAND_HOST: std::sync::OnceLock<Arc<dyn CommandHost>> = std::sync::OnceLock::new();

/// Send every command this process runs to `host`. Only the daemon calls
/// this, once, before it runs any command.
pub fn install_command_host(host: Arc<dyn CommandHost>) -> Result<()> {
    COMMAND_HOST
        .set(host)
        .map_err(|_| anyhow::anyhow!("a command host is already installed"))
}

/// The host that runs `command`, or `None` to run it in this process. Every
/// caller that starts a process asks this, so none decides on its own.
pub fn command_host(command: &CommandSpec) -> Option<&'static Arc<dyn CommandHost>> {
    COMMAND_HOST.get().filter(|_| !command.in_daemon)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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
    /// The local host has none: sysinfo reads its CPU and memory, and its
    /// storage is measured over `local_storage_paths`.
    pub probes: Vec<CommandSpec>,
    /// The paths on this machine whose filesystems local targets write to.
    /// Empty for every other host, whose probes carry their own paths.
    pub local_storage_paths: Vec<String>,
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
    /// Free space on the filesystems Mjolnir writes to, per machine probed:
    /// one for a host, one per instance for a fleet.
    pub storage: Vec<storage::HostStorageSample>,
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

    /// Stop the source's children only once destination preparation has succeeded.
    fn before_move_source_stop(
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
        if gate.is_some() || command.sensitive_stdin.is_some() {
            // A session that reports its start is read through the shared
            // collector; with no cancellation it only ends when the child does.
            return CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
                .run_once(command, gate);
        }
        let started = Instant::now();
        let output = configured_command(command)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
        let status = output.status.code().unwrap_or(-1);
        Ok(finished_command(
            command,
            started,
            status,
            output.stdout,
            output.stderr,
        ))
    }
}

// Both process executors wait for their child, and for the SSH session lease
// before it, through `off_async_worker`: a caller several calls below an `async fn`
// must not hold one of the runtime's async worker threads for the length of a
// target command.
impl CommandExecutor for ProcessExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        crate::runtime::off_async_worker(|| {
            let _running = BlockingOperation::command(command);
            if let Some(host) = command_host(command) {
                return host.run(command, None, &|| false);
            }
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
            if let Some(host) = command_host(command) {
                return host.run(command, Some(input), &|| false);
            }
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
                    gate,
                )
            })
        })
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
    gate: Option<&Arc<StartGate>>,
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
    let group =
        (!command.detaches).then(|| crate::subprocess::ProcessGroupGuard::new(Some(child.id())));
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
    let stdout_reader = PipeCollector::spawn_scanning(stdout, gate.cloned());
    let stderr_reader = PipeCollector::spawn(stderr);
    let process_result = std::thread::scope(|scope| -> Result<_> {
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
            stdin.flush().context("flush command input")?;
            Ok(true)
        });
        let mut status = None;
        let mut exited_at = None;
        let mut group_killed = false;
        let status = loop {
            if is_cancelled() {
                terminate_cancellable_child(&mut child, false);
                if let Err(error) = input_writer.join() {
                    tracing::warn!(
                        purpose = command.purpose.as_str(),
                        "streamed command input writer panicked while cancelling: {error:?}"
                    );
                }
                bail!("operation cancelled while {}", command.purpose);
            }
            match if status.is_some() {
                Ok(status)
            } else {
                child.try_wait()
            } {
                Ok(observed) => status = observed,
                Err(error) => {
                    terminate_cancellable_child(&mut child, false);
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
                let exited_at = *exited_at.get_or_insert_with(Instant::now);
                let drained = (stdout_reader.is_finished() && stderr_reader.is_finished())
                    || exited_at.elapsed() >= IO_DRAIN_TIMEOUT;
                if drained && !input_writer.is_finished() && !group_killed {
                    group_killed = true;
                    if let Some(group) = &group {
                        group.kill();
                    }
                }
                if drained && input_writer.is_finished() {
                    break status;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        };
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
    deadline: Option<(Instant, Duration)>,
}

/// Cancels synchronous executor work when its supervising async wait is dropped.
pub struct ProcessCancellationGuard(Arc<AtomicBool>);

impl Drop for ProcessCancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl CancellableProcessExecutor {
    pub fn cancel_on_drop(&self) -> ProcessCancellationGuard {
        ProcessCancellationGuard(self.cancelled.clone())
    }

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
                .is_some_and(|(deadline, _)| Instant::now() >= deadline)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Some((Instant::now() + timeout, timeout)),
        }
    }

    /// Bounds an existing flag-based executor with a deadline, so a wedged
    /// child becomes a reported failure instead of running forever.
    pub fn with_deadline(mut self, timeout: Duration) -> Self {
        self.deadline = Some((Instant::now() + timeout, timeout));
        self
    }

    fn cancellation_message(&self, purpose: &str) -> String {
        if !self.cancelled.load(Ordering::Acquire)
            && let Some((deadline, timeout)) = self.deadline
            && Instant::now() >= deadline
        {
            return format!(
                "timed out after {} s while {purpose}",
                timeout.as_secs_f64()
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
    thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl PipeCollector {
    fn spawn(pipe: impl PollablePipe) -> Self {
        Self::spawn_scanning(pipe, None)
    }

    /// Collect a session's stdout. With a `gate` the start marker is removed
    /// from the front, the first byte reports the session's start, and the end
    /// of the pipe reports that it ended; the gate settles however the thread
    /// stops.
    fn spawn_scanning(mut pipe: impl PollablePipe, gate: Option<Arc<StartGate>>) -> Self {
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
                                    collected.extend_from_slice(&passed);
                                }
                                _ => collected.extend_from_slice(&chunk[..count]),
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            })();
            if let Some(scan) = &mut scan {
                passed.clear();
                scan.finish(&mut passed);
                collected
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(&passed);
            }
            if let Some(gate) = &gate {
                gate.mark_ended();
            }
            result
        });
        Self {
            bytes,
            stop,
            thread: Some(thread),
        }
    }

    fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Waits for end of stream until `deadline`, then stops the reader and
    /// returns everything it collected.
    fn finish(mut self, stream: &str, deadline: Instant) -> Result<Vec<u8>> {
        while !self.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .context("command reader already joined")?
            .join()
            .map_err(|_| anyhow::anyhow!("command {stream} reader panicked"))?
            .with_context(|| format!("read command {stream}"))?;
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
        if let Some(input) = &command.sensitive_stdin {
            let mut input = std::io::Cursor::new(input.0.as_slice());
            // Owned bytes, so each attempt gets its own reader.
            return stream_command_with_stdin(
                cancellable_command(command),
                command,
                &mut input,
                &|| self.is_cancelled(),
                gate,
            );
        }
        let started = Instant::now();
        self.check_cancelled(&command.purpose)?;
        let mut child = cancellable_command(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run {} for {}", command.program, command.purpose))?;
        let group = (!command.detaches && !command.preserve_children_on_cancel)
            .then(|| crate::subprocess::ProcessGroupGuard::new(Some(child.id())));
        let stdout = child.stdout.take().context("command stdout missing")?;
        let stderr = child.stderr.take().context("command stderr missing")?;
        let stdout_reader = PipeCollector::spawn_scanning(stdout, gate.cloned());
        let stderr_reader = PipeCollector::spawn(stderr);
        let mut status = None;
        let mut exited_at = None;
        let status = loop {
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
            if status.is_none() {
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
            std::thread::sleep(Duration::from_millis(25));
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

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        crate::runtime::off_async_worker(|| {
            let _running = BlockingOperation::command(command);
            if let Some(host) = command_host(command) {
                return self.describe_cancellation(
                    command,
                    host.run(command, None, &|| self.is_cancelled()),
                );
            }
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
        self.describe_cancellation(
            command,
            crate::runtime::off_async_worker(|| {
                let _running = BlockingOperation::command(command);
                if let Some(host) = command_host(command) {
                    return host.run(command, Some(input), &|| self.is_cancelled());
                }
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
                            gate,
                        )
                    },
                )
            }),
        )
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
        validate_inherited_environment_name(name)?;
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
            TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. }
            | TargetTemplate::SshAppleContainer { .. } => {
                let remote = command
                    .args
                    .last_mut()
                    .context("remote container command has no SSH command argument")?;
                *remote = format!("{read_and_export} exec {remote}");
            }
            TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => {
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
    /// A missing or unusable reference is only an optimization miss. With
    /// `history_cutoff`, it is instead a Git bundle in the target that holds
    /// only the cutoff history, and the checkout fetches from it.
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
}

impl ImageHost {
    pub const fn engine(&self) -> &'static str {
        match self {
            Self::LocalPodman | Self::SshPodman(_) => "podman",
            Self::LocalDocker | Self::SshDocker(_) => "docker",
            Self::AppleContainer | Self::SshAppleContainer(_) => "container",
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
        }
    }

    /// Run `args` (an engine command line) on this host.
    pub fn command(&self, args: Vec<String>, purpose: String) -> CommandSpec {
        match self {
            Self::LocalPodman | Self::LocalDocker | Self::AppleContainer => {
                CommandSpec::new(args[0].clone(), args[1..].iter().cloned())
            }
            Self::SshPodman(ssh) | Self::SshDocker(ssh) | Self::SshAppleContainer(ssh) => {
                ssh_command_owned(ssh, args)
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
}

impl TargetTemplate {
    pub const fn container_engine(&self) -> Option<&'static str> {
        match self {
            Self::LocalPodman(_) | Self::SshPodman { .. } => Some("podman"),
            Self::LocalDocker(_) | Self::SshDocker { .. } => Some("docker"),
            Self::AppleContainer(_) | Self::SshAppleContainer { .. } => Some("container"),
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
            Self::LocalBare | Self::SshBare { .. } => None,
        }
    }
}

impl TargetLocator {
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
        }
    }

    pub const fn container_engine(&self) -> Option<&'static str> {
        match self {
            Self::LocalPodman { .. } | Self::SshPodman { .. } => Some("podman"),
            Self::LocalDocker { .. } | Self::SshDocker { .. } => Some("docker"),
            Self::AppleContainer { .. } | Self::SshAppleContainer { .. } => Some("container"),
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
        | TargetTemplate::SshAppleContainer { .. } => {
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
    locator: &TargetLocator,
    session_id: &str,
    args: Vec<String>,
    purpose: impl Into<String>,
) -> Result<CommandSpec> {
    verify_locator(locator, session_id)?;
    if args.is_empty() {
        bail!("target command must not be empty");
    }
    Ok(locator_command(locator, args).purpose(purpose))
}

/// Wrap a non-empty argv vector for execution at a target, without checking
/// that the locator belongs to a particular session. Every per-target command
/// is built here, so each target kind is spelled out once.
pub fn locator_command(locator: &TargetLocator, args: Vec<String>) -> CommandSpec {
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
            ssh_command_owned(ssh, args)
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker {
            ssh, container_id, ..
        }
        | TargetLocator::SshAppleContainer {
            ssh, container_id, ..
        } => {
            let remote = drained_container_exec(
                locator.container_engine().expect("remote container"),
                container_id,
                args,
            );
            let mut argv = vec![remote.program];
            argv.extend(remote.args);
            ssh_command_owned(ssh, argv)
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
        | TargetLocator::SshAppleContainer { .. } => {
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
    CommandSpec::new(engine, command_args)
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
    use std::fs;

    use super::*;

    // Hard-won: leaked exec records: a broken SSH pipe killed the attached client.
    #[test]
    fn container_exec_drain_preserves_streams_stdin_and_status() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("podman");
        fs::write(
            &fake,
            "#!/bin/sh\nshift 3\ncat\nprintf 'stderr bytes\\n' >&2\nexit \"$1\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
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
        fs::write(&fake, "#!/bin/sh\nset -e\nshift 3\nhead -c 204800 /dev/zero\nhead -c 204800 /dev/zero >&2\nprintf done > \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
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
    fn flaky_ssh_script(directory: &Path, refusals: u32) -> CommandSpec {
        let counter = directory.join("attempts");
        let script = format!(
            "count=$(cat {counter} 2>/dev/null || echo 0)\n\
             echo $((count + 1)) > {counter}\n\
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
        let output = executor.execute(&ssh_command(&ssh, ["true"]));
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
            assert!(started.elapsed() < Duration::from_secs(10), "{message}");
            for needle in ["build@10.0.0.1", "MaxStartups", "firewall", "kex_exchange"] {
                assert!(message.contains(needle), "{needle} missing: {message}");
            }
            assert!(
                attempts(directory.path()) > 3,
                "cancellable: {cancellable}, retried within the bound"
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
            let command = fake.command(&["wc", "-c"]);
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
        }
        set_ssh_connection_sharing_for_test(None);
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
        let fake = directory.path().join("scp");
        std::fs::write(&fake, script).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
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
}

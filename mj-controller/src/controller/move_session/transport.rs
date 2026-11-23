//! Direct archive + `.mj/keep/` transport for Move operations.
//!
//! Chooses the transfer route in strict order:
//! 1. **SameHost**: single host-local pipe through `exec -i` wrappers; zero
//!    bytes leave the host.
//! 2. **DirectPush**: ephemeral ed25519 keypair + verified peer `known_hosts`
//!    (`StrictHostKeyChecking=yes`, `HostKeyAlias`) + restricted
//!    `authorized_keys` on destination + optional shared EC2 SG ingress grant.
//! 3. **DirectPull**: reverse direction when source cannot reach destination.
//! 4. **DaemonStream**: streams source stdout through daemon memory directly
//!    into destination stdin without writing to daemon disk.

pub mod ec2_grant;

use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Context, Result, ensure};
use mj_core::hex::lower_hex;
use mj_core::subprocess::run_with_input;
use mj_core::targets::{
    self, CommandExecutor, CommandSpec, ProcessExecutor, SshTarget, SshTrafficClass, SshWalk,
    TargetLocator,
};
use sha2::{Digest, Sha256};

use self::ec2_grant::{
    AcquireOutcome, Ec2GrantKey, Ec2GrantManager, Ec2GrantStore, Ec2SecurityGroupClient,
};

pub use mj_core::state::{RoutePreview, TransferRoute};

/// Preview the transfer route without mutating any host, key, or AWS resource.
pub fn preview_transfer_route(source: &TargetLocator, destination: &TargetLocator) -> RoutePreview {
    if targets_share_host(source, destination) {
        RoutePreview::SameHost
    } else {
        RoutePreview::DirectOrDaemonStream
    }
}

/// Normalized host identity used to decide whether two targets share a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostIdentity {
    Local,
    Ssh(SshTarget),
}

fn host_identity(locator: &TargetLocator) -> HostIdentity {
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. } => HostIdentity::Local,
        TargetLocator::AwsEc2 { ssh, .. }
        | TargetLocator::SshBare { ssh, .. }
        | TargetLocator::SshPodman { ssh, .. }
        | TargetLocator::SshDocker { ssh, .. }
        | TargetLocator::SshAppleContainer { ssh, .. }
        | TargetLocator::SshNitro { ssh, .. } => HostIdentity::Ssh(ssh.clone()),
    }
}

/// Returns `true` when `source` and `destination` resolve to the same machine
/// (both local, or equal SSH host identity).
pub fn targets_share_host(source: &TargetLocator, destination: &TargetLocator) -> bool {
    host_identity(source) == host_identity(destination)
}

/// Optional `.mj/keep/` transfer parameters threaded into export/receive commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeepTransferOptions {
    /// Source `.mj/keep/<session-id>/` directory to stream after the archive.
    pub keep_dir: Option<PathBuf>,
    /// Destination `.mj/keep/<session-id>/` directory to extract into.
    pub keep_dest: Option<PathBuf>,
    /// Relative paths under `keep_dir` to exclude from the keep tar stream.
    pub skip: Vec<PathBuf>,
}

/// Worker binaries and `.mj/keep/` options for framed Move streams
/// (`export-move-stream | receive-move-stream`). Used on `SameHost` and
/// `DaemonStream` routes even when direct host-to-host SSH is unavailable.
#[derive(Debug, Clone)]
pub struct WorkerStreamConfig {
    pub source_worker_bin: String,
    pub destination_worker_bin: String,
    pub keep: KeepTransferOptions,
}

/// Verified outcome of a Move archive (+ optional `.mj/keep/`) transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferReceipt {
    pub route: TransferRoute,
    /// `.mj/keep/` tar payload size in bytes reported by `receive-move-stream`
    /// (0 when no keep directory was streamed or a raw transfer ran).
    pub keep_bytes: u64,
}

fn parse_keep_tar_bytes(stdout: &[u8]) -> u64 {
    let trimmed = String::from_utf8_lossy(stdout);
    let last_line = trimmed
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    serde_json::from_str::<mj_checkpoint::move_stream::MoveStreamReceipt>(last_line.trim())
        .map(|receipt| receipt.keep_tar_bytes)
        .unwrap_or(0)
}

/// Optional parameters for direct host-to-host SSH push/pull transfers.
#[derive(Debug, Clone)]
pub struct DirectPeerConfig {
    pub operation_id: String,
    /// Initiator's source address for `from="<addr>"` in `authorized_keys`.
    pub source_address: String,
    /// Destination's address for `from="<addr>"` on reverse pull.
    pub destination_address: String,
    /// SSH `user@host` (or host) used by source when pushing to destination.
    pub destination_ssh_destination: String,
    /// SSH `user@host` (or host) used by destination when pulling from source.
    pub source_ssh_destination: String,
    /// Verified SSH host key line for destination (without alias prefix).
    pub destination_host_key: String,
    /// Verified SSH host key line for source (without alias prefix).
    pub source_host_key: String,
    /// Stage directory on source host for temporary private key & `known_hosts`.
    pub source_stage_dir: PathBuf,
    /// Stage directory on destination host for temporary private key & `known_hosts`.
    pub destination_stage_dir: PathBuf,
    /// Path to `authorized_keys` on destination host (for DirectPush).
    pub destination_authorized_keys: PathBuf,
    /// Path to `authorized_keys` on source host (for DirectPull).
    pub source_authorized_keys: PathBuf,
    /// Worker binary path on source host (or inside source container).
    pub source_worker_bin: String,
    /// Worker binary path on destination host (or inside destination container).
    pub destination_worker_bin: String,
    /// Optional EC2 SG grant key to open on destination for DirectPush.
    pub destination_ec2_grant: Option<Ec2GrantKey>,
    /// Optional EC2 SG grant key to open on source for DirectPull.
    pub source_ec2_grant: Option<Ec2GrantKey>,
    /// Optional `.mj/keep/` source, destination, and `--skip` exclusions.
    pub keep: KeepTransferOptions,
}

/// Transfer `source_path` on `source` to `destination_path` on `destination`
/// and verify its SHA-256 at the destination.
///
/// Chooses `SameHost` when both targets share a machine, otherwise falls back
/// to `DaemonStream` (use [`transfer_archive_with_direct`] when direct SSH
/// peer metadata and optional EC2 grant manager are available).
pub fn transfer_archive(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    sha256: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<TransferRoute> {
    if targets_share_host(source, destination) {
        transfer_same_host(
            source,
            source_path,
            destination,
            destination_path,
            sha256,
            executor,
        )?;
        return Ok(TransferRoute::SameHost);
    }

    transfer_daemon_stream(
        source,
        source_path,
        destination,
        destination_path,
        sha256,
        executor,
    )?;
    Ok(TransferRoute::DaemonStream)
}

/// Full route-ordered transfer supporting `SameHost` -> `DirectPush` ->
/// `DirectPull` -> `DaemonStream`, with automatic cleanup of temporary keys,
/// `known_hosts`, `authorized_keys` entries, and EC2 security-group grants on
/// every exit path.
#[allow(clippy::too_many_arguments)]
pub fn transfer_archive_with_direct<S, C>(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    sha256: &str,
    executor: &(impl CommandExecutor + Sync),
    direct: Option<&DirectPeerConfig>,
    ec2_grants: Option<&Ec2GrantManager<S, C>>,
) -> Result<TransferRoute>
where
    S: Ec2GrantStore,
    C: Ec2SecurityGroupClient,
{
    let stream = direct.map(|peer| WorkerStreamConfig {
        source_worker_bin: peer.source_worker_bin.clone(),
        destination_worker_bin: peer.destination_worker_bin.clone(),
        keep: peer.keep.clone(),
    });
    Ok(transfer_move_stream_with_direct(
        source,
        source_path,
        destination,
        destination_path,
        sha256,
        executor,
        stream.as_ref(),
        direct,
        ec2_grants,
    )?
    .route)
}

/// Full route-ordered Move transfer carrying both archive and `.mj/keep/`
/// (honoring `--skip`) across `SameHost` -> `DirectPush` -> `DirectPull` ->
/// `DaemonStream`, returning both the chosen route and verified keep size.
#[allow(clippy::too_many_arguments)]
pub fn transfer_move_stream_with_direct<S, C>(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    sha256: &str,
    executor: &(impl CommandExecutor + Sync),
    stream: Option<&WorkerStreamConfig>,
    direct: Option<&DirectPeerConfig>,
    ec2_grants: Option<&Ec2GrantManager<S, C>>,
) -> Result<TransferReceipt>
where
    S: Ec2GrantStore,
    C: Ec2SecurityGroupClient,
{
    let fallback_stream = direct.map(|peer| WorkerStreamConfig {
        source_worker_bin: peer.source_worker_bin.clone(),
        destination_worker_bin: peer.destination_worker_bin.clone(),
        keep: peer.keep.clone(),
    });
    let effective_stream = stream.or(fallback_stream.as_ref());

    if targets_share_host(source, destination) {
        let keep_bytes = if let Some(cfg) = effective_stream {
            transfer_same_host_stream_config(
                source,
                source_path,
                destination,
                destination_path,
                sha256,
                executor,
                cfg,
            )?
        } else {
            transfer_same_host(
                source,
                source_path,
                destination,
                destination_path,
                sha256,
                executor,
            )?;
            0
        };
        return Ok(TransferReceipt {
            route: TransferRoute::SameHost,
            keep_bytes,
        });
    }

    if let Some(peer) = direct {
        match try_direct_push(
            source,
            source_path,
            destination,
            destination_path,
            sha256,
            executor,
            peer,
            ec2_grants,
        ) {
            Ok(Some(keep_bytes)) => {
                return Ok(TransferReceipt {
                    route: TransferRoute::DirectPush,
                    keep_bytes,
                });
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    operation_id = %peer.operation_id,
                    error = %err,
                    "direct push failed; attempting direct pull"
                );
            }
        }

        match try_direct_pull(
            source,
            source_path,
            destination,
            destination_path,
            sha256,
            executor,
            peer,
            ec2_grants,
        ) {
            Ok(Some(keep_bytes)) => {
                return Ok(TransferReceipt {
                    route: TransferRoute::DirectPull,
                    keep_bytes,
                });
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    operation_id = %peer.operation_id,
                    error = %err,
                    "direct pull failed; falling back to daemon stream"
                );
            }
        }
    }

    let keep_bytes = if let Some(cfg) = effective_stream {
        transfer_daemon_stream_with_config(
            source,
            source_path,
            destination,
            destination_path,
            sha256,
            cfg,
            &ProcessStreamPipe,
        )?
    } else {
        transfer_daemon_stream(
            source,
            source_path,
            destination,
            destination_path,
            sha256,
            executor,
        )?;
        0
    };
    Ok(TransferReceipt {
        route: TransferRoute::DaemonStream,
        keep_bytes,
    })
}

/// Run one host-local pipeline piping source archive bytes directly into
/// destination archive verification and atomic installation. Zero bytes leave
/// the host.
fn transfer_same_host(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    let read_cmd = host_side_read_command(source, source_path);
    let write_cmd = host_side_write_verify_command(destination, destination_path, expected_sha256);
    let script = format!("set -euo pipefail\n{read_cmd} | {write_cmd}\n");
    let host_locator = host_shell_locator(source);
    let spec = targets::locator_command(
        SshTrafficClass::Bulk,
        &host_locator,
        vec![
            "bash".into(),
            "-c".into(),
            script,
            "mj-same-host-transfer".into(),
        ],
    )
    .purpose("same-host archive pipe transfer");
    let output = executor.execute_without_deadline(&spec)?;
    ensure!(
        output.status == 0,
        "same-host archive transfer failed (exit {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Same-host framed `export-move-stream | receive-move-stream` pipeline so
/// `.mj/keep/` (with `--skip`) travels identically on the `SameHost` route.
#[cfg(all(test, unix))]
fn transfer_same_host_stream(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    executor: &(impl CommandExecutor + Sync),
    peer: &DirectPeerConfig,
) -> Result<()> {
    let cfg = WorkerStreamConfig {
        source_worker_bin: peer.source_worker_bin.clone(),
        destination_worker_bin: peer.destination_worker_bin.clone(),
        keep: peer.keep.clone(),
    };
    let _ = transfer_same_host_stream_config(
        source,
        source_path,
        destination,
        destination_path,
        expected_sha256,
        executor,
        &cfg,
    )?;
    Ok(())
}

/// Same-host framed `export-move-stream | receive-move-stream` pipeline so
/// `.mj/keep/` (with `--skip`) travels identically on the `SameHost` route.
fn transfer_same_host_stream_config(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    executor: &(impl CommandExecutor + Sync),
    cfg: &WorkerStreamConfig,
) -> Result<u64> {
    let export_cmd = format_shell_export_command(
        source,
        &cfg.source_worker_bin,
        source_path,
        cfg.keep.keep_dir.as_deref(),
        &cfg.keep.skip,
    )?;
    let receive_cmd = format_shell_receive_command(
        destination,
        &cfg.destination_worker_bin,
        destination_path,
        expected_sha256,
        cfg.keep.keep_dest.as_deref(),
    )?;
    let script = format!("set -euo pipefail\n{export_cmd} | {receive_cmd}\n");
    let host_locator = host_shell_locator(source);
    let spec = targets::locator_command(
        SshTrafficClass::Bulk,
        &host_locator,
        vec![
            "bash".into(),
            "-c".into(),
            script,
            "mj-same-host-stream-transfer".into(),
        ],
    )
    .purpose("same-host framed archive + keep pipe transfer");
    let output = executor.execute_without_deadline(&spec)?;
    ensure!(
        output.status == 0,
        "same-host framed transfer failed (exit {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(parse_keep_tar_bytes(&output.stdout))
}

/// Bounded-memory concurrent pipe between a source export command and a
/// destination import command.
///
/// When `expected_sha256` is `Some`, the piped bytes are raw archive bytes and
/// hashed on the fly; when `None`, the piped bytes are a framed
/// `export-move-stream` -> `receive-move-stream` stream whose receiver verifies
/// the archive SHA-256 and extracts `.mj/keep/`.
pub trait StreamPipe: Send + Sync {
    fn pipe(
        &self,
        source: &CommandSpec,
        destination: &CommandSpec,
        expected_sha256: Option<&str>,
    ) -> Result<()>;

    fn pipe_capture_stdout(
        &self,
        source: &CommandSpec,
        destination: &CommandSpec,
        expected_sha256: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.pipe(source, destination, expected_sha256)?;
        Ok(Vec::new())
    }
}

/// Real bounded-memory (64 KiB buffer) concurrent subprocess pipe.
///
/// Spawns both source and destination processes, drains stderr/stdout
/// concurrently to avoid >64 KiB pipe deadlocks, hashes bytes on the fly, and
/// never buffers the full archive in memory or writes to daemon disk.
pub struct ProcessStreamPipe;

fn build_std_command(spec: &CommandSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    if spec.clear_env {
        cmd.env_clear();
    }
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd
}

impl StreamPipe for ProcessStreamPipe {
    fn pipe(
        &self,
        source: &CommandSpec,
        destination: &CommandSpec,
        expected_sha256: Option<&str>,
    ) -> Result<()> {
        let _ = self.pipe_capture_stdout(source, destination, expected_sha256)?;
        Ok(())
    }

    fn pipe_capture_stdout(
        &self,
        source: &CommandSpec,
        destination: &CommandSpec,
        expected_sha256: Option<&str>,
    ) -> Result<Vec<u8>> {
        const BUF_SIZE: usize = 64 * 1024;

        // Lease any managed SSH sessions without stdout markers (stdout carries
        // raw archive or framed Move stream bytes) and keep both lease guards
        // alive until after both child processes have been reaped.
        let mut src_walk = SshWalk::default();
        let src_session =
            source.open_ssh_session_without_marker(&ProcessExecutor, &mut src_walk)?;
        let mut dst_walk = SshWalk::default();
        let dst_session =
            destination.open_ssh_session_without_marker(&ProcessExecutor, &mut dst_walk)?;

        let mut src_cmd = build_std_command(src_session.command());
        src_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut src_child = src_cmd.spawn().with_context(|| {
            format!(
                "spawn daemon-stream source {}",
                src_session.command().program
            )
        })?;

        let mut dst_cmd = build_std_command(dst_session.command());
        dst_cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut dst_child = match dst_cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = src_child.kill();
                let _ = src_child.wait();
                return Err(err).with_context(|| {
                    format!(
                        "spawn daemon-stream destination {}",
                        dst_session.command().program
                    )
                });
            }
        };

        let mut src_stdout = src_child
            .stdout
            .take()
            .context("source stdout pipe missing")?;
        let src_stderr = src_child
            .stderr
            .take()
            .context("source stderr pipe missing")?;
        let mut dst_stdin = dst_child
            .stdin
            .take()
            .context("destination stdin pipe missing")?;
        let dst_stdout = dst_child
            .stdout
            .take()
            .context("destination stdout pipe missing")?;
        let dst_stderr = dst_child
            .stderr
            .take()
            .context("destination stderr pipe missing")?;

        let src_err_thread = thread::spawn(move || drain_to_vec(src_stderr));
        let dst_out_thread = thread::spawn(move || drain_to_vec(dst_stdout));
        let dst_err_thread = thread::spawn(move || drain_to_vec(dst_stderr));

        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BUF_SIZE];
        let copy_result: io::Result<()> = (|| {
            loop {
                let n = src_stdout.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                dst_stdin.write_all(&buf[..n])?;
            }
            dst_stdin.flush()?;
            Ok(())
        })();
        // Close both pipe ends before waiting so neither child can block on a
        // full pipe; if copying failed (e.g. destination exited early), kill
        // the source child so a writer with >64 KiB buffered cannot hang wait().
        drop(dst_stdin);
        drop(src_stdout);
        if copy_result.is_err() {
            let _ = src_child.kill();
        }

        let src_status = src_child.wait().context("wait daemon-stream source")?;
        let dst_status = dst_child.wait().context("wait daemon-stream destination")?;
        let src_stderr_bytes = src_err_thread
            .join()
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();
        let dst_stdout_bytes = dst_out_thread
            .join()
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();
        let dst_stderr_bytes = dst_err_thread
            .join()
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();

        ensure!(
            src_status.success(),
            "daemon-stream source read failed ({src_status}): {}",
            String::from_utf8_lossy(&src_stderr_bytes).trim()
        );
        if let Err(err) = copy_result {
            return Err(err).context("daemon-stream pipe copy failed");
        }
        if let Some(expected) = expected_sha256 {
            let actual_sha = lower_hex(hasher.finalize());
            ensure!(
                actual_sha.eq_ignore_ascii_case(expected),
                "daemon-stream archive SHA-256 mismatch: expected {expected}, got {actual_sha}"
            );
        }
        ensure!(
            dst_status.success(),
            "daemon-stream destination write failed ({dst_status}): {}",
            String::from_utf8_lossy(&dst_stderr_bytes).trim()
        );
        Ok(dst_stdout_bytes)
    }
}

fn drain_to_vec<R: Read>(mut reader: R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

/// Stream source archive bytes through daemon memory in bounded 64 KiB chunks
/// directly into destination stdin without writing any temporary archive file
/// to daemon disk.
fn transfer_daemon_stream(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    _executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    transfer_daemon_stream_with_pipe(
        source,
        source_path,
        destination,
        destination_path,
        expected_sha256,
        &ProcessStreamPipe,
    )
}

/// [`transfer_daemon_stream`] parameterized by a [`StreamPipe`] for testing.
pub fn transfer_daemon_stream_with_pipe(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    pipe: &dyn StreamPipe,
) -> Result<()> {
    let export_spec = targets::locator_command(
        SshTrafficClass::Bulk,
        source,
        vec![
            "cat".into(),
            "--".into(),
            source_path.to_string_lossy().into_owned(),
        ],
    )
    .purpose("daemon-stream read source archive");

    let parent = destination_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_string_lossy();
    let dest = destination_path.to_string_lossy();
    let write_script = format!(
        "set -euo pipefail\nmkdir -p -- {parent}\ntmp=$(mktemp {dest}.tmp.XXXXXX)\ntrap 'rm -f -- \"$tmp\"' EXIT\ncat > \"$tmp\"\nactual=$(if command -v sha256sum >/dev/null 2>&1; then sha256sum \"$tmp\"; else shasum -a 256 \"$tmp\"; fi | awk '{{print $1}}')\nif [ \"$actual\" != {sha} ]; then echo \"sha256 mismatch: expected {sha}, got $actual\" >&2; exit 1; fi\nmv -f -- \"$tmp\" {dest}\ntrap - EXIT\n",
        parent = shell_quote(&parent),
        dest = shell_quote(&dest),
        sha = shell_quote(expected_sha256),
    );
    let import_spec = targets::locator_command(
        SshTrafficClass::Bulk,
        destination,
        vec![
            "bash".into(),
            "-c".into(),
            write_script,
            "mj-daemon-stream-import".into(),
        ],
    )
    .purpose("daemon-stream write destination archive");

    pipe.pipe(&export_spec, &import_spec, Some(expected_sha256))
}

/// Framed `DaemonStream` transfer piping `export-move-stream` (with `--keep-dir`
/// and `--skip`) into `receive-move-stream` (with `--keep-dir` from `keep_dest`)
/// through bounded daemon memory.
pub fn transfer_daemon_stream_with_worker(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    peer: &DirectPeerConfig,
    pipe: &dyn StreamPipe,
) -> Result<()> {
    let cfg = WorkerStreamConfig {
        source_worker_bin: peer.source_worker_bin.clone(),
        destination_worker_bin: peer.destination_worker_bin.clone(),
        keep: peer.keep.clone(),
    };
    let _ = transfer_daemon_stream_with_config(
        source,
        source_path,
        destination,
        destination_path,
        expected_sha256,
        &cfg,
        pipe,
    )?;
    Ok(())
}

fn transfer_daemon_stream_with_config(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    cfg: &WorkerStreamConfig,
    pipe: &dyn StreamPipe,
) -> Result<u64> {
    let export_cmd = format_shell_export_command(
        source,
        &cfg.source_worker_bin,
        source_path,
        cfg.keep.keep_dir.as_deref(),
        &cfg.keep.skip,
    )?;
    let receive_cmd = format_shell_receive_command(
        destination,
        &cfg.destination_worker_bin,
        destination_path,
        expected_sha256,
        cfg.keep.keep_dest.as_deref(),
    )?;
    let source_host = host_shell_locator(source);
    let dest_host = host_shell_locator(destination);
    let export_spec = targets::locator_command(
        SshTrafficClass::Bulk,
        &source_host,
        vec![
            "bash".into(),
            "-c".into(),
            export_cmd,
            "mj-daemon-stream-export".into(),
        ],
    )
    .purpose("daemon-stream framed export");
    let import_spec = targets::locator_command(
        SshTrafficClass::Bulk,
        &dest_host,
        vec![
            "bash".into(),
            "-c".into(),
            receive_cmd,
            "mj-daemon-stream-receive".into(),
        ],
    )
    .purpose("daemon-stream framed receive");
    let stdout = pipe.pipe_capture_stdout(&export_spec, &import_spec, None)?;
    Ok(parse_keep_tar_bytes(&stdout))
}

struct EphemeralKeypair {
    private_key: Vec<u8>,
    public_key: String,
}

fn generate_ephemeral_ed25519_keypair() -> Result<EphemeralKeypair> {
    let dir = tempfile::tempdir().context("create temp dir for ephemeral ed25519 key")?;
    let key_path = dir.path().join("id_ed25519");
    let mut cmd = Command::new("ssh-keygen");
    cmd.args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key_path);
    let output = run_with_input(&mut cmd, &[]).context("run ssh-keygen for transfer keypair")?;
    ensure!(
        output.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let private_key = fs::read(&key_path).context("read generated private key")?;
    let public_key = fs::read_to_string(dir.path().join("id_ed25519.pub"))
        .context("read generated public key")?
        .trim()
        .to_owned();
    Ok(EphemeralKeypair {
        private_key,
        public_key,
    })
}

/// RAII guard that cleans up the temporary private key, `known_hosts`,
/// `authorized_keys` entry, and EC2 security-group grant on every exit path.
struct DirectTransferCleanup<'a, E, S, C>
where
    E: CommandExecutor + Sync,
    S: Ec2GrantStore,
    C: Ec2SecurityGroupClient,
{
    executor: &'a E,
    initiator_host: TargetLocator,
    receiver_host: TargetLocator,
    receiver_worker_bin: String,
    receiver_authorized_keys: PathBuf,
    operation_id: String,
    stage_files: Vec<PathBuf>,
    authorized_key_installed: bool,
    ec2_grants: Option<&'a Ec2GrantManager<S, C>>,
    ec2_key: Option<Ec2GrantKey>,
}

impl<E, S, C> Drop for DirectTransferCleanup<'_, E, S, C>
where
    E: CommandExecutor + Sync,
    S: Ec2GrantStore,
    C: Ec2SecurityGroupClient,
{
    fn drop(&mut self) {
        if let (Some(mgr), Some(key)) = (self.ec2_grants, self.ec2_key.as_ref())
            && let Err(err) = mgr.release(key, &self.operation_id)
        {
            tracing::warn!(
                operation_id = %self.operation_id,
                error = %err,
                "failed to release EC2 grant during direct transfer cleanup"
            );
        }
        if self.authorized_key_installed {
            let remove_spec = targets::locator_command(
                SshTrafficClass::Control,
                &self.receiver_host,
                vec![
                    self.receiver_worker_bin.clone(),
                    "worker".into(),
                    "authorized-keys".into(),
                    "remove".into(),
                    "--path".into(),
                    self.receiver_authorized_keys.to_string_lossy().into_owned(),
                    "--operation-id".into(),
                    self.operation_id.clone(),
                ],
            )
            .purpose("remove temporary Move authorized_keys entry");
            match self.executor.execute_cleanup(&remove_spec) {
                Ok(out) if out.status != 0 => {
                    tracing::warn!(
                        operation_id = %self.operation_id,
                        status = out.status,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "remove temporary authorized_keys entry exited non-zero"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        operation_id = %self.operation_id,
                        error = %err,
                        "failed to remove temporary authorized_keys entry"
                    );
                }
                Ok(_) => {}
            }
        }
        if !self.stage_files.is_empty() {
            let mut argv = vec!["rm".into(), "-f".into(), "--".into()];
            for file in &self.stage_files {
                argv.push(file.to_string_lossy().into_owned());
            }
            let rm_spec =
                targets::locator_command(SshTrafficClass::Control, &self.initiator_host, argv)
                    .purpose("remove temporary Move SSH key and known_hosts");
            match self.executor.execute_cleanup(&rm_spec) {
                Ok(out) if out.status != 0 => {
                    tracing::warn!(
                        operation_id = %self.operation_id,
                        status = out.status,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "remove temporary SSH stage files exited non-zero"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        operation_id = %self.operation_id,
                        error = %err,
                        "failed to remove temporary SSH stage files"
                    );
                }
                Ok(_) => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_direct_push<S, C>(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    executor: &(impl CommandExecutor + Sync),
    peer: &DirectPeerConfig,
    ec2_grants: Option<&Ec2GrantManager<S, C>>,
) -> Result<Option<u64>>
where
    S: Ec2GrantStore,
    C: Ec2SecurityGroupClient,
{
    let keypair = generate_ephemeral_ed25519_keypair()?;
    let source_host = host_shell_locator(source);
    let dest_host = host_shell_locator(destination);
    let priv_path = peer
        .source_stage_dir
        .join(format!("mj-move-{}.key", peer.operation_id));
    let kh_path = peer
        .source_stage_dir
        .join(format!("mj-move-{}.known_hosts", peer.operation_id));
    let alias = format!("mj-move-peer-{}", peer.operation_id);

    let mut cleanup = DirectTransferCleanup {
        executor,
        initiator_host: source_host.clone(),
        receiver_host: dest_host.clone(),
        receiver_worker_bin: peer.destination_worker_bin.clone(),
        receiver_authorized_keys: peer.destination_authorized_keys.clone(),
        operation_id: peer.operation_id.clone(),
        stage_files: vec![priv_path.clone(), kh_path.clone()],
        authorized_key_installed: false,
        ec2_grants,
        ec2_key: None,
    };

    // 1. Stage 0600 private key and verified peer known_hosts on source host.
    stage_mode_0600_file(executor, &source_host, &priv_path, &keypair.private_key)?;
    let kh_line = format!("{alias} {}\n", peer.destination_host_key.trim());
    stage_mode_0600_file(executor, &source_host, &kh_path, kh_line.as_bytes())?;

    // 2. Install restricted authorized_keys entry on destination host.
    let restricted_cmd = format_forced_receive_command(
        destination,
        &peer.destination_worker_bin,
        destination_path,
        expected_sha256,
        peer.keep.keep_dest.as_deref(),
    )?;
    let install_spec = targets::locator_command(
        SshTrafficClass::Control,
        &dest_host,
        vec![
            peer.destination_worker_bin.clone(),
            "worker".into(),
            "authorized-keys".into(),
            "install".into(),
            "--path".into(),
            peer.destination_authorized_keys
                .to_string_lossy()
                .into_owned(),
            "--operation-id".into(),
            peer.operation_id.clone(),
            "--from".into(),
            peer.source_address.clone(),
            "--command".into(),
            restricted_cmd,
            "--public-key".into(),
            keypair.public_key.clone(),
        ],
    )
    .purpose("install temporary Move authorized_keys on destination");
    // Arm cleanup BEFORE executing remote `authorized-keys install` so a lost
    // ACK after remote installation still cleans up the idempotent key entry.
    cleanup.authorized_key_installed = true;
    let install_out = executor.execute(&install_spec)?;
    ensure!(
        install_out.status == 0,
        "install destination authorized_keys failed: {}",
        String::from_utf8_lossy(&install_out.stderr).trim()
    );

    // 3. Acquire optional EC2 SG ingress grant on destination.
    if let (Some(mgr), Some(key)) = (ec2_grants, peer.destination_ec2_grant.as_ref()) {
        match mgr.acquire(key, &peer.operation_id)? {
            AcquireOutcome::Granted => {
                cleanup.ec2_key = Some(key.clone());
            }
            AcquireOutcome::FallbackToDaemonStream => {
                return Ok(None);
            }
        }
    }

    // 4. Stream from source host (wrapping in container exec when container-backed)
    // to destination over SSH.
    let export_cmd = format_forced_export_command(
        source,
        &peer.source_worker_bin,
        source_path,
        peer.keep.keep_dir.as_deref(),
        &peer.keep.skip,
    )?;
    let ssh_cmd = format_direct_ssh_command(
        &priv_path,
        &kh_path,
        &alias,
        &peer.destination_ssh_destination,
    );
    let script = format!("set -euo pipefail\n{export_cmd} | {ssh_cmd}\n");
    let push_spec = targets::locator_command(
        SshTrafficClass::Bulk,
        &source_host,
        vec!["bash".into(), "-c".into(), script, "mj-direct-push".into()],
    )
    .purpose("direct push Move stream");
    let push_out = executor.execute_without_deadline(&push_spec)?;
    ensure!(
        push_out.status == 0,
        "direct push failed (exit {}): {}",
        push_out.status,
        String::from_utf8_lossy(&push_out.stderr).trim()
    );
    Ok(Some(parse_keep_tar_bytes(&push_out.stdout)))
}

#[allow(clippy::too_many_arguments)]
fn try_direct_pull<S, C>(
    source: &TargetLocator,
    source_path: &Path,
    destination: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
    executor: &(impl CommandExecutor + Sync),
    peer: &DirectPeerConfig,
    ec2_grants: Option<&Ec2GrantManager<S, C>>,
) -> Result<Option<u64>>
where
    S: Ec2GrantStore,
    C: Ec2SecurityGroupClient,
{
    let keypair = generate_ephemeral_ed25519_keypair()?;
    let source_host = host_shell_locator(source);
    let dest_host = host_shell_locator(destination);
    let priv_path = peer
        .destination_stage_dir
        .join(format!("mj-move-{}.pull.key", peer.operation_id));
    let kh_path = peer
        .destination_stage_dir
        .join(format!("mj-move-{}.pull.known_hosts", peer.operation_id));
    let alias = format!("mj-move-pull-peer-{}", peer.operation_id);

    let mut cleanup = DirectTransferCleanup {
        executor,
        initiator_host: dest_host.clone(),
        receiver_host: source_host.clone(),
        receiver_worker_bin: peer.source_worker_bin.clone(),
        receiver_authorized_keys: peer.source_authorized_keys.clone(),
        operation_id: peer.operation_id.clone(),
        stage_files: vec![priv_path.clone(), kh_path.clone()],
        authorized_key_installed: false,
        ec2_grants,
        ec2_key: None,
    };

    stage_mode_0600_file(executor, &dest_host, &priv_path, &keypair.private_key)?;
    let kh_line = format!("{alias} {}\n", peer.source_host_key.trim());
    stage_mode_0600_file(executor, &dest_host, &kh_path, kh_line.as_bytes())?;

    let restricted_cmd = format_forced_export_command(
        source,
        &peer.source_worker_bin,
        source_path,
        peer.keep.keep_dir.as_deref(),
        &peer.keep.skip,
    )?;
    let install_spec = targets::locator_command(
        SshTrafficClass::Control,
        &source_host,
        vec![
            peer.source_worker_bin.clone(),
            "worker".into(),
            "authorized-keys".into(),
            "install".into(),
            "--path".into(),
            peer.source_authorized_keys.to_string_lossy().into_owned(),
            "--operation-id".into(),
            peer.operation_id.clone(),
            "--from".into(),
            peer.destination_address.clone(),
            "--command".into(),
            restricted_cmd,
            "--public-key".into(),
            keypair.public_key.clone(),
        ],
    )
    .purpose("install temporary Move authorized_keys on source for pull");
    // Arm cleanup BEFORE executing remote `authorized-keys install` so a lost
    // ACK after remote installation still cleans up the idempotent key entry.
    cleanup.authorized_key_installed = true;
    let install_out = executor.execute(&install_spec)?;
    ensure!(
        install_out.status == 0,
        "install source authorized_keys failed: {}",
        String::from_utf8_lossy(&install_out.stderr).trim()
    );

    if let (Some(mgr), Some(key)) = (ec2_grants, peer.source_ec2_grant.as_ref()) {
        match mgr.acquire(key, &peer.operation_id)? {
            AcquireOutcome::Granted => {
                cleanup.ec2_key = Some(key.clone());
            }
            AcquireOutcome::FallbackToDaemonStream => {
                return Ok(None);
            }
        }
    }

    let ssh_cmd =
        format_direct_ssh_command(&priv_path, &kh_path, &alias, &peer.source_ssh_destination);
    let receive_cmd = format_forced_receive_command(
        destination,
        &peer.destination_worker_bin,
        destination_path,
        expected_sha256,
        peer.keep.keep_dest.as_deref(),
    )?;
    let script = format!("set -euo pipefail\n{ssh_cmd} | {receive_cmd}\n");
    let pull_spec = targets::locator_command(
        SshTrafficClass::Bulk,
        &dest_host,
        vec!["bash".into(), "-c".into(), script, "mj-direct-pull".into()],
    )
    .purpose("direct pull Move stream");
    let pull_out = executor.execute_without_deadline(&pull_spec)?;
    ensure!(
        pull_out.status == 0,
        "direct pull failed (exit {}): {}",
        pull_out.status,
        String::from_utf8_lossy(&pull_out.stderr).trim()
    );
    Ok(Some(parse_keep_tar_bytes(&pull_out.stdout)))
}

fn stage_mode_0600_file(
    executor: &(impl CommandExecutor + Sync),
    host: &TargetLocator,
    path: &Path,
    bytes: &[u8],
) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let script = format!(
        "set -euo pipefail\nmkdir -p -- {}\numask 077\ncat > {}\nchmod 0600 -- {}\n",
        shell_quote(&parent.to_string_lossy()),
        shell_quote(&path.to_string_lossy()),
        shell_quote(&path.to_string_lossy()),
    );
    let spec = targets::locator_command(
        SshTrafficClass::Control,
        host,
        vec!["bash".into(), "-c".into(), script, "mj-stage-0600".into()],
    )
    .purpose("stage mode 0600 SSH transfer file");
    let mut cursor = Cursor::new(bytes);
    let out = executor.execute_with_stdin(&spec, &mut cursor)?;
    ensure!(
        out.status == 0,
        "stage 0600 file {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Best-effort revocation of ephemeral Move SSH credentials recorded in
/// `MoveOperation::granted_keys` after an interrupted or recovered Move.
///
/// For each key whose recorded `host` matches one of `candidate_targets` (an
/// SSH/EC2 host), runs both `worker authorized-keys remove --operation-id` and
/// `rm -f -- <private_key_path> <known_hosts_path>` on that host via the
/// daemon's control connection. Keys without a matching remote locator fall
/// back to local file removal.
///
/// Returns the subset of keys whose remote cleanup failed so callers can retain
/// them for retry if a host was unreachable.
pub(crate) fn cleanup_interrupted_move_credentials(
    operation_id: &str,
    granted_keys: &[mj_core::state::GrantedMoveKey],
    candidate_targets: &[&TargetLocator],
    executor: &(impl CommandExecutor + Sync),
) -> Vec<mj_core::state::GrantedMoveKey> {
    let mut remaining = Vec::new();
    for key in granted_keys {
        let matched_host = key
            .ssh_target
            .as_ref()
            .map(|ssh| TargetLocator::SshBare {
                ssh: ssh.clone(),
                workspace: String::new(),
                worker_id: None,
            })
            .or_else(|| {
                candidate_targets
                    .iter()
                    .copied()
                    .find(|locator| target_ssh_host(locator).as_deref() == Some(key.host.as_str()))
                    .map(host_shell_locator)
            });
        let Some(host) = matched_host else {
            super::recovery::cleanup_local_move_key_files(std::slice::from_ref(key));
            continue;
        };
        let key_operation_id = key
            .comment
            .strip_prefix("mj-move-")
            .filter(|id| !id.is_empty())
            .unwrap_or(operation_id);
        let mut ok = true;
        if let (Some(worker_bin), Some(auth_keys_path)) = (
            key.worker_bin.as_deref(),
            key.authorized_keys_path.as_deref(),
        ) {
            let remove_spec = targets::locator_command(
                SshTrafficClass::Control,
                &host,
                vec![
                    worker_bin.to_owned(),
                    "worker".into(),
                    "authorized-keys".into(),
                    "remove".into(),
                    "--path".into(),
                    auth_keys_path.to_string_lossy().into_owned(),
                    "--operation-id".into(),
                    key_operation_id.to_owned(),
                ],
            )
            .purpose("remove temporary Move authorized_keys entry");
            match executor.execute_cleanup(&remove_spec) {
                Ok(out) if out.status == 0 => {}
                Ok(out) => {
                    ok = false;
                    tracing::warn!(
                        operation_id,
                        host = %key.host,
                        status = out.status,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "failed to revoke temporary Move authorized_keys entry during recovery"
                    );
                }
                Err(err) => {
                    ok = false;
                    tracing::warn!(
                        operation_id,
                        host = %key.host,
                        error = %err,
                        "failed to revoke temporary Move authorized_keys entry during recovery"
                    );
                }
            }
        }
        let mut stage_files = Vec::new();
        if let Some(path) = &key.private_key_path {
            stage_files.push(path.to_string_lossy().into_owned());
        }
        if let Some(path) = &key.known_hosts_path {
            stage_files.push(path.to_string_lossy().into_owned());
        }
        if !stage_files.is_empty() {
            let mut argv = vec!["rm".into(), "-f".into(), "--".into()];
            argv.extend(stage_files);
            let rm_spec = targets::locator_command(SshTrafficClass::Control, &host, argv)
                .purpose("remove temporary Move SSH key and known_hosts");
            match executor.execute_cleanup(&rm_spec) {
                Ok(out) if out.status == 0 => {}
                Ok(out) => {
                    ok = false;
                    tracing::warn!(
                        operation_id,
                        host = %key.host,
                        status = out.status,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "failed to remove temporary Move SSH files during recovery"
                    );
                }
                Err(err) => {
                    ok = false;
                    tracing::warn!(
                        operation_id,
                        host = %key.host,
                        error = %err,
                        "failed to remove temporary Move SSH files during recovery"
                    );
                }
            }
        }
        if !ok {
            remaining.push(key.clone());
        }
    }
    remaining
}

fn target_ssh_host(locator: &TargetLocator) -> Option<String> {
    let ssh = match locator {
        TargetLocator::SshBare { ssh, .. }
        | TargetLocator::AwsEc2 { ssh, .. }
        | TargetLocator::SshPodman { ssh, .. }
        | TargetLocator::SshDocker { ssh, .. }
        | TargetLocator::SshAppleContainer { ssh, .. }
        | TargetLocator::SshNitro { ssh, .. } => ssh,
        TargetLocator::LocalBare { .. }
        | TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. } => return None,
    };
    Some(
        ssh.destination
            .rsplit_once('@')
            .map_or(ssh.destination.as_str(), |(_, host)| host)
            .to_owned(),
    )
}

/// Convert any target locator into a host-level locator for running host-local
/// commands (outside containers).
fn host_shell_locator(locator: &TargetLocator) -> TargetLocator {
    match locator {
        TargetLocator::LocalBare { worker_root } => TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        },
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. } => TargetLocator::LocalBare {
            worker_root: String::new(),
        },
        TargetLocator::AwsEc2 { ssh, workspace, .. }
        | TargetLocator::SshBare { ssh, workspace, .. } => TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: workspace.clone(),
            worker_id: None,
        },
        TargetLocator::SshPodman { ssh, .. }
        | TargetLocator::SshDocker { ssh, .. }
        | TargetLocator::SshAppleContainer { ssh, .. }
        | TargetLocator::SshNitro { ssh, .. } => TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: String::new(),
            worker_id: None,
        },
    }
}

fn host_side_read_command(locator: &TargetLocator, path: &Path) -> String {
    let quoted_path = shell_quote(&path.to_string_lossy());
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::SshBare { .. }
        | TargetLocator::AwsEc2 { .. } => format!("cat -- {quoted_path}"),
        TargetLocator::LocalPodman { container_id, .. }
        | TargetLocator::SshPodman { container_id, .. } => {
            format!(
                "podman exec {} cat -- {quoted_path}",
                shell_quote(container_id)
            )
        }
        TargetLocator::LocalDocker { container_id, .. }
        | TargetLocator::SshDocker { container_id, .. } => {
            format!(
                "docker exec {} cat -- {quoted_path}",
                shell_quote(container_id)
            )
        }
        TargetLocator::AppleContainer { container_id, .. }
        | TargetLocator::SshAppleContainer { container_id, .. } => {
            format!(
                "container exec {} cat -- {quoted_path}",
                shell_quote(container_id)
            )
        }
        TargetLocator::SshNitro { container_id, .. } => {
            format!(
                "{} exec {} cat -- {quoted_path}",
                targets::engine_shell_words(targets::MICROVM_ENGINE),
                shell_quote(container_id)
            )
        }
    }
}

fn host_side_write_verify_command(
    locator: &TargetLocator,
    destination_path: &Path,
    expected_sha256: &str,
) -> String {
    let parent = destination_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_string_lossy();
    let dest = destination_path.to_string_lossy();
    let inner = format!(
        "set -euo pipefail; mkdir -p -- {parent}; tmp=$(mktemp {dest}.tmp.XXXXXX); trap 'rm -f -- \"$tmp\"' EXIT; cat > \"$tmp\"; actual=$(if command -v sha256sum >/dev/null 2>&1; then sha256sum \"$tmp\"; else shasum -a 256 \"$tmp\"; fi | awk '{{print $1}}'); if [ \"$actual\" != {sha} ]; then echo \"sha256 mismatch: expected {sha}, got $actual\" >&2; exit 1; fi; mv -f -- \"$tmp\" {dest}; trap - EXIT",
        parent = shell_quote(&parent),
        dest = shell_quote(&dest),
        sha = shell_quote(expected_sha256),
    );
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::SshBare { .. }
        | TargetLocator::AwsEc2 { .. } => format!("bash -c {}", shell_quote(&inner)),
        TargetLocator::LocalPodman { container_id, .. }
        | TargetLocator::SshPodman { container_id, .. } => format!(
            "podman exec -i {} bash -c {}",
            shell_quote(container_id),
            shell_quote(&inner)
        ),
        TargetLocator::LocalDocker { container_id, .. }
        | TargetLocator::SshDocker { container_id, .. } => format!(
            "docker exec -i {} bash -c {}",
            shell_quote(container_id),
            shell_quote(&inner)
        ),
        TargetLocator::AppleContainer { container_id, .. }
        | TargetLocator::SshAppleContainer { container_id, .. } => format!(
            "container exec -i {} bash -c {}",
            shell_quote(container_id),
            shell_quote(&inner)
        ),
        TargetLocator::SshNitro { container_id, .. } => format!(
            "{} exec -i {} bash -c {}",
            targets::engine_shell_words(targets::MICROVM_ENGINE),
            shell_quote(container_id),
            shell_quote(&inner)
        ),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Format the direct-push / direct-pull `ssh` client invocation.
///
/// Explicitly disables ControlMaster/ControlPath/ControlPersist so a host's
/// `~/.ssh/config` multiplexed socket can never bypass `-i <temp-key>` and its
/// forced `authorized_keys` command.
pub fn format_direct_ssh_command(
    priv_path: &Path,
    known_hosts_path: &Path,
    host_key_alias: &str,
    destination: &str,
) -> String {
    format!(
        "ssh -i {} -o IdentitiesOnly=yes -o ControlMaster=no -o ControlPath=none -o ControlPersist=no -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -o HostKeyAlias={} -o BatchMode=yes -- {}",
        shell_quote(&priv_path.to_string_lossy()),
        shell_quote(&known_hosts_path.to_string_lossy()),
        shell_quote(host_key_alias),
        shell_quote(destination),
    )
}

/// Reject ASCII control characters (including NUL/CR/LF) for ordinary POSIX
/// single-quoted shell arguments on `SameHost` / `DaemonStream` routes, while
/// safely allowing apostrophes, double quotes, and backslashes.
fn validate_shell_arg(label: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{label} must not be empty");
    ensure!(
        !value.chars().any(char::is_control),
        "{label} contains control characters"
    );
    Ok(())
}

/// Format a POSIX-single-quoted `receive-move-stream` shell command for
/// non-`authorized_keys` routes (`SameHost` and `DaemonStream`).
pub fn format_shell_receive_command(
    destination: &TargetLocator,
    worker_bin: &str,
    destination_path: &Path,
    expected_sha256: &str,
    keep_dest: Option<&Path>,
) -> Result<String> {
    let dest = destination_path.to_string_lossy();
    validate_shell_arg("worker_bin", worker_bin)?;
    validate_shell_arg("destination_path", &dest)?;
    validate_shell_arg("expected_sha256", expected_sha256)?;

    let mut cmd = String::new();
    if let Some((engine, container_id)) = container_exec_prefix(destination, false) {
        validate_shell_arg("container_id", container_id)?;
        cmd.push_str(&engine);
        cmd.push_str(" exec -i ");
        cmd.push_str(&shell_quote(container_id));
        cmd.push(' ');
    }
    cmd.push_str(&format!(
        "{} worker receive-move-stream --archive {} --expected-sha256 {}",
        shell_quote(worker_bin),
        shell_quote(&dest),
        shell_quote(expected_sha256),
    ));
    if let Some(keep_dir) = keep_dest {
        let keep_str = keep_dir.to_string_lossy();
        validate_shell_arg("keep_dest", &keep_str)?;
        cmd.push_str(" --keep-dir ");
        cmd.push_str(&shell_quote(&keep_str));
    }
    Ok(cmd)
}

/// Format a POSIX-single-quoted `export-move-stream` shell command for
/// non-`authorized_keys` routes (`SameHost` and `DaemonStream`).
pub fn format_shell_export_command(
    source: &TargetLocator,
    worker_bin: &str,
    source_path: &Path,
    keep_dir: Option<&Path>,
    skip: &[PathBuf],
) -> Result<String> {
    let src = source_path.to_string_lossy();
    validate_shell_arg("worker_bin", worker_bin)?;
    validate_shell_arg("source_path", &src)?;

    let mut cmd = String::new();
    if let Some((engine, container_id)) = container_exec_prefix(source, false) {
        validate_shell_arg("container_id", container_id)?;
        cmd.push_str(&engine);
        cmd.push_str(" exec -i ");
        cmd.push_str(&shell_quote(container_id));
        cmd.push(' ');
    }
    cmd.push_str(&format!(
        "{} worker export-move-stream --archive {}",
        shell_quote(worker_bin),
        shell_quote(&src),
    ));
    if let Some(keep) = keep_dir {
        let keep_str = keep.to_string_lossy();
        validate_shell_arg("keep_dir", &keep_str)?;
        cmd.push_str(" --keep-dir ");
        cmd.push_str(&shell_quote(&keep_str));
    }
    for skipped in skip {
        let skip_str = skipped.to_string_lossy();
        validate_shell_arg("skip", &skip_str)?;
        cmd.push_str(" --skip ");
        cmd.push_str(&shell_quote(&skip_str));
    }
    Ok(cmd)
}

/// Reject characters that cannot be safely embedded inside an SSH
/// `authorized_keys` double-quoted `command="..."` option.
fn validate_authorized_keys_command_arg(label: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{label} must not be empty");
    ensure!(
        !value
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\' || c == '\''),
        "{label} contains characters unsafe for forced SSH authorized_keys command"
    );
    Ok(())
}

fn container_exec_prefix(
    locator: &TargetLocator,
    for_authorized_keys: bool,
) -> Option<(String, &str)> {
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::SshBare { .. }
        | TargetLocator::AwsEc2 { .. } => None,
        TargetLocator::LocalPodman { container_id, .. }
        | TargetLocator::SshPodman { container_id, .. } => {
            Some(("podman".to_owned(), container_id.as_str()))
        }
        TargetLocator::LocalDocker { container_id, .. }
        | TargetLocator::SshDocker { container_id, .. } => {
            Some(("docker".to_owned(), container_id.as_str()))
        }
        TargetLocator::AppleContainer { container_id, .. }
        | TargetLocator::SshAppleContainer { container_id, .. } => {
            Some(("container".to_owned(), container_id.as_str()))
        }
        TargetLocator::SshNitro { container_id, .. } => {
            let words = targets::engine_shell_words(targets::MICROVM_ENGINE);
            let engine = if for_authorized_keys {
                words.replace('"', "")
            } else {
                words
            };
            Some((engine, container_id.as_str()))
        }
    }
}

/// Format the POSIX-quoted forced `receive-move-stream` command for
/// `authorized_keys` `command="..."` (wrapping in `<engine> exec -i <container>`
/// when `destination` is container-backed, and threading `--keep-dir` when set).
pub fn format_forced_receive_command(
    destination: &TargetLocator,
    worker_bin: &str,
    destination_path: &Path,
    expected_sha256: &str,
    keep_dest: Option<&Path>,
) -> Result<String> {
    let dest = destination_path.to_string_lossy();
    validate_authorized_keys_command_arg("worker_bin", worker_bin)?;
    validate_authorized_keys_command_arg("destination_path", &dest)?;
    validate_authorized_keys_command_arg("expected_sha256", expected_sha256)?;

    let mut cmd = String::new();
    if let Some((engine, container_id)) = container_exec_prefix(destination, true) {
        validate_authorized_keys_command_arg("container_id", container_id)?;
        cmd.push_str(&engine);
        cmd.push_str(" exec -i ");
        cmd.push_str(&shell_quote(container_id));
        cmd.push(' ');
    }
    cmd.push_str(&format!(
        "{} worker receive-move-stream --archive {} --expected-sha256 {}",
        shell_quote(worker_bin),
        shell_quote(&dest),
        shell_quote(expected_sha256),
    ));
    if let Some(keep_dir) = keep_dest {
        let keep_str = keep_dir.to_string_lossy();
        validate_authorized_keys_command_arg("keep_dest", &keep_str)?;
        cmd.push_str(" --keep-dir ");
        cmd.push_str(&shell_quote(&keep_str));
    }
    Ok(cmd)
}

/// Format the POSIX-quoted forced `export-move-stream` command for
/// `authorized_keys` `command="..."` on reverse-pull transfers (and source push
/// export), wrapping in `<engine> exec -i <container>` when `source` is
/// container-backed and threading `--keep-dir` and repeated `--skip` flags.
pub fn format_forced_export_command(
    source: &TargetLocator,
    worker_bin: &str,
    source_path: &Path,
    keep_dir: Option<&Path>,
    skip: &[PathBuf],
) -> Result<String> {
    let src = source_path.to_string_lossy();
    validate_authorized_keys_command_arg("worker_bin", worker_bin)?;
    validate_authorized_keys_command_arg("source_path", &src)?;

    let mut cmd = String::new();
    if let Some((engine, container_id)) = container_exec_prefix(source, true) {
        validate_authorized_keys_command_arg("container_id", container_id)?;
        cmd.push_str(&engine);
        cmd.push_str(" exec -i ");
        cmd.push_str(&shell_quote(container_id));
        cmd.push(' ');
    }
    cmd.push_str(&format!(
        "{} worker export-move-stream --archive {}",
        shell_quote(worker_bin),
        shell_quote(&src),
    ));
    if let Some(keep) = keep_dir {
        let keep_str = keep.to_string_lossy();
        validate_authorized_keys_command_arg("keep_dir", &keep_str)?;
        cmd.push_str(" --keep-dir ");
        cmd.push_str(&shell_quote(&keep_str));
    }
    for skipped in skip {
        let skip_str = skipped.to_string_lossy();
        validate_authorized_keys_command_arg("skip", &skip_str)?;
        cmd.push_str(" --skip ");
        cmd.push_str(&shell_quote(&skip_str));
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ec2_grant::{Ec2ApiError, Ec2GrantConfig, Ec2IngressRule, InMemoryGrantStore};
    use mj_core::targets::{CommandOutput, CommandSpec, DeadlinePause, ProcessExecutor};

    #[test]
    fn preview_route_is_read_only_and_classifies_same_vs_cross_host() {
        let local_bare = TargetLocator::LocalBare {
            worker_root: "/tmp/a".into(),
        };
        let local_podman = TargetLocator::LocalPodman {
            container_id: "c1".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let ssh_a = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host-a".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let ssh_b = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host-b".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };

        assert_eq!(
            preview_transfer_route(&local_bare, &local_podman),
            RoutePreview::SameHost
        );
        assert_eq!(
            preview_transfer_route(&ssh_a, &ssh_a),
            RoutePreview::SameHost
        );
        assert_eq!(
            preview_transfer_route(&ssh_a, &ssh_b),
            RoutePreview::DirectOrDaemonStream
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs bash on the controller")]
    fn same_host_pipe_transfers_over_128kib_and_verifies_sha256() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        let dst_archive = dir.path().join("dst/dst.hel.zip");

        let payload: Vec<u8> = (0..(192 * 1024)).map(|i| (i % 251) as u8).collect();
        fs::write(&src_archive, &payload).expect("write src");
        let sha = lower_hex(Sha256::digest(&payload));

        let src = TargetLocator::LocalBare {
            worker_root: dir.path().to_string_lossy().into_owned(),
        };
        let dst = TargetLocator::LocalBare {
            worker_root: dir.path().to_string_lossy().into_owned(),
        };
        let executor = ProcessExecutor;

        let route = transfer_archive(&src, &src_archive, &dst, &dst_archive, &sha, &executor)
            .expect("same-host transfer");
        assert_eq!(route, TransferRoute::SameHost);
        assert_eq!(fs::read(&dst_archive).expect("read dst"), payload);
    }

    /// StreamPipe that strips `ssh ... <remote_cmd>` down to `bash -c <remote_cmd>`
    /// and delegates to the real bounded-64 KiB [`ProcessStreamPipe`].
    struct TwoHostLocalStreamPipe;

    impl TwoHostLocalStreamPipe {
        fn unwrap_ssh(spec: &CommandSpec) -> CommandSpec {
            if spec.program == "ssh" {
                let remote_cmd = spec.args.last().cloned().unwrap_or_default();
                CommandSpec::new("bash", vec!["-c".into(), remote_cmd])
            } else {
                CommandSpec::new(&spec.program, spec.args.clone())
            }
        }
    }

    impl StreamPipe for TwoHostLocalStreamPipe {
        fn pipe(
            &self,
            source: &CommandSpec,
            destination: &CommandSpec,
            expected_sha256: Option<&str>,
        ) -> Result<()> {
            ProcessStreamPipe.pipe(
                &Self::unwrap_ssh(source),
                &Self::unwrap_ssh(destination),
                expected_sha256,
            )
        }
    }

    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs bash on the controller")]
    fn daemon_stream_fallback_transfers_over_128kib_with_zero_daemon_disk_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_archive = dir.path().join("src.hel.zip");
        let dst_archive = dir.path().join("dst.hel.zip");
        let daemon_dir = dir.path().join("daemon-disk-watch");
        fs::create_dir_all(&daemon_dir).expect("mkdir daemon watch");

        let payload: Vec<u8> = (0..(160 * 1024)).map(|i| ((i * 13) % 251) as u8).collect();
        fs::write(&src_archive, &payload).expect("write src");
        let sha = lower_hex(Sha256::digest(&payload));

        let src = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host-a".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let dst = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host-b".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };

        transfer_daemon_stream_with_pipe(
            &src,
            &src_archive,
            &dst,
            &dst_archive,
            &sha,
            &TwoHostLocalStreamPipe,
        )
        .expect("daemon stream transfer");

        assert_eq!(fs::read(&dst_archive).expect("read dst"), payload);
        assert_eq!(
            fs::read_dir(&daemon_dir).expect("read daemon dir").count(),
            0,
            "daemon stream must not write temporary archive files to daemon disk"
        );
    }

    #[test]
    fn forced_ssh_commands_quote_paths_with_spaces_and_reject_unsafe_chars() {
        let bare = TargetLocator::LocalBare {
            worker_root: "/w".into(),
        };
        let receive = format_forced_receive_command(
            &bare,
            "/opt/my worker/mj-worker",
            Path::new("/var/tmp/my archive.hel.zip"),
            &"a".repeat(64),
            Some(Path::new("/work/.mj/keep/sess 1")),
        )
        .expect("format receive");
        assert!(receive.contains("'/opt/my worker/mj-worker'"));
        assert!(receive.contains("'/var/tmp/my archive.hel.zip'"));
        assert!(receive.contains("--keep-dir '/work/.mj/keep/sess 1'"));
        assert!(!receive.contains('"'));
        assert!(!receive.contains('\\'));

        let export = format_forced_export_command(
            &bare,
            "/opt/my worker/mj-worker",
            Path::new("/var/tmp/my archive.hel.zip"),
            Some(Path::new("/src/.mj/keep/sess 1")),
            &[PathBuf::from("big file.bin"), PathBuf::from("cache/tmp")],
        )
        .expect("format export");
        assert!(export.contains("'/opt/my worker/mj-worker'"));
        assert!(export.contains("'/var/tmp/my archive.hel.zip'"));
        assert!(export.contains("--keep-dir '/src/.mj/keep/sess 1'"));
        assert!(export.contains("--skip 'big file.bin'"));
        assert!(export.contains("--skip 'cache/tmp'"));
        assert!(!export.contains('"'));
        assert!(!export.contains('\\'));

        // Container-backed targets must wrap export and receive in `<engine> exec -i <container>`.
        let podman = TargetLocator::SshPodman {
            ssh: SshTarget {
                destination: "user@pod".into(),
                ssh_args: vec![],
            },
            container_id: "ctr-pod-1".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let docker = TargetLocator::SshDocker {
            ssh: SshTarget {
                destination: "user@doc".into(),
                ssh_args: vec![],
            },
            container_id: "ctr-doc-2".into(),
            borrowed_from: None,
        };
        let podman_export = format_forced_export_command(
            &podman,
            "/usr/local/bin/mj-worker",
            Path::new("/workspace/archive.hel.zip"),
            None,
            &[],
        )
        .expect("podman export");
        assert!(podman_export.starts_with("podman exec -i 'ctr-pod-1' "));

        let docker_receive = format_forced_receive_command(
            &docker,
            "/usr/local/bin/mj-worker",
            Path::new("/workspace/archive.hel.zip"),
            &"b".repeat(64),
            None,
        )
        .expect("docker receive");
        assert!(docker_receive.starts_with("docker exec -i 'ctr-doc-2' "));

        assert!(
            format_forced_export_command(
                &bare,
                "mj-worker",
                Path::new("/tmp/bad\"path"),
                None,
                &[]
            )
            .is_err()
        );
        assert!(
            format_forced_receive_command(
                &bare,
                "mj-worker",
                Path::new("/tmp/bad\npath"),
                "abc",
                None
            )
            .is_err()
        );
    }

    #[test]
    fn daemon_stream_pipe_does_not_hang_when_destination_exits_early() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_file = dir.path().join("big.bin");
        // >256 KiB payload so the source process would block on a full 64 KiB
        // pipe if `src_stdout` were not closed and `src_child` terminated.
        fs::write(&src_file, vec![0xABu8; 300 * 1024]).expect("write big.bin");

        let source_spec = CommandSpec::new(
            "cat",
            vec!["--".into(), src_file.to_string_lossy().into_owned()],
        );
        let dest_spec = CommandSpec::new("bash", ["-c", "exit 1"]);

        let err = ProcessStreamPipe
            .pipe(&source_spec, &dest_spec, Some(&"0".repeat(64)))
            .expect_err("early destination exit must return Err without hanging");
        assert!(
            err.to_string().contains("daemon-stream"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_host_and_daemon_stream_transfer_keep_dir_with_skip() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        // Tiny POSIX fake worker script that implements `export-move-stream`
        // and `receive-move-stream` via a tar of archive + keep_dir (excluding `--skip`).
        let fake_worker = dir.path().join("fake-mj-worker.sh");
        let script = r#"#!/usr/bin/env bash
set -euo pipefail
shift # drop "worker"
subcmd="$1"; shift
archive=""
keep_dir=""
skips=()
while [ $# -gt 0 ]; do
  case "$1" in
    --archive) archive="$2"; shift 2 ;;
    --keep-dir) keep_dir="$2"; shift 2 ;;
    --expected-sha256) shift 2 ;;
    --skip) skips+=("$2"); shift 2 ;;
    *) shift ;;
  esac
done
if [ "$subcmd" = "export-move-stream" ]; then
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  cp "$archive" "$stage/archive.bin"
  if [ -n "$keep_dir" ] && [ -d "$keep_dir" ]; then
    mkdir -p "$stage/keep"
    cp -R "$keep_dir"/. "$stage/keep/"
    for s in "${skips[@]}"; do
      rm -rf "$stage/keep/$s"
    done
  fi
  tar cf - -C "$stage" .
elif [ "$subcmd" = "receive-move-stream" ]; then
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  tar xf - -C "$stage"
  mkdir -p "$(dirname "$archive")"
  cp "$stage/archive.bin" "$archive"
  if [ -n "$keep_dir" ] && [ -d "$stage/keep" ]; then
    mkdir -p "$keep_dir"
    cp -R "$stage/keep"/. "$keep_dir/"
  fi
fi
"#;
        fs::write(&fake_worker, script).expect("write fake worker");
        fs::set_permissions(&fake_worker, fs::Permissions::from_mode(0o755)).expect("chmod");

        let src_archive = dir.path().join("src.hel.zip");
        fs::write(&src_archive, b"archive-payload").expect("write src archive");
        let sha = lower_hex(Sha256::digest(b"archive-payload"));
        let src_keep = dir.path().join("src_keep");
        fs::create_dir_all(&src_keep).expect("mkdir src_keep");
        fs::write(src_keep.join("keep.txt"), b"must-arrive").expect("write keep.txt");
        fs::write(src_keep.join("skip.txt"), b"must-skip").expect("write skip.txt");

        let make_peer = |keep_dest: PathBuf| DirectPeerConfig {
            operation_id: "op-keep".into(),
            source_address: "10.0.0.1".into(),
            destination_address: "10.0.0.2".into(),
            destination_ssh_destination: "user@dst".into(),
            source_ssh_destination: "user@src".into(),
            destination_host_key: "ssh-ed25519 AAAA".into(),
            source_host_key: "ssh-ed25519 BBBB".into(),
            source_stage_dir: dir.path().join("src-stage"),
            destination_stage_dir: dir.path().join("dst-stage"),
            destination_authorized_keys: dir.path().join("dst-auth"),
            source_authorized_keys: dir.path().join("src-auth"),
            source_worker_bin: fake_worker.to_string_lossy().into_owned(),
            destination_worker_bin: fake_worker.to_string_lossy().into_owned(),
            destination_ec2_grant: None,
            source_ec2_grant: None,
            keep: KeepTransferOptions {
                keep_dir: Some(src_keep.clone()),
                keep_dest: Some(keep_dest),
                skip: vec![PathBuf::from("skip.txt")],
            },
        };

        // 1. SameHost route carries archive + keep_dir (excluding skip.txt).
        let same_dst_archive = dir.path().join("same/dst.hel.zip");
        let same_dst_keep = dir.path().join("same/keep");
        let local = TargetLocator::LocalBare {
            worker_root: dir.path().to_string_lossy().into_owned(),
        };
        transfer_same_host_stream(
            &local,
            &src_archive,
            &local,
            &same_dst_archive,
            &sha,
            &ProcessExecutor,
            &make_peer(same_dst_keep.clone()),
        )
        .expect("same-host stream with keep");
        assert_eq!(fs::read(&same_dst_archive).unwrap(), b"archive-payload");
        assert_eq!(
            fs::read(same_dst_keep.join("keep.txt")).unwrap(),
            b"must-arrive"
        );
        assert!(!same_dst_keep.join("skip.txt").exists());

        // 2. DaemonStream route carries archive + keep_dir (excluding skip.txt).
        let daemon_dst_archive = dir.path().join("daemon/dst.hel.zip");
        let daemon_dst_keep = dir.path().join("daemon/keep");
        let ssh_a = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host-a".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let ssh_b = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host-b".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        transfer_daemon_stream_with_worker(
            &ssh_a,
            &src_archive,
            &ssh_b,
            &daemon_dst_archive,
            &sha,
            &make_peer(daemon_dst_keep.clone()),
            &TwoHostLocalStreamPipe,
        )
        .expect("daemon-stream with keep");
        assert_eq!(fs::read(&daemon_dst_archive).unwrap(), b"archive-payload");
        assert_eq!(
            fs::read(daemon_dst_keep.join("keep.txt")).unwrap(),
            b"must-arrive"
        );
        assert!(!daemon_dst_keep.join("skip.txt").exists());

        // 3. Non-SSH SameHost route safely handles paths containing apostrophes.
        let apo_src = dir.path().join("it's-src.hel.zip");
        let apo_dst = dir.path().join("it's-dst.hel.zip");
        let apo_keep_dst = dir.path().join("it's-keep");
        fs::write(&apo_src, b"archive-payload").expect("write apostrophe archive");
        transfer_same_host_stream(
            &local,
            &apo_src,
            &local,
            &apo_dst,
            &sha,
            &ProcessExecutor,
            &make_peer(apo_keep_dst.clone()),
        )
        .expect("same-host stream with apostrophe path");
        assert_eq!(fs::read(&apo_dst).unwrap(), b"archive-payload");
        assert_eq!(
            fs::read(apo_keep_dst.join("keep.txt")).unwrap(),
            b"must-arrive"
        );
    }

    #[test]
    fn direct_ssh_command_disables_control_master_and_control_path() {
        let cmd = format_direct_ssh_command(
            Path::new("/tmp/mj-move-op1.key"),
            Path::new("/tmp/mj-move-op1.known_hosts"),
            "mj-move-peer-op1",
            "user@peer",
        );
        assert!(cmd.contains("-o ControlMaster=no"));
        assert!(cmd.contains("-o ControlPath=none"));
        assert!(cmd.contains("-o ControlPersist=no"));
        assert!(cmd.contains("-o IdentitiesOnly=yes"));
        assert!(cmd.contains("-o StrictHostKeyChecking=yes"));
    }

    #[test]
    fn ssh_nitro_uses_microvm_engine_shell_words() {
        let nitro = TargetLocator::SshNitro {
            ssh: SshTarget {
                destination: "ec2-user@nitro-host".into(),
                ssh_args: vec![],
            },
            container_id: "vm-123".into(),
            borrowed_from: None,
        };
        let expected_words = targets::engine_shell_words(targets::MICROVM_ENGINE);
        let shell_export = format_shell_export_command(
            &nitro,
            "/usr/local/bin/mj-worker",
            Path::new("/workspace/archive.hel.zip"),
            None,
            &[],
        )
        .expect("nitro shell export");
        assert!(
            shell_export.starts_with(&format!("{expected_words} exec -i 'vm-123' ")),
            "unexpected nitro shell export: {shell_export}"
        );
        assert!(!shell_export.contains("nitro exec"));

        let forced_receive = format_forced_receive_command(
            &nitro,
            "/usr/local/bin/mj-worker",
            Path::new("/workspace/archive.hel.zip"),
            &"c".repeat(64),
            None,
        )
        .expect("nitro forced receive");
        assert!(forced_receive.starts_with("sudo -n "));
        assert!(forced_receive.contains(" microvm exec -i 'vm-123' "));
        assert!(!forced_receive.contains("nitro exec"));
        assert!(!forced_receive.contains('"'));
    }

    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs bash on the controller")]
    fn process_stream_pipe_applies_env_cwd_and_remote_specs_carry_ssh_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst_file = dir.path().join("env-out.txt");

        let mut src_spec =
            CommandSpec::new("bash", ["-c", "printf '%s:%s' \"$MJ_TEST_VAR\" \"$(pwd)\""]);
        src_spec
            .env
            .insert("MJ_TEST_VAR".into(), "hello-pipe".into());
        src_spec.cwd = Some(dir.path().to_path_buf());

        let dst_spec = CommandSpec::new(
            "bash",
            [
                "-c",
                &format!("cat > {}", shell_quote(&dst_file.to_string_lossy())),
            ],
        );

        ProcessStreamPipe
            .pipe(&src_spec, &dst_spec, None)
            .expect("pipe with env and cwd");
        let written = fs::read_to_string(&dst_file).expect("read env-out");
        assert!(written.starts_with("hello-pipe:"));
        assert!(written.ends_with(&dir.path().to_string_lossy().into_owned()));

        // Verify remote SSH locators produce specs carrying `ssh_session: Some(_)`.
        let ssh_target = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@remote-host".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let remote_spec = targets::locator_command(
            SshTrafficClass::Bulk,
            &ssh_target,
            vec!["cat".into(), "--".into(), "/tmp/a".into()],
        );
        assert!(
            remote_spec.ssh_session.is_some(),
            "remote SSH CommandSpec must carry ssh_session for ProcessStreamPipe leasing"
        );
    }

    #[derive(Clone, Default)]
    struct FailingTransferFakeEc2 {
        rules: Arc<Mutex<BTreeMap<String, Vec<Ec2IngressRule>>>>,
    }

    impl Ec2SecurityGroupClient for FailingTransferFakeEc2 {
        fn authorize_ingress(
            &self,
            group_id: &str,
            port: u16,
            cidr_ip: &str,
            description: &str,
        ) -> std::result::Result<(), Ec2ApiError> {
            self.rules
                .lock()
                .unwrap()
                .entry(group_id.to_owned())
                .or_default()
                .push(Ec2IngressRule {
                    port,
                    cidr_ip: cidr_ip.to_owned(),
                    description: Some(description.to_owned()),
                });
            Ok(())
        }

        fn revoke_ingress(
            &self,
            group_id: &str,
            port: u16,
            cidr_ip: &str,
        ) -> std::result::Result<(), Ec2ApiError> {
            let mut map = self.rules.lock().unwrap();
            let list = map.entry(group_id.to_owned()).or_default();
            let before = list.len();
            list.retain(|r| !(r.port == port && r.cidr_ip == cidr_ip));
            if list.len() == before {
                return Err(Ec2ApiError::NotFound);
            }
            Ok(())
        }

        fn describe_ingress_rules(
            &self,
            group_id: &str,
        ) -> std::result::Result<Vec<Ec2IngressRule>, Ec2ApiError> {
            Ok(self
                .rules
                .lock()
                .unwrap()
                .get(group_id)
                .cloned()
                .unwrap_or_default())
        }
    }

    struct MidTransferFailExecutor {
        recorded_purposes: Mutex<Vec<String>>,
    }

    impl MidTransferFailExecutor {
        fn new() -> Self {
            Self {
                recorded_purposes: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandExecutor for MidTransferFailExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let purpose = command.purpose.clone();
            self.recorded_purposes.lock().unwrap().push(purpose.clone());
            if purpose == "direct push Move stream" {
                return Ok(CommandOutput {
                    status: 255,
                    stdout: Vec::new(),
                    stderr: b"simulated mid-transfer SSH disconnect".to_vec(),
                });
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            _input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> DeadlinePause {
            DeadlinePause::none()
        }
    }

    #[test]
    fn mid_transfer_failure_cleans_up_key_known_hosts_authorized_keys_and_ec2_grant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = InMemoryGrantStore::new();
        let fake_ec2 = FailingTransferFakeEc2::default();
        let mgr = Ec2GrantManager::new(
            store,
            fake_ec2.clone(),
            "daemon-test",
            Ec2GrantConfig {
                backoff: Duration::from_millis(1),
                max_wait_attempts: 2,
            },
        );
        let executor = MidTransferFailExecutor::new();

        let src = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@src".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let dst = TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@dst".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let peer = DirectPeerConfig {
            operation_id: "op-midfail".into(),
            source_address: "10.0.0.1".into(),
            destination_address: "10.0.0.2".into(),
            destination_ssh_destination: "user@dst".into(),
            source_ssh_destination: "user@src".into(),
            destination_host_key: "ssh-ed25519 AAAA".into(),
            source_host_key: "ssh-ed25519 BBBB".into(),
            source_stage_dir: dir.path().join("src-stage"),
            destination_stage_dir: dir.path().join("dst-stage"),
            destination_authorized_keys: dir.path().join("dst-auth-keys"),
            source_authorized_keys: dir.path().join("src-auth-keys"),
            source_worker_bin: "mj-worker".into(),
            destination_worker_bin: "mj-worker".into(),
            destination_ec2_grant: Some(Ec2GrantKey::ssh("sg-dst", "10.0.0.1/32")),
            source_ec2_grant: None,
            keep: KeepTransferOptions::default(),
        };

        let err = try_direct_push(
            &src,
            Path::new("/tmp/src.hel.zip"),
            &dst,
            Path::new("/tmp/dst.hel.zip"),
            &"a".repeat(64),
            &executor,
            &peer,
            Some(&mgr),
        )
        .expect_err("mid-transfer failure must return Err");
        assert!(err.to_string().contains("direct push failed"));

        // EC2 grant must have been revoked and removed from store.
        assert!(
            fake_ec2
                .describe_ingress_rules("sg-dst")
                .unwrap()
                .is_empty(),
            "EC2 ingress rule must be revoked on mid-transfer failure"
        );
        assert!(mgr.store().snapshot().is_empty());

        // Cleanup commands for authorized_keys and stage files must have run.
        let purposes = executor.recorded_purposes.lock().unwrap().clone();
        assert!(
            purposes
                .iter()
                .any(|p| p == "remove temporary Move authorized_keys entry")
        );
        assert!(
            purposes
                .iter()
                .any(|p| p == "remove temporary Move SSH key and known_hosts")
        );
    }

    #[test]
    fn cleanup_interrupted_move_credentials_revokes_remote_authorized_keys_and_stage_files() {
        #[derive(Default)]
        struct RecordingCleanupExecutor {
            commands: std::sync::Mutex<Vec<(String, Vec<String>)>>,
        }
        impl CommandExecutor for RecordingCleanupExecutor {
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }
            fn cancellation_requested_without_deadline(&self) -> bool {
                false
            }
            fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.execute(command)
            }
            fn execute_with_stdin_without_deadline(
                &self,
                command: &CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> Result<CommandOutput> {
                self.execute_with_stdin(command, input)
            }
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands
                    .lock()
                    .unwrap()
                    .push((command.purpose.clone(), command.args.clone()));
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
            fn execute_with_stdin(
                &self,
                command: &CommandSpec,
                _input: &mut (dyn std::io::Read + Send),
            ) -> Result<CommandOutput> {
                self.execute(command)
            }
        }
        let src = TargetLocator::SshBare {
            ssh: mj_core::targets::SshTarget {
                destination: "user@src.example.com".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let dst = TargetLocator::SshBare {
            ssh: mj_core::targets::SshTarget {
                destination: "user@dst.example.com".into(),
                ssh_args: vec![],
            },
            workspace: "/w".into(),
            worker_id: None,
        };
        let keys = vec![
            mj_core::state::GrantedMoveKey {
                host: "src.example.com".into(),
                comment: "mj-move-op-crash".into(),
                private_key_path: Some(PathBuf::from(
                    "/home/u/.cache/mjolnir/move-stage/mj-move-op-crash.key",
                )),
                known_hosts_path: Some(PathBuf::from(
                    "/home/u/.cache/mjolnir/move-stage/mj-move-op-crash.known_hosts",
                )),
                authorized_keys_path: Some(PathBuf::from("/home/u/.ssh/authorized_keys")),
                worker_bin: Some("/home/u/.local/share/mjolnir/workers/s1/hel".into()),
                ssh_target: None,
            },
            mj_core::state::GrantedMoveKey {
                host: "dst.example.com".into(),
                comment: "mj-move-op-crash".into(),
                private_key_path: Some(PathBuf::from(
                    "/home/u/.cache/mjolnir/move-stage/mj-move-op-crash.pull.key",
                )),
                known_hosts_path: Some(PathBuf::from(
                    "/home/u/.cache/mjolnir/move-stage/mj-move-op-crash.pull.known_hosts",
                )),
                authorized_keys_path: Some(PathBuf::from("/home/u/.ssh/authorized_keys")),
                worker_bin: Some("/home/u/.local/share/mjolnir/workers/s1/hel".into()),
                ssh_target: Some(mj_core::targets::SshTarget {
                    destination: "user@dst.example.com".into(),
                    ssh_args: vec![],
                }),
            },
        ];
        let executor = RecordingCleanupExecutor::default();
        let remaining =
            cleanup_interrupted_move_credentials("op-different", &keys, &[&src, &dst], &executor);
        assert!(
            remaining.is_empty(),
            "all remote credentials should be revoked"
        );
        let commands = executor.commands.lock().unwrap();
        assert_eq!(
            commands.len(),
            4,
            "expected 2 authorized-keys remove + 2 rm -f: {commands:?}"
        );
        assert!(commands.iter().any(|(p, args)| {
            p == "remove temporary Move authorized_keys entry"
                && args.iter().any(|a| a.contains("src.example.com"))
                && args.iter().any(|a| a.contains("authorized-keys"))
                && args.iter().any(|a| a.contains("op-crash"))
        }));
        assert!(commands.iter().any(|(p, args)| {
            p == "remove temporary Move authorized_keys entry"
                && args.iter().any(|a| a.contains("dst.example.com"))
                && args.iter().any(|a| a.contains("authorized-keys"))
                && args.iter().any(|a| a.contains("op-crash"))
        }));
        assert!(commands.iter().any(|(p, args)| {
            p == "remove temporary Move SSH key and known_hosts"
                && args.iter().any(|a| a.contains("src.example.com"))
                && args.iter().any(|a| a.contains("mj-move-op-crash.key"))
        }));
        assert!(commands.iter().any(|(p, args)| {
            p == "remove temporary Move SSH key and known_hosts"
                && args.iter().any(|a| a.contains("dst.example.com"))
                && args.iter().any(|a| a.contains("mj-move-op-crash.pull.key"))
        }));

        // Self-contained `ssh_target` revokes even when no candidate targets exist
        // (e.g. after source retirement or carry-over onto a newer Move).
        let deferred_executor = RecordingCleanupExecutor::default();
        let deferred_remaining = cleanup_interrupted_move_credentials(
            "op-later-move",
            std::slice::from_ref(&keys[1]),
            &[],
            &deferred_executor,
        );
        assert!(deferred_remaining.is_empty());
        assert_eq!(deferred_executor.commands.lock().unwrap().len(), 2);
    }
}

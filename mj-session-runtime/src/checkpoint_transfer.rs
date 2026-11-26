//! Controller-side checkpoint transport and verified teardown gates.
use crate::targets::{CommandExecutor, CommandPlan, CommandSpec, TargetLocator, worker_root};
use anyhow::{Context, Result, bail, ensure};
use mj_checkpoint::archive::validate_component;
use mj_checkpoint::checkpoint::*;
use mj_core::targets::SshTrafficClass;
use std::fs;
use std::path::{Path, PathBuf};
/// Export by streaming the spec to the worker's standard input.
///
/// Every wrapper this builds keeps the target's stdin attached: the container
/// engines are invoked with `exec -i` and `ssh` forwards stdin by default.
///
/// The spec crosses SSH gzipped. The decompression runs in the SSH shell, so
/// it works with any worker version.
pub fn export_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    export_command(locator, session_id, EXPORT_SPEC_STDIN).map(CommandSpec::gzip_ssh_stdin)
}

pub fn suspend_preflight_stdin_command(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandSpec> {
    suspend_preflight_stdin_command_with_binary(locator, session_id, "hel")
}

pub fn suspend_preflight_stdin_command_with_binary(
    locator: &TargetLocator,
    session_id: &str,
    binary_name: &str,
) -> Result<CommandSpec> {
    ensure!(
        !binary_name.is_empty() && !binary_name.contains('/') && !binary_name.contains(".."),
        "invalid worker binary name {binary_name:?}"
    );
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/{binary_name}"),
        "worker".into(),
        "suspend-preflight".into(),
        "--spec".into(),
        EXPORT_SPEC_STDIN.into(),
    ];
    crate::targets::command_on_locator(
        SshTrafficClass::Bulk,
        locator,
        session_id,
        args,
        "suspend preflight target",
    )
}

pub fn resolve_suspend_blockers_stdin_command(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandSpec> {
    resolve_suspend_blockers_stdin_command_with_binary(locator, session_id, "hel")
}

pub fn resolve_suspend_blockers_stdin_command_with_binary(
    locator: &TargetLocator,
    session_id: &str,
    binary_name: &str,
) -> Result<CommandSpec> {
    ensure!(
        !binary_name.is_empty() && !binary_name.contains('/') && !binary_name.contains(".."),
        "invalid worker binary name {binary_name:?}"
    );
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/{binary_name}"),
        "worker".into(),
        "resolve-suspend-blockers".into(),
        "--spec".into(),
        EXPORT_SPEC_STDIN.into(),
    ];
    crate::targets::command_on_locator(
        SshTrafficClass::Bulk,
        locator,
        session_id,
        args,
        "resolve suspend blockers target",
    )
}

pub fn capture_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "capture-checkpoint",
        "capture target checkpoint",
    )
}

/// Pack a captured checkpoint by streaming `CheckpointPackSpec` (which carries
/// `canonical_session`) over stdin, gzipped across SSH.
pub fn pack_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "pack-checkpoint",
        "pack target checkpoint",
    )
    .map(CommandSpec::gzip_ssh_stdin)
}

fn checkpoint_stdin_command(
    locator: &TargetLocator,
    session_id: &str,
    subcommand: &str,
    purpose: &str,
) -> Result<CommandSpec> {
    let root = worker_root(locator, session_id)?;
    let args = vec![format!("{root}/hel"), "worker".into(), subcommand.into()];
    crate::targets::command_on_locator(SshTrafficClass::Bulk, locator, session_id, args, purpose)
}

pub fn export_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_checkpoint_path(locator, spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "export-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    crate::targets::command_on_locator(
        SshTrafficClass::Bulk,
        locator,
        session_id,
        args,
        "export target checkpoint",
    )
}

/// Restore by streaming the spec to the worker's standard input, gzipped over
/// SSH. The worker must be this controller's build: an older `restore-checkpoint`
/// would read `-` as a file name.
pub fn restore_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "restore-checkpoint".into(),
        "--spec".into(),
        EXPORT_SPEC_STDIN.into(),
    ];
    crate::targets::command_on_locator(
        SshTrafficClass::Bulk,
        locator,
        session_id,
        args,
        "restore target checkpoint",
    )
    .map(CommandSpec::gzip_ssh_stdin)
}

#[derive(Debug, Clone)]
pub struct CheckpointTransfer<'a> {
    pub locator: &'a TargetLocator,
    pub session_id: &'a str,
    pub operation_id: &'a str,
    pub remote_archive: &'a str,
    pub destination: &'a Path,
    pub expected_sha256: &'a str,
    pub expected_event_frontier: u64,
    pub expected_event_frontier_digest: &'a str,
}

/// Unforgeable outside this module: proof that a controller-local archive has
/// the exact digest reported by the target after its atomic install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCheckpoint {
    session_id: String,
    archive_path: PathBuf,
    sha256: String,
    event_frontier: u64,
    event_frontier_digest: String,
}

impl VerifiedCheckpoint {
    pub fn archive_path(&self) -> &Path {
        &self.archive_path
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn event_frontier(&self) -> u64 {
        self.event_frontier
    }
    pub fn event_frontier_digest(&self) -> &str {
        &self.event_frontier_digest
    }
    pub const fn teardown_allowed(&self) -> bool {
        true
    }
}

/// A host staging archive on an SSH container host that was downloaded and
/// verified but not yet removed. The caller owns its removal (see
/// [`cleanup_host_staging`]) or promotes it in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedHostStaging {
    /// Home-relative path of the staged archive on the SSH host.
    pub staging_path: String,
    /// The host's `$HOME`, reported by the staging command itself, when it
    /// reported one.
    pub remote_home: Option<String>,
}

/// Error context marking that the retained host staging file may still exist
/// because its removal failed. An error without it means the file is gone or
/// was never created.
#[derive(Debug)]
pub struct HostStagingNotRemoved;

impl std::fmt::Display for HostStagingNotRemoved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("target checkpoint host staging may remain on the SSH host")
    }
}

/// Whether the failed [`CheckpointTransfer::execute_retaining_host_staging`]
/// left its host staging file in place.
pub fn host_staging_may_remain(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HostStagingNotRemoved>().is_some()
}

const REMOTE_HOME_MARKER: &str = "MJ_REMOTE_HOME=";

impl CheckpointTransfer<'_> {
    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<VerifiedCheckpoint> {
        self.run(executor, false).map(|(verified, _)| verified)
    }

    /// The host staging path this transfer uses, when its target is a
    /// container or host-bare worker behind SSH.
    pub fn host_staging_path(&self) -> Option<Result<String>> {
        (matches!(self.locator, TargetLocator::SshHostBare { .. })
            || self
                .locator
                .container_endpoint()
                .is_some_and(|endpoint| endpoint.ssh.is_some()))
        .then(|| remote_staging_path(self.session_id, self.operation_id))
    }

    /// [`Self::execute`], except that an SSH target's host staging
    /// archive is kept after verification so the caller can promote it
    /// without uploading it again. The caller must remove it with
    /// [`cleanup_host_staging`] or consume it. A failure removes the staging
    /// file itself; [`host_staging_may_remain`] reports when that failed.
    pub fn execute_retaining_host_staging(
        &self,
        executor: &impl CommandExecutor,
    ) -> Result<(VerifiedCheckpoint, Option<RetainedHostStaging>)> {
        self.run(executor, true)
    }

    fn run(
        &self,
        executor: &impl CommandExecutor,
        retain_host_staging: bool,
    ) -> Result<(VerifiedCheckpoint, Option<RetainedHostStaging>)> {
        validate_checkpoint_path(self.locator, self.remote_archive)?;
        let parent = self.destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        // Keep only the reserved name, not an open handle: the copy commands
        // write this file from another process, and Windows OpenSSH `scp`
        // cannot open a file this process still holds open.
        let temporary = tempfile::Builder::new()
            .prefix(".hel-checkpoint-")
            .tempfile_in(parent)?
            .into_temp_path();
        let path = temporary.to_path_buf();
        let staging = remote_staging_path(self.session_id, self.operation_id)?;
        // While retained, the staging file outlives this call, so every later
        // failure removes it before returning.
        let abandon = |error: anyhow::Error| {
            if retain_host_staging {
                abandon_host_staging(self.locator, &staging, executor, error)
            } else {
                error
            }
        };
        let transfer_result = transfer_plan(
            self.locator,
            self.session_id,
            self.remote_archive,
            &path,
            &staging,
        )?
        .execute(executor)
        .context("download target checkpoint");
        let remote_home = transfer_result
            .as_ref()
            .ok()
            .and_then(|outputs| outputs.first())
            .and_then(|output| parse_remote_home(&output.stdout));
        if retain_host_staging {
            if let Err(error) = transfer_result {
                return Err(abandon(error));
            }
        } else {
            let staging_cleanup_result = cleanup_host_staging(self.locator, &staging, executor);
            if let Err(error) = transfer_result {
                return match staging_cleanup_result {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!(
                        "clean target checkpoint host staging also failed: {cleanup:#}"
                    ))),
                };
            }
            staging_cleanup_result.context("clean target checkpoint host staging")?;
        }
        let sha256 = match checkpoint_sha256(&path).context("hash downloaded checkpoint") {
            Ok(sha256) => sha256,
            Err(error) => return Err(abandon(error)),
        };
        if sha256 != self.expected_sha256 {
            let downloaded_bytes = match fs::metadata(&path).context("stat downloaded checkpoint") {
                Ok(metadata) => metadata.len(),
                Err(error) => return Err(abandon(error)),
            };
            return Err(abandon(anyhow::anyhow!(
                "target and controller checkpoint checksums differ for complete checkpoint archive: \
                 session={}, operation={}, expected_sha256={}, downloaded_sha256={}, downloaded_bytes={}; \
                 target archive retained at {}. The previous verified checkpoint was not replaced and \
                 the source workspace was not removed. Preserve the source and retry a fresh export; \
                 if this repeats, inspect the retained archive and its transfer path",
                self.session_id,
                self.operation_id,
                self.expected_sha256,
                sha256,
                downloaded_bytes,
                self.remote_archive,
            )));
        }
        if let Err(error) = temporary.persist(self.destination) {
            return Err(abandon(error.error.into()));
        }
        // The bytes were already checksum-verified in this same directory and
        // the rename is atomic, so installation only has to make the copy
        // private and durable; re-reading it would hash the same archive again.
        let post_install = (|| -> Result<()> {
            restrict_permissions(self.destination)?;
            sync_directory(parent)
        })();
        if let Err(error) = post_install {
            return Err(abandon(remove_failed_checkpoint_install(
                self.destination,
                error,
            )));
        }
        let retained = (retain_host_staging && self.host_staging_path().is_some()).then_some(
            RetainedHostStaging {
                staging_path: staging,
                remote_home,
            },
        );
        Ok((
            VerifiedCheckpoint {
                session_id: self.session_id.to_owned(),
                archive_path: self.destination.to_path_buf(),
                sha256,
                event_frontier: self.expected_event_frontier,
                event_frontier_digest: self.expected_event_frontier_digest.to_owned(),
            },
            retained,
        ))
    }

    pub fn cleanup_plan(&self, gate: &VerifiedCheckpoint) -> Result<CommandPlan> {
        ensure!(
            gate.session_id == self.session_id,
            "checkpoint gate belongs to another session"
        );
        cleanup_plan(self.locator, self.session_id, self.remote_archive)
    }
}

/// Remove the host-side staging copy of an SSH transfer. Other
/// targets have none, so this does nothing for them.
///
/// Containers need a host-side copy, and host-bare targets need an ASCII path
/// that SCP can address safely. The copy is disposable; the target archive
/// remains gated by [`CheckpointTransfer::cleanup_plan`] until the
/// local copy is verified.
pub fn cleanup_host_staging(
    locator: &TargetLocator,
    staging: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_remote_path(staging)?;
    let ssh = match locator {
        TargetLocator::SshHostBare { ssh, .. } => Some(ssh),
        _ => locator
            .container_endpoint()
            .and_then(|endpoint| endpoint.ssh),
    };
    if let Some(ssh) = ssh {
        let command =
            crate::targets::ssh_command(SshTrafficClass::Control, ssh, ["rm", "-f", "--", staging])
                .purpose("remove remote checkpoint staging");
        let output = executor.execute(&command)?;
        if output.status != 0 {
            bail!(
                "{} failed with status {}: {}",
                command.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

/// Remove retained host staging after `error`, reporting a removal failure
/// both in the message and through [`host_staging_may_remain`].
fn abandon_host_staging(
    locator: &TargetLocator,
    staging: &str,
    executor: &impl CommandExecutor,
    error: anyhow::Error,
) -> anyhow::Error {
    match cleanup_host_staging(locator, staging, executor) {
        Ok(()) => error,
        Err(cleanup) => error
            .context(format!(
                "clean target checkpoint host staging also failed: {cleanup:#}"
            ))
            .context(HostStagingNotRemoved),
    }
}

/// The `$HOME` the staging command printed, if it printed an absolute one.
fn parse_remote_home(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|line| line.strip_prefix(REMOTE_HOME_MARKER))
        .map(str::trim)
        .filter(|home| home.starts_with('/'))
        .map(str::to_owned)
}

pub fn transfer_plan(
    locator: &TargetLocator,
    session_id: &str,
    remote_archive: &str,
    local_temporary: &Path,
    staging: &str,
) -> Result<CommandPlan> {
    validate_checkpoint_path(locator, remote_archive)?;
    validate_remote_path(staging)?;
    ensure!(
        local_temporary.is_absolute(),
        "local temporary path must be absolute"
    );
    worker_root(locator, session_id)?;
    let local = local_temporary.to_string_lossy().into_owned();
    let commands = if let Some(endpoint) = locator.container_endpoint() {
        let source = format!("{}:{remote_archive}", endpoint.container_id);
        if let Some(ssh) = endpoint.ssh {
            // One SSH session creates the staging directory, copies the
            // archive out of the container, and reports the host's `$HOME`,
            // so a caller that promotes the staged file needs no further
            // round trip to learn where the cache lives.
            let script = format!(
                "set -eu; (umask 077; mkdir -p -- .local/share/hel/transfers); \
                 chmod 0700 .local/share/hel/transfers; \
                 {} cp \"$1\" \"$2\"; printf '{REMOTE_HOME_MARKER}%s\\n' \"$HOME\"",
                crate::targets::engine_shell_words(endpoint.engine)
            );
            vec![
                crate::targets::ssh_command_owned(
                    SshTrafficClass::Bulk,
                    ssh,
                    vec![
                        "sh".to_owned(),
                        "-c".to_owned(),
                        script,
                        "hel-checkpoint-stage".to_owned(),
                        source,
                        staging.to_owned(),
                    ],
                )
                .purpose("stage remote container checkpoint"),
                crate::targets::scp_download(ssh, staging, &local)
                    .purpose("download remote container checkpoint over SSH"),
            ]
        } else {
            vec![
                CommandSpec::new(endpoint.engine, ["cp", &source, &local]).purpose(format!(
                    "download checkpoint from local {}",
                    endpoint.engine
                )),
            ]
        }
    } else {
        match locator {
            TargetLocator::LocalBare { .. } => vec![
                CommandSpec::new("cp", ["--", remote_archive, local.as_str()])
                    .purpose("copy local bare checkpoint"),
            ],
            TargetLocator::SshHostBare { ssh, .. } => {
                let script = format!(
                    "set -eu; (umask 077; mkdir -p -- .local/share/hel/transfers); chmod 0700 .local/share/hel/transfers; ln -f -- \"$1\" \"$2\" 2>/dev/null || cp -f -- \"$1\" \"$2\"; printf '{REMOTE_HOME_MARKER}%s\\n' \"$HOME\""
                );
                vec![
                    crate::targets::ssh_command_owned(
                        SshTrafficClass::Bulk,
                        ssh,
                        vec![
                            "sh".into(),
                            "-c".into(),
                            script,
                            "hel-checkpoint-stage".into(),
                            remote_archive.into(),
                            staging.into(),
                        ],
                    )
                    .purpose("stage remote host checkpoint"),
                    crate::targets::scp_download(ssh, staging, &local)
                        .purpose("download checkpoint over SSH"),
                ]
            }
            TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => vec![
                crate::targets::scp_download(ssh, remote_archive, &local)
                    .purpose("download checkpoint over SSH"),
            ],
            _ => unreachable!("containers handled above"),
        }
    };
    Ok(CommandPlan {
        description: format!("download checkpoint for {session_id}"),
        commands,
    })
}

fn cleanup_plan(locator: &TargetLocator, session_id: &str, remote: &str) -> Result<CommandPlan> {
    validate_checkpoint_path(locator, remote)?;
    worker_root(locator, session_id)?;
    let commands = vec![
        crate::targets::locator_command(
            SshTrafficClass::Control,
            locator,
            ["rm", "-f", "--", remote].map(str::to_owned).to_vec(),
        )
        .purpose("remove checkpoint staging"),
    ];
    Ok(CommandPlan {
        description: format!("clean checkpoint for {session_id}"),
        commands,
    })
}

fn remote_staging_path(session_id: &str, operation_id: &str) -> Result<String> {
    validate_component(session_id, "session ID")?;
    validate_component(operation_id, "checkpoint operation ID")?;
    Ok(format!(
        ".local/share/hel/transfers/{session_id}-{operation_id}.hel.zip"
    ))
}

fn validate_checkpoint_path(locator: &TargetLocator, path: &str) -> Result<()> {
    if matches!(locator, TargetLocator::SshHostBare { .. }) {
        ensure!(
            mj_core::target_path::is_absolute(Path::new(path))
                && !path.chars().any(char::is_control)
                && !Path::new(path)
                    .components()
                    .any(|part| part == std::path::Component::ParentDir),
            "invalid host-bare checkpoint path {path:?}"
        );
        return Ok(());
    }
    if matches!(locator, TargetLocator::LocalBare { .. }) {
        // Local commands receive argv directly. Their paths never pass through
        // SCP or a remote shell and may contain spaces, Unicode, and punctuation.
        ensure!(
            !path.is_empty()
                && !path.contains('\0')
                && !Path::new(path)
                    .components()
                    .any(|part| part == std::path::Component::ParentDir),
            "invalid local checkpoint path {path:?}"
        );
        Ok(())
    } else {
        validate_remote_path(path)
    }
}

fn validate_remote_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "empty remote checkpoint path");
    ensure!(
        path.bytes()
            .all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'~' | b'.' | b'-' | b'_')),
        "unsafe remote checkpoint path {path:?}"
    );
    ensure!(
        !path.split('/').any(|component| component == ".."),
        "remote checkpoint path traverses parent: {path:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests;

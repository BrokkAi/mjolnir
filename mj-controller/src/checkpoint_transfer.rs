//! Controller-side checkpoint transport and verified teardown gates.
use crate::targets::{CommandExecutor, CommandPlan, CommandSpec, TargetLocator, worker_root};
use anyhow::{Context, Result, bail, ensure};
use mj_checkpoint::archive::validate_component;
use mj_checkpoint::checkpoint::*;
use std::fs;
use std::path::{Path, PathBuf};
/// Export by streaming the spec to the worker's standard input.
///
/// Every wrapper this builds keeps the target's stdin attached: the container
/// engines are invoked with `exec -i` and `ssh` forwards stdin by default.
pub fn export_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    export_command(locator, session_id, EXPORT_SPEC_STDIN)
}

pub fn capture_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "capture-checkpoint",
        "capture target checkpoint",
    )
}

pub fn pack_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "pack-checkpoint",
        "pack target checkpoint",
    )
}

fn checkpoint_stdin_command(
    locator: &TargetLocator,
    session_id: &str,
    subcommand: &str,
    purpose: &str,
) -> Result<CommandSpec> {
    let root = worker_root(locator, session_id)?;
    let args = vec![format!("{root}/hel"), "worker".into(), subcommand.into()];
    crate::targets::command_on_locator(locator, session_id, args, purpose)
}

pub fn export_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_remote_path(spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "export-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    crate::targets::command_on_locator(locator, session_id, args, "export target checkpoint")
}

pub fn restore_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_remote_path(spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "restore-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    crate::targets::command_on_locator(locator, session_id, args, "restore target checkpoint")
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

impl CheckpointTransfer<'_> {
    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<VerifiedCheckpoint> {
        validate_remote_path(self.remote_archive)?;
        let parent = self.destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".hel-checkpoint-")
            .tempfile_in(parent)?;
        let path = temporary.path().to_path_buf();
        let staging = remote_staging_path(self.session_id, self.operation_id)?;
        let transfer_result = transfer_plan(
            self.locator,
            self.session_id,
            self.remote_archive,
            &path,
            &staging,
        )?
        .execute(executor)
        .context("download target checkpoint");
        let staging_cleanup_result = cleanup_transfer_staging(self.locator, &staging, executor);
        if let Err(error) = transfer_result {
            return match staging_cleanup_result {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "clean target checkpoint host staging also failed: {cleanup:#}"
                ))),
            };
        }
        staging_cleanup_result.context("clean target checkpoint host staging")?;
        let sha256 = checkpoint_sha256(&path).context("hash downloaded checkpoint")?;
        ensure!(
            sha256 == self.expected_sha256,
            "target and controller checkpoint checksums differ for complete checkpoint archive: \
             session={}, operation={}, expected_sha256={}, downloaded_sha256={}, downloaded_bytes={}; \
             target archive retained at {}. The previous verified checkpoint was not replaced and \
             the source workspace was not removed. Preserve the source and retry a fresh export; \
             if this repeats, inspect the retained archive and its transfer path",
            self.session_id,
            self.operation_id,
            self.expected_sha256,
            sha256,
            fs::metadata(&path)
                .context("stat downloaded checkpoint")?
                .len(),
            self.remote_archive,
        );
        temporary
            .persist(self.destination)
            .map_err(|error| error.error)?;
        // The bytes were already checksum-verified in this same directory and
        // the rename is atomic, so installation only has to make the copy
        // private and durable; re-reading it would hash the same archive again.
        let post_install = (|| -> Result<()> {
            restrict_permissions(self.destination)?;
            sync_directory(parent)
        })();
        if let Err(error) = post_install {
            return Err(remove_failed_checkpoint_install(self.destination, error));
        }
        Ok(VerifiedCheckpoint {
            session_id: self.session_id.to_owned(),
            archive_path: self.destination.to_path_buf(),
            sha256,
            event_frontier: self.expected_event_frontier,
            event_frontier_digest: self.expected_event_frontier_digest.to_owned(),
        })
    }

    pub fn cleanup_plan(&self, gate: &VerifiedCheckpoint) -> Result<CommandPlan> {
        ensure!(
            gate.session_id == self.session_id,
            "checkpoint gate belongs to another session"
        );
        cleanup_plan(self.locator, self.session_id, self.remote_archive)
    }
}

/// SSH container transfers use a host-side copy because `scp` cannot address
/// a path inside the container. The copy is disposable and must be removed as
/// soon as it has been downloaded; the in-container archive remains gated by
/// [`CheckpointTransfer::cleanup_plan`] until the local copy is verified.
fn cleanup_transfer_staging(
    locator: &TargetLocator,
    staging: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_remote_path(staging)?;
    let command = match locator {
        TargetLocator::SshPodman { ssh, .. } | TargetLocator::SshDocker { ssh, .. } => Some(
            crate::targets::ssh_command(ssh, ["rm", "-f", "--", staging])
                .purpose("remove remote checkpoint staging"),
        ),
        _ => None,
    };
    if let Some(command) = command {
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

pub fn transfer_plan(
    locator: &TargetLocator,
    session_id: &str,
    remote_archive: &str,
    local_temporary: &Path,
    staging: &str,
) -> Result<CommandPlan> {
    validate_remote_path(remote_archive)?;
    validate_remote_path(staging)?;
    ensure!(
        local_temporary.is_absolute(),
        "local temporary path must be absolute"
    );
    worker_root(locator, session_id)?;
    let local = local_temporary.to_string_lossy().into_owned();
    let mut commands = match locator {
        TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new("cp", [remote_archive, local.as_str()])
                .purpose("copy local bare checkpoint"),
        ],
        TargetLocator::LocalPodman { container_id, .. } => vec![
            CommandSpec::new(
                "podman",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from local Podman"),
        ],
        TargetLocator::LocalDocker { container_id } => vec![
            CommandSpec::new(
                "docker",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from local Docker"),
        ],
        TargetLocator::AppleContainer { container_id } => vec![
            CommandSpec::new(
                "container",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from Apple container"),
        ],
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            vec![
                crate::targets::scp_download(ssh, remote_archive, &local)
                    .purpose("download checkpoint over SSH"),
            ]
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker { ssh, container_id } => {
            vec![
                crate::targets::ssh_command(ssh, ["mkdir", "-p", ".local/share/hel/transfers"])
                    .purpose("create remote checkpoint staging directory"),
                crate::targets::ssh_command(
                    ssh,
                    [
                        locator.container_engine().expect("remote container"),
                        "cp",
                        &format!("{container_id}:{remote_archive}"),
                        staging,
                    ],
                )
                .purpose("stage remote container checkpoint"),
            ]
        }
    };
    if let TargetLocator::SshPodman { ssh, .. } | TargetLocator::SshDocker { ssh, .. } = locator {
        commands.push(
            crate::targets::scp_download(ssh, staging, &local)
                .purpose("download remote container checkpoint over SSH"),
        );
    }
    Ok(CommandPlan {
        description: format!("download checkpoint for {session_id}"),
        commands,
    })
}

fn cleanup_plan(locator: &TargetLocator, session_id: &str, remote: &str) -> Result<CommandPlan> {
    validate_remote_path(remote)?;
    worker_root(locator, session_id)?;
    let commands = vec![
        crate::targets::locator_command(
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

fn validate_remote_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty());
    ensure!(
        path.bytes()
            .all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'~' | b'.' | b'-' | b'_')),
        "unsafe remote path"
    );
    ensure!(
        !path.split('/').any(|component| component == ".."),
        "remote path traverses parent"
    );
    Ok(())
}

#[cfg(test)]
mod tests;

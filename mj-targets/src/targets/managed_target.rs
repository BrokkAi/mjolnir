//! Commands addressed to the machine that holds a managed worktree: this one,
//! or another reached over SSH.

use std::path::Path;

use anyhow::{Context, Result, bail};
use mj_core::state::ManagedWorktreeTarget;
use mj_core::targets::{CommandExecutor, CommandSpec, SshTarget, SshTrafficClass};
pub fn managed_target_ssh(target: &ManagedWorktreeTarget) -> Option<SshTarget> {
    match target {
        ManagedWorktreeTarget::Local => None,
        ManagedWorktreeTarget::Ssh {
            destination,
            ssh_args,
        } => Some(SshTarget {
            destination: destination.clone(),
            ssh_args: ssh_args.clone(),
        }),
    }
}

pub fn managed_target_command(
    class: SshTrafficClass,
    target: &ManagedWorktreeTarget,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> CommandSpec {
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    match managed_target_ssh(target) {
        None => CommandSpec::new(program, args),
        Some(ssh) => {
            let mut remote = vec![program.to_owned()];
            remote.extend(args);
            super::ssh_command(class, &ssh, remote)
        }
    }
}

pub fn path_exists_on_managed_target(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    path: &Path,
) -> Result<bool> {
    match target {
        ManagedWorktreeTarget::Local => path
            .try_exists()
            .with_context(|| format!("check managed project path {}", path.display())),
        ManagedWorktreeTarget::Ssh { .. } => {
            let command = managed_target_command(
                SshTrafficClass::Control,
                target,
                "test",
                ["-e", &path.to_string_lossy()],
            )
            .purpose("check managed worktree path");
            let output = executor.execute(&command)?;
            match output.status {
                0 => Ok(true),
                1 => Ok(false),
                status => bail!(
                    "check managed worktree path failed with status {status}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            }
        }
    }
}

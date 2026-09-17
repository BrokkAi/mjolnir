//! Running commands on the machine that hosts a container target.
//!
//! A container target's caches live on the host that runs the container
//! engine, not on the machine running the controller. This is the one place
//! that knows how to reach that host for a local or SSH target, shared by the
//! Git clone cache and the mbx build cache.

use crate::targets::{self, CommandSpec, SshTarget};

#[derive(Debug, Clone)]
pub(super) enum CacheHost {
    LocalPodman,
    LocalDocker,
    Apple,
    SshPodman(SshTarget),
    SshDocker(SshTarget),
}

impl CacheHost {
    pub(super) fn for_target(target: &targets::TargetTemplate) -> Option<Self> {
        match target {
            targets::TargetTemplate::LocalPodman(_) => Some(Self::LocalPodman),
            targets::TargetTemplate::LocalDocker(_) => Some(Self::LocalDocker),
            targets::TargetTemplate::AppleContainer(_) => Some(Self::Apple),
            targets::TargetTemplate::SshPodman { ssh, .. } => Some(Self::SshPodman(ssh.clone())),
            targets::TargetTemplate::SshDocker { ssh, .. } => Some(Self::SshDocker(ssh.clone())),
            targets::TargetTemplate::LocalBare
            | targets::TargetTemplate::AwsEc2(_)
            | targets::TargetTemplate::SshBare { .. } => None,
        }
    }

    /// The SSH connection to this host, or `None` when it is this machine.
    pub(super) fn ssh(&self) -> Option<&SshTarget> {
        match self {
            Self::LocalPodman | Self::LocalDocker | Self::Apple => None,
            Self::SshPodman(ssh) | Self::SshDocker(ssh) => Some(ssh),
        }
    }

    /// An identity for this host, used to key cached host-side answers.
    pub(super) fn key(&self) -> String {
        match self {
            Self::LocalPodman => "local-podman".to_owned(),
            Self::LocalDocker => "local-docker".to_owned(),
            Self::Apple => "apple".to_owned(),
            Self::SshPodman(ssh) => format!("ssh-podman:{}", ssh.destination),
            Self::SshDocker(ssh) => format!("ssh-docker:{}", ssh.destination),
        }
    }

    pub(super) fn command(&self, remote: Vec<String>, purpose: impl Into<String>) -> CommandSpec {
        let command = match self {
            Self::LocalPodman | Self::LocalDocker | Self::Apple => {
                CommandSpec::new(remote[0].clone(), remote[1..].iter().cloned())
            }
            Self::SshPodman(ssh) | Self::SshDocker(ssh) => {
                let mut args = ssh.ssh_args.clone();
                targets::push_connection_sharing_args(&mut args);
                args.push(ssh.destination.clone());
                args.push(targets::join_remote_command(&remote));
                CommandSpec::new("ssh", args).ssh_destination(ssh.destination.clone())
            }
        };
        command.purpose(purpose)
    }

    /// Run `script` through `sh -c` on the host. `label` becomes `$0`, and
    /// `arguments` become `$1` onwards.
    pub(super) fn shell_command(
        &self,
        script: &str,
        label: &str,
        arguments: impl IntoIterator<Item = String>,
        purpose: impl Into<String>,
    ) -> CommandSpec {
        let mut remote = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            label.to_owned(),
        ];
        remote.extend(arguments);
        self.command(remote, purpose)
    }
}

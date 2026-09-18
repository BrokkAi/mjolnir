//! Running commands on the machine that hosts a container target.
//!
//! A container target's caches live on the host that runs the container
//! engine, not on the machine running the controller. This is the one place
//! that knows how to reach that host for a local or SSH target, shared by the
//! Git clone cache and the mbx build cache.
//!
//! A host is a machine, not a runtime: local Podman and local Docker are the
//! same computer, so they share one identity here and one inspection of it.

use crate::targets::{self, CommandSpec, SshTarget};

#[derive(Debug, Clone)]
pub(super) enum CacheHost {
    /// The machine running the controller.
    Local,
    Ssh(SshTarget),
}

impl CacheHost {
    pub(super) fn for_target(target: &targets::TargetTemplate) -> Option<Self> {
        match target {
            targets::TargetTemplate::LocalPodman(_)
            | targets::TargetTemplate::LocalDocker(_)
            | targets::TargetTemplate::AppleContainer(_) => Some(Self::Local),
            targets::TargetTemplate::SshPodman { ssh, .. }
            | targets::TargetTemplate::SshDocker { ssh, .. } => Some(Self::Ssh(ssh.clone())),
            targets::TargetTemplate::LocalBare
            | targets::TargetTemplate::AwsEc2(_)
            | targets::TargetTemplate::SshBare { .. } => None,
        }
    }

    /// The host a configured machine is, or `None` for a machine that has no
    /// standing host: an EC2 launch template creates an instance per session,
    /// so nothing persists between them to cache.
    pub(super) fn for_machine(machine: &mj_core::config::Machine) -> Option<Self> {
        match machine {
            mj_core::config::Machine::Local { .. } => Some(Self::Local),
            mj_core::config::Machine::Ssh { ssh, .. } => Some(Self::Ssh(SshTarget::from(ssh))),
            mj_core::config::Machine::AwsEc2 { .. } => None,
        }
    }

    /// The SSH connection to this host, or `None` when it is this machine.
    pub(super) fn ssh(&self) -> Option<&SshTarget> {
        match self {
            Self::Local => None,
            Self::Ssh(ssh) => Some(ssh),
        }
    }

    /// An identity for this host, used to key cached host-side answers. Every
    /// runtime on one machine shares it.
    pub(super) fn key(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Ssh(ssh) => format!("ssh:{}", ssh.destination),
        }
    }

    pub(super) fn command(&self, remote: Vec<String>, purpose: impl Into<String>) -> CommandSpec {
        let command = match self {
            Self::Local => CommandSpec::new(remote[0].clone(), remote[1..].iter().cloned()),
            Self::Ssh(ssh) => {
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

//! Running commands on the machine that hosts a container target.
//!
//! A container target's caches live on the host that runs the container
//! engine, not on the machine running the controller. This is the one place
//! that knows how to reach that host for a local or SSH target, shared by the
//! Git clone cache and the mbx build cache.
//!
//! A host is a machine, not a runtime: local Podman and local Docker are the
//! same computer, so they share one identity here and one inspection of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use crate::targets::{self, CommandExecutor, CommandSpec, SshTarget};

/// How long a resolved login home is reused. A home does not move while a
/// screen is open, and a burst of completions must not probe it per keystroke.
const HOME_LIFETIME: Duration = Duration::from_secs(600);

/// Successfully resolved login homes, keyed by `CacheHost::key`.
static HOMES: LazyLock<Mutex<BTreeMap<String, (Instant, PathBuf)>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// `$0` for the shell that prints a remote home.
const HOME_LABEL: &str = "hel-home";

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

    /// The host that owns a path a target's runtime reads. Unlike
    /// `for_target`, every target kind has one: a bare target's paths live on
    /// its machine, and an EC2 template's editable paths are this machine's,
    /// because no instance exists before a session starts.
    pub(super) fn for_path_target(target: &mj_core::config::TargetTemplate) -> Result<Self> {
        match target {
            mj_core::config::TargetTemplate::LocalBare
            | mj_core::config::TargetTemplate::LocalPodman { .. }
            | mj_core::config::TargetTemplate::LocalDocker { .. }
            | mj_core::config::TargetTemplate::AppleContainer { .. }
            | mj_core::config::TargetTemplate::AwsEc2 { .. } => Ok(Self::Local),
            mj_core::config::TargetTemplate::SshBare { ssh, .. }
            | mj_core::config::TargetTemplate::SshPodman { ssh, .. }
            | mj_core::config::TargetTemplate::SshDocker { ssh, .. } => Self::for_ssh_input(ssh),
        }
    }

    /// The host that owns a path on a configured machine. An EC2 machine has
    /// no standing instance, so its editable paths are this machine's.
    pub(super) fn for_path_machine(machine: &mj_core::config::Machine) -> Result<Self> {
        match machine {
            mj_core::config::Machine::Local { .. } | mj_core::config::Machine::AwsEc2 { .. } => {
                Ok(Self::Local)
            }
            mj_core::config::Machine::Ssh { ssh, .. } => Self::for_ssh_input(ssh),
        }
    }

    /// An SSH host reached with the identity file as this machine reads it:
    /// `~` in a configured key path is the controller user's home.
    fn for_ssh_input(ssh: &mj_core::config::SshConnection) -> Result<Self> {
        let mut ssh = ssh.clone();
        ssh.identity_file = ssh
            .identity_file
            .as_deref()
            .map(mj_core::path_input::expand_local)
            .transpose()?;
        Ok(Self::Ssh(SshTarget::from(&ssh)))
    }

    /// The login home on this host, cached for ten minutes per host. Only a
    /// successful answer is cached; a host that was unreachable is retried.
    pub(super) fn home(&self, executor: &impl CommandExecutor) -> Result<PathBuf> {
        let Self::Ssh(_) = self else {
            return dirs::home_dir().context("Cannot expand ~: the home directory is unavailable.");
        };
        let key = self.key();
        if let Some((recorded, home)) = HOMES.lock().expect("resolved homes").get(&key)
            && recorded.elapsed() < HOME_LIFETIME
        {
            return Ok(home.clone());
        }
        let command = self.shell_command(
            r#"printf '%s' "$HOME""#,
            HOME_LABEL,
            [],
            "resolve remote home directory",
        );
        let output = executor.execute(&command)?;
        ensure!(
            output.status == 0,
            "Could not resolve remote home: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let home = String::from_utf8(output.stdout).context("Remote home is not valid UTF-8")?;
        ensure!(!home.is_empty(), "Remote home is empty");
        let home = PathBuf::from(home);
        ensure!(home.is_absolute(), "Remote home is not absolute");
        HOMES
            .lock()
            .expect("resolved homes")
            .insert(key, (Instant::now(), home.clone()));
        Ok(home)
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
                args.push(ssh.destination.clone());
                args.push(targets::join_remote_command(&remote));
                CommandSpec::new("ssh", args).ssh_session(ssh)
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

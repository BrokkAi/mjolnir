//! Running commands on the machine that hosts a container target.
//!
//! A container target's caches live on the host that runs the container
//! engine, not on the machine running the controller. This is the one place
//! that knows how to reach that host for a local or SSH target, shared by the
//! Git clone cache and the mbx build cache.
//!
//! A host is a machine, not a runtime: local Podman and local Docker are the
//! same computer, so they share one identity here and one inspection of it.

use mj_core::targets::SshTrafficClass;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
const HOME_SCRIPT: &str = r#"printf '%s' "$HOME""#;
const HOME_PURPOSE: &str = "resolve remote home directory";

fn parse_home_output(output: &targets::CommandOutput) -> Result<PathBuf> {
    ensure!(
        output.status == 0,
        "Could not resolve remote home: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let home =
        String::from_utf8(output.stdout.clone()).context("Remote home is not valid UTF-8")?;
    ensure!(!home.is_empty(), "Remote home is empty");
    let home = PathBuf::from(home);
    ensure!(
        mj_core::target_path::is_absolute(&home),
        "Remote home is not absolute"
    );
    Ok(home)
}

fn remember_home(key: String, home: &Path) {
    HOMES
        .lock()
        .expect("resolved homes")
        .insert(key, (Instant::now(), home.to_path_buf()));
}

#[derive(Debug, Clone)]
pub enum CacheHost {
    /// The machine running the controller.
    Local,
    Ssh(SshTarget),
}

/// Resolve the login home on the machine that owns an editable path.
pub fn resolve_target_input_path(
    target: &mj_core::config::TargetTemplate,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    if !mj_core::path_input::needs_home(path)? {
        return Ok(path.to_path_buf());
    }
    let host = CacheHost::for_path_target(target)?;
    mj_core::path_input::expand_home(path, Some(&host.home(executor)?))
}

/// A cache host's operating system, which decides which of its sessions can
/// use what the host itself has installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    /// Containers run on the host's own kernel, so they can mount its caches
    /// and run its mbx.
    Linux,
    /// Bare sessions run on the Mac itself and use its native tools. Its
    /// containers run Linux in a virtual machine, which can neither run a
    /// macOS executable nor rely on the Mac's filesystems for shared locks.
    Darwin,
    /// A Windows controller. It has no `uname` to ask, runs no bare workers,
    /// and its container engine runs in a VM this machine's paths do not
    /// reach.
    Windows,
}

/// What the platform probe is called in a failure.
pub(crate) const PLATFORM_PURPOSE: &str = "detect build cache host platform";

/// Read the answer to `uname -sm`, however the probe was run: on its own or
/// framed in a batch with other probes.
pub(crate) fn parse_platform_output(output: &targets::CommandOutput) -> Result<HostPlatform> {
    ensure!(
        output.status == 0,
        "{PLATFORM_PURPOSE} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let platform = targets::TargetPlatform::parse(
        std::str::from_utf8(&output.stdout).context("decode build cache host platform")?,
    )?;
    Ok(match platform.os {
        targets::TargetOs::Linux => HostPlatform::Linux,
        targets::TargetOs::Darwin => HostPlatform::Darwin,
    })
}

impl CacheHost {
    /// Ask the host itself, not the controller or a container on that host.
    pub fn platform(&self, executor: &impl CommandExecutor) -> Result<HostPlatform> {
        if self.ssh().is_none() && !cfg!(unix) {
            return Ok(HostPlatform::Windows);
        }
        let command = self.command(vec!["uname".into(), "-sm".into()], PLATFORM_PURPOSE);
        parse_platform_output(&executor.execute(&command)?)
    }

    pub fn for_target(target: &targets::TargetTemplate) -> Option<Self> {
        match target {
            targets::TargetTemplate::LocalPodman(_)
            | targets::TargetTemplate::LocalDocker(_)
            | targets::TargetTemplate::AppleContainer(_) => Some(Self::Local),
            targets::TargetTemplate::SshPodman { ssh, .. }
            | targets::TargetTemplate::SshDocker { ssh, .. }
            | targets::TargetTemplate::SshAppleContainer { ssh, .. } => {
                Some(Self::Ssh(ssh.clone()))
            }
            // The host's caches are directories a container mounts; a sandbox
            // is a VM and cannot, so it has no cache host.
            targets::TargetTemplate::SshNitro { .. }
            | targets::TargetTemplate::LocalBare
            | targets::TargetTemplate::SshBare { .. } => None,
        }
    }

    /// The host a configured machine is, or `None` for a machine that has no
    /// standing host: an EC2 launch template creates an instance per session,
    /// so nothing persists between them to cache.
    pub fn for_machine(machine: &mj_core::config::Machine) -> Option<Self> {
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
    pub fn for_path_target(target: &mj_core::config::TargetTemplate) -> Result<Self> {
        match target {
            mj_core::config::TargetTemplate::LocalBare
            | mj_core::config::TargetTemplate::LocalPodman { .. }
            | mj_core::config::TargetTemplate::LocalDocker { .. }
            | mj_core::config::TargetTemplate::AppleContainer { .. }
            | mj_core::config::TargetTemplate::AwsEc2 { .. } => Ok(Self::Local),
            mj_core::config::TargetTemplate::SshBare { ssh, .. }
            | mj_core::config::TargetTemplate::SshPodman { ssh, .. }
            | mj_core::config::TargetTemplate::SshDocker { ssh, .. }
            | mj_core::config::TargetTemplate::SshAppleContainer { ssh, .. }
            | mj_core::config::TargetTemplate::SshNitro { ssh, .. } => Self::for_ssh_input(ssh),
        }
    }

    /// The host that owns a path on a configured machine. An EC2 machine has
    /// no standing instance, so its editable paths are this machine's.
    pub fn for_path_machine(machine: &mj_core::config::Machine) -> Result<Self> {
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
    pub fn home(&self, executor: &impl CommandExecutor) -> Result<PathBuf> {
        let Self::Ssh(_) = self else {
            return mj_core::path_input::user_home()
                .context("Cannot expand ~: the home directory is unavailable.");
        };
        let key = self.key();
        if let Some((recorded, home)) = HOMES.lock().expect("resolved homes").get(&key)
            && recorded.elapsed() < HOME_LIFETIME
        {
            return Ok(home.clone());
        }
        let command = self.shell_command(HOME_SCRIPT, HOME_LABEL, [], HOME_PURPOSE);
        let home = parse_home_output(&executor.execute(&command)?)?;
        remember_home(key, &home);
        Ok(home)
    }

    /// The platform and the login home, which an SSH host answers in one round
    /// trip. The home is reported separately so a caller that has no use for it
    /// on this platform is not failed by it; a local host runs no command for
    /// either.
    pub fn platform_and_home(
        &self,
        executor: &impl CommandExecutor,
    ) -> Result<(HostPlatform, Result<PathBuf>)> {
        let Self::Ssh(_) = self else {
            return Ok((self.platform(executor)?, self.home(executor)));
        };
        let script = format!(
            "{}probe platform uname -sm || exit 0\nprobe home sh -c \"$1\" {HOME_LABEL}\nexit 0\n",
            targets::PROBE_HELPER_SH
        );
        let command = self.shell_command(
            &script,
            "hel-platform-home",
            [HOME_SCRIPT.to_owned()],
            "detect build cache host platform and home",
        );
        let output = executor.execute(&command)?;
        let probes = targets::parse_framed_probes(&output.stdout, targets::DEFAULT_PROBE_FRAMES);
        let platform = probes.get("platform").with_context(|| {
            format!(
                "{} failed with status {}: {}",
                command.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })?;
        let platform = parse_platform_output(platform)?;
        let home = probes
            .get("home")
            .context("the host did not report its home")
            .and_then(parse_home_output);
        if let Ok(home) = &home {
            remember_home(self.key(), home);
        }
        Ok((platform, home))
    }

    /// The SSH connection to this host, or `None` when it is this machine.
    pub fn ssh(&self) -> Option<&SshTarget> {
        match self {
            Self::Local => None,
            Self::Ssh(ssh) => Some(ssh),
        }
    }

    /// An identity for this host, used to key cached host-side answers. Every
    /// runtime on one machine shares it.
    pub fn key(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Ssh(ssh) => format!("ssh:{}", ssh.destination),
        }
    }

    pub fn command(&self, remote: Vec<String>, purpose: impl Into<String>) -> CommandSpec {
        let command = match self {
            Self::Local => CommandSpec::new(remote[0].clone(), remote[1..].iter().cloned()),
            Self::Ssh(ssh) => targets::ssh_command_owned(SshTrafficClass::Control, ssh, remote),
        };
        command.purpose(purpose)
    }

    /// Run `script` through `sh -c` on the host. `label` becomes `$0`, and
    /// `arguments` become `$1` onwards.
    pub fn shell_command(
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

//! Where the daemon process runs: `[daemon]` in config.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The `[daemon]` table, chosen the way `[quota] runtime` is.
///
/// ```toml
/// [daemon]
/// runtime = "podman"          # "host" (default), "podman", "docker" or "ssh"
/// image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
/// mounts = ["/srv/projects"]  # optional host paths, mounted explicitly
/// ```
///
/// `image` and `mounts` apply only to a container runtime. A container
/// otherwise has no host filesystem access; its data lives in a named volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(skip_serializing_if = "DaemonRuntime::is_default")]
    pub runtime: DaemonRuntime,
    /// SSH destination, using the user's OpenSSH configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Remote instance; omitted means the client's selected instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ssh_args: Vec<String>,
    /// Remote executable; omitted means `mj` on the login shell PATH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_command: Option<String>,
    /// The daemon container's image. `None` is this build's agent-dev image,
    /// which carries the harnesses local sessions run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Host directories explicitly mounted into the daemon container at the
    /// same path. Nothing is mounted by default; the daemon's own data lives
    /// in a named volume.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<PathBuf>,
}

impl DaemonConfig {
    /// Read only client-owned placement. Other tables are deliberately not
    /// deserialized: profiles and credentials belong to the daemon machine.
    pub fn load_placement(path: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Placement {
            daemon: DaemonConfig,
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read placement {}", path.display()));
            }
        };
        toml::from_str::<Placement>(&contents)
            .map(|placement| placement.daemon)
            .with_context(|| format!("parse placement {}", path.display()))
    }

    pub fn remote_command(&self) -> &str {
        self.remote_command.as_deref().unwrap_or("mj")
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Where `mj daemon-run` runs. `Podman` and `Docker` run it in a long-lived
/// per-instance container of that engine. The container is its own machine:
/// its data is a named volume, its network is private, and the only host
/// directories it sees are the ones named in [`DaemonConfig::mounts`]. Local
/// sessions' workers run inside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonRuntime {
    /// A detached process on this computer.
    #[default]
    Host,
    Podman,
    Docker,
    Ssh,
}

impl DaemonRuntime {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::Host
    }

    /// The container engine program, or `None` for host and SSH placement.
    #[must_use]
    pub fn engine(self) -> Option<&'static str> {
        match self {
            Self::Host | Self::Ssh => None,
            Self::Podman => Some("podman"),
            Self::Docker => Some("docker"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn placement_loading_does_not_resolve_daemon_profiles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        // Profiles intentionally have the wrong type for Config: a client
        // must not interpret daemon-owned settings before connecting.
        std::fs::write(
            &path,
            r#"
profiles = "owned by remote"
[daemon]
runtime = "ssh"
host = "test-box"
ssh_args = ["-p", "2222"]
"#,
        )
        .unwrap();
        let placement = super::DaemonConfig::load_placement(&path).unwrap();
        assert_eq!(placement.runtime, super::DaemonRuntime::Ssh);
        assert_eq!(placement.host.as_deref(), Some("test-box"));
        assert_eq!(placement.ssh_args, ["-p", "2222"]);
    }
}

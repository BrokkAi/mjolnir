//! Where the daemon process runs: `[daemon]` in config.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The `[daemon]` table, chosen the way `[quota] runtime` is.
///
/// ```toml
/// [daemon]
/// runtime = "podman"          # "host" (default), "podman" or "docker"
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
}

impl DaemonRuntime {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::Host
    }

    /// The container engine program, or `None` on the host.
    #[must_use]
    pub fn engine(self) -> Option<&'static str> {
        match self {
            Self::Host => None,
            Self::Podman => Some("podman"),
            Self::Docker => Some("docker"),
        }
    }
}

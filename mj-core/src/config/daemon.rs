//! Where the daemon process runs: `[daemon]` in config.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The `[daemon]` table, chosen the way `[quota] runtime` is.
///
/// ```toml
/// [daemon]
/// runtime = "podman"          # "host" (default), "podman" or "docker"
/// image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
/// mounts = ["/srv/projects"]  # host paths visible to the daemon, beside $HOME
/// ```
///
/// `image` and `mounts` apply only to a container runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(skip_serializing_if = "DaemonRuntime::is_default")]
    pub runtime: DaemonRuntime,
    /// The daemon container's image. `None` is this build's agent-dev image,
    /// which carries the harnesses local sessions run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Host directories mounted into the daemon container at the same path,
    /// in addition to `$HOME`, the data and configuration directories, `/tmp`
    /// and the user's runtime directory. Local projects outside those belong
    /// here.
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
/// per-instance container of that engine, sharing this computer's network and
/// process namespaces. Local sessions' workers run in it too, so they outlive
/// a daemon replacement exactly as host workers do.
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

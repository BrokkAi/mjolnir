//! Launch descriptions the controller writes and the target-side worker reads.
//!
//! These are plain data types and the constants they name: the file layout of
//! a worker root, the launch configuration for a primary session and for a
//! reviewer beside it, and how a harness learns about its MCP servers. They
//! carry no process, network, or worker-runtime behaviour, so both sides of
//! the relay can depend on them without depending on each other.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hel_config::{ExecutionPolicy, HarnessKind};

pub const DISCOVER_LOGIN_PATH_ENV: &str = "MJ_DISCOVER_LOGIN_PATH";
/// Directory inside the primary worker root that holds everything the reviewer owns.
pub const REVIEWER_DIR: &str = "reviewer";
/// Where the controller stages the chosen profile, inside [`REVIEWER_DIR`].
pub const REVIEWER_PROFILE_DIR: &str = "profile";

/// Who owns the ACP adapter and harness executable selected by a worker.
///
/// Ambient launch preserves container behavior. Managed launch resolves the
/// exact pin compiled into the worker and never falls back to an executable
/// from `PATH`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessRuntimePolicy {
    #[default]
    Ambient,
    /// The worker owns the exact ACP bridge installation.
    #[serde(rename = "managed_remote", alias = "managed")]
    Managed,
}

impl HarnessRuntimePolicy {
    pub const fn is_ambient(&self) -> bool {
        matches!(self, Self::Ambient)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOwnership {
    pub version: u32,
    #[serde(default = "default_worker_workspace_id")]
    pub workspace_id: String,
    pub session_id: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_template_id: String,
}

impl WorkerOwnership {
    pub const VERSION: u32 = 2;

    pub fn write(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec(self)?;
        crate::hel_config::atomic_write(path, &body)
    }
}

fn default_worker_workspace_id() -> String {
    crate::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLaunchConfig {
    pub session_id: String,
    pub harness: HarnessKind,
    pub bridge_command: PathBuf,
    pub bridge_args: Vec<String>,
    #[serde(default, skip_serializing_if = "HarnessRuntimePolicy::is_ambient")]
    pub harness_runtime: HarnessRuntimePolicy,
    pub environment: std::collections::BTreeMap<String, String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_memory: Option<ProjectMemoryLaunchConfig>,
    /// Target-level policy translated into harness-specific controls by the
    /// worker. Raw localhost and guardian SSH targets preserve configured
    /// approvals for harnesses that support them; Codex ACP is forced into
    /// full access as a compatibility workaround. Other targets run
    /// unconstrained.
    #[serde(
        alias = "force_unrestricted_mode",
        deserialize_with = "deserialize_execution_policy"
    )]
    pub execution_policy: ExecutionPolicy,
}

fn deserialize_execution_policy<'de, D>(deserializer: D) -> Result<ExecutionPolicy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum WirePolicy {
        Current(ExecutionPolicy),
        Legacy(bool),
    }

    Ok(match WirePolicy::deserialize(deserializer)? {
        WirePolicy::Current(policy) => policy,
        WirePolicy::Legacy(true) => ExecutionPolicy::Unconstrained,
        WirePolicy::Legacy(false) => ExecutionPolicy::ConfiguredApprovals,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemoryLaunchConfig {
    /// Stable controller-derived identity for this repository or bundle.
    pub project_key: String,
    /// Target-side replica used by native Claude and the MCP server.
    pub root: PathBuf,
    /// Session-private copy of the canonical tree from the last successful
    /// synchronization, used as the three-way merge base.
    #[serde(default)]
    pub baseline_root: PathBuf,
    /// Bundle repository IDs mapped to the roots presented over ACP.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub repository_roots: std::collections::BTreeMap<String, PathBuf>,
    /// How the harness learns about the project-memory MCP server. Most ACP
    /// adapters accept a stdio server in `session/new`; adapters that need
    /// harness-specific runtime metadata receive it through their staged
    /// profile instead.
    #[serde(default, skip_serializing_if = "ProjectMemoryMcpDelivery::is_acp")]
    pub mcp_delivery: ProjectMemoryMcpDelivery,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMemoryMcpDelivery {
    #[default]
    Acp,
    HarnessProfile,
}

impl ProjectMemoryMcpDelivery {
    fn is_acp(&self) -> bool {
        *self == Self::Acp
    }
}

/// How to launch the second-opinion reviewer beside a primary session.
///
/// The reviewer shares the primary's target and working directory and nothing
/// else: its harness home is a fresh copy of the chosen profile, staged under
/// the primary worker root, and the worker sets that home itself so a
/// controller can never point a reviewer at the primary's credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerLaunchConfig {
    /// Configured profile this reviewer was staged from, for display and for
    /// deciding whether a saved reviewer still matches the user's choice.
    pub profile_id: String,
    pub harness: HarnessKind,
    pub bridge_command: PathBuf,
    pub bridge_args: Vec<String>,
    /// Harness environment without its home variable: the worker fills that in
    /// from the staged reviewer directory it owns.
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, String>,
    pub execution_policy: ExecutionPolicy,
    /// Model to apply once the session opens, or `None` when the harness
    /// advertises no model selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Bumped whenever native continuity is lost, so a reviewer that outlived
    /// its harness starts a visibly new conversation instead of pretending to
    /// resume one.
    #[serde(default)]
    pub generation: u64,
    /// Analyzer and navigation servers this reviewer gets over MCP. A turn
    /// review attaches Bifrost here; plan review attaches nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<ReviewMcpServer>,
}

/// One stdio MCP server a reviewing agent is given.
///
/// How it reaches the harness depends on the harness: most accept a server in
/// the ACP `session/new` request, while Claude and Kimi read their own
/// configuration files, which the controller patches while staging the
/// reviewer's profile. [`ReviewMcpDelivery`] is the single place that decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewMcpServer {
    pub name: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

/// How a harness learns about a reviewing agent's MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMcpDelivery {
    /// Attached to the ACP `session/new` request.
    Acp,
    /// Written into the staged profile the harness reads at startup.
    HarnessProfile,
}

impl ReviewMcpDelivery {
    /// Claude and Kimi both ignore servers offered over ACP -- Claude is not
    /// given them at all (see `project_memory_mcp` in `src/hel_acp.rs`), and
    /// Kimi needs runtime metadata its own schema carries -- so both are
    /// configured through their staged profile instead.
    #[must_use]
    pub const fn for_harness(harness: HarnessKind) -> Self {
        match harness {
            HarnessKind::Claude | HarnessKind::Kimi => Self::HarnessProfile,
            _ => Self::Acp,
        }
    }
}

impl ReviewerLaunchConfig {
    /// Whether a running reviewer launched from `self` can serve `other`
    /// without being restarted. Model and effort are applied on the live
    /// session, so they never force a restart; identity does.
    #[must_use]
    pub fn reusable_for(&self, other: &Self) -> bool {
        self.profile_id == other.profile_id
            && self.harness == other.harness
            && self.generation == other.generation
    }
}

impl WorkerLaunchConfig {
    pub fn read(path: &Path) -> Result<Self> {
        let body = std::fs::read(path)
            .with_context(|| format!("read worker launch config {}", path.display()))?;
        serde_json::from_slice(&body)
            .with_context(|| format!("parse worker launch config {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let body = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// Content address of a worker executable.
///
/// One definition, because both sides of the upgrade decision compare it: the
/// worker reports the digest of the file serving it, and the controller
/// computes the digest of the file it would install. Streamed, because a
/// worker binary is tens of megabytes.
pub fn worker_executable_digest(path: &Path) -> Result<String> {
    use sha2::Digest;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open worker executable {}", path.display()))?;
    let mut digest = sha2::Sha256::new();
    std::io::copy(&mut file, &mut digest)
        .with_context(|| format!("hash worker executable {}", path.display()))?;
    Ok(format!("{:x}", digest.finalize()))
}

/// Content address of the executable running this process, or `None` when it
/// cannot be read.
///
/// Read through `/proc/self/exe` on Linux: a worker whose file was replaced
/// under it - which is exactly what an upgrade does - still has its own image
/// there, while the resolved path no longer names it.
pub fn running_executable_digest() -> Option<String> {
    let path = if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, "could not resolve this executable to report its build");
                return None;
            }
        }
    };
    match worker_executable_digest(&path) {
        Ok(digest) => Some(digest),
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not hash this executable to report its build"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_json() -> serde_json::Value {
        serde_json::json!({
            "session_id": "session",
            "harness": "codex",
            "bridge_command": "codex-acp",
            "bridge_args": [],
            "environment": {},
            "cwd": "/workspace/project",
            "execution_policy": "configured_approvals"
        })
    }

    #[test]
    fn old_launch_configs_default_to_ambient_harnesses() {
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Ambient);
    }

    #[test]
    fn managed_policy_accepts_new_and_legacy_wire_names() {
        let mut value = launch_json();
        value["harness_runtime"] = serde_json::json!("managed_remote");
        let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Managed);
        assert_eq!(
            serde_json::to_value(&launch).unwrap()["harness_runtime"],
            "managed_remote"
        );

        let mut value = launch_json();
        value["harness_runtime"] = serde_json::json!("managed");
        let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Managed);
    }
}

//! Target-side daemon and stdio proxy for the durable ACP relay protocol.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use hel::hel_config::HarnessKind;

pub use hel::hel_worker::WORKER_PID_FILE;

// The launch descriptions and MCP shapes both sides of the relay share live in
// the foundation. The runtime and its submodules keep naming them here.
use hel::hel_worker_launch::{
    DISCOVER_LOGIN_PATH_ENV, ProjectMemoryLaunchConfig, REVIEWER_DIR, REVIEWER_PROFILE_DIR,
    ReviewerLaunchConfig, WorkerLaunchConfig,
};

pub(crate) const GITHUB_CLI_BIN_ENV: &str = "MJ_GITHUB_CLI_BIN";
/// Where the worker keeps one directory per reviewing role, inside
/// [`REVIEWER_DIR`]. Each holds that role's own copy of the staged profile and
/// its own relay journal.
#[cfg(unix)]
pub(crate) const REVIEWER_ROLES_DIR: &str = "roles";

pub(crate) fn github_cli_login_shell_command(command: &str) -> String {
    format!(
        "if [ -n \"${{{GITHUB_CLI_BIN_ENV}:-}}\" ]; then PATH=\"${GITHUB_CLI_BIN_ENV}:$PATH\"; export PATH; fi; unset {GITHUB_CLI_BIN_ENV} GH_TOKEN GITHUB_TOKEN; {command}"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpSupervisorSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: std::collections::BTreeMap<String, String>,
    pub cwd: PathBuf,
}

impl AcpSupervisorSpec {
    pub fn read(path: &Path) -> Result<Self> {
        let body = std::fs::read(path)
            .with_context(|| format!("read ACP supervisor spec {}", path.display()))?;
        serde_json::from_slice(&body)
            .with_context(|| format!("parse ACP supervisor spec {}", path.display()))
    }

    #[cfg(unix)]
    pub(crate) fn write_spec(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        hel::hel_config::atomic_write(path, &body)
    }
}

/// Applies a target's execution policy to the harness environment.
///
/// This is worker behaviour, so it lives here rather than with the foundation
/// type it edits. It is a free function because Rust only allows an inherent
/// method on [`WorkerLaunchConfig`] in the crate that defines it.
#[cfg(unix)]
pub(crate) fn enforce_execution_policy(config: &mut WorkerLaunchConfig) {
    config
        .harness
        .configure_execution_environment(config.execution_policy, &mut config.environment);
}

#[cfg(unix)]
pub(crate) mod reviewer;
#[cfg(unix)]
mod unix;

#[cfg(not(unix))]
pub fn lead_process_group() {}

/// Where this relay's harness keeps its home, resolved solely from the launch
/// config. Credential and skills requests carry no path, so a caller cannot
/// steer a read or write outside the session's harness home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialEndpoint {
    pub harness: HarnessKind,
    /// The session's harness home; skills trees sync under it.
    pub home: PathBuf,
    pub marker: PathBuf,
}

#[cfg(unix)]
fn credential_endpoint(
    config: &WorkerLaunchConfig,
) -> std::result::Result<CredentialEndpoint, String> {
    let key = config.harness.home_env();
    let home = config.environment.get(key).ok_or_else(|| {
        format!("worker launch config has no {key} entry, so it cannot locate harness credentials")
    })?;
    Ok(CredentialEndpoint {
        harness: config.harness,
        home: PathBuf::from(home.as_str()),
        marker: hel::hel_config::harness_authentication_marker(
            config.harness,
            Path::new(home.as_str()),
        ),
    })
}

#[cfg(unix)]
fn resolve_relative_harness_home(config: &mut WorkerLaunchConfig, base: &Path) {
    let key = config.harness.home_env();
    if let Some(value) = config.environment.get_mut(key) {
        let path = Path::new(value);
        if path.is_relative() {
            *value = base.join(path).to_string_lossy().into_owned();
        }
    }
    if let Some(memory) = config.project_memory.as_mut() {
        if memory.root.is_relative() {
            memory.root = base.join(&memory.root);
        }
        if memory.baseline_root.as_os_str().is_empty() {
            memory.baseline_root = memory
                .root
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".hel-memory-baseline");
        }
        if memory.baseline_root.is_relative() {
            memory.baseline_root = base.join(&memory.baseline_root);
        }
    }
}

#[cfg(unix)]
fn resolve_relative_worker_root(root: PathBuf, base: &Path) -> PathBuf {
    if root.is_relative() {
        base.join(root)
    } else {
        root
    }
}

#[cfg(unix)]
pub use unix::{lead_process_group, proxy, run_acp_supervisor, run_daemon};

#[cfg(not(unix))]
pub async fn run_daemon(
    _root: std::path::PathBuf,
    _config: WorkerLaunchConfig,
) -> anyhow::Result<()> {
    anyhow::bail!("target workers require Unix")
}

#[cfg(not(unix))]
pub async fn proxy(_root: std::path::PathBuf) -> anyhow::Result<()> {
    anyhow::bail!("target workers require Unix")
}

#[cfg(not(unix))]
pub async fn run_acp_supervisor(_spec: AcpSupervisorSpec) -> anyhow::Result<()> {
    anyhow::bail!("ACP supervision requires Unix")
}

#[cfg(all(test, unix))]
mod relay_tests;
#[cfg(all(test, unix))]
mod reviewer_tests;

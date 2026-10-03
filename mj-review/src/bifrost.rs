//! Bifrost: the code-navigation tools every reviewing agent gets.
//!
//! Bifrost is a first-party code-analysis tool baked into the session's
//! container image. During a review it runs as an MCP server per reviewed
//! repository, so the reviewing agents can find symbols, read their sources,
//! follow callers and usages, and, in a specialist lane, run the slop-cop
//! analyzers. The review reads the change itself from the capture -- the diff
//! and Git's per-file line counts -- not from Bifrost.

use std::path::{Path, PathBuf};

/// Set on the daemon to choose the Bifrost binary. The daemon passes it to each
/// worker in its launch configuration (`WorkerLaunchConfig::bifrost_binary`),
/// because the worker's own environment is the target's login environment.
/// Unset, a review runs `bifrost` from the target's `PATH`.
pub const BIFROST_BIN_ENV: &str = "MJ_BIFROST_BIN";
const DEFAULT_BIFROST_BIN: &str = "bifrost";

/// The Bifrost release the container image installs
/// (`containers/Containerfile.agent-dev`), and the oldest one `mj doctor`
/// accepts for the reviewers' MCP tools.
pub const REQUIRED_BIFROST_VERSION: &str = "0.12.0";

/// The Bifrost the operator chose with `MJ_BIFROST_BIN` on the daemon, if any.
/// The daemon passes it to each worker in the launch configuration.
#[must_use]
pub fn configured_bifrost_binary() -> Option<PathBuf> {
    mj_core::config::env_override_os("BIFROST_BIN")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// What a review runs Bifrost as on the daemon's machine: the operator's
/// choice, else `bifrost` on `PATH`.
#[must_use]
pub fn bifrost_binary() -> PathBuf {
    configured_bifrost_binary().unwrap_or_else(default_bifrost_binary)
}

/// The Bifrost a review runs when nothing selects one: `bifrost` on `PATH`.
#[must_use]
pub fn default_bifrost_binary() -> PathBuf {
    PathBuf::from(DEFAULT_BIFROST_BIN)
}

/// The MCP server command line for one reviewed repository. `toolset` is
/// `core` for the supervisor and the quick reviewer, and `core|slopcop` for a
/// specialist lane, whose analyzers live in the `slopcop` set.
#[must_use]
pub fn mcp_server_args(repository: &Path, toolset: &str) -> Vec<String> {
    vec![
        "--root".to_string(),
        repository.display().to_string(),
        "--mcp".to_string(),
        toolset.to_string(),
    ]
}

/// The Bifrost MCP servers one reviewing role gets: one per reviewed
/// repository, named the way the review prompts name them, so an agent told to
/// "use the server whose root contains the changed path" can.
#[must_use]
pub fn review_mcp_servers(
    repositories: &[PathBuf],
    toolset: &str,
) -> Vec<mj_core::worker_launch::ReviewMcpServer> {
    let binary = bifrost_binary();
    repositories
        .iter()
        .enumerate()
        .map(
            |(index, repository)| mj_core::worker_launch::ReviewMcpServer {
                name: if index == 0 {
                    "bifrost".to_string()
                } else {
                    format!("bifrost_{}", index + 1)
                },
                command: binary.clone(),
                args: mcp_server_args(repository, toolset),
            },
        )
        .collect()
}

//! Target-side daemon and stdio proxy for the durable ACP relay protocol.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use mj_core::config::HarnessKind;

pub use mj_core::relay::{
    REVIEW_UNTRACKED_FILE, WORKER_EXIT_FILE, WORKER_PID_FILE, WORKER_STARTUP_FILE,
};

/// Record which startup step this worker is on, in `worker-startup.json` in
/// the worker root.
///
/// A detached worker's only channel before it binds its control socket is its
/// root directory, and its log stays empty until something logs at `warn`.
/// Without this file a controller waiting for the socket cannot tell a worker
/// that is still making progress from one that died without a word: both look
/// like a directory with nothing in it. Every step is appended, so the record
/// also says how long each step took, and the login-environment re-exec shows
/// up as a second `start`.
///
/// Best effort by design: a breadcrumb must never be the reason a worker fails
/// to start.
pub fn record_startup_step(root: &Path, step: &str) {
    if !root.is_dir() {
        return;
    }
    let path = root.join(WORKER_STARTUP_FILE);
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut steps = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|record| record.get("steps").cloned())
        .and_then(|steps| serde_json::from_value::<Vec<serde_json::Value>>(steps).ok())
        .unwrap_or_default();
    // A worker that somehow restarted many times would otherwise grow this
    // file without end.
    if steps.len() >= 64 {
        steps.drain(..steps.len() - 63);
    }
    steps.push(serde_json::json!({"step": step, "at": at}));
    let record = serde_json::json!({
        "step": step,
        "at": at,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "steps": steps,
    });
    match serde_json::to_vec_pretty(&record) {
        Ok(bytes) => {
            if let Err(error) = mj_core::config::atomic_write(&path, &bytes) {
                eprintln!("Mjolnir: could not record startup step {step}: {error}");
            }
        }
        Err(error) => eprintln!("Mjolnir: could not serialize startup step {step}: {error}"),
    }
}

// The launch descriptions and MCP shapes both sides of the relay share live in
// the foundation. The runtime and its submodules keep naming them here.
use mj_core::worker_launch::WorkerLaunchConfig;
#[cfg(unix)]
use mj_core::worker_launch::{
    ProjectMemoryLaunchConfig, REVIEWER_DIR, REVIEWER_PROFILE_DIR, ReviewerLaunchConfig,
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
    /// Shared advisory lock the supervisor holds for the complete lifetime of
    /// a managed harness process tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_lease: Option<PathBuf>,
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
        mj_core::config::atomic_write(path, &body)
    }
}

/// Applies a target's execution policy to the harness environment.
///
/// This is worker behaviour, so it lives here rather than with the foundation
/// type it edits. It is a free function because Rust only allows an inherent
/// method on [`WorkerLaunchConfig`] in the crate that defines it.
#[cfg(unix)]
pub(crate) fn enforce_execution_policy(config: &mut WorkerLaunchConfig) -> Result<()> {
    // Applied here as well as in the controller so a launch config persisted
    // by an older Hel converges on the harness's effective policy.
    config.execution_policy = config
        .harness
        .effective_execution_policy(config.execution_policy);
    config
        .harness
        .configure_execution_environment(config.execution_policy, &mut config.environment)
}

/// Environment variable codex-acp reads as its startup configuration.
#[cfg(unix)]
const CODEX_CONFIG_ENV: &str = "CODEX_CONFIG";

/// Pin the selectors a harness can only take before it opens a session into
/// the bridge's environment.
///
/// This is the one place that knows what a harness needs pre-start. Claude
/// takes its model in the ACP `session/new` and `session/load` request, which
/// [`crate::acp`] builds from the same accepted configuration at every bridge
/// launch; Codex has no such field and takes it here instead. Kimi, Grok and
/// Muse need nothing before the handshake: the worker restores their accepted
/// selectors on the live session once it is open.
///
/// Codex derives a resumed thread's turn context from the launch request, not
/// from what the thread recorded, and codex-acp sends the profile's provider
/// but never a model. A resume therefore opens on the `model` in the profile's
/// `config.toml` — and Codex reports that as a mismatch ("This session was
/// recorded with X but is resuming with Y") before the worker can restore the
/// accepted selector after the handshake. `CODEX_CONFIG` is the only pre-start
/// channel codex-acp exposes for this; the model cannot travel in the ACP
/// request, the way Claude's `claudeCode.options.model` does.
///
/// Keys already in `CODEX_CONFIG` are kept, and only `model` is written: mode
/// and permission keys set here would be discarded, because codex-acp merges
/// this configuration ahead of the policy keys it derives from the target.
///
/// The accepted reasoning effort is deliberately not pinned. Codex does not
/// warn about it, and the worker restores it on the live session once the
/// handshake completes, so pinning it here would only risk changing the
/// effort the resumed thread starts on.
#[cfg(unix)]
pub(crate) fn pin_accepted_bridge_selectors(
    harness: HarnessKind,
    environment: &mut std::collections::BTreeMap<String, String>,
    accepted: &mj_core::acp::AcceptedSessionConfig,
) -> Result<()> {
    if harness != HarnessKind::Codex {
        return Ok(());
    }
    let Some(model) = accepted.model.as_deref() else {
        return Ok(());
    };
    let mut config = match environment.get(CODEX_CONFIG_ENV) {
        None => serde_json::Map::new(),
        Some(existing) => serde_json::from_str::<serde_json::Value>(existing)
            .ok()
            .and_then(|value| match value {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            })
            .with_context(|| {
                format!("{CODEX_CONFIG_ENV} must be a JSON object to restore this session's model")
            })?,
    };
    config.insert("model".to_owned(), model.into());
    environment.insert(
        CODEX_CONFIG_ENV.to_owned(),
        serde_json::Value::Object(config).to_string(),
    );
    Ok(())
}

/// Bring the bridge's supervisor spec up to date with what this session has
/// accepted, so the next bridge process starts on it.
///
/// The spec is written once, before the worker's first bridge starts, and
/// every later bridge re-execs the supervisor against that same file. A
/// session accepts its model after that point far more often than before it:
/// a session created with an explicit model applies the selector once the
/// session exists, and a model set later never reaches the file at all. So a
/// pin taken only at worker start is missing exactly when it is needed, and
/// every in-worker resume reopens the harness on the profile default. Rewrite
/// it from the live accepted configuration at each launch instead.
#[cfg(unix)]
pub(crate) fn repin_bridge_selectors(
    path: &Path,
    harness: HarnessKind,
    accepted: &mj_core::acp::AcceptedSessionConfig,
) -> Result<()> {
    let mut spec = AcpSupervisorSpec::read(path)?;
    let environment = spec.environment.clone();
    pin_accepted_bridge_selectors(harness, &mut spec.environment, accepted)?;
    if spec.environment == environment {
        return Ok(());
    }
    spec.write_spec(path)
}

#[cfg(unix)]
mod discovery;
#[cfg(unix)]
pub(crate) mod harness;
#[cfg(unix)]
pub use discovery::discover_profile_config;
#[cfg(not(unix))]
pub async fn discover_profile_config(
    _spec: mj_core::worker_launch::ProfileProbeSpec,
) -> anyhow::Result<mj_core::worker_launch::ProfileConfig> {
    anyhow::bail!("profile discovery requires a Unix worker host")
}

#[cfg(unix)]
pub(crate) mod reviewer;
#[cfg(unix)]
pub(crate) mod subagents;
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
    let home = if config.harness_home.as_os_str().is_empty() {
        // A config persisted before the home was stated outright. Those
        // releases always set the harness home variable.
        let key = config.harness.home_env();
        let value = config.environment.get(key).ok_or_else(|| {
            format!(
                "worker launch config has no harness home and no {key} entry, so it cannot locate harness credentials"
            )
        })?;
        config.harness.home_from_environment(value)
    } else {
        config.harness_home.clone()
    };
    let marker = match config.authentication_marker.as_deref() {
        Some(name) => home.join(name),
        // Configs persisted before the controller stated the marker.
        None => mj_core::config::harness_authentication_marker(config.harness, &home),
    };
    Ok(CredentialEndpoint {
        harness: config.harness,
        home: home.clone(),
        marker,
    })
}

#[cfg(unix)]
fn resolve_relative_harness_home(config: &mut WorkerLaunchConfig, base: &Path) {
    if config.harness == mj_core::config::HarnessKind::Muse
        && let Some(value) = config.environment.get_mut("XDG_DATA_HOME")
        && Path::new(value).is_relative()
    {
        *value = base.join(&*value).to_string_lossy().into_owned();
    }
    let key = config.harness.home_env();
    if let Some(value) = config.environment.get_mut(key) {
        let path = Path::new(value);
        if path.is_relative() {
            *value = base.join(path).to_string_lossy().into_owned();
        }
    }
    if config.harness_home.is_relative() && !config.harness_home.as_os_str().is_empty() {
        config.harness_home = base.join(&config.harness_home);
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
pub use unix::{
    SESSION_SETUP_GUIDANCE, attach_session_git_environment, configure_github_cli,
    lead_process_group, prepare_managed_harness, proxy, run_acp_supervisor, run_daemon,
};

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
pub async fn prepare_managed_harness(_config: WorkerLaunchConfig) -> anyhow::Result<()> {
    anyhow::bail!("managed target harnesses require Unix")
}

#[cfg(not(unix))]
pub async fn run_acp_supervisor(_spec: AcpSupervisorSpec) -> anyhow::Result<()> {
    anyhow::bail!("ACP supervision requires Unix")
}

#[cfg(all(test, unix))]
mod model_pin_tests {
    use super::*;
    use mj_core::acp::AcceptedSessionConfig;

    fn accepted(model: &str) -> AcceptedSessionConfig {
        AcceptedSessionConfig {
            model: Some(model.to_owned()),
            effort: Some("high".to_owned()),
        }
    }

    fn codex_config(environment: &std::collections::BTreeMap<String, String>) -> serde_json::Value {
        serde_json::from_str(&environment[CODEX_CONFIG_ENV]).unwrap()
    }

    #[test]
    fn codex_launch_starts_a_resumed_bridge_on_the_accepted_model() {
        let mut environment = std::collections::BTreeMap::new();
        pin_accepted_bridge_selectors(HarnessKind::Codex, &mut environment, &accepted("flash"))
            .unwrap();
        assert_eq!(
            codex_config(&environment),
            serde_json::json!({ "model": "flash" })
        );
    }

    /// A profile may already set `CODEX_CONFIG`. Only the model this session
    /// accepted may change, because everything else in there is the host's.
    #[test]
    fn codex_launch_keeps_the_rest_of_a_host_supplied_config() {
        let mut environment = std::collections::BTreeMap::from([(
            CODEX_CONFIG_ENV.to_owned(),
            r#"{"default_permissions":"project","model":"configured-model","tui":"never"}"#
                .to_owned(),
        )]);
        pin_accepted_bridge_selectors(HarnessKind::Codex, &mut environment, &accepted("flash"))
            .unwrap();
        assert_eq!(
            codex_config(&environment),
            serde_json::json!({
                "default_permissions": "project",
                "model": "flash",
                "tui": "never",
            })
        );
    }

    #[test]
    fn sessions_without_an_accepted_model_keep_their_environment() {
        for harness in [HarnessKind::Codex, HarnessKind::Claude] {
            let mut environment = std::collections::BTreeMap::from([(
                CODEX_CONFIG_ENV.to_owned(),
                r#"{"model":"configured-model"}"#.to_owned(),
            )]);
            pin_accepted_bridge_selectors(
                harness,
                &mut environment,
                &AcceptedSessionConfig::default(),
            )
            .unwrap();
            assert_eq!(
                environment[CODEX_CONFIG_ENV],
                r#"{"model":"configured-model"}"#
            );
        }
    }

    #[test]
    fn other_harnesses_never_receive_a_codex_config() {
        let mut environment = std::collections::BTreeMap::new();
        pin_accepted_bridge_selectors(HarnessKind::Claude, &mut environment, &accepted("flash"))
            .unwrap();
        pin_accepted_bridge_selectors(HarnessKind::Kimi, &mut environment, &accepted("flash"))
            .unwrap();
        assert!(environment.is_empty());
    }

    /// codex-acp parses this variable at startup, so a value it cannot parse
    /// is a broken profile rather than a reason to launch without the model.
    #[test]
    fn an_unparsable_codex_config_is_reported_rather_than_overwritten() {
        for broken in ["[]", "not json"] {
            let mut environment = std::collections::BTreeMap::from([(
                CODEX_CONFIG_ENV.to_owned(),
                broken.to_owned(),
            )]);
            let error = pin_accepted_bridge_selectors(
                HarnessKind::Codex,
                &mut environment,
                &accepted("flash"),
            )
            .expect_err("a non-object configuration cannot be merged");
            assert!(
                format!("{error:#}").contains(CODEX_CONFIG_ENV),
                "unexpected error: {error:#}"
            );
            assert_eq!(environment[CODEX_CONFIG_ENV], broken);
        }
    }
}

#[cfg(test)]
mod startup_record_tests {
    use super::*;

    /// The controller reads this file to decide whether a worker that has not
    /// answered yet is still making progress, so the latest step and the order
    /// of the steps are the two things that must hold.
    #[test]
    fn the_startup_record_names_the_latest_step_and_keeps_the_order() {
        let root = tempfile::tempdir().unwrap();
        record_startup_step(root.path(), "start");
        record_startup_step(root.path(), "login-environment");
        record_startup_step(root.path(), "bind-socket");

        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join(WORKER_STARTUP_FILE)).unwrap())
                .unwrap();

        assert_eq!(record["step"], "bind-socket");
        assert_eq!(record["pid"], std::process::id());
        let steps: Vec<String> = record["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["step"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(steps, ["start", "login-environment", "bind-socket"]);
        assert!(
            record["steps"][0]["at"]
                .as_str()
                .is_some_and(|at| !at.is_empty()),
            "every step carries when it happened: {record}"
        );
    }

    /// A breadcrumb must never be the reason a worker fails to start, so a
    /// root that is not there is silently skipped.
    #[test]
    fn recording_a_step_without_a_root_is_silent() {
        record_startup_step(
            std::path::Path::new("/definitely/not/a/worker/root"),
            "start",
        );
    }
}

#[cfg(all(test, unix))]
mod relay_tests;
#[cfg(all(test, unix))]
mod reviewer_tests;

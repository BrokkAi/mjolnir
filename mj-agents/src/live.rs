//! Wiring shared by every frontend that launches a live primary session:
//! subagent service configuration, review fan-out diagnostics, and isolated
//! Codex homes for delegated roles.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::{Context, Result};

use crate::{quota, subagent};
use mj_core::{acp, config, roster};

/// Inputs shared by the primary session's long-lived subagent MCP endpoint.
/// The endpoint can replace its launch configuration without replacing the
/// primary ACP session.
#[derive(Clone)]
pub struct LiveSubagentOptions {
    pub agent_stderr: Option<PathBuf>,
    pub snapshot_exclusions: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub session_tag: String,
    pub handoff_counter: Arc<AtomicUsize>,
    pub id_allocator: subagent::SubagentIdAllocator,
    pub active_workers: subagent::ActiveSubagentWorkers,
    pub review_checkpoint: subagent::ReviewCheckpointClient,
    pub reports: subagent::SubagentReportBus,
    pub runs: subagent::SubagentRegistry,
}

pub fn configured_subagent_service(
    pool: quota::RolePool,
    options: &LiveSubagentOptions,
    config: &config::SubagentsConfig,
    mcp_discrete_review: bool,
) -> subagent::Config {
    let mut service = subagent::Config::new(pool, options.agent_stderr.clone());
    if let Some(role) = service.role_config.as_mut() {
        role.session_tag = Some(options.session_tag.clone());
    }
    service
        .with_subagent_handoff_counter(options.handoff_counter.clone())
        .with_id_allocator(options.id_allocator.clone())
        .with_active_implementation_workers(options.active_workers.clone())
        .with_review_checkpoint(options.review_checkpoint.clone(), mcp_discrete_review)
        .with_max_parallel(config.max_parallel)
        .with_debrief(config.debrief)
        .with_permission_mode(config.permission)
        .with_reports(options.reports.clone())
        .with_run_registry(options.runs.clone())
        .with_prewarm(subagent::RunContext {
            cwd: options.cwd.clone(),
            additional_directories: options.additional_directories.clone(),
            snapshot_exclusions: options.snapshot_exclusions.clone(),
            fs_max_text_bytes: options.fs_max_text_bytes,
            access_mode: acp::RuntimeAccessMode::Full,
        })
}

/// Preserve the resolver's original error when a review cannot construct its
/// specialist fan-out. The orchestrator must never receive a bare `None` and
/// invent an explanation later.
pub fn review_fanout_error(
    workers_available: bool,
    supervisor_available: bool,
    subagents_model: &str,
    review_route_enabled: bool,
    roster_warnings: &[String],
) -> String {
    let mut causes = Vec::new();
    if !workers_available {
        if matches!(subagents_model, config::DISABLED_MODEL | "none") {
            causes.push("`subagents.model` is disabled in the active configuration".to_string());
        } else if let Some(warning) = roster_warnings
            .iter()
            .find(|warning| warning.starts_with("subagent delegation is disabled:"))
        {
            causes.push(warning.clone());
        }
    }
    if !supervisor_available {
        if !review_route_enabled {
            causes.push(
                "both `agent.discrete_review` and `agent.mcp_discrete_review` are disabled in the active configuration"
                    .to_string(),
            );
        } else if let Some(warning) = roster_warnings
            .iter()
            .find(|warning| warning.starts_with("agentic review supervisor is disabled:"))
        {
            causes.push(warning.clone());
        }
    }
    causes.extend(
        roster_warnings
            .iter()
            .filter(|warning| warning.contains(" unavailable: "))
            .cloned(),
    );
    causes.sort();
    causes.dedup();
    assert!(
        !causes.is_empty(),
        "roster resolution did not record why the review fan-out is unavailable"
    );
    causes.join("\n")
}

pub fn primary_route_matches(
    active: &roster::ResolvedAgent,
    candidate: &roster::ResolvedAgent,
) -> bool {
    active.launch.source_id == candidate.launch.source_id
        && active.model.model == candidate.model.model
        && active.model_value == candidate.model_value
        && active.reasoning_effort == candidate.reasoning_effort
}

pub fn isolated_subagent_role(
    role: roster::ResolvedAgent,
    label: &str,
) -> Result<(roster::ResolvedAgent, Option<tempfile::TempDir>)> {
    if role.launch.kind != roster::AdapterKind::Codex {
        return Ok((role, None));
    }
    let source = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| anyhow::anyhow!("could not locate CODEX_HOME for {label}"))?;
    isolated_subagent_role_from_home(role, label, &source)
}

pub fn isolated_subagent_role_from_home(
    mut role: roster::ResolvedAgent,
    label: &str,
    source: &Path,
) -> Result<(roster::ResolvedAgent, Option<tempfile::TempDir>)> {
    let isolated = tempfile::Builder::new()
        .prefix(&format!("mj-{label}-codex-"))
        .tempdir()
        .with_context(|| format!("create isolated Codex home for {label}"))?;
    for name in ["config.toml", "models_cache.json", "version.json"] {
        let from = source.join(name);
        if from.is_file() {
            std::fs::copy(&from, isolated.path().join(name)).with_context(|| {
                format!("copy {} into isolated {label} Codex home", from.display())
            })?;
        }
    }
    let source_auth = source.join("auth.json");
    if !source_auth.is_file() {
        anyhow::bail!(
            "Codex is available but {} has no auth.json; sign in from /mjconfig",
            source.display()
        );
    }
    // Credentials must stay shared, never snapshotted: OpenAI rotates refresh
    // tokens, so a private copy goes stale as soon as any other process
    // refreshes or the user signs in again, and the seat then fails every
    // request with "refresh token was revoked" until the session restarts.
    // Codex rewrites auth.json in place, so a symlink keeps the seat on the
    // live grant in both directions.
    share_auth_json(&source_auth, &isolated.path().join("auth.json"), label)?;
    role.launch.env.insert(
        "CODEX_HOME".to_string(),
        isolated.path().display().to_string(),
    );
    Ok((role, Some(isolated)))
}

/// Codex rewrites auth.json in place, so a symlink — or a same-volume hard
/// link on Windows, where symlinks need developer mode or elevation — behaves
/// exactly like the real file. The plain copy is a last resort that reopens
/// the stale-credential window.
pub fn share_auth_json(source: &Path, target: &Path, label: &str) -> Result<()> {
    let source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&source, target);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&source, target)
        .or_else(|_| std::fs::hard_link(&source, target))
        .or_else(|_| std::fs::copy(&source, target).map(|_| ()));
    #[cfg(not(any(unix, windows)))]
    let linked = std::fs::copy(&source, target).map(|_| ());
    linked.with_context(|| {
        format!(
            "share {} with the isolated {label} Codex home",
            source.display()
        )
    })
}

pub fn isolated_subagent_roles(
    mut roles: Vec<roster::ResolvedAgent>,
    label: &str,
) -> Result<(Vec<roster::ResolvedAgent>, Option<tempfile::TempDir>)> {
    let Some(index) = roles
        .iter()
        .position(|role| role.launch.kind == roster::AdapterKind::Codex)
    else {
        return Ok((roles, None));
    };
    let (prepared, guard) = isolated_subagent_role(roles[index].clone(), label)?;
    let codex_home = prepared
        .launch
        .env
        .get("CODEX_HOME")
        .cloned()
        .expect("isolated Codex role has CODEX_HOME");
    roles[index] = prepared;
    for role in &mut roles {
        if role.launch.kind == roster::AdapterKind::Codex {
            role.launch
                .env
                .insert("CODEX_HOME".to_string(), codex_home.clone());
        }
    }
    Ok((roles, guard))
}

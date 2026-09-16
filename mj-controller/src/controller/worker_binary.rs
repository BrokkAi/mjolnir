//! Worker binary acquisition, profile staging, and worker installation.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::session_manager::{
    ProjectMemorySyncTarget, RemoteWorkerBinaryRefresh, WorkerBinaryRefresh,
    WorkerBinaryRefreshPlan, WorkerLaunchRefreshPlan, WorkerRecoveryPlan, WorkerWorkspace,
};
use crate::targets::{self, CommandExecutor, CommandPlan, CommandSpec, ProvisionStage, SshTarget};
use mj_core::config::{
    HarnessKind, HarnessProfile, ProjectBundle, ProjectRepository, atomic_write, data_dir,
};
use mj_core::harness_runtime::{CLAUDE_ACP_VERSION, CODEX_ACP_PACKAGE, CODEX_ACP_VERSION};
use mj_core::project_memory::{ProjectMemoryIdentity, RepositoryMemoryIdentity};
use mj_core::worker_launch::{
    HarnessRuntimePolicy, ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery, WorkerLaunchConfig,
    WorkerOwnership,
};

use super::backend::backend_locator;
use super::readiness::WORKER_EXIT_RECORD_MARKER;
use super::{Controller, execute_checked, target_profile_home};

impl Controller {
    /// Where this session's worker lives. This is decided from the session
    /// record and configuration alone, so a caller can name the worker root
    /// before anything is installed into it.
    pub(super) fn worker_placement(
        &self,
        session_id: &str,
    ) -> Result<(targets::TargetLocator, String)> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let locator = session
            .target
            .as_ref()
            .context("session target is missing")?;
        let backend = backend_locator(locator, session, &self.config)?;
        let worker_root = targets::worker_root(&backend, session_id)?;
        Ok((backend, worker_root))
    }

    pub(super) fn prepare_worker_files(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.validate_configuration(&self.config)?;
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let bundle = session
            .project_directory
            .is_none()
            .then(|| self.config.bundles.get(&session.bundle_id))
            .flatten();
        let target = self
            .config
            .targets
            .get(&session.target_template_id)
            .context("session target template is missing")?;
        let subagent = crate::database::load_subagent(session_id)?;
        let workspace_session_id = subagent.as_ref().map_or_else(
            || session_id.to_owned(),
            |child| child.parent_session_id.clone(),
        );
        let (mut launch, project_memory, target_profile_home) = worker_launch_config(
            session,
            profile,
            bundle,
            backend,
            session_id,
            &workspace_session_id,
            target,
        )?;
        launch.subagent_tools =
            subagent_tools_enabled(session, self.config.subagents.enabled, subagent.is_some());
        if let Some(subagent) = &subagent {
            let parent = self
                .state
                .sessions
                .get(&subagent.parent_session_id)
                .context("sub-agent parent session is missing")?;
            let parent_profile = self
                .config
                .profiles
                .get(&parent.last_profile)
                .context("sub-agent parent profile is missing")?;
            let parent_target = self
                .config
                .targets
                .get(&parent.target_template_id)
                .context("sub-agent parent target template is missing")?;
            let parent_locator = parent
                .target
                .as_ref()
                .context("sub-agent parent has no live target")?;
            let parent_backend = backend_locator(parent_locator, parent, &self.config)?;
            let parent_bundle = parent
                .project_directory
                .is_none()
                .then(|| self.config.bundles.get(&parent.bundle_id))
                .flatten();
            let (parent_launch, _, _) = worker_launch_config(
                parent,
                parent_profile,
                parent_bundle,
                &parent_backend,
                &parent.id,
                &parent.id,
                parent_target,
            )?;
            launch.cwd = if subagent.working_directory.as_os_str().is_empty() {
                parent_launch.cwd
            } else {
                parent_launch.cwd.join(&subagent.working_directory)
            };
            launch.additional_directories = parent_launch.additional_directories;
        }

        if session.native_session_id.is_some()
            && profile.kind == mj_core::config::HarnessKind::Codex
        {
            launch.goal_resume_request = Some(mj_core::state::new_session_id()?);
        }
        let staging = tempfile::tempdir().context("create worker staging directory")?;
        let launch_path = staging.path().join("launch.json");
        launch.write(&launch_path)?;
        let ownership_path = staging.path().join("ownership.json");
        WorkerOwnership {
            version: WorkerOwnership::VERSION,
            workspace_id: session.workspace_id.clone(),
            session_id: session_id.to_string(),
            profile_id: session.last_profile.clone(),
            bundle_id: session.bundle_id.clone(),
            target_template_id: session.target_template_id.clone(),
        }
        .write(&ownership_path)?;
        let profile_stage = staging.path().join("profile");
        if !matches!(backend, targets::TargetLocator::LocalBare { .. })
            || matches!(
                profile.kind,
                mj_core::config::HarnessKind::Claude | mj_core::config::HarnessKind::Muse
            )
            || super::requires_private_profile_home(profile)
        {
            let started = Instant::now();
            let result = stage_profile(profile, &profile_stage);
            tracing::debug!(
                session_id,
                elapsed_ms = started.elapsed().as_millis(),
                "profile staging completed"
            );
            result?;
            stage_codex_catalog(
                &session.last_profile,
                profile,
                &profile_stage,
                &fetch_catalog_over_https,
                &SharedCatalogCache,
            )?;
            append_hel_target_environment(profile.kind, &profile_stage, backend)?;
            apply_staged_execution_setting(profile.kind, launch.execution_policy, &profile_stage)?;
            if launch.subagent_tools && profile.kind == mj_core::config::HarnessKind::Claude {
                configure_claude_subagent_mcp(&profile_stage, worker_root)?;
            }
            stage_memory_replica(
                &project_memory,
                Path::new(&target_profile_home),
                &profile_stage,
            )?;
            if project_memory.mcp_delivery == ProjectMemoryMcpDelivery::HarnessProfile {
                configure_kimi_project_memory_mcp(&profile_stage, worker_root, &project_memory)?;
            }
        } else {
            seed_local_memory_replica(&project_memory)?;
        }
        let worker_binary = worker_binary_for(backend, executor)?;

        install_worker_files(
            executor,
            backend,
            session_id,
            worker_root,
            &target_profile_home,
            &worker_binary,
            &launch_path,
            &ownership_path,
            &profile_stage,
        )?;
        prepare_installed_managed_harness(executor, backend, worker_root, &launch)
    }

    pub fn diagnose_worker_controlled(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Option<String> {
        let session = self.state.sessions.get(session_id)?;
        let locator = session.target.as_ref()?;
        let backend = match backend_locator(locator, session, &self.config) {
            Ok(backend) => backend,
            Err(error) => {
                tracing::debug!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not construct a worker diagnostic probe"
                );
                return None;
            }
        };
        let worker_root = match targets::worker_root(&backend, session_id) {
            Ok(root) => root,
            Err(error) => {
                tracing::debug!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not derive the worker diagnostic root"
                );
                return None;
            }
        };
        let binary_failure = worker_binary_probe_failure(executor, &backend, &worker_root);
        let last_words = worker_last_words(executor, &backend, &worker_root);
        match (binary_failure, last_words) {
            (Some(binary_failure), Some(last_words)) => {
                Some(format!("{binary_failure}; {last_words}"))
            }
            (Some(binary_failure), None) => Some(binary_failure),
            (None, last_words) => last_words,
        }
    }

    /// A non-destructive liveness probe plus commands that replace a confirmed
    /// dead session worker without touching its durable relay files. The
    /// session manager runs both off its async actor.
    pub fn worker_recovery_plan(&self, session_id: &str) -> Result<WorkerRecoveryPlan> {
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        let workspace = worker_workspace_for_recovery(&backend, &launch.cwd);
        Ok(WorkerRecoveryPlan {
            source_target: self.state.sessions[session_id]
                .target
                .clone()
                .context("session target is missing")?,
            target: targets::target_recovery_plan(&backend, session_id)?,
            workspace,
            liveness_probe: worker_liveness_command(&backend, &worker_root),
            binary_refresh: worker_binary_refresh_plan(&backend, session_id)?,
            launch_refresh: Some(worker_launch_refresh_plan(&backend, session_id, &launch)?),
            restart: CommandPlan {
                description: format!("restart Mjolnir worker for session {session_id}"),
                commands: vec![
                    stop_worker_command(&backend, &worker_root),
                    start_worker_command(&backend, &worker_root),
                ],
            },
        })
    }

    pub(super) fn current_worker_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
    ) -> Result<WorkerLaunchConfig> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.validate_configuration(&self.config)?;
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let bundle = session
            .project_directory
            .is_none()
            .then(|| self.config.bundles.get(&session.bundle_id))
            .flatten();
        let target = self
            .config
            .targets
            .get(&session.target_template_id)
            .context("session target template is missing")?;
        let (mut launch, _, _) = worker_launch_config(
            session, profile, bundle, backend, session_id, session_id, target,
        )?;
        if crate::database::load_move_operation(session_id)?.is_some_and(|operation| {
            operation.source_checkpoint_only
                && operation.destination_target.is_none()
                && matches!(
                    operation.phase,
                    mj_core::state::MovePhase::Preparing
                        | mj_core::state::MovePhase::ClosingSource
                        | mj_core::state::MovePhase::Failed
                        | mj_core::state::MovePhase::Cancelled
                )
                && session.last_profile == operation.source_profile_id
                && session.target == operation.source_target
                && matches!(
                    session.state,
                    mj_core::state::SessionState::Running
                        | mj_core::state::SessionState::Disconnected
                        | mj_core::state::SessionState::Closing
                )
        }) {
            launch.run_mode = mj_core::worker_launch::WorkerRunMode::CheckpointOnly;
        }
        Ok(launch)
    }

    pub fn project_memory_sync_target(&self, session_id: &str) -> Result<ProjectMemorySyncTarget> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.validate_configuration(&self.config)?;
        let locator = session
            .target
            .as_ref()
            .context("session target is missing")?;
        let backend = backend_locator(locator, session, &self.config)?;
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let bundle = session
            .project_directory
            .is_none()
            .then(|| self.config.bundles.get(&session.bundle_id))
            .flatten();
        let workspace = if let Some(project_directory) = &session.project_directory {
            (project_directory.to_string_lossy().into_owned(), Vec::new())
        } else {
            workspace_paths(
                &backend,
                bundle.context("session bundle is missing")?,
                session_id,
            )?
        };
        let target_home = target_profile_home(&backend, session_id, profile);
        let launch = project_memory_launch(session, bundle, &workspace, &target_home)?;
        Ok(ProjectMemorySyncTarget {
            canonical_root: canonical_memory_root(&launch.project_key),
        })
    }
}

/// Whether this session gets Mjolnir's delegation tools in place of its
/// harness's own. The session's stored choice governs and `None` follows the
/// global `[subagents] enabled` setting, so a session created before the
/// per-session choice existed behaves as it always did. A child never gets
/// them, and only Claude and Codex can receive them at all.
fn subagent_tools_enabled(
    session: &mj_core::state::SessionRecord,
    global_enabled: bool,
    is_child: bool,
) -> bool {
    session.mjolnir_subagents.unwrap_or(global_enabled)
        && !is_child
        && matches!(
            session.harness_kind,
            mj_core::config::HarnessKind::Claude | mj_core::config::HarnessKind::Codex
        )
}

fn worker_workspace_for_recovery(
    backend: &targets::TargetLocator,
    directory: &Path,
) -> Option<WorkerWorkspace> {
    let target = match backend {
        targets::TargetLocator::LocalBare { .. } => mj_core::state::ManagedWorktreeTarget::Local,
        targets::TargetLocator::SshBare { ssh, .. } => mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: ssh.destination.clone(),
            ssh_args: ssh.ssh_args.clone(),
        },
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => return None,
    };
    Some(WorkerWorkspace {
        target,
        directory: directory.to_path_buf(),
    })
}

fn worker_launch_config(
    session: &mj_core::state::SessionRecord,
    profile: &mj_core::config::HarnessProfile,
    bundle: Option<&ProjectBundle>,
    backend: &targets::TargetLocator,
    session_id: &str,
    workspace_session_id: &str,
    target: &mj_core::config::TargetTemplate,
) -> Result<(WorkerLaunchConfig, ProjectMemoryLaunchConfig, String)> {
    let execution_policy = profile
        .kind
        .effective_execution_policy(target.execution_policy());
    let target_profile_home = target_profile_home(backend, session_id, profile);
    let workspace = if let Some(project_directory) = &session.project_directory {
        (project_directory.to_string_lossy().into_owned(), Vec::new())
    } else {
        workspace_paths(
            backend,
            bundle.context("session bundle is missing")?,
            workspace_session_id,
        )?
    };
    let mut additional_directories = workspace.1.iter().map(PathBuf::from).collect::<Vec<_>>();
    additional_directories.extend(
        session
            .additional_mounts
            .iter()
            .map(|resource| resource.destination.clone()),
    );
    if profile.kind == mj_core::config::HarnessKind::Muse && !additional_directories.is_empty() {
        bail!(
            "{} ACP does not support multiple workspace roots; use a single-repository bundle",
            profile.kind.display_name()
        );
    }
    let (bridge_command, bridge_args) = bridge_launch(profile.kind, execution_policy);
    use mj_core::config::TargetTemplate;
    let target_environment = match target {
        TargetTemplate::LocalPodman { container }
        | TargetTemplate::LocalDocker { container }
        | TargetTemplate::AppleContainer { container }
        | TargetTemplate::SshPodman { container, .. }
        | TargetTemplate::SshDocker { container, .. } => container.environment.clone(),
        _ => Default::default(),
    };
    let mut environment = target_environment.clone();
    environment.extend(profile.environment.clone());
    profile
        .kind
        .configure_home_environment(Path::new(&target_profile_home), &mut environment);
    profile
        .kind
        .configure_execution_environment(execution_policy, &mut environment)?;
    environment.remove(mj_core::worker_launch::DISCOVER_LOGIN_PATH_ENV);
    let mut project_memory =
        project_memory_launch(session, bundle, &workspace, &target_profile_home)?;
    project_memory.mcp_delivery = project_memory_mcp_delivery(profile.kind, backend);
    if profile.kind == mj_core::config::HarnessKind::Claude {
        environment.insert(
            "CLAUDE_CODE_PROJECT_DIR_NAME".into(),
            project_memory_replica_slug(&project_memory.project_key, session_id),
        );
    }
    apply_claude_setup_token(
        &mut environment,
        profile.kind,
        &mj_core::credentials::claude_oauth_token_path(&session.last_profile),
    );
    Ok((
        WorkerLaunchConfig {
            goal_resume_request: None,
            target_environment,
            run_mode: Default::default(),
            session_id: session_id.to_string(),
            subagent_tools: false,
            harness: profile.kind,
            // The staged home mirrors the profile home, so the controller's
            // marker file name is the one the worker must check.
            authentication_marker: profile
                .authentication_marker()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            bridge_command: PathBuf::from(bridge_command),
            bridge_args,
            harness_runtime: harness_runtime_policy(backend),
            environment,
            cwd: PathBuf::from(&workspace.0),
            additional_directories,
            native_session_id: session.native_session_id.clone(),
            project_memory: profile
                .kind
                .supports_injected_mcp()
                .then(|| project_memory.clone()),
            execution_policy,
        },
        project_memory,
        target_profile_home,
    ))
}

fn harness_runtime_policy(backend: &targets::TargetLocator) -> HarnessRuntimePolicy {
    match backend {
        targets::TargetLocator::LocalBare { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => HarnessRuntimePolicy::Managed,
        _ => HarnessRuntimePolicy::Ambient,
    }
}

/// Hand a Claude worker the profile's long-lived setup token, when it has one.
///
/// Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN` ahead of the `/login`
/// credentials file, and a setup token does not rotate, so a container copy
/// cannot lose the single-use refresh race with the host. A profile that sets
/// the variable itself stays authoritative.
pub(super) fn apply_claude_setup_token(
    environment: &mut std::collections::BTreeMap<String, String>,
    kind: mj_core::config::HarnessKind,
    token_path: &Path,
) {
    use mj_core::credentials::CLAUDE_OAUTH_TOKEN_ENV;

    if kind != mj_core::config::HarnessKind::Claude
        || environment.contains_key(CLAUDE_OAUTH_TOKEN_ENV)
    {
        return;
    }
    match mj_core::credentials::read_claude_oauth_token(token_path) {
        Ok(Some(token)) => {
            environment.insert(CLAUDE_OAUTH_TOKEN_ENV.to_owned(), token);
        }
        Ok(None) => {}
        // A stored token Hel cannot read is worth reporting, but the session
        // still starts on the synced credentials file.
        Err(error) => tracing::warn!(
            path = %token_path.display(),
            %error,
            "ignoring an unreadable Claude setup token"
        ),
    }
}

fn project_memory_launch(
    session: &mj_core::state::SessionRecord,
    bundle: Option<&ProjectBundle>,
    workspace: &(String, Vec<String>),
    target_profile_home: &str,
) -> Result<ProjectMemoryLaunchConfig> {
    let identity = if let Some(worktree) = &session.managed_worktree {
        ProjectMemoryIdentity::Repository {
            repository: RepositoryMemoryIdentity::Local {
                canonical_root: std::fs::canonicalize(&worktree.source_repository)
                    .unwrap_or_else(|_| worktree.source_repository.clone()),
            },
        }
    } else if let Some(bundle) = bundle {
        let primary =
            configured_memory_identity(bundle.primary().context("bundle primary is missing")?)?;
        let members = bundle
            .repositories
            .iter()
            .map(configured_memory_identity)
            .collect::<Result<Vec<_>>>()?;
        ProjectMemoryIdentity::bundle(primary, members)
    } else {
        let project = session
            .project_directory
            .as_ref()
            .context("raw session project directory is missing")?;
        let repository = match session.target.as_ref() {
            Some(mj_core::state::TargetLocator::LocalBare { .. }) => {
                RepositoryMemoryIdentity::Local {
                    canonical_root: std::fs::canonicalize(project)
                        .unwrap_or_else(|_| project.clone()),
                }
            }
            _ => RepositoryMemoryIdentity::Remote {
                target: session.target_template_id.clone(),
                canonical_root: project.clone(),
            },
        };
        ProjectMemoryIdentity::Repository { repository }
    };
    let project_key = identity.key()?;
    let replica_slug = project_memory_replica_slug(&project_key, &session.id);
    let project_root = PathBuf::from(target_profile_home)
        .join("projects")
        .join(replica_slug);
    let root = project_root.join("memory");
    let baseline_root = project_root.join(".hel-memory-baseline");
    let mut repository_roots = std::collections::BTreeMap::new();
    if let Some(bundle) = bundle {
        let target_roots =
            std::iter::once(workspace.0.as_str()).chain(workspace.1.iter().map(String::as_str));
        let repositories = std::iter::once(bundle.primary().context("bundle primary is missing")?)
            .chain(
                bundle
                    .repositories
                    .iter()
                    .filter(|repository| repository.id != bundle.primary_repo),
            );
        repository_roots.extend(
            repositories
                .zip(target_roots)
                .map(|(repository, root)| (repository.id.clone(), PathBuf::from(root))),
        );
    }
    Ok(ProjectMemoryLaunchConfig {
        project_key,
        root,
        baseline_root,
        repository_roots,
        mcp_delivery: ProjectMemoryMcpDelivery::Acp,
    })
}

fn project_memory_replica_slug(project_key: &str, session_id: &str) -> String {
    format!("hel-{}-{session_id}", &project_key[..16])
}

fn project_memory_mcp_delivery(
    harness: mj_core::config::HarnessKind,
    target: &targets::TargetLocator,
) -> ProjectMemoryMcpDelivery {
    if harness == mj_core::config::HarnessKind::Kimi
        && !matches!(target, targets::TargetLocator::LocalBare { .. })
    {
        ProjectMemoryMcpDelivery::HarnessProfile
    } else {
        ProjectMemoryMcpDelivery::Acp
    }
}

fn configured_memory_identity(repository: &ProjectRepository) -> Result<RepositoryMemoryIdentity> {
    if let Some(source) = repository.github.as_deref() {
        let github = crate::setup::github_repository_from_origin(source)
            .with_context(|| format!("parse repository source {source:?} for project memory"))?;
        return Ok(RepositoryMemoryIdentity::Github {
            owner: github.owner.to_ascii_lowercase(),
            repository: github.repository.to_ascii_lowercase(),
        });
    }
    let root = repository
        .local
        .as_ref()
        .context("project repository has no source for memory identity")?;
    Ok(RepositoryMemoryIdentity::Local {
        canonical_root: mj_core::local_git::main_worktree_root(root)
            .or_else(|_| std::fs::canonicalize(root).map_err(anyhow::Error::from))
            .unwrap_or_else(|_| root.clone()),
    })
}

fn canonical_memory_root(project_key: &str) -> PathBuf {
    data_dir().join("projects").join(project_key).join("memory")
}

fn stage_memory_replica(
    memory: &ProjectMemoryLaunchConfig,
    target_profile_home: &Path,
    profile_stage: &Path,
) -> Result<()> {
    let canonical = canonical_memory_root(&memory.project_key);
    std::fs::create_dir_all(&canonical)?;
    let replica = memory.root.strip_prefix(target_profile_home)?;
    let baseline = memory.baseline_root.strip_prefix(target_profile_home)?;
    copy_profile_entry(&canonical, &profile_stage.join(replica))?;
    copy_profile_entry(&canonical, &profile_stage.join(baseline))
}

fn seed_local_memory_replica(memory: &ProjectMemoryLaunchConfig) -> Result<()> {
    let canonical = canonical_memory_root(&memory.project_key);
    std::fs::create_dir_all(&canonical)?;
    let canonical_has_files = directory_has_files(&canonical)?;
    let replica_has_files = directory_has_files(&memory.root)?;
    match (canonical_has_files, replica_has_files) {
        (false, true) => copy_profile_entry(&memory.root, &canonical),
        (true, false) => copy_profile_entry(&canonical, &memory.root),
        _ => Ok(()),
    }?;
    copy_profile_entry(&canonical, &memory.baseline_root)
}

/// Kimi's runtime-aware engine cannot infer a runtime identity from an ACP
/// stdio server. Add Hel's server to the session-private profile instead,
/// where Kimi's native schema can bind it to the target's local runtime.
fn configure_kimi_project_memory_mcp(
    profile_stage: &Path,
    worker_root: &str,
    memory: &ProjectMemoryLaunchConfig,
) -> Result<()> {
    let path = profile_stage.join("mcp.json");
    edit_staged_json_object(&path, "staged Kimi MCP configuration", |root| {
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "mcpServers in staged Kimi MCP configuration {} must be a JSON object",
                    path.display()
                )
            })?;

        let worker = Path::new(worker_root).join("hel");
        let server = if worker.is_absolute() && memory.root.is_absolute() {
            serde_json::json!({
                "transport": "stdio",
                "command": worker,
                "args": ["worker", "memory-mcp", "--root", memory.root],
                "runtime_id": "local"
            })
        } else {
            let worker = worker.to_string_lossy();
            let memory_root = memory.root.to_string_lossy();
            serde_json::json!({
                "transport": "stdio",
                "command": "sh",
                "args": [
                    "-c",
                    "exec \"$HOME/$1\" worker memory-mcp --root \"$HOME/$2\"",
                    "mj-memory",
                    worker,
                    memory_root
                ],
                "runtime_id": "local"
            })
        };
        servers.insert("mj-memory".into(), server);
        Ok(())
    })
}

/// Claude reads MCP servers from its private profile rather than ACP. Parent
/// sessions always use an isolated staged profile, including on local bare
/// targets, so this never modifies the user's source profile.
fn configure_claude_subagent_mcp(profile_stage: &Path, worker_root: &str) -> Result<()> {
    let path = profile_stage.join(".claude.json");
    edit_staged_json_object(&path, "staged Claude configuration", |root| {
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "mcpServers in staged Claude configuration {} must be a JSON object",
                    path.display()
                )
            })?;
        servers.insert(
            "mj-agents".into(),
            serde_json::json!({
                "type":"stdio",
                "command":Path::new(worker_root).join("hel"),
                "args":[
                    "worker",
                    "subagent-mcp",
                    "--socket",
                    Path::new(worker_root).join(mj_worker_socket_name())
                ]
            }),
        );
        Ok(())
    })
}

/// Write the enforcement table's staged setting, if the harness has one. Muse
/// composes a session's permission profile from its settings file and nothing
/// on the ACP wire overrides that choice, so the profile has to be staged.
fn apply_staged_execution_setting(
    kind: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
    profile_stage: &Path,
) -> Result<()> {
    let Some(setting) = kind
        .execution_enforcement(policy)
        .and_then(mj_core::config::ExecutionEnforcement::staged_setting)
    else {
        return Ok(());
    };
    let path = profile_stage.join(setting.file);
    let label = format!("staged {} settings", kind.display_name());
    edit_staged_json_object(&path, &label, |root| {
        setting
            .apply(root)
            .with_context(|| format!("{label} {}", path.display()))
    })
}

/// Read a staged JSON settings file (treating a missing file as an empty
/// object), let `edit` change its root object, and write it back atomically.
/// `label` names the file in every error message.
fn edit_staged_json_object(
    path: &Path,
    label: &str,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
) -> Result<()> {
    let mut document = match std::fs::read(path) {
        Ok(body) => serde_json::from_slice::<serde_json::Value>(&body)
            .with_context(|| format!("parse {label} {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {label} {}", path.display()));
        }
    };
    let root = document
        .as_object_mut()
        .with_context(|| format!("{label} {} must contain a JSON object", path.display()))?;
    edit(root)?;
    let mut body = serde_json::to_vec_pretty(&document)?;
    body.push(b'\n');
    atomic_write(path, &body).with_context(|| format!("write {label} {}", path.display()))
}

fn mj_worker_socket_name() -> &'static str {
    "subagents.sock"
}

fn directory_has_files(path: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() || (metadata.is_dir() && directory_has_files(&entry.path())?) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBinaryAvailability {
    Local {
        path: PathBuf,
        source: String,
    },
    Remote {
        url: String,
        sha256: String,
        triple: String,
    },
}

/// Sources captured before the daemon starts its managers and coordinators.
///
/// Local sources are copied into an immutable, content-addressed cache during
/// capture. Remote sources retain only their URL, digest, and target triple;
/// the network fetch still happens when a target is provisioned.
#[derive(Debug)]
struct WorkerBinarySourceSnapshot {
    entries: HashMap<
        (String, WorkerBinaryRequirement),
        std::result::Result<WorkerBinaryAvailability, String>,
    >,
}

static PINNED_WORKER_BINARY_SOURCES: OnceLock<WorkerBinarySourceSnapshot> = OnceLock::new();

fn packaged_worker_binary_path(directory: &Path, triple: &str) -> PathBuf {
    directory.join(format!("mj-worker-{triple}"))
}

/// Linux exposes an unlinked running executable through `/proc` with a
/// ` (deleted)` suffix. `current_exe` preserves that suffix, but it is not
/// part of the executable's real file name and must not leak into sibling
/// lookup after `cargo` or a package upgrade replaces the controller.
fn running_executable_file_name(controller: &Path) -> Option<std::ffi::OsString> {
    let name = controller.file_name()?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        if let Some(name) = name.as_bytes().strip_suffix(b" (deleted)") {
            return Some(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    Some(name.to_os_string())
}

/// File names a worker binary may carry when it sits beside the controller or
/// in a development sibling directory. The controller's own file name comes
/// first (after the 2.0 rename that is `mj`), then the legacy `hel` name that
/// older packages shipped, so both resolve without hardcoding one.
fn worker_sibling_names(controller: &Path) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut names = Vec::new();
    if let Some(own) = running_executable_file_name(controller) {
        names.push(own);
    }
    let legacy = OsString::from("hel");
    if !names.contains(&legacy) {
        names.push(legacy);
    }
    names
}

/// A local-bare session runs on the controller host, so it may use the native
/// worker built or packaged beside `mj`. Managed targets never consider this
/// name because a macOS or glibc binary is not portable into Linux targets.
fn select_native_worker(
    controller: &Path,
    is_file: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, &'static str)> {
    let directory = controller.parent()?;
    if let (Some(profile), Some(target_dir)) = (directory.file_name(), directory.parent()) {
        let development_worker = target_dir.join("worker").join(profile).join("mj-worker");
        if is_file(&development_worker) {
            return Some((development_worker, "isolated native development worker"));
        }
    }
    let packaged_worker = directory.join("mj-worker");
    is_file(&packaged_worker).then_some((packaged_worker, "native worker beside mj"))
}

/// Choose a worker binary that ships beside the controller or in a development
/// musl sibling directory. `is_file` probes the filesystem; tests pass a
/// hand-written probe. The static musl sibling is probed before the worker in
/// the controller's own directory, because in a development checkout that
/// same-directory candidate resolves to the controller itself, whose glibc may
/// be newer than the target's.
fn select_sibling_worker(
    controller: &Path,
    triple: &str,
    is_file: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, &'static str)> {
    let directory = controller.parent()?;
    let names = worker_sibling_names(controller);
    let mut candidates: Vec<(PathBuf, &'static str)> = Vec::new();
    // Packaged worker beside the controller, named for the target triple.
    candidates.push((
        packaged_worker_binary_path(directory, triple),
        "beside the mj binary",
    ));
    // Development checkout: a controller at target/<profile>/<name> finds its
    // musl sibling at target/<triple>/<profile>/<name>. The static build is
    // preferred because the target's glibc may be older than the host's, so it
    // is probed before the same-directory worker (which is the controller
    // itself in a development checkout).
    if let (Some(profile), Some(target_dir)) = (directory.file_name(), directory.parent()) {
        candidates.push((
            target_dir
                .join("worker")
                .join(triple)
                .join(profile)
                .join("mj-worker"),
            "isolated development musl worker",
        ));
        candidates.push((
            target_dir.join(triple).join(profile).join("mj-worker"),
            "development musl worker",
        ));
        for name in &names {
            candidates.push((
                target_dir.join(triple).join(profile).join(name),
                "development musl sibling",
            ));
        }
    }
    // A legacy package may put an `hel`-named worker beside an `mj`
    // controller. Never select the controller's own same-directory path: on
    // glibc Linux that is not a portable worker, and after an upgrade it is
    // the replacement controller rather than the still-running executable.
    let controller_name = running_executable_file_name(controller);
    for name in names
        .iter()
        .filter(|name| Some(name.as_os_str()) != controller_name.as_deref())
    {
        candidates.push((directory.join(name), "beside the running executable"));
    }
    candidates.into_iter().find(|(path, _)| is_file(path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WorkerBinaryRequirement {
    PortableLinux,
    LocalHost,
}

impl WorkerBinarySourceSnapshot {
    fn capture<F>(cache_root: &Path, resolve: F) -> Self
    where
        F: Fn(&str, WorkerBinaryRequirement) -> Result<WorkerBinaryAvailability>,
    {
        let mut entries = HashMap::new();
        let mut local_cache = HashMap::<PathBuf, PathBuf>::new();
        let architectures = [
            (std::env::consts::ARCH, WorkerBinaryRequirement::LocalHost),
            ("x86_64", WorkerBinaryRequirement::PortableLinux),
            ("aarch64", WorkerBinaryRequirement::PortableLinux),
        ];

        for (arch, requirement) in architectures {
            let pinned = match resolve(arch, requirement) {
                Ok(WorkerBinaryAvailability::Local { path, source }) => {
                    match local_cache.get(&path).cloned().map(Ok).unwrap_or_else(|| {
                        copy_worker_source_to_cache(&path, cache_root).inspect(|cached| {
                            local_cache.insert(path.clone(), cached.clone());
                        })
                    }) {
                        Ok(cached) => Ok(WorkerBinaryAvailability::Local {
                            path: cached,
                            source,
                        }),
                        Err(error) => {
                            let error = format!(
                                "pin worker source {} for {arch} ({requirement:?}): {error:#}",
                                path.display()
                            );
                            tracing::warn!(arch, requirement = ?requirement, error = %error);
                            Err(error)
                        }
                    }
                }
                Ok(WorkerBinaryAvailability::Remote {
                    url,
                    sha256,
                    triple,
                }) => Ok(WorkerBinaryAvailability::Remote {
                    url,
                    sha256,
                    triple,
                }),
                Err(error) => {
                    let error = format!("{error:#}");
                    tracing::debug!(
                        arch,
                        requirement = ?requirement,
                        error = %error,
                        "worker source was unavailable when the daemon started"
                    );
                    Err(error)
                }
            };
            entries.insert((arch.to_owned(), requirement), pinned);
        }

        Self { entries }
    }

    fn resolve(
        &self,
        arch: &str,
        requirement: WorkerBinaryRequirement,
    ) -> Result<WorkerBinaryAvailability> {
        let Some(source) = self.entries.get(&(arch.to_owned(), requirement)) else {
            bail!(
                "worker source for {arch} ({requirement:?}) was not captured when the daemon started"
            );
        };
        match source {
            Ok(availability) => Ok(availability.clone()),
            Err(error) => bail!(
                "worker source for {arch} ({requirement:?}) was unavailable when the daemon started; install it and restart the daemon to retry: {error}"
            ),
        }
    }
}

/// Capture the worker sources used by this daemon before its asynchronous
/// managers start. Missing sources are retained as per-architecture errors so
/// an unused architecture does not prevent daemon startup.
pub fn pin_worker_binary_sources() -> Result<()> {
    if PINNED_WORKER_BINARY_SOURCES.get().is_some() {
        return Ok(());
    }
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    let cache_root = data_dir().join("workers").join("pinned");
    let started = std::time::Instant::now();
    let snapshot = WorkerBinarySourceSnapshot::capture(&cache_root, |arch, requirement| {
        worker_binary_prerequisite_for_current(arch, requirement, &current, &|path| path.is_file())
    });
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "worker sources pinned"
    );
    // The daemon boot path calls this once. If a second caller races it, keep
    // the first complete snapshot and never replace paths it may already use.
    let _ = PINNED_WORKER_BINARY_SOURCES.set(snapshot);
    Ok(())
}

fn copy_worker_source_to_cache(source: &Path, cache_root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("create pinned worker cache {}", cache_root.display()))?;
    let mut input =
        File::open(source).with_context(|| format!("open worker source {}", source.display()))?;
    let metadata = input
        .metadata()
        .with_context(|| format!("stat worker source {}", source.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(cache_root)
        .with_context(|| format!("create pinned worker staging file {}", cache_root.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("read worker source {}", source.display()))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .with_context(|| format!("copy worker source {}", source.display()))?;
        digest.update(&buffer[..count]);
    }
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("flush pinned worker source {}", source.display()))?;
    std::fs::set_permissions(temporary.path(), metadata.permissions())
        .with_context(|| format!("preserve permissions for {}", source.display()))?;
    let digest = format!("{:x}", digest.finalize());
    publish_cached_worker(temporary, cache_root, &digest)
}

/// Publish one immutable cache artifact. persist_noclobber makes the final
/// publication atomic and never replaces an artifact another daemon may have
/// already captured.
fn publish_cached_worker(
    temporary: tempfile::NamedTempFile,
    cache_root: &Path,
    digest: &str,
) -> Result<PathBuf> {
    let directory = cache_root.join(digest);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create pinned worker cache {}", directory.display()))?;
    let destination = directory.join("hel");
    if destination.is_file() {
        return Ok(destination);
    }
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {
            #[cfg(unix)]
            File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("flush pinned worker cache {}", directory.display()))?;
            Ok(destination)
        }
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            if destination.is_file() {
                Ok(destination)
            } else {
                Err(error.error).with_context(|| {
                    format!("publish pinned worker artifact {}", destination.display())
                })
            }
        }
        Err(error) => Err(error.error)
            .with_context(|| format!("publish pinned worker artifact {}", destination.display())),
    }
}

/// Find a worker source without downloading it.
///
/// Container provisioning resolves this after discovering the target
/// architecture. Doctor uses the same lookup with the selected container's
/// expected architecture, so it can recommend a fix without creating a
/// container or making a network request.
pub fn worker_binary_prerequisite_for_arch(arch: &str) -> Result<WorkerBinaryAvailability> {
    worker_binary_for_arch(arch, WorkerBinaryRequirement::PortableLinux)
}

fn worker_binary_for_arch(
    arch: &str,
    requirement: WorkerBinaryRequirement,
) -> Result<WorkerBinaryAvailability> {
    if let Some(snapshot) = PINNED_WORKER_BINARY_SOURCES.get() {
        return snapshot.resolve(arch, requirement);
    }
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    worker_binary_prerequisite_for_current(arch, requirement, &current, &|path| path.is_file())
}

/// The lookup itself, with the controller's own path and the file probe passed
/// in so both can be exercised without the machine they describe.
fn worker_binary_prerequisite_for_current(
    arch: &str,
    requirement: WorkerBinaryRequirement,
    current: &Path,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<WorkerBinaryAvailability> {
    let triple = format!("{arch}-unknown-linux-musl");
    if let Some(path) = mj_core::config::env_override_os("WORKER_BINARY").map(PathBuf::from) {
        if !is_file(&path) {
            bail!("MJ_WORKER_BINARY is not a file: {}", path.display());
        }
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: "MJ_WORKER_BINARY".into(),
        });
    }
    // A rebuilt or renamed checkout leaves a running controller pointing at a
    // path that no longer holds a binary. Every lookup derived from that path
    // is meaningless, so remember the fact and skip those lookups.
    let controller_replaced = !is_file(current);
    let mut candidates = Vec::new();
    if let Some(directory) = mj_core::config::env_override_os("WORKER_DIR").map(PathBuf::from) {
        candidates.push((
            packaged_worker_binary_path(&directory, &triple),
            "MJ_WORKER_DIR",
        ));
        candidates.push((directory.join(&triple).join("hel"), "MJ_WORKER_DIR"));
    }
    if let Some((path, source)) = candidates.into_iter().find(|(path, _)| is_file(path)) {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if requirement == WorkerBinaryRequirement::LocalHost
        && let Some((path, source)) = select_native_worker(current, is_file)
    {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if !controller_replaced
        && let Some((path, source)) = select_sibling_worker(current, &triple, is_file)
    {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if let Some(template) = mj_core::config::env_override("WORKER_URL") {
        let expected = mj_core::config::env_override("WORKER_SHA256")
            .context("MJ_WORKER_URL requires MJ_WORKER_SHA256")?;
        validate_worker_sha256(&expected)?;
        return Ok(WorkerBinaryAvailability::Remote {
            url: template.replace("{target}", &triple),
            sha256: expected,
            triple,
        });
    }
    // Telling someone to install a worker beside a binary that is no longer
    // there sends them looking in the wrong place.
    ensure!(
        !controller_replaced,
        "the running mj binary was replaced or removed on disk ({}); restart the Mjolnir daemon so it runs the current build, then retry",
        display_path(current)
    );
    bail!(
        "no Linux worker for {triple}; install mj-worker-{triple} beside mj, set MJ_WORKER_DIR/MJ_WORKER_BINARY, or configure MJ_WORKER_URL and MJ_WORKER_SHA256"
    )
}

/// Linux appends " (deleted)" to `/proc/<pid>/exe` for a removed image. That
/// marker belongs in a message but never in a decision, which `is_file` makes.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned()
}

/// The architecture a configured template names outright, if it names one. A
/// container `platform` such as `linux/arm64` decides what the target runs
/// whatever the controller's own machine is, and it is the only architecture a
/// configured target template can state: the configured `AwsEc2` variant names
/// a launch template, whose instance type is only discoverable through the AWS
/// API.
fn template_architecture(template: &mj_core::config::TargetTemplate) -> Option<&'static str> {
    use mj_core::config::TargetTemplate as Template;
    let platform = match template {
        Template::LocalPodman { container }
        | Template::LocalDocker { container }
        | Template::AppleContainer { container }
        | Template::SshPodman { container, .. }
        | Template::SshDocker { container, .. } => container.platform.as_deref()?,
        Template::LocalBare | Template::SshBare { .. } | Template::AwsEc2 { .. } => return None,
    };
    // Platform strings appear as "linux/arm64", "arm64", or "linux/arm64/v8".
    platform.split('/').find_map(|part| match part.trim() {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    })
}

/// Architectures a resume must be able to serve, knowing only the configured
/// template. Provisioning learns the real answer by running `uname -m` on the
/// live target; a resume has no target yet, so this uses what is knowable
/// without one: an architecture the template names, else the controller's own
/// architecture for a target that runs on this machine, else either Linux
/// architecture for a remote target.
fn preflight_architectures(template: &mj_core::config::TargetTemplate) -> Vec<&'static str> {
    use mj_core::config::TargetTemplate as Template;
    if let Some(arch) = template_architecture(template) {
        return vec![arch];
    }
    match template {
        Template::LocalBare
        | Template::LocalPodman { .. }
        | Template::LocalDocker { .. }
        | Template::AppleContainer { .. } => vec![std::env::consts::ARCH],
        Template::SshBare { .. }
        | Template::SshPodman { .. }
        | Template::SshDocker { .. }
        | Template::AwsEc2 { .. } => {
            vec!["x86_64", "aarch64"]
        }
    }
}

/// Whether this controller could produce a Linux worker binary for a target
/// that does not exist yet.
///
/// A resume compacts a cross-harness transcript before it provisions anything,
/// which costs minutes and paid model requests. Resolving the worker binary is
/// local and takes microseconds, so a resume that could never install a worker
/// must fail before spending any of that. This downloads nothing: a remote
/// source counts as available, because fetching it belongs to provisioning.
pub(super) fn preflight_worker_binary(template: &mj_core::config::TargetTemplate) -> Result<()> {
    // Only a bare local target may run the controller's own host binary as
    // its worker; every other target needs a portable Linux worker.
    let requirement = if matches!(template, mj_core::config::TargetTemplate::LocalBare) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    let mut failure = None;
    for arch in preflight_architectures(template) {
        match worker_binary_for_arch(arch, requirement) {
            Ok(_) => return Ok(()),
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        // The message is the one provisioning would have printed later, so the
        // user reads the same fix, sooner.
        Some(error) => Err(error).context("preflight the worker binary before resuming"),
        None => Ok(()),
    }
}

pub(super) fn worker_binary_for(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let arch = target_architecture(locator, executor)?;
    let requirement = if matches!(locator, targets::TargetLocator::LocalBare { .. }) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    match worker_binary_for_arch(arch, requirement)? {
        WorkerBinaryAvailability::Local { path, .. } => Ok(path),
        WorkerBinaryAvailability::Remote {
            url,
            sha256,
            triple,
        } => download_worker(&url, &sha256, &triple),
    }
}

fn target_architecture(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<&'static str> {
    let command = targets::locator_command(locator, vec!["uname".into(), "-m".into()])
        .purpose("detect target architecture");
    let output = execute_checked(executor, command)?;
    match String::from_utf8(output.stdout)?.trim() {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        architecture => bail!("unsupported target architecture {architecture:?}"),
    }
}

fn download_worker(url: &str, expected_sha256: &str, triple: &str) -> Result<PathBuf> {
    validate_worker_sha256(expected_sha256)?;
    let digest = expected_sha256.to_ascii_lowercase();
    let directory = data_dir().join("workers").join("pinned");
    let destination = directory.join(&digest).join("hel");
    std::fs::create_dir_all(destination.parent().unwrap_or(&directory))?;
    if destination.is_file() {
        let bytes = std::fs::read(&destination).with_context(|| {
            format!(
                "read cached worker for {triple} from {}",
                destination.display()
            )
        })?;
        if format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(expected_sha256) {
            return Ok(destination);
        }
        bail!(
            "content-addressed worker cache {} does not match {} checksum",
            destination.display(),
            expected_sha256
        );
    }
    let bytes = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(url)
        .send()?
        .error_for_status()?
        .bytes()?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        bail!("downloaded worker checksum mismatch: expected {expected_sha256}, got {actual}");
    }
    std::fs::create_dir_all(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    std::io::Write::write_all(&mut temporary, &bytes)?;
    temporary.as_file_mut().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    publish_cached_worker(temporary, &directory, &digest)
}

fn validate_worker_sha256(expected_sha256: &str) -> Result<()> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("MJ_WORKER_SHA256 must be a 64-character hexadecimal digest");
    }
    Ok(())
}

fn workspace_paths(
    locator: &targets::TargetLocator,
    bundle: &ProjectBundle,
    session_id: &str,
) -> Result<(String, Vec<String>)> {
    let root = match locator {
        targets::TargetLocator::LocalBare { .. } => {
            bail!("local bare projects use their selected directory")
        }
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => "/workspace".to_string(),
        targets::TargetLocator::AwsEc2 { workspace, .. }
        | targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
    };
    if matches!(locator, targets::TargetLocator::AwsEc2 { .. }) {
        let expected = format!(".local/share/hel/workspaces/{session_id}");
        if root != expected {
            bail!("AWS workspace does not match session")
        }
    }
    let primary = bundle.primary().context("bundle primary is missing")?;
    let primary_path = format!("{root}/{}", primary.destination.to_string_lossy());
    let additional = bundle
        .repositories
        .iter()
        .filter(|repository| repository.id != bundle.primary_repo)
        .map(|repository| format!("{root}/{}", repository.destination.to_string_lossy()))
        .collect();
    Ok((primary_path, additional))
}

// Package versions for ACP bridges and their harnesses. Keep these in lockstep with the global
// npm installs in containers/Containerfile.agent-dev; bridge_pins_match_containerfile() below
// fails the build when they drift.
// Codex 0.148 reuses pending MCP startups during runtime reconciliation. Older
// releases could cancel the first project-memory startup while immediately
// replacing it with an equivalent connection, leaving a false failed-tool
// event at the beginning of every session.
/// Stage shown after the worker is reachable and while its ACP bridge becomes
/// ready. Every remaining harness launches through a default launcher that can
/// fetch it, so the stage names the harness being installed.
pub(super) fn bridge_readiness_stage(profile: &HarnessProfile) -> ProvisionStage {
    ProvisionStage::Installing(profile.kind)
}

pub(super) fn bridge_launch(
    harness: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
) -> (String, Vec<String>) {
    match harness {
        mj_core::config::HarnessKind::Muse => ("muse-acp".into(), Vec::new()),
        mj_core::config::HarnessKind::Codex => (
            "sh".into(),
            vec![
                "-c".into(),
                format!("if command -v codex-acp >/dev/null 2>&1 && [ \"$(codex-acp --version 2>/dev/null)\" = \"{CODEX_ACP_PACKAGE} {CODEX_ACP_VERSION}\" ]; then exec codex-acp; fi; {}; exec npx -y {CODEX_ACP_PACKAGE}@{CODEX_ACP_VERSION}", ensure_node_script()),
            ],
        ),
        mj_core::config::HarnessKind::Claude => (
            "sh".into(),
            vec![
                "-c".into(),
                format!("if command -v claude-agent-acp >/dev/null 2>&1; then exec claude-agent-acp; fi; {}; exec npx -y @agentclientprotocol/claude-agent-acp@{CLAUDE_ACP_VERSION}", ensure_node_script()),
            ],
        ),
        mj_core::config::HarnessKind::Kimi => (
            "sh".into(),
            vec![
                "-c".into(),
                "if command -v kimi >/dev/null 2>&1; then exec kimi acp; elif [ -x \"$HOME/.kimi-code/bin/kimi\" ]; then exec \"$HOME/.kimi-code/bin/kimi\" acp; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash && exec \"$HOME/.kimi-code/bin/kimi\" acp; else echo 'Mjolnir needs compatible Kimi Code or curl for its official installer; add the tool to PATH' >&2; exit 127; fi".into(),
            ],
        ),
        mj_core::config::HarnessKind::Grok => {
            let acp = mj_core::config::HarnessKind::Grok
                .bridge_args(policy)
                .join(" ");
            (
                "sh".into(),
                vec![
                    "-c".into(),
                    format!(
                        "if command -v grok >/dev/null 2>&1; then exec grok {acp}; elif [ -x \"$GROK_HOME/bin/grok\" ]; then exec \"$GROK_HOME/bin/grok\" {acp}; elif [ -x \"$HOME/.grok/bin/grok\" ]; then exec \"$HOME/.grok/bin/grok\" {acp}; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://x.ai/cli/install.sh | bash && exec \"$HOME/.grok/bin/grok\" {acp}; else echo 'Mjolnir needs compatible Grok Build or curl for its official installer; add the tool to PATH' >&2; exit 127; fi"
                    ),
                ],
            )
        }
    }
}

pub(super) fn preflight_harness(
    template: &mj_core::config::TargetTemplate,
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> Result<()> {
    use mj_core::config::TargetTemplate;
    if !matches!(profile.kind, HarnessKind::Codex | HarnessKind::Claude) {
        return Ok(());
    }
    if !matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    ) {
        return Ok(());
    }
    let script = "if ! command -v node >/dev/null 2>&1; then echo 'Node.js is missing from PATH; install Node.js 22 or newer in the target environment' >&2; exit 127; fi; if ! node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)'; then echo 'Node.js 22 or newer is required in the target environment' >&2; exit 1; fi; if ! command -v npm >/dev/null 2>&1 || ! npm --version >/dev/null; then echo 'npm is missing or unusable; install npm in the target environment' >&2; exit 127; fi";
    let mut args = if profile.environment.contains_key("PATH") {
        vec![
            "-c".to_owned(),
            format!("export PATH=\"$1\"; {script}"),
            "mj-node-preflight".into(),
            profile.environment["PATH"].clone(),
        ]
    } else {
        vec!["-lc".to_owned(), script.to_owned()]
    };
    let (command, destination) = match template {
        TargetTemplate::LocalBare => (CommandSpec::new("sh", args), "local host".to_owned()),
        TargetTemplate::SshBare { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            args.insert(0, "sh".into());
            (crate::targets::ssh_command(&ssh, args), ssh.destination)
        }
        _ => unreachable!(),
    };
    execute_checked(executor, command.purpose("preflight managed harness Node.js and npm"))
        .with_context(|| format!("{} launch preflight failed on {destination}; Node.js 22+ and npm must be available on the target PATH", profile.kind.display_name()))?;
    Ok(())
}

fn ensure_node_script() -> &'static str {
    "if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v npx >/dev/null 2>&1; then echo 'Mjolnir needs Node.js, npm, and npx on PATH; install Node in the target environment' >&2; exit 127; fi"
}

const MJ_CONTAINER_ENVIRONMENT: &str = "## Mjolnir disposable environment\n\nThis session runs in a disposable Mjolnir container. When the session closes, Mjolnir checkpoints everything in project workspace directories under `/workspace`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then removes the container.\n\nEverything outside `/workspace`, including installed packages, `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n";

pub(super) fn stage_profile(
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
) -> Result<()> {
    let harness = profile.kind;
    let source = profile.home.as_path();
    std::fs::create_dir_all(destination)?;
    let allowlist: &[&str] = match harness {
        mj_core::config::HarnessKind::Muse => &[
            "auth.json",
            "settings.json",
            "trust.json",
            "AGENTS.md",
            "skills",
            "rules",
        ],
        mj_core::config::HarnessKind::Codex => &[
            "auth.json",
            "config.toml",
            "AGENTS.md",
            "instructions.md",
            "rules",
            "skills",
        ],
        mj_core::config::HarnessKind::Claude => &[
            ".claude.json",
            ".credentials.json",
            "settings.json",
            "CLAUDE.md",
            "skills",
            "plugins",
        ],
        mj_core::config::HarnessKind::Kimi => &[
            "credentials",
            "config.toml",
            "device_id",
            "AGENTS.md",
            "SYSTEM.md",
            "mcp.json",
            "skills",
            "agents",
            "plugins",
        ],
        mj_core::config::HarnessKind::Grok => &[
            "auth.json",
            "config.toml",
            "AGENTS.md",
            "agent_id",
            "skills",
            "plugins",
        ],
    };
    // Allowlist entries (and, within each, a copied directory's children) are
    // independent of one another, so copying them concurrently shortens the
    // stage step for profiles with large skills/plugins trees.
    allowlist.par_iter().try_for_each(|name| -> Result<()> {
        let from = source.join(name);
        if from.exists() {
            copy_profile_entry(&from, &destination.join(name))?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Fetches a provider's model catalog. A function parameter so tests can supply
/// a body without reaching the network. Takes the catalog URL and the API key;
/// returns the raw response body.
pub(super) type CatalogFetch<'a> = &'a dyn Fn(&str, &str) -> Result<Vec<u8>>;

/// The catalog file Mjolnir writes into a staged Codex home, and the key it
/// points `config.toml` at. Relative to `CODEX_HOME`, so the staged copy works
/// unchanged on any target.
const STAGED_CATALOG_FILE: &str = "models.json";

/// Where a fetched catalog is remembered so a provider outage cannot block a
/// launch. The live store is one implementation; a test can supply another.
pub(super) trait CatalogCache {
    fn load(&self, profile_id: &str, fingerprint: &str) -> Option<String>;
    fn store(&self, profile_id: &str, fingerprint: &str, body: &str);
}

/// The catalog cache backed by Mjolnir's own `profile_config_cache` table.
pub(super) struct SharedCatalogCache;

impl CatalogCache for SharedCatalogCache {
    fn load(&self, profile_id: &str, fingerprint: &str) -> Option<String> {
        crate::database::load_profile_config_cache(profile_id, "", fingerprint)
            .ok()
            .flatten()
    }

    fn store(&self, profile_id: &str, fingerprint: &str, body: &str) {
        if let Err(error) = crate::database::save_profile_config_cache(
            profile_id.to_owned(),
            String::new(),
            fingerprint.to_owned(),
            body.to_owned(),
        ) {
            tracing::warn!(profile_id, "could not cache the model catalog: {error:#}");
        }
    }
}

/// Give a staged Codex home the model catalog its provider advertises.
///
/// Codex fetches its model list from its own service only for ChatGPT logins.
/// Without a catalog file a profile pointed at another provider would offer
/// OpenAI's built-in model names and send them to that provider, so Mjolnir
/// fetches the provider's own catalog, stamps the Guardian reviewer on every
/// entry, writes it beside the staged `config.toml`, and points the staged
/// configuration at it.
///
/// Profiles with no custom provider are left alone. A failed fetch falls back to
/// the last catalog stored for this profile, so a provider outage does not block
/// a launch; with neither, the launch fails naming the profile and the URL.
pub(super) fn stage_codex_catalog(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
    fetch: CatalogFetch<'_>,
    cache: &dyn CatalogCache,
) -> Result<()> {
    let Some(provider) = profile.codex_provider()? else {
        return Ok(());
    };
    let Some(env_key) = provider.env_key.as_deref() else {
        // An inline `experimental_bearer_token` provider carries its key in the
        // staged file itself; Mjolnir has no key of its own to authorize a
        // catalog fetch with.
        return Ok(());
    };
    let api_key = profile.environment.get(env_key).with_context(|| {
        format!("profile {profile_id:?} has no {env_key} entry to read its model catalog with")
    })?;
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let fingerprint = format!("catalog:{}", provider.base_url);
    let body = match fetch(&url, api_key) {
        Ok(body) => {
            if let Ok(text) = std::str::from_utf8(&body) {
                cache.store(profile_id, &fingerprint, text);
            }
            body
        }
        Err(error) => match cache.load(profile_id, &fingerprint) {
            Some(body) => {
                tracing::warn!(
                    profile_id,
                    provider = %provider.id,
                    "could not fetch the model catalog from {url}, using the last cached copy: {error:#}"
                );
                body.into_bytes()
            }
            None => bail!(
                "profile {profile_id:?}: could not fetch the model catalog from {url} and no cached copy is available: {error:#}"
            ),
        },
    };
    let mut catalog = mj_core::codex_catalog::parse(&body)
        .with_context(|| format!("profile {profile_id:?}: model catalog from {url}"))?;
    apply_catalog_overrides(profile_id, &profile.home, &mut catalog)?;
    stamp_guardian_reviewer(profile_id, profile, &mut catalog)?;
    std::fs::create_dir_all(destination)?;
    std::fs::write(destination.join(STAGED_CATALOG_FILE), catalog.to_json())?;
    point_config_at_catalog(&destination.join("config.toml"))
}

/// Refine the fetched catalog with the user's own `models.json`, when the
/// profile home has one.
///
/// A provider that serves OpenAI's plain model list gives Mjolnir only model
/// ids, so the translated entries carry conservative defaults. The override
/// file is how a user states what that provider actually supports: each entry
/// is matched by `slug` and its fields are copied over the fetched entry, and a
/// slug the provider did not list is added. Mjolnir writes the merged result
/// over the staged `models.json`, so the user's own file never reaches Codex
/// unmerged.
fn apply_catalog_overrides(
    profile_id: &str,
    home: &Path,
    catalog: &mut mj_core::codex_catalog::CodexCatalog,
) -> Result<()> {
    let path = home.join(STAGED_CATALOG_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    let overrides = mj_core::codex_catalog::parse_codex_shape(&bytes).with_context(|| {
        format!(
            "profile {profile_id:?}: model catalog overrides in {}",
            path.display()
        )
    })?;
    mj_core::codex_catalog::merge_overrides(catalog, &overrides);
    Ok(())
}

/// Record the Guardian reviewer choice on every catalog entry.
///
/// Codex reads the reviewer from the session model's own catalog entry, so the
/// override is stamped on all of them. The profile setting decides which model
/// that is: the newest flash model by default, the session model itself when
/// the setting is `session` (nothing is stamped, which is Codex's own
/// fallback), or a named slug. A named slug the catalog does not list fails the
/// launch, because stamping it would leave Codex silently reviewing with
/// something else.
fn stamp_guardian_reviewer(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    catalog: &mut mj_core::codex_catalog::CodexCatalog,
) -> Result<()> {
    let setting = profile
        .guardian_review_model
        .as_deref()
        .unwrap_or(mj_core::config::GUARDIAN_REVIEW_NEWEST_FLASH);
    if setting == mj_core::config::GUARDIAN_REVIEW_SESSION {
        tracing::info!(
            profile_id,
            "guardian_review_model is \"session\"; Guardian reviews run on the session model"
        );
        return Ok(());
    }
    if setting == mj_core::config::GUARDIAN_REVIEW_NEWEST_FLASH {
        match mj_core::codex_catalog::guardian_review_model(catalog.slugs()) {
            Some(reviewer) => mj_core::codex_catalog::stamp_reviewer(catalog, &reviewer),
            // With no small model to review with, Codex falls back to reviewing
            // with the session model, which still runs Guardian.
            None => tracing::info!(
                profile_id,
                "the model catalog lists no flash model; Guardian reviews run on the session model"
            ),
        }
        return Ok(());
    }
    let slugs = catalog.slugs();
    if !slugs.iter().any(|slug| slug == setting) {
        bail!(
            "profile {profile_id:?}: guardian_review_model {setting:?} is not in the provider's model catalog, which lists {}",
            slugs.join(", ")
        );
    }
    mj_core::codex_catalog::stamp_reviewer(catalog, setting);
    Ok(())
}

/// Prepend `model_catalog_json` to a staged Codex `config.toml`.
///
/// The key is top-level in Codex's configuration, and TOML puts every top-level
/// key before the first table header, so the line goes at the front. Appending
/// would make it a key of whichever table happens to come last, which Codex
/// ignores. Profile validation guarantees the user wrote no such key.
fn point_config_at_catalog(path: &Path) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    std::fs::write(
        path,
        format!("model_catalog_json = \"{STAGED_CATALOG_FILE}\"\n{existing}"),
    )
    .with_context(|| format!("point {} at the staged model catalog", path.display()))
}

/// Fetch a provider's catalog over HTTPS. Mirrors the bounded client the Coding
/// Plan quota reader uses: a short timeout and no redirects.
pub(super) fn fetch_catalog_over_https(url: &str, api_key: &str) -> Result<Vec<u8>> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(url)
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()?
        .error_for_status()?;
    Ok(response.bytes()?.to_vec())
}

/// Add lifecycle guidance only for targets that Hel destroys as a whole.
fn append_hel_target_environment(
    harness: mj_core::config::HarnessKind,
    destination: &Path,
    target: &targets::TargetLocator,
) -> Result<()> {
    let environment = match target {
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => MJ_CONTAINER_ENVIRONMENT.to_owned(),
        targets::TargetLocator::AwsEc2 { workspace, .. } => format!(
            "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/{workspace}`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/{workspace}`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"
        ),
        targets::TargetLocator::LocalBare { .. } | targets::TargetLocator::SshBare { .. } => {
            return Ok(());
        }
    };
    let instructions = match harness {
        mj_core::config::HarnessKind::Codex => "AGENTS.md",
        mj_core::config::HarnessKind::Claude => "CLAUDE.md",
        mj_core::config::HarnessKind::Kimi => "AGENTS.md",
        mj_core::config::HarnessKind::Grok => "AGENTS.md",
        mj_core::config::HarnessKind::Muse => "AGENTS.md",
    };
    let path = destination.join(instructions);
    let separator = match std::fs::read_to_string(&path) {
        Ok(contents) if !contents.is_empty() && !contents.ends_with('\n') => "\n\n",
        Ok(contents) if !contents.is_empty() => "\n",
        Ok(_) => "",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "",
        Err(error) => return Err(error.into()),
    };
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open staged harness instructions {}", path.display()))?;
    file.write_all(separator.as_bytes())?;
    file.write_all(environment.as_bytes())?;
    Ok(())
}

fn copy_profile_entry(source: &Path, destination: &Path) -> Result<()> {
    copy_profile_entry_within(source, destination, &HashSet::new())
}

/// Copy one profile entry, following symlinks so a profile home that links its
/// settings or instructions elsewhere still stages their contents. `entered`
/// holds the canonical paths of the directories already entered on this branch
/// of the recursion, which stops a symlinked directory cycle.
fn copy_profile_entry_within(
    source: &Path,
    destination: &Path,
    entered: &HashSet<PathBuf>,
) -> Result<()> {
    std::fs::symlink_metadata(source)
        .with_context(|| format!("read staged profile entry metadata {}", source.display()))?;
    let metadata = match std::fs::metadata(source) {
        Ok(metadata) => metadata,
        // The entry exists but its link target does not; staging the rest of
        // the profile is more useful than failing on a stale link.
        Err(error) if error.kind() == ErrorKind::NotFound => {
            tracing::warn!(
                source = %source.display(),
                "skipping staged profile entry whose symlink target is missing"
            );
            return Ok(());
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "read staged profile entry metadata {}",
                source.display()
            )));
        }
    };
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create staged profile directory {}", parent.display()))?;
        }
        std::fs::copy(source, destination).with_context(|| {
            format!(
                "copy staged profile file {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }
    if metadata.is_dir() {
        let canonical = std::fs::canonicalize(source)
            .with_context(|| format!("resolve staged profile directory {}", source.display()))?;
        if entered.contains(&canonical) {
            tracing::warn!(
                source = %source.display(),
                target = %canonical.display(),
                "skipping staged profile directory that links back into itself"
            );
            return Ok(());
        }
        let mut entered = entered.clone();
        entered.insert(canonical);
        std::fs::create_dir_all(destination).with_context(|| {
            format!("create staged profile directory {}", destination.display())
        })?;
        let entries = std::fs::read_dir(source)
            .with_context(|| format!("list staged profile directory {}", source.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| {
                format!(
                    "read staged profile directory entries in {}",
                    source.display()
                )
            })?;
        // Sibling entries in one directory are independent, so recurse in
        // parallel; this is the level most likely to hold many files (e.g. a
        // skills or plugins tree).
        entries.par_iter().try_for_each(|entry| {
            copy_profile_entry_within(
                &entry.path(),
                &destination.join(entry.file_name()),
                &entered,
            )
        })?;
        std::fs::set_permissions(destination, metadata.permissions()).with_context(|| {
            format!(
                "set permissions for staged profile directory {}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

// Container copies can create root-owned files even when exec defaults to a
// non-root image user. The worker directory was created by that user, so use
// its ownership for uploaded files before restricting their permissions.
pub(super) fn container_upload_ownership_args(
    container_id: &str,
    worker_root: &str,
    paths: &[&str],
) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "--user".into(),
        "0".into(),
        container_id.into(),
        "sh".into(),
        "-c".into(),
        // GNU and BusyBox stat both support this numeric ownership format.
        r#"set -eu; owner=$(stat -c '%u:%g' -- "$1"); shift; chown -R "$owner" -- "$@""#.into(),
        "sh".into(),
        worker_root.into(),
    ];
    args.extend(paths.iter().map(|path| (*path).to_owned()));
    args
}

#[allow(clippy::too_many_arguments)]
fn install_worker_files(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
    profile_home: &str,
    worker_binary: &Path,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
) -> Result<()> {
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            if profile_stage.is_dir() {
                std::fs::create_dir_all(profile_home).context("create isolated local profile")?;
                for entry in std::fs::read_dir(profile_stage)? {
                    let entry = entry?;
                    copy_profile_entry(
                        &entry.path(),
                        &Path::new(profile_home).join(entry.file_name()),
                    )?;
                }
            }
            for command in [
                CommandSpec::new("mkdir", ["-p", worker_root])
                    .purpose("create local bare worker directory"),
                CommandSpec::new(
                    "cp",
                    [
                        worker_binary.to_string_lossy().into_owned(),
                        format!("{worker_root}/hel"),
                    ],
                )
                .purpose("install local Mjolnir worker"),
                CommandSpec::new(
                    "cp",
                    [
                        launch_config.to_string_lossy().into_owned(),
                        format!("{worker_root}/launch.json"),
                    ],
                )
                .purpose("install local worker launch configuration"),
                CommandSpec::new(
                    "cp",
                    [
                        ownership.to_string_lossy().into_owned(),
                        format!("{worker_root}/ownership.json"),
                    ],
                )
                .purpose("install local worker ownership marker"),
                CommandSpec::new("chmod", ["700", &format!("{worker_root}/hel")])
                    .purpose("make local Mjolnir worker executable"),
            ] {
                execute_checked(executor, command)?;
            }
        }
        targets::TargetLocator::LocalPodman { container_id, .. }
        | targets::TargetLocator::LocalDocker { container_id }
        | targets::TargetLocator::AppleContainer { container_id } => {
            let engine = match locator {
                targets::TargetLocator::LocalPodman { .. } => "podman",
                targets::TargetLocator::LocalDocker { .. } => "docker",
                targets::TargetLocator::AppleContainer { .. } => "container",
                _ => unreachable!("matched local container target"),
            };
            for command in [
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "mkdir".into(),
                        "-p".into(),
                        worker_root.into(),
                        profile_home.into(),
                    ],
                )
                .purpose("create target worker directories"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        worker_binary.to_string_lossy().into_owned(),
                        format!("{container_id}:{worker_root}/hel"),
                    ],
                )
                .purpose("upload Mjolnir worker"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        launch_config.to_string_lossy().into_owned(),
                        format!("{container_id}:{worker_root}/launch.json"),
                    ],
                )
                .purpose("upload worker launch configuration"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        ownership.to_string_lossy().into_owned(),
                        format!("{container_id}:{worker_root}/ownership.json"),
                    ],
                )
                .purpose("upload worker ownership marker"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        format!("{}/.", profile_stage.display()),
                        format!("{container_id}:{profile_home}"),
                    ],
                )
                .purpose("upload harness profile allowlist"),
                CommandSpec::new(
                    engine,
                    container_upload_ownership_args(
                        container_id,
                        worker_root,
                        &[
                            &format!("{worker_root}/hel"),
                            &format!("{worker_root}/launch.json"),
                            &format!("{worker_root}/ownership.json"),
                            profile_home,
                        ],
                    ),
                )
                .purpose("assign uploaded files to the worker user"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "chmod".into(),
                        "700".into(),
                        format!("{worker_root}/hel"),
                    ],
                )
                .purpose("make Mjolnir worker executable"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "chmod".into(),
                        "-R".into(),
                        "go-rwx".into(),
                        profile_home.into(),
                    ],
                )
                .purpose("restrict harness profile permissions"),
            ] {
                execute_checked(executor, command)?;
            }
        }
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => {
            install_worker_over_ssh(
                executor,
                ssh,
                worker_root,
                profile_home,
                worker_binary,
                launch_config,
                ownership,
                profile_stage,
            )?;
        }
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | targets::TargetLocator::SshDocker { ssh, container_id } => {
            let engine = match locator {
                targets::TargetLocator::SshPodman { .. } => "podman",
                targets::TargetLocator::SshDocker { .. } => "docker",
                _ => unreachable!("matched remote container target"),
            };
            // The worker binary is 10-30 MB and identical across sessions, so
            // keep it in a content-addressed cache on the remote host and copy
            // it over the wire only once per unique binary.
            let digest = mj_core::worker_launch::worker_executable_digest(worker_binary)?;
            // Home-relative, not "~/": targets::ssh_command single-quotes every
            // argument, so a tilde would stay literal in the remote shell
            // while scp expands it, and the two sides would disagree. Both
            // ssh commands (cwd is the login home) and scp resolve a relative
            // path against the remote home.
            let cache_dir = format!(".cache/mjolnir/workers/{digest}");
            let cached_worker = format!("{cache_dir}/hel");
            let cached = matches!(
                executor.execute(
                    &crate::targets::ssh_command(ssh, ["test", "-f", &cached_worker])
                        .purpose("probe cached remote Mjolnir worker"),
                ),
                Ok(output) if output.status == 0
            );
            if !cached {
                execute_checked(
                    executor,
                    crate::targets::ssh_command(ssh, ["mkdir", "-p", &cache_dir])
                        .purpose("create remote worker cache"),
                )?;
                let partial = format!("{cache_dir}/hel.partial-{session_id}");
                execute_checked(
                    executor,
                    crate::targets::scp_upload(ssh, worker_binary, &partial, false)
                        .purpose("upload remote container worker binary"),
                )?;
                // Rename within the cache directory so the final path only
                // ever names a complete upload.
                execute_checked(
                    executor,
                    crate::targets::ssh_command(ssh, ["mv", &partial, &cached_worker])
                        .purpose("publish cached remote Mjolnir worker"),
                )?;
            }
            let upload = format!("{}/{session_id}", targets::REMOTE_UPLOAD_STAGING);
            execute_checked(
                executor,
                crate::targets::ssh_command(ssh, ["mkdir", "-p", &upload])
                    .purpose("create remote upload staging"),
            )?;
            for (source, name) in [
                (launch_config, "launch.json"),
                (ownership, "ownership.json"),
            ] {
                execute_checked(
                    executor,
                    crate::targets::scp_upload(ssh, source, &format!("{upload}/{name}"), false)
                        .purpose("upload remote container worker file"),
                )?;
            }
            execute_checked(
                executor,
                crate::targets::scp_upload(ssh, profile_stage, &format!("{upload}/profile"), true)
                    .purpose("upload remote container profile allowlist"),
            )?;
            let remote = [
                vec![
                    engine.into(),
                    "exec".into(),
                    container_id.clone(),
                    "mkdir".into(),
                    "-p".into(),
                    worker_root.into(),
                    profile_home.into(),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    cached_worker.clone(),
                    format!("{container_id}:{worker_root}/hel"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/launch.json"),
                    format!("{container_id}:{worker_root}/launch.json"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/ownership.json"),
                    format!("{container_id}:{worker_root}/ownership.json"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/profile/."),
                    format!("{container_id}:{profile_home}"),
                ],
                std::iter::once(engine.to_owned())
                    .chain(container_upload_ownership_args(
                        container_id,
                        worker_root,
                        &[
                            &format!("{worker_root}/hel"),
                            &format!("{worker_root}/launch.json"),
                            &format!("{worker_root}/ownership.json"),
                            profile_home,
                        ],
                    ))
                    .collect(),
                vec![
                    engine.into(),
                    "exec".into(),
                    container_id.clone(),
                    "chmod".into(),
                    "700".into(),
                    format!("{worker_root}/hel"),
                ],
                vec![
                    engine.into(),
                    "exec".into(),
                    container_id.clone(),
                    "chmod".into(),
                    "-R".into(),
                    "go-rwx".into(),
                    profile_home.into(),
                ],
                vec!["rm".into(), "-rf".into(), "--".into(), upload.clone()],
            ];
            for args in remote {
                execute_checked(
                    executor,
                    crate::targets::ssh_command(ssh, args)
                        .purpose("install remote container worker"),
                )?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn install_worker_over_ssh(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    worker_root: &str,
    profile_home: &str,
    worker_binary: &Path,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
) -> Result<()> {
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["mkdir", "-p", worker_root, profile_home])
            .purpose("create SSH worker directories"),
    )?;
    for (source, remote, recursive) in [
        (worker_binary, format!("{worker_root}/hel"), false),
        (launch_config, format!("{worker_root}/launch.json"), false),
        (ownership, format!("{worker_root}/ownership.json"), false),
    ] {
        execute_checked(
            executor,
            crate::targets::scp_upload(ssh, source, &remote, recursive)
                .purpose("upload SSH worker file"),
        )?;
    }
    let incoming_profile = format!("{profile_home}.incoming");
    execute_checked(
        executor,
        crate::targets::scp_upload(ssh, profile_stage, &incoming_profile, true)
            .purpose("upload SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(
            ssh,
            ["cp", "-R", &format!("{incoming_profile}/."), profile_home],
        )
        .purpose("install SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["rm", "-rf", "--", &incoming_profile])
            .purpose("remove SSH profile staging"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["chmod", "700", &format!("{worker_root}/hel")])
            .purpose("make SSH worker executable"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["chmod", "-R", "go-rwx", profile_home])
            .purpose("restrict SSH harness profile permissions"),
    )?;
    Ok(())
}

/// Replace `{worker_root}/hel` with the controller's current worker binary.
///
/// Checkpoint export starts that path as a new process. A live daemon already
/// has the previous inode mapped, so this does not restart it. Writing through
/// `hel.next` and renaming avoids `ETXTBSY` on a running image.
pub(super) fn replace_installed_worker_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<()> {
    let plan = installed_worker_binary_replacement_plan(locator, session_id, worker_binary)?;
    for command in plan.commands {
        execute_checked(executor, command)?;
    }
    Ok(())
}

pub(super) fn replace_installed_worker_launch_config(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    launch: &WorkerLaunchConfig,
) -> Result<()> {
    let plan = worker_launch_refresh_plan(locator, session_id, launch)?;
    for command in plan.replace.commands {
        execute_checked(executor, command)?;
    }
    Ok(())
}

/// Prepare the exact managed harness using the current worker binary. Remote
/// targets receive a separately staged copy; local bare targets run the binary
/// directly with a private launch config. The running worker is not stopped or
/// replaced, so any failure here leaves the quiet session attachable on its
/// previous build.
pub(super) fn prepare_managed_harness_for_upgrade(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
    launch: &WorkerLaunchConfig,
) -> Result<()> {
    if launch.harness_runtime != HarnessRuntimePolicy::Managed {
        return Ok(());
    }
    let worker_root = targets::worker_root(locator, session_id)?;
    let staging_root = format!("{worker_root}/harness-prepare");
    let staging_binary = format!("{staging_root}/hel");
    let staging_config = format!("{staging_root}/launch.json");
    let staging = tempfile::tempdir().context("create managed harness upgrade staging")?;
    let local_config = staging.path().join("launch.json");
    launch.write(&local_config)?;

    // Local bare workers already share the controller's filesystem. Running
    // the current binary against a private launch config is enough to prepare
    // the cache, and leaves the live worker root completely untouched.
    if matches!(locator, targets::TargetLocator::LocalBare { .. }) {
        execute_checked(
            executor,
            CommandSpec::new(
                worker_binary.to_string_lossy().into_owned(),
                [
                    "worker".to_owned(),
                    "prepare-harness".to_owned(),
                    "--config".to_owned(),
                    local_config.to_string_lossy().into_owned(),
                ],
            )
            .purpose("prepare exact managed harness"),
        )?;
        return Ok(());
    }

    let ssh = match locator {
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => ssh,
        _ => bail!("managed harness policy requires a local bare, SSH-bare, or EC2 target"),
    };
    let result = (|| {
        execute_checked(
            executor,
            crate::targets::ssh_command(ssh, ["rm", "-rf", "--", &staging_root])
                .purpose("clear managed harness preparation staging"),
        )?;
        execute_checked(
            executor,
            crate::targets::ssh_command(ssh, ["mkdir", "-p", &staging_root])
                .purpose("create managed harness preparation staging"),
        )?;
        execute_checked(
            executor,
            crate::targets::scp_upload(ssh, worker_binary, &staging_binary, false)
                .purpose("stage current worker for managed harness preparation"),
        )?;
        execute_checked(
            executor,
            crate::targets::scp_upload(ssh, &local_config, &staging_config, false)
                .purpose("stage managed harness launch configuration"),
        )?;
        execute_checked(
            executor,
            crate::targets::ssh_command(ssh, ["chmod", "700", &staging_binary])
                .purpose("make managed harness preparation worker executable"),
        )?;
        execute_checked(
            executor,
            crate::targets::ssh_command(
                ssh,
                [
                    staging_binary.as_str(),
                    "worker",
                    "prepare-harness",
                    "--config",
                    staging_config.as_str(),
                ],
            )
            .purpose("prepare exact managed harness"),
        )?;
        Ok(())
    })();
    let cleanup = execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["rm", "-rf", "--", &staging_root])
            .purpose("remove managed harness preparation staging"),
    );
    match (result, cleanup) {
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(error)) => Err(error).context("clean managed harness preparation staging"),
        (Err(error), Ok(_)) => Err(error),
        (Err(error), Err(cleanup)) => {
            tracing::warn!(%cleanup, path = %staging_root, "managed harness preparation staging cleanup failed");
            Err(error)
        }
    }
}

fn prepare_installed_managed_harness(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    launch: &WorkerLaunchConfig,
) -> Result<()> {
    if launch.harness_runtime != HarnessRuntimePolicy::Managed {
        return Ok(());
    }
    let worker_binary = format!("{worker_root}/hel");
    let launch_config = format!("{worker_root}/launch.json");
    let command = match locator {
        targets::TargetLocator::LocalBare { .. } => CommandSpec::new(
            worker_binary.clone(),
            [
                "worker",
                "prepare-harness",
                "--config",
                launch_config.as_str(),
            ],
        ),
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => crate::targets::ssh_command(
            ssh,
            [
                worker_binary.as_str(),
                "worker",
                "prepare-harness",
                "--config",
                launch_config.as_str(),
            ],
        ),
        _ => bail!("managed harness policy requires a local bare, SSH-bare, or EC2 target"),
    };
    execute_checked(
        executor,
        command.purpose("prepare exact managed harness before worker startup"),
    )?;
    Ok(())
}

fn installed_worker_binary_replacement_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<CommandPlan> {
    let worker_root = targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/hel");
    let staged = format!("{worker_root}/hel.next");
    let commands = match locator {
        targets::TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new(
                "cp",
                [worker_binary.to_string_lossy().into_owned(), staged.clone()],
            )
            .purpose("stage replacement Mjolnir worker"),
            CommandSpec::new("mv", ["-f", &staged, &installed])
                .purpose("replace installed Mjolnir worker"),
            CommandSpec::new("chmod", ["700", &installed])
                .purpose("make replaced Mjolnir worker executable"),
        ],
        targets::TargetLocator::LocalPodman { container_id, .. }
        | targets::TargetLocator::LocalDocker { container_id }
        | targets::TargetLocator::AppleContainer { container_id } => {
            let engine = match locator {
                targets::TargetLocator::LocalPodman { .. } => "podman",
                targets::TargetLocator::LocalDocker { .. } => "docker",
                targets::TargetLocator::AppleContainer { .. } => "container",
                _ => unreachable!("matched local container target"),
            };
            vec![
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        worker_binary.to_string_lossy().into_owned(),
                        format!("{container_id}:{staged}"),
                    ],
                )
                .purpose("stage replacement Mjolnir worker"),
                CommandSpec::new(
                    engine,
                    container_upload_ownership_args(container_id, &worker_root, &[&staged]),
                )
                .purpose("assign replacement worker to the worker user"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "mv".into(),
                        "-f".into(),
                        staged,
                        installed.clone(),
                    ],
                )
                .purpose("replace installed Mjolnir worker"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "chmod".into(),
                        "700".into(),
                        installed,
                    ],
                )
                .purpose("make replaced Mjolnir worker executable"),
            ]
        }
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => vec![
            crate::targets::scp_upload(ssh, worker_binary, &staged, false)
                .purpose("stage replacement Mjolnir worker"),
            crate::targets::ssh_command(ssh, ["mv", "-f", "--", &staged, &installed])
                .purpose("replace installed Mjolnir worker"),
            crate::targets::ssh_command(ssh, ["chmod", "700", &installed])
                .purpose("make replaced Mjolnir worker executable"),
        ],
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | targets::TargetLocator::SshDocker { ssh, container_id } => {
            let engine = match locator {
                targets::TargetLocator::SshPodman { .. } => "podman",
                targets::TargetLocator::SshDocker { .. } => "docker",
                _ => unreachable!("matched remote container target"),
            };
            let upload = format!("{}/{session_id}-hel.next", targets::REMOTE_UPLOAD_STAGING);
            vec![
                crate::targets::ssh_command(ssh, ["mkdir", "-p", targets::REMOTE_UPLOAD_STAGING])
                    .purpose("create remote replacement worker staging"),
                crate::targets::scp_upload(ssh, worker_binary, &upload, false)
                    .purpose("stage replacement Mjolnir worker"),
                crate::targets::ssh_command(
                    ssh,
                    [engine, "cp", &upload, &format!("{container_id}:{staged}")],
                )
                .purpose("stage replacement Mjolnir worker"),
                crate::targets::ssh_command(
                    ssh,
                    std::iter::once(engine.to_owned()).chain(container_upload_ownership_args(
                        container_id,
                        &worker_root,
                        &[&staged],
                    )),
                )
                .purpose("assign replacement worker to the worker user"),
                crate::targets::ssh_command(
                    ssh,
                    [
                        engine,
                        "exec",
                        container_id,
                        "mv",
                        "-f",
                        "--",
                        &staged,
                        &installed,
                    ],
                )
                .purpose("replace installed Mjolnir worker"),
                crate::targets::ssh_command(
                    ssh,
                    [engine, "exec", container_id, "chmod", "700", &installed],
                )
                .purpose("make replaced Mjolnir worker executable"),
                crate::targets::ssh_command(ssh, ["rm", "-f", "--", &upload])
                    .purpose("remove remote replacement worker staging"),
            ]
        }
    };
    Ok(CommandPlan {
        description: format!("replace stale Mjolnir worker for session {session_id}"),
        commands,
    })
}

fn installed_file_digest_command(
    locator: &targets::TargetLocator,
    path: &str,
    purpose: &str,
) -> CommandSpec {
    targets::locator_command(locator, vec!["sha256sum".into(), path.into()]).purpose(purpose)
}

fn worker_launch_refresh_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
    launch: &WorkerLaunchConfig,
) -> Result<WorkerLaunchRefreshPlan> {
    let worker_root = targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/launch.json");
    let staged = format!("{installed}.next");
    let staged_arg = targets::join_remote_command(std::slice::from_ref(&staged));
    let installed_arg = targets::join_remote_command(std::slice::from_ref(&installed));
    let script = format!("umask 077; cat > {staged_arg} && mv -f -- {staged_arg} {installed_arg}");
    let body = serde_json::to_vec_pretty(launch).context("serialize worker launch config")?;
    let expected_sha256 = format!("{:x}", Sha256::digest(&body));
    let replace = targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("replace stale Mjolnir worker launch config")
        .with_sensitive_stdin(body);
    Ok(WorkerLaunchRefreshPlan {
        expected_sha256,
        installed_digest: installed_file_digest_command(
            locator,
            &installed,
            "identify installed Mjolnir worker launch config",
        ),
        replace: CommandPlan {
            description: format!("replace stale Mjolnir launch config for session {session_id}"),
            commands: vec![replace],
        },
    })
}

/// Prepare a local refresh without hashing the controller binary. Digesting
/// happens only after recovery has proved that the worker needs a restart.
fn worker_binary_refresh_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
) -> Result<Option<WorkerBinaryRefresh>> {
    let worker_root = targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/hel");
    // Remote targets defer source selection to the recovery task: choosing the
    // binary needs the target's architecture, and probing it (plus hashing the
    // remote binary) is blocking ssh work that must not run on this UI/event
    // path. Building the refresh here stays cheap.
    if matches!(
        locator,
        targets::TargetLocator::AwsEc2 { .. }
            | targets::TargetLocator::SshBare { .. }
            | targets::TargetLocator::SshPodman { .. }
            | targets::TargetLocator::SshDocker { .. }
    ) {
        return Ok(Some(WorkerBinaryRefresh::Remote(
            RemoteWorkerBinaryRefresh {
                locator: locator.clone(),
                session_id: session_id.to_owned(),
                installed_digest: installed_file_digest_command(
                    locator,
                    &installed,
                    "identify installed Mjolnir worker binary",
                ),
            },
        )));
    }
    // Local: resolve the source now. Resolving a deleted running executable
    // materializes /proc/self/exe and can copy hundreds of megabytes; target
    // lists are assembled on UI/event loops, so leave refresh disabled until
    // the next controller start rather than doing that work here.
    if PINNED_WORKER_BINARY_SOURCES.get().is_none()
        && !std::env::current_exe().is_ok_and(|path| path.is_file())
    {
        return Ok(None);
    }
    let requirement = if matches!(locator, targets::TargetLocator::LocalBare { .. }) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    let source = match worker_binary_for_arch(std::env::consts::ARCH, requirement) {
        Ok(WorkerBinaryAvailability::Local { path, .. }) => path,
        Ok(WorkerBinaryAvailability::Remote { .. }) | Err(_) => return Ok(None),
    };
    Ok(Some(WorkerBinaryRefresh::Prepared(
        WorkerBinaryRefreshPlan {
            replace: installed_worker_binary_replacement_plan(locator, session_id, &source)?,
            source,
            installed_digest: installed_file_digest_command(
                locator,
                &installed,
                "identify installed Mjolnir worker binary",
            ),
        },
    )))
}

/// Refresh a remote worker binary during recovery: pick the worker binary for
/// the target's own architecture, and copy it over the installed one only when
/// their digests differ. This runs inside the recovery task, where blocking
/// ssh work is allowed; it must never be called from a UI/event loop.
///
/// The digest gate is what stops a redeploy loop: once the right binary is
/// installed, its digest matches the source and nothing is copied again, even
/// though recovery may still restart the worker.
pub(crate) fn refresh_remote_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    refresh: &RemoteWorkerBinaryRefresh,
) -> Result<()> {
    let source = worker_binary_for(&refresh.locator, executor)
        .context("resolve the worker binary for the recovering target")?;
    replace_remote_worker_binary_if_stale(
        executor,
        &refresh.locator,
        &refresh.session_id,
        &refresh.installed_digest,
        &source,
    )
    .map(|_| ())
}

/// Copy `source` over the installed remote worker only when the installed
/// digest differs from `source`'s. Returns whether a copy ran. Split from the
/// resolver above so the digest gate is testable without resolving a real
/// worker binary for a target architecture.
fn replace_remote_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    installed_digest: &CommandSpec,
    source: &Path,
) -> Result<bool> {
    let expected = mj_core::worker_launch::worker_executable_digest(source)?;
    let installed = executor
        .execute(installed_digest)
        .context("read the installed remote worker digest")?;
    let matches = installed.status == 0
        && String::from_utf8_lossy(&installed.stdout)
            .split_whitespace()
            .next()
            .is_some_and(|digest| digest.eq_ignore_ascii_case(&expected));
    if matches {
        return Ok(false);
    }
    installed_worker_binary_replacement_plan(locator, session_id, source)?
        .execute(executor)
        .context("replace stale remote relay worker binary")?;
    Ok(true)
}

/// Stop the detached worker daemon at `worker_root` without deleting its files.
///
/// The script signals the worker's process group so a wedged ACP child dies
/// with it. Checkpoint then restarts the daemon against the same relay root.
pub(super) fn stop_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    execute_checked(executor, stop_worker_command(locator, worker_root))?;
    Ok(())
}

/// Restore a stopped Podman target before signaling its worker. Checkpoint
/// recovery uses this instead of assuming every persisted target is running.
pub(super) fn stop_worker_after_target_recovery(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
) -> Result<()> {
    let target = targets::target_recovery_plan(locator, session_id)?;
    targets::ensure_recovery_target_running(executor, target.as_ref())
        .context("restore Mjolnir worker target")?;
    stop_worker(executor, locator, worker_root)
}

fn stop_worker_command(locator: &targets::TargetLocator, worker_root: &str) -> CommandSpec {
    let script = targets::stop_worker_daemon_script(worker_root);
    targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("stop Mjolnir worker daemon")
}

fn worker_liveness_command(locator: &targets::TargetLocator, worker_root: &str) -> CommandSpec {
    let script = targets::worker_daemon_liveness_script(worker_root);
    targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("probe Mjolnir worker daemon liveness")
}

pub(super) fn start_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    execute_checked(executor, start_worker_command(locator, worker_root))?;
    Ok(())
}

fn start_worker_command(locator: &targets::TargetLocator, worker_root: &str) -> CommandSpec {
    let binary = format!("{worker_root}/hel");
    let config = format!("{worker_root}/launch.json");
    // These files describe the worker's previous life. Clear them as part of
    // the launch, before the new daemon can be probed: a stale exit record
    // aborts startup, while a stale socket makes a recovering daemon look
    // ready and invites the reconnect actor to kill it as unresponsive.
    let clear_stale_runtime = format!(
        "rm -f {} {}; ",
        targets::join_remote_command(&[format!("{worker_root}/worker-exit.json")]),
        targets::join_remote_command(&[format!("{worker_root}/control.sock")]),
    );
    let detached_script = format!(
        "{clear_stale_runtime}nohup {} >{} 2>&1 </dev/null &",
        targets::join_remote_command(&[
            binary.clone(),
            "worker".into(),
            "run".into(),
            "--root".into(),
            worker_root.into(),
            "--config".into(),
            config.clone(),
        ]),
        targets::join_remote_command(&[format!("{worker_root}/worker.log")]),
    );
    // Redirect daemon output to worker.log in every launch mode; an
    // unexplained dead worker is undebuggable without it.
    let exec_script = format!(
        "{clear_stale_runtime}exec {} >{} 2>&1",
        targets::join_remote_command(&[
            binary.clone(),
            "worker".into(),
            "run".into(),
            "--root".into(),
            worker_root.into(),
            "--config".into(),
            config.clone(),
        ]),
        targets::join_remote_command(&[format!("{worker_root}/worker.log")]),
    );
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            CommandSpec::new("sh", ["-c", &detached_script])
        }
        targets::TargetLocator::LocalPodman { container_id, .. } => CommandSpec::new(
            "podman",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        targets::TargetLocator::LocalDocker { container_id } => CommandSpec::new(
            "docker",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        targets::TargetLocator::AppleContainer { container_id } => CommandSpec::new(
            "container",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => {
            crate::targets::ssh_command(ssh, ["sh", "-c", &detached_script])
        }
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        } => crate::targets::ssh_command(
            ssh,
            [
                "podman",
                "exec",
                "--detach",
                container_id,
                "sh",
                "-c",
                &exec_script,
            ],
        ),
        targets::TargetLocator::SshDocker { ssh, container_id } => crate::targets::ssh_command(
            ssh,
            [
                "docker",
                "exec",
                "--detach",
                container_id,
                "sh",
                "-c",
                &exec_script,
            ],
        ),
    }
    .purpose("start detached Mjolnir worker")
    // Everything before this moves data into the target and reports as Sync.
    // Start begins here, with the daemon launch.
    .stage(ProvisionStage::Starting)
}

/// Enrich an opaque handshake failure by running the installed worker binary
/// directly in the target. This surfaces loader errors (for example a
/// glibc-linked worker inside an older-glibc container) that a detached start
/// swallows.
pub(super) fn worker_probe_diagnosis(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let error = match worker_binary_probe_failure(executor, locator, worker_root) {
        Some(failure) => error.context(failure),
        None => error,
    };
    match worker_last_words(executor, locator, worker_root) {
        Some(last_words) => error.context(last_words),
        None => error,
    }
}

fn worker_binary_probe_failure(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Option<String> {
    let binary = format!("{worker_root}/hel");
    let command = targets::locator_command(locator, vec![binary.clone(), "--version".into()])
        .purpose("probe installed worker binary");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => None,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if !stderr.trim().is_empty() {
                stderr.trim()
            } else if !stdout.trim().is_empty() {
                stdout.trim()
            } else {
                "the process exited unsuccessfully without output"
            };
            Some(format!(
                "worker binary {binary} fails to run in the target: {detail}; \
                 if this is a loader/glibc error, provide a musl worker \
                 (cargo build --release --target <arch>-unknown-linux-musl \
                  -p brokk-mj-worker --bin mj-worker, \
                 or set MJ_WORKER_BINARY/MJ_WORKER_DIR)"
            ))
        }
        Err(probe_error) => Some(format!("worker probe failed: {probe_error:#}")),
    }
}

/// Fetch the dead worker's structured exit record and log tail from the
/// target, so unreachable-worker errors carry the root cause.
pub(super) fn worker_last_words(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Option<String> {
    let script = format!(
        "if [ -f {root}/worker-exit.json ]; then echo '{marker}'; cat {root}/worker-exit.json; fi; if [ -f {root}/worker.log ]; then echo '--- worker.log (tail) ---'; tail -n 20 {root}/worker.log; fi",
        root = targets::posix_quote(worker_root),
        marker = WORKER_EXIT_RECORD_MARKER
    );
    let command = targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("collect worker last words");
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(
                worker_root,
                %error,
                "could not collect worker diagnostics"
            );
            return None;
        }
    };
    if output.status != 0 {
        tracing::debug!(
            worker_root,
            status = output.status,
            "worker diagnostic probe returned a failure"
        );
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then(|| format!("worker diagnostics:\n{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::targets::ProcessExecutor;

    use anyhow::Result;

    use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec, SshTarget};
    use mj_core::config::ExecutionPolicy;

    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use std::path::{Path, PathBuf};

    /// The session's stored choice decides, with the global setting as the
    /// fallback, and a child never gets the tools whatever either says.
    #[test]
    fn the_session_choice_decides_whether_mjolnir_replaces_native_delegation() {
        let claude = |choice| {
            let mut session = crate::controller::test_support::checkpoint_test_session("s-1");
            session.harness_kind = HarnessKind::Claude;
            session.mjolnir_subagents = choice;
            session
        };

        assert!(!subagent_tools_enabled(&claude(Some(false)), true, false));
        assert!(subagent_tools_enabled(&claude(Some(true)), false, false));
        assert!(subagent_tools_enabled(&claude(None), true, false));
        assert!(!subagent_tools_enabled(&claude(None), false, false));
        assert!(!subagent_tools_enabled(&claude(Some(true)), true, true));

        let mut grok = claude(Some(true));
        grok.harness_kind = HarnessKind::Grok;
        assert!(!subagent_tools_enabled(&grok, true, false));

        let mut codex = claude(None);
        codex.harness_kind = HarnessKind::Codex;
        assert!(subagent_tools_enabled(&codex, true, false));
        codex.mjolnir_subagents = Some(false);
        assert!(!subagent_tools_enabled(&codex, true, false));
    }

    #[cfg(unix)]
    #[test]
    fn node_preflight_checks_missing_old_and_supported_tools_on_profile_path() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: directory.path().into(),
            environment: std::collections::BTreeMap::from([(
                "PATH".into(),
                directory.path().to_string_lossy().into_owned(),
            )]),
            context_window_bytes: None,
            guardian_review_model: None,
        };
        let check = || {
            preflight_harness(
                &mj_core::config::TargetTemplate::LocalBare,
                &profile,
                &ProcessExecutor,
            )
        };
        let write_tool = |name: &str, body: &str| {
            let path = directory.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        assert!(format!("{:#}", check().unwrap_err()).contains("Node.js is missing"));
        write_tool("node", "exit 1");
        assert!(format!("{:#}", check().unwrap_err()).contains("Node.js 22 or newer is required"));
        write_tool("node", "exit 0");
        assert!(format!("{:#}", check().unwrap_err()).contains("npm is missing or unusable"));
        write_tool("npm", "exit 0");
        check().unwrap();
    }

    #[test]
    fn a_stored_setup_token_reaches_only_claude_workers_that_do_not_set_their_own() {
        use mj_core::config::HarnessKind;
        use mj_core::credentials::{CLAUDE_OAUTH_TOKEN_ENV, write_claude_oauth_token};

        let directory = tempfile::tempdir().unwrap();
        let token_path = directory.path().join("profiles/claude/claude-oauth-token");
        let missing = directory.path().join("profiles/absent/claude-oauth-token");
        write_claude_oauth_token(&token_path, b"sk-ant-oat01-stored").unwrap();

        let mut claude = BTreeMap::new();
        apply_claude_setup_token(&mut claude, HarnessKind::Claude, &token_path);
        assert_eq!(
            claude.get(CLAUDE_OAUTH_TOKEN_ENV).map(String::as_str),
            Some("sk-ant-oat01-stored")
        );

        // Every other harness ignores the variable, so it must not appear.
        for kind in HarnessKind::ALL
            .into_iter()
            .filter(|kind| *kind != HarnessKind::Claude)
        {
            let mut environment = BTreeMap::new();
            apply_claude_setup_token(&mut environment, kind, &token_path);
            assert!(environment.is_empty(), "{kind:?} must not read the token");
        }

        // A profile that sets the variable itself stays authoritative.
        let mut overridden = BTreeMap::from([(
            CLAUDE_OAUTH_TOKEN_ENV.to_owned(),
            "profile-token".to_owned(),
        )]);
        apply_claude_setup_token(&mut overridden, HarnessKind::Claude, &token_path);
        assert_eq!(
            overridden.get(CLAUDE_OAUTH_TOKEN_ENV).map(String::as_str),
            Some("profile-token")
        );

        // A profile with no stored token launches exactly as before.
        let mut without = BTreeMap::new();
        apply_claude_setup_token(&mut without, HarnessKind::Claude, &missing);
        assert!(without.is_empty());
    }

    #[test]
    fn packaged_worker_names_match_release_archives() {
        let directory = Path::new("/opt/hel/bin");
        assert_eq!(
            packaged_worker_binary_path(directory, "x86_64-unknown-linux-musl"),
            directory.join("mj-worker-x86_64-unknown-linux-musl")
        );
        assert_eq!(
            packaged_worker_binary_path(directory, "aarch64-unknown-linux-musl"),
            directory.join("mj-worker-aarch64-unknown-linux-musl")
        );
    }

    #[test]
    fn pinned_snapshot_keeps_native_and_portable_sources_stable() {
        let directory = tempfile::tempdir().unwrap();
        let native = directory.path().join("native-worker");
        let x86 = directory.path().join("x86-worker");
        let arm = directory.path().join("arm-worker");
        std::fs::write(&native, b"native bytes").unwrap();
        std::fs::write(&x86, b"x86 bytes").unwrap();
        std::fs::write(&arm, b"arm bytes").unwrap();
        let cache = directory.path().join("cache");
        let snapshot = WorkerBinarySourceSnapshot::capture(&cache, |arch, requirement| {
            let path = match requirement {
                WorkerBinaryRequirement::LocalHost => &native,
                WorkerBinaryRequirement::PortableLinux if arch == "x86_64" => &x86,
                WorkerBinaryRequirement::PortableLinux => &arm,
            };
            Ok(WorkerBinaryAvailability::Local {
                path: path.clone(),
                source: format!("{arch}-{requirement:?}"),
            })
        });

        let native = snapshot
            .resolve(std::env::consts::ARCH, WorkerBinaryRequirement::LocalHost)
            .unwrap();
        let x86 = snapshot
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .unwrap();
        let arm = snapshot
            .resolve("aarch64", WorkerBinaryRequirement::PortableLinux)
            .unwrap();
        let WorkerBinaryAvailability::Local { path: native, .. } = native else {
            panic!("native source should be local");
        };
        let WorkerBinaryAvailability::Local { path: x86, .. } = x86 else {
            panic!("x86 source should be local");
        };
        let WorkerBinaryAvailability::Local { path: arm, .. } = arm else {
            panic!("arm source should be local");
        };
        assert_eq!(std::fs::read(native).unwrap(), b"native bytes");
        assert_eq!(std::fs::read(x86).unwrap(), b"x86 bytes");
        assert_eq!(std::fs::read(arm).unwrap(), b"arm bytes");
    }

    #[test]
    fn pinned_snapshot_survives_source_replacement_and_missing_candidate_install() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("worker");
        std::fs::write(&source, b"before").unwrap();
        let cache = directory.path().join("cache");
        let resolve_source = |_: &str, _: WorkerBinaryRequirement| {
            Ok(WorkerBinaryAvailability::Local {
                path: source.clone(),
                source: "test source".into(),
            })
        };
        let pinned = WorkerBinarySourceSnapshot::capture(&cache, resolve_source);

        std::fs::write(&source, b"in-place mutation").unwrap();
        let WorkerBinaryAvailability::Local { path, .. } = pinned
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .unwrap()
        else {
            panic!("source should be local");
        };
        assert_eq!(std::fs::read(path).unwrap(), b"before");

        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"after").unwrap();
        std::fs::rename(replacement, &source).unwrap();
        let WorkerBinaryAvailability::Local { path, .. } = pinned
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .unwrap()
        else {
            panic!("source should be local");
        };
        assert_eq!(std::fs::read(path).unwrap(), b"before");
        let fresh_replaced = WorkerBinarySourceSnapshot::capture(&cache, resolve_source);
        let WorkerBinaryAvailability::Local { path, .. } = fresh_replaced
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .unwrap()
        else {
            panic!("source should be local");
        };
        assert_eq!(std::fs::read(path).unwrap(), b"after");

        let missing = directory.path().join("missing-worker");
        let missing_snapshot = WorkerBinarySourceSnapshot::capture(&cache, {
            let missing = missing.clone();
            move |_: &str, _: WorkerBinaryRequirement| {
                if missing.is_file() {
                    Ok(WorkerBinaryAvailability::Local {
                        path: missing.clone(),
                        source: "new source".into(),
                    })
                } else {
                    Err(anyhow::anyhow!("candidate is unavailable"))
                }
            }
        });
        std::fs::write(&missing, b"now installed").unwrap();
        assert!(
            missing_snapshot
                .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
                .is_err()
        );
        let fresh_snapshot = WorkerBinarySourceSnapshot::capture(&cache, {
            let missing = missing.clone();
            move |_: &str, _: WorkerBinaryRequirement| {
                Ok(WorkerBinaryAvailability::Local {
                    path: missing.clone(),
                    source: "new source".into(),
                })
            }
        });
        assert!(
            fresh_snapshot
                .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
                .is_ok()
        );

        let remote_url = std::cell::RefCell::new("https://old.example/{target}".to_owned());
        let remote_snapshot = WorkerBinarySourceSnapshot::capture(
            &directory.path().join("remote-cache"),
            |arch, _| {
                Ok(WorkerBinaryAvailability::Remote {
                    url: remote_url.borrow().replace("{target}", arch),
                    sha256: "a".repeat(64),
                    triple: format!("{arch}-unknown-linux-musl"),
                })
            },
        );
        *remote_url.borrow_mut() = "https://new.example/{target}".into();
        let WorkerBinaryAvailability::Remote { url, .. } = remote_snapshot
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .unwrap()
        else {
            panic!("source should be remote");
        };
        assert_eq!(url, "https://old.example/x86_64");

        let blocked_cache = directory.path().join("blocked-cache");
        std::fs::write(&blocked_cache, b"not a directory").unwrap();
        let failed_snapshot = WorkerBinarySourceSnapshot::capture(&blocked_cache, resolve_source);
        assert!(
            failed_snapshot
                .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
                .is_err()
        );
    }

    #[test]
    fn dev_checkout_prefers_the_dedicated_musl_worker() {
        let controller = PathBuf::from("target/debug/mj");
        let musl = PathBuf::from("target/worker/x86_64-unknown-linux-musl/debug/mj-worker");
        let shared_target_worker =
            PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj-worker");
        let legacy = PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj");
        let present = [
            controller.clone(),
            musl.clone(),
            shared_target_worker,
            legacy,
        ];
        let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
            present.iter().any(|p| p == path)
        });
        assert_eq!(
            selected,
            Some((musl, "isolated development musl worker")),
            "the dedicated worker must win over legacy artifacts"
        );
    }

    #[test]
    fn local_bare_may_use_a_native_worker_beside_the_controller() {
        let controller = PathBuf::from("target/debug/mj");
        let worker = PathBuf::from("target/debug/mj-worker");
        let selected = worker_binary_prerequisite_for_current(
            std::env::consts::ARCH,
            WorkerBinaryRequirement::LocalHost,
            &controller,
            &|path| path == controller || path == worker,
        )
        .unwrap();
        assert_eq!(
            selected,
            WorkerBinaryAvailability::Local {
                path: worker,
                source: "native worker beside mj".into(),
            }
        );
    }

    #[test]
    fn local_bare_prefers_the_isolated_native_development_worker() {
        let controller = PathBuf::from("target/debug/mj");
        let worker = PathBuf::from("target/worker/debug/mj-worker");
        let packaged = PathBuf::from("target/debug/mj-worker");
        let selected = worker_binary_prerequisite_for_current(
            std::env::consts::ARCH,
            WorkerBinaryRequirement::LocalHost,
            &controller,
            &|path| path == controller || path == worker || path == packaged,
        )
        .unwrap();
        assert_eq!(
            selected,
            WorkerBinaryAvailability::Local {
                path: worker,
                source: "isolated native development worker".into(),
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_dev_controller_still_finds_its_musl_sibling() {
        let controller = PathBuf::from("target/debug/mj (deleted)");
        let musl = PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj");
        let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
            path == musl
        });

        assert_eq!(selected, Some((musl, "development musl sibling")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_dev_controller_never_selects_the_new_glibc_controller_as_its_worker() {
        let controller = PathBuf::from("target/debug/mj (deleted)");
        let replacement = PathBuf::from("target/debug/mj");
        let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
            path == replacement
        });

        assert_eq!(selected, None);
    }

    /// A configured container template for the preflight tests. Only the
    /// platform matters here; the rest is the smallest valid template.
    fn container_template(platform: Option<&str>) -> mj_core::config::ContainerTemplate {
        mj_core::config::ContainerTemplate {
            image: "example.invalid/mj-test:latest".into(),
            pull_policy: Default::default(),
            platform: platform.map(str::to_owned),
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        }
    }

    fn ssh_connection() -> mj_core::config::SshConnection {
        mj_core::config::SshConnection {
            host: "builder".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: Vec::new(),
        }
    }

    #[test]
    fn recovery_workspace_uses_the_launch_directory_for_bare_targets_only() {
        let cwd = PathBuf::from("/workspace/session/project");
        let local = worker_workspace_for_recovery(
            &targets::TargetLocator::LocalBare {
                worker_root: "/workspace/session/worker".into(),
            },
            &cwd,
        )
        .expect("local bare targets need a workspace probe");
        assert_eq!(local.directory, cwd);
        assert_eq!(local.target, mj_core::state::ManagedWorktreeTarget::Local);

        let remote = worker_workspace_for_recovery(
            &targets::TargetLocator::SshBare {
                worker_id: None,
                ssh: SshTarget {
                    destination: "dev@builder".into(),
                    ssh_args: vec!["-oBatchMode=yes".into()],
                },
                workspace: "/workspace/session".into(),
            },
            &cwd,
        )
        .expect("SSH bare targets need a workspace probe");
        assert_eq!(remote.directory, cwd);
        assert_eq!(
            remote.target,
            mj_core::state::ManagedWorktreeTarget::Ssh {
                destination: "dev@builder".into(),
                ssh_args: vec!["-oBatchMode=yes".into()],
            }
        );

        assert!(
            worker_workspace_for_recovery(
                &targets::TargetLocator::LocalPodman {
                    container_id: "container".into(),
                    workspace_storage: Default::default(),
                },
                &cwd,
            )
            .is_none()
        );
        assert!(
            worker_workspace_for_recovery(
                &targets::TargetLocator::AwsEc2 {
                    profile: "default".into(),
                    region: "us-east-1".into(),
                    instance_id: "i-test".into(),
                    ssh: SshTarget {
                        destination: "dev@builder".into(),
                        ssh_args: Vec::new(),
                    },
                    workspace: "/workspace/session".into(),
                },
                &cwd,
            )
            .is_none()
        );
    }

    #[test]
    fn preflight_reads_the_architecture_a_template_names() {
        use mj_core::config::TargetTemplate;

        for (platform, expected) in [
            ("linux/arm64", "aarch64"),
            ("linux/arm64/v8", "aarch64"),
            ("linux/amd64", "x86_64"),
            ("aarch64", "aarch64"),
        ] {
            assert_eq!(
                preflight_architectures(&TargetTemplate::LocalPodman {
                    container: container_template(Some(platform)),
                }),
                vec![expected],
                "platform {platform}"
            );
        }
        // A named platform decides a remote container target too, so a resume
        // onto an arm64 container never asks about the host's architecture.
        assert_eq!(
            preflight_architectures(&TargetTemplate::SshPodman {
                ssh: ssh_connection(),
                container: container_template(Some("linux/arm64")),
            }),
            vec!["aarch64"]
        );
    }

    #[test]
    fn preflight_uses_the_host_architecture_for_a_local_target() {
        use mj_core::config::TargetTemplate;

        for template in [
            TargetTemplate::LocalBare,
            TargetTemplate::LocalPodman {
                container: container_template(None),
            },
            TargetTemplate::LocalDocker {
                container: container_template(None),
            },
            TargetTemplate::AppleContainer {
                container: container_template(None),
            },
        ] {
            assert_eq!(
                preflight_architectures(&template),
                vec![std::env::consts::ARCH],
                "{template:?}"
            );
        }
    }

    #[test]
    fn preflight_accepts_either_linux_architecture_for_a_remote_target() {
        use mj_core::config::TargetTemplate;

        // Nothing in the configuration says what a remote machine runs, so the
        // preflight passes as long as one architecture could be served; the
        // real architecture is read from the live target during provisioning.
        for template in [
            TargetTemplate::SshBare {
                ssh: ssh_connection(),
                permissions: mj_core::config::PermissionMode::Yolo,
                workspace_prefix: PathBuf::from(".local/share/hel/workspaces"),
            },
            TargetTemplate::SshPodman {
                ssh: ssh_connection(),
                container: container_template(None),
            },
            TargetTemplate::AwsEc2 {
                aws_profile: None,
                region: "us-east-1".into(),
                launch_template: "lt-mj".into(),
                launch_template_version: None,
                ssh_user: "dev".into(),
                address_source: Default::default(),
                identity_file: None,
                ssh_args: Vec::new(),
            },
        ] {
            assert_eq!(
                preflight_architectures(&template),
                vec!["x86_64", "aarch64"],
                "{template:?}"
            );
        }
    }

    #[test]
    fn dev_checkout_still_finds_a_hel_named_sibling() {
        let controller = PathBuf::from("target/debug/hel");
        let musl = PathBuf::from("target/x86_64-unknown-linux-musl/debug/hel");
        let present = [controller.clone(), musl.clone()];
        let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
            present.iter().any(|p| p == path)
        });
        assert_eq!(selected, Some((musl, "development musl sibling")));
    }

    /// An architecture no host builds for, so the lookup cannot take one of
    /// the "native mj binary" shortcuts and reaches the end on any machine.
    const FOREIGN_ARCH: &str = "riscv64";

    /// A rebuilt or renamed checkout leaves a running daemon pointing at a
    /// path that holds nothing. Searching beside that path finds nothing and
    /// blames the user for a worker that may well be installed correctly.
    #[test]
    fn a_replaced_controller_is_reported_instead_of_a_missing_worker() {
        let stale = PathBuf::from("/src/.backup-vHXvCs/target/debug/mj (deleted)");
        let probed = RefCell::new(Vec::new());

        let error = worker_binary_prerequisite_for_current(
            FOREIGN_ARCH,
            WorkerBinaryRequirement::PortableLinux,
            &stale,
            &|path| {
                probed.borrow_mut().push(path.to_path_buf());
                false
            },
        )
        .unwrap_err();

        let detail = format!("{error:#}");
        assert!(
            detail.contains("was replaced or removed on disk"),
            "{detail}"
        );
        assert!(detail.contains("restart the Mjolnir daemon"), "{detail}");
        // The path is named without the kernel's deletion marker.
        assert!(
            detail.contains("/src/.backup-vHXvCs/target/debug/mj)"),
            "{detail}"
        );
        assert!(!detail.contains("(deleted)"), "{detail}");
        assert_eq!(
            probed.into_inner(),
            vec![stale],
            "nothing beside a path that no longer exists is worth probing"
        );
    }

    /// The guard is about a controller path that no longer exists and nothing
    /// else: a controller still on disk keeps its whole sibling lookup, and
    /// keeps the plain "no Linux worker" answer when that lookup comes up
    /// empty. A present controller is never its own portable worker, so with
    /// nothing installed beside it the lookup ends in that plain answer.
    #[test]
    fn a_present_controller_still_looks_beside_itself() {
        let controller = PathBuf::from("/opt/brokk/mj");
        let probed = RefCell::new(Vec::new());

        let error = worker_binary_prerequisite_for_current(
            FOREIGN_ARCH,
            WorkerBinaryRequirement::PortableLinux,
            &controller,
            &|path| {
                probed.borrow_mut().push(path.to_path_buf());
                path == controller
            },
        )
        .unwrap_err();

        let probed = probed.into_inner();
        assert!(
            probed
                .iter()
                .any(|path| path.ends_with("mj-worker-riscv64-unknown-linux-musl")),
            "the packaged worker name must still be probed: {probed:?}"
        );
        let detail = format!("{error:#}");
        assert!(
            detail.contains("no Linux worker for riscv64-unknown-linux-musl"),
            "{detail}"
        );
        assert!(!detail.contains("restart the Mjolnir daemon"), "{detail}");

        // With nothing beside it either, a present controller still gets the
        // generic message; only a replaced one is told to restart.
        let root = PathBuf::from("/");
        let error = worker_binary_prerequisite_for_current(
            FOREIGN_ARCH,
            WorkerBinaryRequirement::PortableLinux,
            &root,
            &|path| path == root,
        )
        .unwrap_err();
        let detail = format!("{error:#}");
        assert!(
            detail.contains("no Linux worker for riscv64-unknown-linux-musl"),
            "{detail}"
        );
        assert!(!detail.contains("restart the Mjolnir daemon"), "{detail}");
    }

    const WORKER_BINARY_OVERRIDE_CHILD: &str = "MJ_WORKER_BINARY_OVERRIDE_CHILD";

    /// The override names a worker outright, so it does not care where the
    /// controller lives or whether that path still exists.
    #[test]
    fn a_replaced_controller_still_honors_the_worker_binary_override() {
        // MJ_WORKER_BINARY is process-global and other tests resolve worker
        // binaries, so set it only in an exact child test.
        if std::env::var_os(WORKER_BINARY_OVERRIDE_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let worker = directory.path().join("mj-worker");
            std::fs::write(&worker, b"worker").unwrap();
            let test_name = format!(
                "{}::a_replaced_controller_still_honors_the_worker_binary_override",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(WORKER_BINARY_OVERRIDE_CHILD, "1")
                .env("MJ_WORKER_BINARY", &worker)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated worker override test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let stale = PathBuf::from("/src/.backup-vHXvCs/target/debug/mj (deleted)");
        let availability = worker_binary_prerequisite_for_current(
            FOREIGN_ARCH,
            WorkerBinaryRequirement::PortableLinux,
            &stale,
            &|path| path.is_file(),
        )
        .unwrap();

        match availability {
            WorkerBinaryAvailability::Local { source, .. } => {
                assert_eq!(source, "MJ_WORKER_BINARY");
            }
            other => panic!("expected the override to resolve, got {other:?}"),
        }
    }

    #[test]
    fn sibling_lookup_falls_back_to_the_legacy_hel_name_beside_an_mj_controller() {
        let controller = PathBuf::from("/opt/brokk/mj");
        let legacy = PathBuf::from("/opt/brokk/hel");
        let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
            path == legacy
        });
        assert_eq!(selected, Some((legacy, "beside the running executable")));
    }

    #[test]
    fn worker_diagnosis_surfaces_a_loader_failure_from_the_installed_binary() {
        struct FailedProbe;

        impl CommandExecutor for FailedProbe {
            fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"libc.so.6: version `GLIBC_2.39' not found\n".to_vec(),
                })
            }
        }

        let failure = worker_binary_probe_failure(
            &FailedProbe,
            &targets::TargetLocator::LocalBare {
                worker_root: "/worker/root".into(),
            },
            "/worker/root",
        )
        .expect("an unsuccessful --version probe should explain the dead worker");

        assert!(failure.contains("GLIBC_2.39"), "{failure}");
        assert!(failure.contains("provide a musl worker"), "{failure}");
    }

    /// macOS puts worker roots under `~/Library/Application Support/...`.
    /// An unquoted root split the diagnostic script into separate words, so
    /// the probe silently reported nothing exactly when it was needed.
    #[test]
    fn worker_last_words_reads_a_root_containing_spaces() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Application Support").join("hel worker");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("worker-exit.json"),
            b"{\n  \"reason\": \"panic\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("worker.log"),
            b"Mjolnir worker exited with an error\n",
        )
        .unwrap();
        let root = root.to_str().unwrap();

        let locator = targets::TargetLocator::LocalBare {
            worker_root: root.into(),
        };
        let reported = worker_last_words(&ProcessExecutor, &locator, root)
            .expect("the probe reads a root containing spaces");
        assert!(reported.contains(WORKER_EXIT_RECORD_MARKER), "{reported}");
        assert!(reported.contains("\"reason\": \"panic\""), "{reported}");
        assert!(
            reported.contains("Mjolnir worker exited with an error"),
            "{reported}"
        );

        let recorder = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        worker_last_words(&recorder, &locator, root);
        let commands = recorder.commands.borrow();
        let script = commands
            .iter()
            .flat_map(|command| command.args.iter())
            .find(|argument| argument.contains("worker-exit.json"))
            .expect("the probe builds a diagnostic script");
        assert!(
            script.contains(&format!("'{root}'")),
            "the root must be single-quoted: {script}"
        );
    }

    /// A worker that died leaves an exit record behind. Starting a new worker
    /// must clear it first, or the startup connect loop reads the previous
    /// death as this worker's and gives up on a healthy daemon.
    #[test]
    fn starting_a_worker_clears_stale_runtime_files_before_launching() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        for locator in [
            targets::TargetLocator::LocalBare {
                worker_root: "/worker/root".into(),
            },
            targets::TargetLocator::LocalPodman {
                container_id: "container-1".into(),
                workspace_storage: Default::default(),
            },
        ] {
            let executor = RecordingExecutor {
                commands: RefCell::new(Vec::new()),
            };
            start_worker(&executor, &locator, "/worker/root").unwrap();

            let commands = executor.commands.borrow();
            let script = commands
                .iter()
                .flat_map(|command| command.args.iter())
                .find(|argument| argument.contains("worker-exit.json"))
                .unwrap_or_else(|| {
                    panic!("no launch script cleared the exit record: {commands:?}")
                });
            let cleared = script.find("rm -f").expect("the exit record is removed");
            let launched = script.find("worker").expect("the daemon is launched");
            assert!(
                script.contains("control.sock"),
                "the stale relay endpoint must be cleared before startup: {script}"
            );
            assert!(
                cleared < launched,
                "stale runtime files must be cleared before the daemon starts: {script}"
            );
        }
    }
    #[test]
    fn stopping_a_worker_runs_the_daemon_stop_script() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let locator = targets::TargetLocator::SshBare {
            worker_id: None,
            ssh: SshTarget {
                destination: "user@example.test".into(),
                ssh_args: Vec::new(),
            },
            workspace: "/workspace".into(),
        };
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        stop_worker(&executor, &locator, "/worker/root").unwrap();

        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].purpose, "stop Mjolnir worker daemon");
        assert!(
            commands[0]
                .args
                .last()
                .is_some_and(|remote| remote.starts_with("'sh' '-c' ")),
            "raw SSH worker management must not source login profiles: {commands:?}"
        );
        let script = commands[0]
            .args
            .iter()
            .find(|argument| argument.contains("worker run --root"))
            .unwrap_or_else(|| panic!("stop script missing from {commands:?}"));
        assert!(
            script.contains("hel_match=\"hel worker run --root $hel_root\""),
            "stop must match only this session's worker: {script}"
        );
        assert!(
            script.contains("hel_match_home=\"hel worker run --root $HOME/$hel_root\""),
            "stop must also match a login-home-absolute --root: {script}"
        );
        assert!(
            !script.contains("grep -F"),
            "leftover detection must not grep the match string: {script}"
        );
    }
    #[test]
    fn checkpoint_worker_stop_restores_a_stopped_podman_target_first() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
            outputs: RefCell<Vec<CommandOutput>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(self.outputs.borrow_mut().remove(0))
            }
        }

        let session = "0123456789abcdef0123456789abcdef";
        let container_id = targets::resource_name(session).unwrap();
        let inspection = |status: &str| CommandOutput {
            status: 0,
            stdout: serde_json::to_vec(&serde_json::json!([{
                "Config": { "Labels": {
                    (targets::MANAGED_LABEL): "true",
                    (targets::SESSION_LABEL): session,
                }},
                "State": { "Status": status },
            }]))
            .unwrap(),
            stderr: Vec::new(),
        };
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
            outputs: RefCell::new(vec![
                CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
                inspection("exited"),
                CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
                inspection("running"),
                CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            ]),
        };
        let locator = targets::TargetLocator::LocalPodman {
            container_id,
            workspace_storage: Default::default(),
        };

        stop_worker_after_target_recovery(&executor, &locator, session, "/worker/root").unwrap();

        let commands = executor.commands.borrow();
        let purposes = commands
            .iter()
            .map(|command| command.purpose.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            purposes,
            [
                "check for Mjolnir session container",
                "inspect Mjolnir session container",
                "start stopped Mjolnir session container",
                "inspect Mjolnir session container",
                "stop Mjolnir worker daemon",
            ]
        );
    }

    struct PodmanInstallExecutor {
        commands: RefCell<Vec<CommandSpec>>,
        worker_cached: bool,
    }
    impl CommandExecutor for PodmanInstallExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let probing_cache = command
                .args
                .iter()
                .any(|argument| argument.contains("'test' '-f'"));
            let status = if probing_cache && !self.worker_cached {
                1
            } else {
                0
            };
            Ok(CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }
    struct PodmanInstallFixture {
        _root: tempfile::TempDir,
        worker_binary: PathBuf,
        launch_config: PathBuf,
        ownership: PathBuf,
        profile_stage: PathBuf,
        locator: targets::TargetLocator,
        digest: String,
    }
    fn podman_install_fixture() -> PodmanInstallFixture {
        let root = tempfile::tempdir().unwrap();
        let worker_binary = root.path().join("hel");
        std::fs::write(&worker_binary, b"worker-binary-bytes").unwrap();
        let launch_config = root.path().join("launch.json");
        std::fs::write(&launch_config, b"{}").unwrap();
        let ownership = root.path().join("ownership.json");
        std::fs::write(&ownership, b"{}").unwrap();
        let profile_stage = root.path().join("profile");
        std::fs::create_dir_all(&profile_stage).unwrap();
        let digest = format!("{:x}", Sha256::digest(b"worker-binary-bytes"));
        PodmanInstallFixture {
            _root: root,
            worker_binary,
            launch_config,
            ownership,
            profile_stage,
            locator: targets::TargetLocator::SshPodman {
                ssh: SshTarget {
                    destination: "user@example.test".into(),
                    ssh_args: Vec::new(),
                },
                container_id: "container-1".into(),
                workspace_storage: Default::default(),
            },
            digest,
        }
    }
    fn run_podman_install(worker_cached: bool) -> (Vec<CommandSpec>, PodmanInstallFixture) {
        let fixture = podman_install_fixture();
        let executor = PodmanInstallExecutor {
            commands: RefCell::new(Vec::new()),
            worker_cached,
        };
        install_worker_files(
            &executor,
            &fixture.locator,
            "0123456789abcdef0123456789abcdef",
            "/workspace/.hel/worker",
            "/workspace/.hel/profile",
            &fixture.worker_binary,
            &fixture.launch_config,
            &fixture.ownership,
            &fixture.profile_stage,
        )
        .unwrap();
        let commands = executor.commands.borrow().clone();
        (commands, fixture)
    }
    fn rendered(commands: &[CommandSpec]) -> Vec<String> {
        commands
            .iter()
            .map(|command| format!("{} {}", command.program, command.args.join(" ")))
            .collect()
    }
    #[test]
    fn ssh_podman_install_caches_the_worker_binary_on_a_cache_miss() {
        let (commands, fixture) = run_podman_install(false);
        let lines = rendered(&commands);
        let digest = &fixture.digest;
        let cache_dir = format!(".cache/mjolnir/workers/{digest}");
        let session = "0123456789abcdef0123456789abcdef";

        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("ssh") && line.contains("'test' '-f'")),
            "expected a cache probe, got {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains('~')),
            "remote staging paths must be home-relative: ssh arguments are \
                 single-quoted so a tilde stays literal in the remote shell while \
                 scp expands it, got {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("ssh")
                && line.contains(&format!("'mkdir' '-p' '{cache_dir}'"))),
            "expected the cache directory to be created, got {lines:#?}"
        );
        let partial = format!("{cache_dir}/hel.partial-{session}");
        assert!(
            lines.iter().any(|line| line.starts_with("scp ")
                && line.ends_with(&format!(
                    "{} user@example.test:{partial}",
                    fixture.worker_binary.display()
                ))),
            "expected the worker to be uploaded to the partial cache path, got {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("ssh")
                && line.contains(&format!("'mv' '{partial}' '{cache_dir}/hel'"))),
            "expected an atomic rename into the cache, got {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("'podman' 'cp'")
                && line.contains(&format!("'{cache_dir}/hel'"))),
            "expected podman cp to read the cached worker, got {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("scp")
                && line.ends_with(&format!(
                    "user@example.test:.cache/mjolnir/uploads/{session}/hel"
                ))),
            "the worker must not be staged in the per-session upload directory, got {lines:#?}"
        );
    }
    #[test]
    fn ssh_podman_install_skips_the_worker_upload_on_a_cache_hit() {
        let (commands, fixture) = run_podman_install(true);
        let lines = rendered(&commands);
        let digest = &fixture.digest;
        let cache_dir = format!(".cache/mjolnir/workers/{digest}");
        let session = "0123456789abcdef0123456789abcdef";

        assert!(
            !lines.iter().any(|line| line.starts_with("scp")
                && line.contains(&fixture.worker_binary.display().to_string())),
            "a cached worker must not be re-uploaded, got {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("'mv'")),
            "a cache hit must not rename anything, got {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("'podman' 'cp'")
                && line.contains(&format!("'{cache_dir}/hel'"))),
            "expected podman cp to read the cached worker, got {lines:#?}"
        );
        for name in ["launch.json", "ownership.json"] {
            assert!(
                lines.iter().any(|line| line.starts_with("scp")
                    && line.ends_with(&format!(
                        "user@example.test:.cache/mjolnir/uploads/{session}/{name}"
                    ))),
                "expected {name} to still be uploaded per session, got {lines:#?}"
            );
        }
    }

    #[test]
    fn ssh_docker_install_uses_docker_for_remote_container_operations() {
        let mut fixture = podman_install_fixture();
        fixture.locator = targets::TargetLocator::SshDocker {
            ssh: SshTarget {
                destination: "user@example.test".into(),
                ssh_args: Vec::new(),
            },
            container_id: "container-1".into(),
        };
        let executor = PodmanInstallExecutor {
            commands: RefCell::new(Vec::new()),
            worker_cached: true,
        };
        install_worker_files(
            &executor,
            &fixture.locator,
            "0123456789abcdef0123456789abcdef",
            "/workspace/.hel/worker",
            "/workspace/.hel/profile",
            &fixture.worker_binary,
            &fixture.launch_config,
            &fixture.ownership,
            &fixture.profile_stage,
        )
        .unwrap();

        let lines = rendered(&executor.commands.borrow());
        assert!(
            lines.iter().any(|line| line.contains("'docker' 'cp'")),
            "expected Docker to copy the cached worker, got {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("'docker' 'exec'")),
            "expected Docker to prepare the worker directories, got {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("'podman'")),
            "Docker installation accidentally used Podman: {lines:#?}"
        );
    }

    #[test]
    #[ignore = "requires Docker and the locally installed agent-dev image"]
    fn docker_uploads_and_replacements_are_usable_by_the_non_root_worker() {
        let fixture = podman_install_fixture();
        let session = mj_core::state::new_session_id().unwrap();
        let container_id = targets::resource_name(&session).unwrap();
        let locator = targets::TargetLocator::LocalDocker {
            container_id: container_id.clone(),
        };
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "run",
                    "--pull=never",
                    "-d",
                    "--name",
                    &container_id,
                    "ghcr.io/brokkai/mjolnir/agent-dev:latest",
                    "sleep",
                    "infinity",
                ],
            ),
        )
        .unwrap();
        let result = (|| -> Result<()> {
            let root = targets::worker_root(&locator, &session)?;
            let profile = format!("{root}/profile");
            std::fs::write(fixture.profile_stage.join("credential"), "private")?;
            install_worker_files(
                &ProcessExecutor,
                &locator,
                &session,
                &root,
                &profile,
                &fixture.worker_binary,
                &fixture.launch_config,
                &fixture.ownership,
                &fixture.profile_stage,
            )?;
            replace_installed_worker_binary(
                &ProcessExecutor,
                &locator,
                &session,
                &fixture.worker_binary,
            )?;
            execute_checked(
                &ProcessExecutor,
                CommandSpec::new(
                    "docker",
                    [
                        "exec",
                        &container_id,
                        "sh",
                        "-c",
                        "test \"$(id -u)\" != 0 && test -x \"$1/hel\" && test -r \"$1/launch.json\" && test -r \"$1/ownership.json\" && test -r \"$1/profile/credential\" && test -w \"$1/profile/credential\"",
                        "sh",
                        &root,
                    ],
                ),
            )?;
            Ok(())
        })();
        let cleanup = execute_checked(
            &ProcessExecutor,
            CommandSpec::new("docker", ["rm", "-f", &container_id]),
        );
        result.unwrap();
        cleanup.unwrap();
    }

    #[test]
    fn replacing_an_installed_podman_worker_writes_through_a_next_path() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }
        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let session = "0123456789abcdef0123456789abcdef";
        let container_id = targets::resource_name(session).unwrap();
        let locator = targets::TargetLocator::LocalPodman {
            container_id: container_id.clone(),
            workspace_storage: Default::default(),
        };
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        replace_installed_worker_binary(&executor, &locator, session, Path::new("/controller/hel"))
            .unwrap();

        let mut lines = rendered(&executor.commands.borrow());
        let ownership = lines.remove(1);
        assert!(ownership.starts_with(&format!("podman exec --user 0 {container_id} sh -c")));
        assert!(ownership.contains("chown -R"));
        assert!(ownership.ends_with(&format!("/var/lib/hel/workers/{session}/hel.next")));
        assert_eq!(
            lines,
            vec![
                format!(
                    "podman cp /controller/hel {container_id}:/var/lib/hel/workers/{session}/hel.next"
                ),
                format!(
                    "podman exec {container_id} mv -f /var/lib/hel/workers/{session}/hel.next /var/lib/hel/workers/{session}/hel"
                ),
                format!("podman exec {container_id} chmod 700 /var/lib/hel/workers/{session}/hel"),
            ]
        );
    }
    #[test]
    fn default_bridges_pin_command_capable_adapter_versions() {
        let (codex_command, codex_arguments) = bridge_launch(
            mj_core::config::HarnessKind::Codex,
            ExecutionPolicy::Unconstrained,
        );
        assert_eq!(codex_command, "sh");
        assert_eq!(codex_arguments[0], "-c");
        assert!(codex_arguments[1].contains("@brokkai/codex-acp@1.11.4"));
        assert!(codex_arguments[1].contains("codex-acp --version"));
        assert!(codex_arguments[1].contains("npx -y @brokkai/codex-acp@1.11.4"));

        let (claude_command, claude_arguments) = bridge_launch(
            mj_core::config::HarnessKind::Claude,
            ExecutionPolicy::Unconstrained,
        );
        assert_eq!(claude_command, "sh");
        assert_eq!(claude_arguments[0], "-c");
        assert!(claude_arguments[1].contains("@agentclientprotocol/claude-agent-acp@0.73.0"));
    }

    #[test]
    fn readiness_stage_names_only_install_capable_default_harnesses() {
        let profile = |kind| mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: PathBuf::from("/profiles/test"),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        for harness in HarnessKind::ALL {
            assert_eq!(
                bridge_readiness_stage(&profile(harness)),
                ProvisionStage::Installing(harness)
            );
        }
    }
    #[test]
    fn codex_execution_environment_follows_the_target_policy() {
        let mut podman_environment =
            BTreeMap::from([("INITIAL_AGENT_MODE".to_owned(), "read-only".to_owned())]);
        mj_core::config::HarnessKind::Codex
            .configure_execution_environment(
                ExecutionPolicy::Unconstrained,
                &mut podman_environment,
            )
            .unwrap();
        assert_eq!(
            podman_environment
                .get("INITIAL_AGENT_MODE")
                .map(String::as_str),
            Some("agent-full-access")
        );

        let mut bare_environment =
            BTreeMap::from([("INITIAL_AGENT_MODE".to_owned(), "read-only".to_owned())]);
        mj_core::config::HarnessKind::Codex
            .configure_execution_environment(
                ExecutionPolicy::ConfiguredApprovals,
                &mut bare_environment,
            )
            .unwrap();
        assert_eq!(
            bare_environment
                .get("INITIAL_AGENT_MODE")
                .map(String::as_str),
            Some("agent"),
            "Codex uses guardian on raw localhost"
        );
    }
    #[test]
    fn bare_targets_use_managed_harnesses_but_containers_stay_ambient() {
        let ssh = SshTarget {
            destination: "user@example.test".into(),
            ssh_args: Vec::new(),
        };
        let targets = [
            (
                targets::TargetLocator::LocalBare {
                    worker_root: "/worker".into(),
                },
                HarnessRuntimePolicy::Managed,
            ),
            (
                targets::TargetLocator::LocalPodman {
                    container_id: "container".into(),
                    workspace_storage: Default::default(),
                },
                HarnessRuntimePolicy::Ambient,
            ),
            (
                targets::TargetLocator::SshBare {
                    worker_id: None,
                    ssh: ssh.clone(),
                    workspace: "/workspace/session".into(),
                },
                HarnessRuntimePolicy::Managed,
            ),
            (
                targets::TargetLocator::AwsEc2 {
                    profile: "profile".into(),
                    region: "us-east-1".into(),
                    instance_id: "i-test".into(),
                    ssh,
                    workspace: "/workspace/session".into(),
                },
                HarnessRuntimePolicy::Managed,
            ),
        ];

        for (target, expected) in targets {
            assert_eq!(harness_runtime_policy(&target), expected, "{target:?}");
        }
    }
    #[test]
    fn grok_sandbox_environment_follows_the_target_policy() {
        let mut isolated = BTreeMap::from([("GROK_SANDBOX".to_owned(), "strict".to_owned())]);
        mj_core::config::HarnessKind::Grok
            .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut isolated)
            .unwrap();
        assert_eq!(
            isolated.get("GROK_SANDBOX").map(String::as_str),
            Some("off")
        );

        let mut local = BTreeMap::from([("GROK_SANDBOX".to_owned(), "strict".to_owned())]);
        mj_core::config::HarnessKind::Grok
            .configure_execution_environment(ExecutionPolicy::ConfiguredApprovals, &mut local)
            .unwrap();
        assert_eq!(
            local.get("GROK_SANDBOX").map(String::as_str),
            Some("strict"),
            "raw localhost must preserve the profile's configured sandbox"
        );
    }
    #[test]
    fn bridge_fallback_pins_match_the_agent_dev_containerfile() {
        const CONTAINERFILE: &str = include_str!("../../../containers/Containerfile.agent-dev");

        let codex = format!("codex-acp@{CODEX_ACP_VERSION}");
        assert!(
            CONTAINERFILE.contains(&codex),
            "containers/Containerfile.agent-dev must install {codex}. The image and the \
                 bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container \
                 session and an npx session run different adapter versions."
        );

        let claude = format!("claude-agent-acp@{CLAUDE_ACP_VERSION}");
        assert!(
            CONTAINERFILE.contains(&claude),
            "containers/Containerfile.agent-dev must install {claude}. The image and the \
                 bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container \
                 session and an npx session run different adapter versions."
        );
    }
    #[test]
    fn kimi_default_bridge_is_non_login_and_uses_bash_for_the_official_installer() {
        let (command, arguments) = bridge_launch(
            mj_core::config::HarnessKind::Kimi,
            ExecutionPolicy::Unconstrained,
        );
        assert_eq!(command, "sh");
        assert_eq!(arguments[0], "-c");
        assert!(arguments[1].contains("install.sh | bash &&"));
        assert!(arguments[1].contains("$HOME/.kimi-code/bin/kimi"));
        assert!(arguments[1].contains("Mjolnir needs compatible Kimi Code"));
        assert!(!arguments[1].contains("Hel"));
    }
    #[test]
    fn grok_default_bridge_is_non_login_and_uses_bash_for_the_official_installer() {
        let (command, arguments) = bridge_launch(
            mj_core::config::HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
        );
        assert_eq!(command, "sh");
        assert_eq!(arguments[0], "-c");
        let script = &arguments[1];
        assert!(script.contains("https://x.ai/cli/install.sh | bash &&"));
        assert!(script.contains("command -v grok"));
        assert!(script.contains("[ -x \"$GROK_HOME/bin/grok\" ]"));
        assert!(script.contains("[ -x \"$HOME/.grok/bin/grok\" ]"));
        assert!(script.contains("exit 127"));
        assert!(script.contains("exec grok agent stdio"));
        assert!(!script.contains("--always-approve"));
        assert!(script.contains("Mjolnir needs compatible Grok Build"));
        assert!(!script.contains("Hel"));
    }
    #[test]
    fn node_bootstrap_errors_name_mjolnir() {
        let script = ensure_node_script();
        assert!(script.contains("Mjolnir needs Node.js, npm, and npx"));
        assert!(!script.contains("sudo"));
        assert!(!script.contains("apt-get"));
        assert!(!script.contains("Hel"));
    }
    #[test]
    fn grok_default_bridge_adds_the_always_approve_flag_when_unrestricted() {
        let (_, arguments) = bridge_launch(
            mj_core::config::HarnessKind::Grok,
            ExecutionPolicy::Unconstrained,
        );
        let script = &arguments[1];
        assert!(script.contains("exec grok agent --always-approve stdio"));
        assert!(script.contains("exec \"$GROK_HOME/bin/grok\" agent --always-approve stdio"));
        assert!(script.contains("exec \"$HOME/.grok/bin/grok\" agent --always-approve stdio"));
    }
    #[test]
    fn kimi_uses_runtime_aware_memory_delivery_only_on_staged_targets() {
        let local = targets::TargetLocator::LocalBare {
            worker_root: "/worker".into(),
        };
        let podman = targets::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
        };

        assert_eq!(
            project_memory_mcp_delivery(mj_core::config::HarnessKind::Kimi, &local),
            ProjectMemoryMcpDelivery::Acp
        );
        assert_eq!(
            project_memory_mcp_delivery(mj_core::config::HarnessKind::Kimi, &podman),
            ProjectMemoryMcpDelivery::HarnessProfile
        );
        assert_eq!(
            project_memory_mcp_delivery(mj_core::config::HarnessKind::Codex, &podman),
            ProjectMemoryMcpDelivery::Acp
        );
    }
    /// A catalog cache backed by an isolated copy of Mjolnir's own
    /// `profile_config_cache` table, so the fallback path is exercised against
    /// the real schema without touching the live store.
    struct IsolatedCatalogCache(std::path::PathBuf);

    impl CatalogCache for IsolatedCatalogCache {
        fn load(&self, profile_id: &str, fingerprint: &str) -> Option<String> {
            crate::database::load_profile_config_cache_from(&self.0, profile_id, "", fingerprint)
                .ok()
                .flatten()
        }

        fn store(&self, profile_id: &str, fingerprint: &str, body: &str) {
            crate::database::save_profile_config_cache_at(
                &self.0,
                profile_id,
                "",
                fingerprint,
                body,
            )
            .expect("write the isolated catalog cache");
        }
    }

    const ZAI_CONFIG: &str = "model = \"glm-5.3\"\n\
                              model_provider = \"zai\"\n\
                              \n\
                              [model_providers.zai]\n\
                              base_url = \"https://api.z.ai/api/v1\"\n\
                              env_key = \"ZAI_API_KEY\"\n\
                              wire_api = \"responses\"\n";

    const ZAI_CATALOG: &str = r#"{"models":[
        {"slug":"glm-5.3","supported_reasoning_levels":["low","high","max"]},
        {"slug":"glm-5.3-flash","supported_reasoning_levels":["low","high","max"]}
    ]}"#;

    fn zai_profile(home: &Path) -> mj_core::config::HarnessProfile {
        std::fs::write(home.join("config.toml"), ZAI_CONFIG).unwrap();
        mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.to_path_buf(),
            environment: BTreeMap::from([("ZAI_API_KEY".to_owned(), "coding-plan-key".to_owned())]),
            context_window_bytes: None,
            guardian_review_model: None,
        }
    }

    #[test]
    fn staging_a_custom_provider_profile_writes_a_catalog_the_session_can_pick_from() {
        let home = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let profile = zai_profile(home.path());
        let cache = IsolatedCatalogCache(cache.path().join("cache.sqlite3"));
        let asked = std::cell::RefCell::new(Vec::new());

        stage_profile(&profile, staged.path()).unwrap();
        stage_codex_catalog(
            "glm",
            &profile,
            staged.path(),
            &|url, key| {
                asked.borrow_mut().push((url.to_owned(), key.to_owned()));
                Ok(ZAI_CATALOG.as_bytes().to_vec())
            },
            &cache,
        )
        .unwrap();

        assert_eq!(
            asked.into_inner(),
            vec![(
                "https://api.z.ai/api/v1/models".to_owned(),
                "coding-plan-key".to_owned()
            )],
            "the provider's own key authorizes its catalog fetch"
        );
        let catalog = mj_core::codex_catalog::parse(
            &std::fs::read(staged.path().join("models.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(catalog.slugs(), ["glm-5.3", "glm-5.3-flash"]);
        for model in &catalog.models {
            assert_eq!(
                model["auto_review_model_override"],
                serde_json::Value::from("glm-5.3-flash"),
                "Guardian reviews run on the newest flash model"
            );
        }
        // The key must be top-level, so it precedes the provider table, and the
        // user's own lines survive unchanged.
        let config = std::fs::read_to_string(staged.path().join("config.toml")).unwrap();
        assert!(
            config.starts_with("model_catalog_json = \"models.json\"\n"),
            "{config}"
        );
        assert!(config.ends_with(ZAI_CONFIG), "{config}");
        assert_eq!(
            mj_core::codex_provider::codex_provider(staged.path())
                .unwrap()
                .unwrap()
                .model_catalog_json
                .as_deref(),
            Some(Path::new("models.json")),
            "Codex reads the staged catalog as a top-level key"
        );
        // The staged copy is what the session runs from, so a session on a
        // local bare target must not use the profile home directly.
        assert!(super::super::requires_private_profile_home(&profile));
        assert!(
            !home.path().join("models.json").exists(),
            "the user's own profile home stays untouched"
        );
    }

    #[test]
    fn a_failed_catalog_fetch_falls_back_to_the_last_cached_catalog() {
        let home = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let profile = zai_profile(home.path());
        let cache = IsolatedCatalogCache(store.path().join("cache.sqlite3"));

        stage_codex_catalog(
            "glm",
            &profile,
            staged.path(),
            &|_, _| Ok(ZAI_CATALOG.as_bytes().to_vec()),
            &cache,
        )
        .unwrap();
        std::fs::remove_file(staged.path().join("models.json")).unwrap();

        stage_codex_catalog(
            "glm",
            &profile,
            staged.path(),
            &|_, _| bail!("the provider is unreachable"),
            &cache,
        )
        .expect("a provider outage must not block a launch");
        let catalog = mj_core::codex_catalog::parse(
            &std::fs::read(staged.path().join("models.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(catalog.slugs(), ["glm-5.3", "glm-5.3-flash"]);

        // With nothing cached for a different provider, the launch fails and
        // says which profile and URL could not be reached.
        let empty = tempfile::tempdir().unwrap();
        let error = stage_codex_catalog(
            "glm",
            &profile,
            staged.path(),
            &|_, _| bail!("the provider is unreachable"),
            &IsolatedCatalogCache(empty.path().join("empty.sqlite3")),
        )
        .expect_err("no catalog and no cache cannot launch")
        .to_string();
        assert!(error.contains("glm"), "{error}");
        assert!(error.contains("https://api.z.ai/api/v1/models"), "{error}");
    }

    const DEEPSEEK_CONFIG: &str = "model = \"deepseek-v4-pro\"\n\
                                   model_provider = \"deepseek\"\n\
                                   \n\
                                   [model_providers.deepseek]\n\
                                   base_url = \"https://api.deepseek.com/v1\"\n\
                                   env_key = \"DEEPSEEK_API_KEY\"\n\
                                   wire_api = \"responses\"\n";

    const DEEPSEEK_LIST: &str = r#"{"object":"list","data":[
        {"id":"deepseek-flash","object":"model","owned_by":"deepseek"},
        {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
    ]}"#;

    fn deepseek_profile(home: &Path) -> mj_core::config::HarnessProfile {
        std::fs::write(home.join("config.toml"), DEEPSEEK_CONFIG).unwrap();
        mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.to_path_buf(),
            environment: BTreeMap::from([(
                "DEEPSEEK_API_KEY".to_owned(),
                "deepseek-key".to_owned(),
            )]),
            context_window_bytes: None,
            guardian_review_model: None,
        }
    }

    fn stage_catalog_for(
        profile: &mj_core::config::HarnessProfile,
        body: &str,
        staged: &Path,
        store: &Path,
    ) -> Result<mj_core::codex_catalog::CodexCatalog> {
        stage_codex_catalog(
            "deepseek",
            profile,
            staged,
            &|_, _| Ok(body.as_bytes().to_vec()),
            &IsolatedCatalogCache(store.to_path_buf()),
        )?;
        mj_core::codex_catalog::parse(&std::fs::read(staged.join("models.json")).unwrap())
    }

    #[test]
    fn a_plain_model_list_becomes_a_catalog_the_profiles_overrides_refine() {
        let home = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let profile = deepseek_profile(home.path());
        std::fs::write(
            home.path().join("models.json"),
            r#"{"models":[
                {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high"]},
                {"slug":"deepseek-preview","display_name":"DeepSeek Preview"}
            ]}"#,
        )
        .unwrap();

        let catalog = stage_catalog_for(
            &profile,
            DEEPSEEK_LIST,
            staged.path(),
            &store.path().join("cache.sqlite3"),
        )
        .expect("an OpenAI-format model list stages a catalog");

        assert_eq!(
            catalog.slugs(),
            ["deepseek-flash", "deepseek-v4-pro", "deepseek-preview"],
            "the override adds a model the provider's list omits"
        );
        assert_eq!(
            catalog.models[1]["supported_reasoning_levels"],
            serde_json::json!(["low", "high"]),
            "the override gives the translated entry its reasoning levels"
        );
        assert_eq!(
            catalog.models[0]["auto_review_model_override"],
            serde_json::Value::from("deepseek-flash"),
            "the newest flash model reviews by default"
        );
    }

    #[test]
    fn the_guardian_review_setting_picks_which_model_reviews() {
        let home = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let mut profile = deepseek_profile(home.path());
        let cache = store.path().join("cache.sqlite3");

        profile.guardian_review_model = Some("session".to_owned());
        let staged = tempfile::tempdir().unwrap();
        let catalog =
            stage_catalog_for(&profile, DEEPSEEK_LIST, staged.path(), &cache).expect("stage");
        assert!(
            catalog
                .models
                .iter()
                .all(|model| !model.contains_key("auto_review_model_override")),
            "with \"session\" Codex reviews with the session model, so nothing is stamped"
        );

        profile.guardian_review_model = Some("deepseek-v4-pro".to_owned());
        let staged = tempfile::tempdir().unwrap();
        let catalog =
            stage_catalog_for(&profile, DEEPSEEK_LIST, staged.path(), &cache).expect("stage");
        for model in &catalog.models {
            assert_eq!(
                model["auto_review_model_override"],
                serde_json::Value::from("deepseek-v4-pro"),
                "a named slug reviews whichever model the session runs on"
            );
        }

        profile.guardian_review_model = Some("deepseek-nonesuch".to_owned());
        let staged = tempfile::tempdir().unwrap();
        let error = stage_catalog_for(&profile, DEEPSEEK_LIST, staged.path(), &cache)
            .expect_err("a reviewer the provider does not serve cannot review")
            .to_string();
        assert!(error.contains("deepseek-nonesuch"), "{error}");
        assert!(error.contains("deepseek"), "{error}");
        assert!(error.contains("deepseek-flash"), "{error}");
        assert!(
            !staged.path().join("models.json").exists(),
            "a rejected reviewer stages no catalog at all"
        );
    }

    #[test]
    fn a_native_codex_profile_gets_no_generated_catalog() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let staged = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();
        stage_codex_catalog(
            "work",
            &profile,
            staged.path(),
            &|_, _| panic!("a profile with no custom provider must not fetch a catalog"),
            &IsolatedCatalogCache(store.path().join("cache.sqlite3")),
        )
        .unwrap();

        assert!(!staged.path().join("models.json").exists());
        assert_eq!(
            std::fs::read_to_string(staged.path().join("config.toml")).unwrap(),
            "model = \"gpt-5.5\"\n"
        );
        assert!(!super::super::requires_private_profile_home(&profile));
    }

    #[test]
    fn stage_grok_profile_copies_authentication_and_agent_identity() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            "{\"https://auth.x.ai::1\":{}}",
        )
        .unwrap();
        std::fs::write(home.path().join("agent_id"), "stable-agent-id").unwrap();
        std::fs::write(home.path().join("config.toml"), "model = \"grok-4.6\"\n").unwrap();
        // Native session storage is checkpointed, never staged.
        std::fs::create_dir(home.path().join("sessions")).unwrap();
        std::fs::write(home.path().join("sessions/session_search.sqlite"), "x").unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Grok,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(staged.path().join("agent_id")).unwrap(),
            "stable-agent-id"
        );
        assert!(staged.path().join("auth.json").is_file());
        assert!(staged.path().join("config.toml").is_file());
        assert!(!staged.path().join("sessions").exists());
    }
    #[test]
    fn stage_claude_profile_preserves_rollout_identity() {
        let home = tempfile::tempdir().unwrap();
        let identity = r#"{
                "machineID": "stable-machine",
                "userID": "stable-user",
                "cachedGrowthBookFeatures": {
                    "tengu_velvet_mallet_fable_5": true
                }
            }"#;
        std::fs::write(home.path().join(".claude.json"), identity).unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Claude,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(staged.path().join(".claude.json")).unwrap(),
            identity
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_claude_profile_follows_symlinked_entries() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("settings.json"), "{\"model\":\"opus\"}").unwrap();
        std::fs::write(outside.path().join("CLAUDE.md"), "# linked instructions\n").unwrap();
        let skills = outside.path().join("skills");
        std::fs::create_dir_all(skills.join("review")).unwrap();
        std::fs::write(skills.join("review/SKILL.md"), "review skill\n").unwrap();
        // A dangling link inside a copied tree must not fail staging.
        std::os::unix::fs::symlink(outside.path().join("missing"), skills.join("dangling.md"))
            .unwrap();

        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("settings.json"),
            home.path().join("settings.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("CLAUDE.md"),
            home.path().join("CLAUDE.md"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&skills, home.path().join("skills")).unwrap();

        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Claude,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        for (relative, contents) in [
            ("settings.json", "{\"model\":\"opus\"}"),
            ("CLAUDE.md", "# linked instructions\n"),
            ("skills/review/SKILL.md", "review skill\n"),
        ] {
            let path = staged.path().join(relative);
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(
                metadata.file_type().is_file(),
                "{relative} should be staged as a regular file"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
        assert!(!staged.path().join("skills/dangling.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn stage_claude_profile_skips_dangling_allowlist_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("missing"),
            home.path().join("CLAUDE.md"),
        )
        .unwrap();
        std::fs::write(home.path().join("settings.json"), "{}").unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Claude,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        assert!(!staged.path().join("CLAUDE.md").exists());
        assert!(staged.path().join("settings.json").is_file());
    }

    fn staged_muse_settings(body: &str) -> (tempfile::TempDir, PathBuf) {
        let staged = tempfile::tempdir().unwrap();
        let path = staged.path().join("settings.json");
        std::fs::write(&path, body).unwrap();
        (staged, path)
    }

    fn stage_muse_settings(profile_stage: &Path) {
        apply_staged_execution_setting(
            HarnessKind::Muse,
            ExecutionPolicy::Unconstrained,
            profile_stage,
        )
        .unwrap();
    }

    #[test]
    fn muse_staged_settings_select_the_unrestricted_profile() {
        let (staged, path) = staged_muse_settings(
            r#"{
                "schema_version": 1,
                "provider": "anthropic",
                "model": "muse-1",
                "tui": {"theme": "dark"},
                "permissions": {"schema_version": 1, "default_profile": ":auto-review"}
            }"#,
        );

        stage_muse_settings(staged.path());

        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(document["provider"], "anthropic");
        assert_eq!(document["model"], "muse-1");
        assert_eq!(document["tui"]["theme"], "dark");
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["permissions"]["schema_version"], 1);
        assert_eq!(document["permissions"]["default_profile"], ":unrestricted");
    }

    #[test]
    fn muse_staged_settings_are_created_when_absent() {
        let staged = tempfile::tempdir().unwrap();

        stage_muse_settings(staged.path());

        let body = std::fs::read_to_string(staged.path().join("settings.json")).unwrap();
        assert!(body.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "permissions": {"schema_version": 1, "default_profile": ":unrestricted"}
            })
        );
    }

    #[test]
    fn a_harness_without_a_staged_setting_leaves_the_profile_untouched() {
        let source = r#"{"schema_version": 1, "permissions": {"default_profile": ":ask-me"}}"#;

        for (kind, policy) in [
            (HarnessKind::Claude, ExecutionPolicy::Unconstrained),
            (HarnessKind::Muse, ExecutionPolicy::ConfiguredApprovals),
        ] {
            let (staged, path) = staged_muse_settings(source);

            apply_staged_execution_setting(kind, policy, staged.path()).unwrap();

            assert_eq!(std::fs::read_to_string(&path).unwrap(), source, "{kind:?}");
        }
    }

    #[test]
    fn muse_settings_that_are_not_an_object_report_the_staged_file() {
        let (staged, path) = staged_muse_settings("[]");

        let error = apply_staged_execution_setting(
            HarnessKind::Muse,
            ExecutionPolicy::Unconstrained,
            staged.path(),
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains(&path.display().to_string()),
            "error should name the staged file: {error:#}"
        );
    }

    #[test]
    fn a_custom_provider_session_carries_its_key_and_runs_from_a_private_home() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let profile = zai_profile(home.path());
        let mut session = crate::controller::test_support::checkpoint_test_session("s-glm");
        session.harness_kind = HarnessKind::Codex;
        session.last_profile = "glm".into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some(project.path().to_path_buf());
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/worker".into(),
        });

        let (launch, _, target_home) = worker_launch_config(
            &session,
            &profile,
            None,
            &targets::TargetLocator::LocalBare {
                worker_root: "/home/me/.local/share/hel/worker".into(),
            },
            &session.id,
            &session.id,
            &mj_core::config::TargetTemplate::LocalBare,
        )
        .unwrap();

        assert_eq!(launch.environment["ZAI_API_KEY"], "coding-plan-key");
        assert_eq!(launch.environment["CODEX_HOME"], target_home);
        assert_eq!(
            target_home, "/home/me/.local/share/hel/worker/profile",
            "the session runs from the staged copy, not the user's profile home"
        );
        assert_eq!(
            launch.authentication_marker.as_deref(),
            Some("config.toml"),
            "the worker checks the Codex configuration, not a ChatGPT auth file"
        );
        // Guardian still applies: a raw local target keeps configured approvals.
        assert_eq!(
            launch.execution_policy,
            ExecutionPolicy::ConfiguredApprovals
        );
        assert_eq!(launch.environment["INITIAL_AGENT_MODE"], "agent");
        assert!(profile.supports_guardian_approvals());
    }

    /// Muse has no guardian mode, so even a raw local target launches it
    /// unconstrained.
    #[test]
    fn raw_local_muse_launches_unconstrained() {
        let project = tempfile::tempdir().unwrap();
        let mut session = crate::controller::test_support::checkpoint_test_session("s-muse");
        session.harness_kind = HarnessKind::Muse;
        session.last_profile = "muse".into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some(project.path().to_path_buf());
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/worker".into(),
        });
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: HarnessKind::Muse,
            home: PathBuf::from("/profiles/muse"),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        let (launch, _, _) = worker_launch_config(
            &session,
            &profile,
            None,
            &targets::TargetLocator::LocalBare {
                worker_root: "/home/me/.local/share/hel/worker".into(),
            },
            &session.id,
            &session.id,
            &mj_core::config::TargetTemplate::LocalBare,
        )
        .unwrap();

        assert_eq!(launch.execution_policy, ExecutionPolicy::Unconstrained);
        assert_eq!(launch.environment["MUSE_APPROVAL_MODE"], "allowAll");
        assert_eq!(launch.environment["MUSE_SERVE_ARGS"], "--disable-sandbox");
    }

    #[test]
    fn stage_kimi_profile_preserves_device_identity() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "default_model = \"k3\"\n").unwrap();
        std::fs::write(home.path().join("device_id"), "stable-device-id").unwrap();
        std::fs::create_dir(home.path().join("credentials")).unwrap();
        std::fs::write(
            home.path().join("credentials/kimi-code.json"),
            "{\"access_token\":\"secret\"}",
        )
        .unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Kimi,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(staged.path().join("device_id")).unwrap(),
            "stable-device-id"
        );
        assert!(staged.path().join("credentials/kimi-code.json").is_file());
    }
    #[test]
    fn staged_kimi_profile_binds_project_memory_to_the_target_runtime() {
        let home = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "mcpServers": {
                "user-server": {
                    "command": "user-mcp",
                    "args": ["serve"]
                }
            },
            "userSetting": true
        });
        let original_body = serde_json::to_vec_pretty(&original).unwrap();
        std::fs::write(home.path().join("mcp.json"), &original_body).unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Kimi,
            home: home.path().to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };
        stage_profile(&profile, staged.path()).unwrap();
        let memory = ProjectMemoryLaunchConfig {
            project_key: "project".into(),
            root: "/var/lib/hel/profiles/session/projects/project/memory".into(),
            baseline_root: PathBuf::new(),
            repository_roots: BTreeMap::new(),
            mcp_delivery: ProjectMemoryMcpDelivery::HarnessProfile,
        };

        configure_kimi_project_memory_mcp(staged.path(), "/var/lib/hel/workers/session", &memory)
            .unwrap();

        let configured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staged.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(configured["userSetting"], true);
        assert_eq!(
            configured["mcpServers"]["user-server"]["command"],
            "user-mcp"
        );
        assert_eq!(
            configured["mcpServers"]["mj-memory"],
            serde_json::json!({
                "transport": "stdio",
                "command": "/var/lib/hel/workers/session/hel",
                "args": [
                    "worker",
                    "memory-mcp",
                    "--root",
                    "/var/lib/hel/profiles/session/projects/project/memory"
                ],
                "runtime_id": "local"
            })
        );
        assert_eq!(
            std::fs::read(home.path().join("mcp.json")).unwrap(),
            original_body,
            "the controller-side Kimi profile must remain unchanged"
        );
    }

    #[test]
    fn staged_kimi_project_memory_resolves_ssh_paths_from_target_home() {
        let staged = tempfile::tempdir().unwrap();
        let memory = ProjectMemoryLaunchConfig {
            project_key: "project".into(),
            root: ".local/share/hel/profiles/session/projects/project/memory".into(),
            baseline_root: PathBuf::new(),
            repository_roots: BTreeMap::new(),
            mcp_delivery: ProjectMemoryMcpDelivery::HarnessProfile,
        };

        configure_kimi_project_memory_mcp(
            staged.path(),
            ".local/share/hel/workers/session",
            &memory,
        )
        .unwrap();

        let configured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staged.path().join("mcp.json")).unwrap())
                .unwrap();
        let server = &configured["mcpServers"]["mj-memory"];
        assert_eq!(server["command"], "sh");
        assert_eq!(server["runtime_id"], "local");
        assert_eq!(
            server["args"],
            serde_json::json!([
                "-c",
                "exec \"$HOME/$1\" worker memory-mcp --root \"$HOME/$2\"",
                "mj-memory",
                ".local/share/hel/workers/session/hel",
                ".local/share/hel/profiles/session/projects/project/memory"
            ])
        );
    }
    #[test]
    fn disposable_container_guidance_reaches_each_harness_without_touching_home() {
        let target = targets::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
        };
        for (kind, instructions) in [
            (mj_core::config::HarnessKind::Codex, "AGENTS.md"),
            (mj_core::config::HarnessKind::Claude, "CLAUDE.md"),
            (mj_core::config::HarnessKind::Kimi, "AGENTS.md"),
            (mj_core::config::HarnessKind::Grok, "AGENTS.md"),
            (mj_core::config::HarnessKind::Muse, "AGENTS.md"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let original = "# Controller instructions\n\nKeep this source unchanged.\n";
            let source_instructions = home.path().join(instructions);
            std::fs::write(&source_instructions, original).unwrap();
            let staged = tempfile::tempdir().unwrap();
            let profile = mj_core::config::HarnessProfile {
                enabled: true,
                kind,
                home: home.path().to_path_buf(),
                environment: std::collections::BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            };

            stage_profile(&profile, staged.path()).unwrap();
            append_hel_target_environment(kind, staged.path(), &target).unwrap();

            let guidance = std::fs::read_to_string(staged.path().join(instructions)).unwrap();
            assert_eq!(
                guidance,
                format!("{original}\n{MJ_CONTAINER_ENVIRONMENT}"),
                "{instructions} receives the section in the staged profile"
            );
            assert!(guidance.contains("## Mjolnir disposable environment"));
            assert!(!guidance.contains("## Hel disposable environment"));
            assert_eq!(
                std::fs::read_to_string(source_instructions).unwrap(),
                original,
                "{instructions} in the controller-side home stays untouched"
            );
        }
    }
    #[test]
    fn kimi_guidance_uses_agents_md_without_mutating_the_system_override() {
        let home = tempfile::tempdir().unwrap();
        let system_override = "# Custom Kimi system prompt\n";
        std::fs::write(home.path().join("SYSTEM.md"), system_override).unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Kimi,
            home: home.path().to_path_buf(),
            environment: std::collections::BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();
        append_hel_target_environment(
            profile.kind,
            staged.path(),
            &targets::TargetLocator::LocalPodman {
                container_id: "container".into(),
                workspace_storage: Default::default(),
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(staged.path().join("AGENTS.md")).unwrap(),
            MJ_CONTAINER_ENVIRONMENT
        );
        assert_eq!(
            std::fs::read_to_string(staged.path().join("SYSTEM.md")).unwrap(),
            system_override
        );
        assert!(!home.path().join("AGENTS.md").exists());
        assert_eq!(
            std::fs::read_to_string(home.path().join("SYSTEM.md")).unwrap(),
            system_override
        );
    }

    #[test]
    fn ec2_guidance_names_its_real_workspace_and_ssh_bare_gets_none() {
        let ec2 = tempfile::tempdir().unwrap();
        append_hel_target_environment(
            mj_core::config::HarnessKind::Codex,
            ec2.path(),
            &targets::TargetLocator::AwsEc2 {
                profile: "profile".into(),
                region: "region".into(),
                instance_id: "instance".into(),
                ssh: targets::SshTarget {
                    destination: "host".into(),
                    ssh_args: Vec::new(),
                },
                workspace: ".local/share/hel/workspaces/session".into(),
            },
        )
        .unwrap();
        let guidance = std::fs::read_to_string(ec2.path().join("AGENTS.md")).unwrap();
        assert_eq!(
            guidance,
            "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/.local/share/hel/workspaces/session`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/.local/share/hel/workspaces/session`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"
        );
        assert!(!guidance.contains("## Hel disposable environment"));

        let ssh_bare = tempfile::tempdir().unwrap();
        append_hel_target_environment(
            mj_core::config::HarnessKind::Codex,
            ssh_bare.path(),
            &targets::TargetLocator::SshBare {
                worker_id: None,
                ssh: targets::SshTarget {
                    destination: "host".into(),
                    ssh_args: Vec::new(),
                },
                workspace: ".local/share/hel/workspaces/session".into(),
            },
        )
        .unwrap();
        assert!(!ssh_bare.path().join("AGENTS.md").exists());
    }

    #[test]
    fn project_memory_replicas_are_session_private() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            project_memory_replica_slug(key, "session-a"),
            "hel-0123456789abcdef-session-a"
        );
        assert_ne!(
            project_memory_replica_slug(key, "session-a"),
            project_memory_replica_slug(key, "session-b")
        );
    }

    /// Returns a fixed digest line for every command and records what it ran,
    /// so a remote refresh can be driven without a real ssh host.
    struct DigestExecutor {
        installed_line: String,
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for DigestExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: self.installed_line.clone().into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    // The SshBare worker_root guard requires the workspace to end in the exact
    // session ID, so build the locator around the session under test.
    fn ssh_bare_locator(session_id: &str) -> targets::TargetLocator {
        targets::TargetLocator::SshBare {
            worker_id: None,
            ssh: SshTarget {
                destination: "user@host.test".into(),
                ssh_args: Vec::new(),
            },
            workspace: format!("/srv/mj/{session_id}"),
        }
    }

    #[test]
    fn remote_upgrade_prepares_managed_harness_without_touching_running_worker() {
        let session = "session-remote";
        let executor = DigestExecutor {
            installed_line: String::new(),
            commands: RefCell::new(Vec::new()),
        };
        let launch = WorkerLaunchConfig {
            subagent_tools: false,
            goal_resume_request: Default::default(),
            target_environment: Default::default(),
            run_mode: Default::default(),
            session_id: session.into(),
            harness: HarnessKind::Codex,
            authentication_marker: None,
            bridge_command: "ignored".into(),
            bridge_args: Vec::new(),
            harness_runtime: HarnessRuntimePolicy::Managed,
            environment: BTreeMap::new(),
            cwd: "/srv/mj/session-remote/project".into(),
            additional_directories: Vec::new(),
            native_session_id: None,
            project_memory: None,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
        };

        prepare_managed_harness_for_upgrade(
            &executor,
            &ssh_bare_locator(session),
            session,
            Path::new("/controller/hel"),
            &launch,
        )
        .unwrap();

        let commands = executor.commands.borrow();
        let purposes = commands
            .iter()
            .map(|command| command.purpose.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            purposes,
            vec![
                "clear managed harness preparation staging",
                "create managed harness preparation staging",
                "stage current worker for managed harness preparation",
                "stage managed harness launch configuration",
                "make managed harness preparation worker executable",
                "prepare exact managed harness",
                "remove managed harness preparation staging",
            ]
        );
        assert!(commands.iter().all(|command| {
            !command.purpose.contains("stop Mjolnir worker")
                && !command.purpose.contains("start Mjolnir worker")
                && !command
                    .purpose
                    .contains("install the current Mjolnir worker binary")
        }));
        let prepare = commands
            .iter()
            .find(|command| command.purpose == "prepare exact managed harness")
            .unwrap();
        let rendered = format!("{} {}", prepare.program, prepare.args.join(" "));
        assert!(rendered.contains("worker' 'prepare-harness' '--config'"));
    }

    #[test]
    fn local_upgrade_preflight_uses_current_binary_and_preserves_launch_policy() {
        struct ConfigRecordingExecutor {
            command: RefCell<Option<CommandSpec>>,
            launch: RefCell<Option<WorkerLaunchConfig>>,
        }

        impl CommandExecutor for ConfigRecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                let config_path = command
                    .args
                    .get(3)
                    .context("local prepare command did not include its config path")?;
                *self.command.borrow_mut() = Some(command.clone());
                *self.launch.borrow_mut() = Some(WorkerLaunchConfig::read(Path::new(config_path))?);
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        struct FailingExecutor {
            purposes: RefCell<Vec<String>>,
        }

        impl CommandExecutor for FailingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.purposes.borrow_mut().push(command.purpose.clone());
                Err(anyhow::anyhow!("managed harness installation failed"))
            }
        }

        let executor = ConfigRecordingExecutor {
            command: RefCell::new(None),
            launch: RefCell::new(None),
        };
        let launch = WorkerLaunchConfig {
            subagent_tools: false,
            goal_resume_request: Default::default(),
            target_environment: Default::default(),
            run_mode: Default::default(),
            session_id: "session-local".into(),
            harness: HarnessKind::Codex,
            authentication_marker: None,
            bridge_command: "ignored".into(),
            bridge_args: Vec::new(),
            harness_runtime: HarnessRuntimePolicy::Managed,
            environment: BTreeMap::from([("CODEX_HOME".into(), "/configured/profile/home".into())]),
            cwd: "/workspace/project".into(),
            additional_directories: Vec::new(),
            native_session_id: None,
            project_memory: None,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
        };
        let locator = targets::TargetLocator::LocalBare {
            worker_root: "/worker/session-local".into(),
        };

        prepare_managed_harness_for_upgrade(
            &executor,
            &locator,
            "session-local",
            Path::new("/controller/hel"),
            &launch,
        )
        .unwrap();

        {
            let command = executor.command.borrow();
            let command = command.as_ref().unwrap();
            assert_eq!(command.purpose, "prepare exact managed harness");
            assert_eq!(command.program, "/controller/hel");
            assert_eq!(
                &command.args[..3],
                ["worker", "prepare-harness", "--config"]
            );
            assert!(!command.args[3].contains("/worker/session-local"));
        }

        let prepared = executor.launch.borrow();
        let prepared = prepared.as_ref().unwrap();
        assert_eq!(
            prepared.environment.get("CODEX_HOME").map(String::as_str),
            Some("/configured/profile/home")
        );
        assert_eq!(
            prepared.execution_policy,
            ExecutionPolicy::ConfiguredApprovals
        );

        let failing = FailingExecutor {
            purposes: RefCell::new(Vec::new()),
        };
        let error = prepare_managed_harness_for_upgrade(
            &failing,
            &locator,
            "session-local",
            Path::new("/controller/hel"),
            &launch,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("managed harness installation failed")
        );
        assert_eq!(
            failing.purposes.borrow().as_slice(),
            ["prepare exact managed harness"]
        );
    }

    #[test]
    fn initial_bare_provision_prepares_the_harness_from_installed_files() {
        let session = "session-remote";
        let executor = DigestExecutor {
            installed_line: String::new(),
            commands: RefCell::new(Vec::new()),
        };
        let mut launch = WorkerLaunchConfig {
            subagent_tools: false,
            goal_resume_request: Default::default(),
            target_environment: Default::default(),
            run_mode: Default::default(),
            session_id: session.into(),
            harness: HarnessKind::Kimi,
            authentication_marker: None,
            bridge_command: "ignored".into(),
            bridge_args: Vec::new(),
            harness_runtime: HarnessRuntimePolicy::Managed,
            environment: BTreeMap::new(),
            cwd: "/srv/mj/session-remote/project".into(),
            additional_directories: Vec::new(),
            native_session_id: None,
            project_memory: None,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
        };

        let locator = ssh_bare_locator(session);
        prepare_installed_managed_harness(&executor, &locator, "/worker/root", &launch).unwrap();
        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].purpose,
            "prepare exact managed harness before worker startup"
        );
        let rendered = format!("{} {}", commands[0].program, commands[0].args.join(" "));
        assert!(rendered.contains("'/worker/root/hel' 'worker' 'prepare-harness'"));
        drop(commands);

        let local = targets::TargetLocator::LocalBare {
            worker_root: "/worker/session-remote".into(),
        };
        prepare_installed_managed_harness(&executor, &local, "/worker/session-remote", &launch)
            .unwrap();
        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].program, "/worker/session-remote/hel");
        assert_eq!(
            commands[1].args,
            vec![
                "worker".to_owned(),
                "prepare-harness".to_owned(),
                "--config".to_owned(),
                "/worker/session-remote/launch.json".to_owned(),
            ]
        );
        drop(commands);

        launch.harness_runtime = HarnessRuntimePolicy::Ambient;
        prepare_installed_managed_harness(&executor, &locator, "/worker/root", &launch).unwrap();
        assert_eq!(executor.commands.borrow().len(), 2);
    }

    #[test]
    fn a_remote_worker_with_a_mismatched_binary_is_replaced_before_restart() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("worker");
        std::fs::write(&source, b"fresh musl worker").unwrap();
        let executor = DigestExecutor {
            installed_line: format!("{}  /root/hel\n", "0".repeat(64)),
            commands: RefCell::new(Vec::new()),
        };
        let replaced = replace_remote_worker_binary_if_stale(
            &executor,
            &ssh_bare_locator("session-remote"),
            "session-remote",
            &CommandSpec::new("true", Vec::<String>::new()),
            &source,
        )
        .unwrap();
        assert!(replaced, "a stale remote binary must be replaced");
        assert!(
            executor.commands.borrow().len() > 1,
            "the digest probe must be followed by replacement commands"
        );
    }

    #[test]
    fn a_remote_worker_already_current_is_restarted_without_recopying() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("worker");
        std::fs::write(&source, b"fresh musl worker").unwrap();
        let current = mj_core::worker_launch::worker_executable_digest(&source).unwrap();
        let executor = DigestExecutor {
            installed_line: format!("{current}  /root/hel\n"),
            commands: RefCell::new(Vec::new()),
        };
        let replaced = replace_remote_worker_binary_if_stale(
            &executor,
            &ssh_bare_locator("session-remote"),
            "session-remote",
            &CommandSpec::new("true", Vec::<String>::new()),
            &source,
        )
        .unwrap();
        assert!(!replaced, "a current remote binary must not be recopied");
        assert_eq!(
            executor.commands.borrow().len(),
            1,
            "only the digest probe runs when the binary is already current"
        );
    }

    #[test]
    fn a_remote_recovery_plan_defers_binary_refresh_to_the_recovery_task() {
        let locator = ssh_bare_locator("session-remote");
        let refresh = worker_binary_refresh_plan(&locator, "session-remote")
            .unwrap()
            .expect("a remote target now gets a binary refresh");
        match refresh {
            WorkerBinaryRefresh::Remote(remote) => {
                assert_eq!(remote.session_id, "session-remote");
                assert_eq!(remote.locator, locator);
            }
            WorkerBinaryRefresh::Prepared(_) => {
                panic!("a remote target must defer, not prepare, its binary refresh")
            }
        }
    }
}

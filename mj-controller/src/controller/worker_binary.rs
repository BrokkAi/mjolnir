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
mod tests;

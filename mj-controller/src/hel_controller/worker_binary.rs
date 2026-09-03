//! Worker binary acquisition, profile staging, and worker installation.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::hel_session_manager::{
    ProjectMemorySyncTarget, RemoteWorkerBinaryRefresh, WorkerBinaryRefresh,
    WorkerBinaryRefreshPlan, WorkerLaunchRefreshPlan, WorkerRecoveryPlan,
};
use hel::hel_config::{ExecutionPolicy, ProjectBundle, ProjectRepository, atomic_write, data_dir};
use hel::hel_project_memory::{ProjectMemoryIdentity, RepositoryMemoryIdentity};
use hel::hel_targets::{
    self, CommandExecutor, CommandPlan, CommandSpec, ProcessExecutor, ProvisionStage, SshTarget,
};
use hel::hel_worker_launch::{
    DISCOVER_LOGIN_PATH_ENV, ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery,
    WorkerLaunchConfig, WorkerOwnership,
};

use super::backend::backend_locator;
use super::readiness::WORKER_EXIT_RECORD_MARKER;
use super::{Controller, execute_checked, scp_command_spec, ssh_command_spec, target_profile_home};

impl Controller {
    /// Where this session's worker lives. This is decided from the session
    /// record and configuration alone, so a caller can name the worker root
    /// before anything is installed into it.
    pub(super) fn worker_placement(
        &self,
        session_id: &str,
    ) -> Result<(hel_targets::TargetLocator, String)> {
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
        let worker_root = hel_targets::worker_root(&backend, session_id)?;
        Ok((backend, worker_root))
    }

    pub(super) fn prepare_worker_files(
        &self,
        session_id: &str,
        backend: &hel_targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
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
        let (launch, project_memory, target_profile_home) = worker_launch_config(
            session,
            profile,
            bundle,
            backend,
            session_id,
            target.execution_policy(),
        )?;

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
        if !matches!(backend, hel_targets::TargetLocator::LocalBare { .. }) {
            let started = Instant::now();
            let result = stage_profile(profile, &profile_stage);
            tracing::debug!(
                session_id,
                elapsed_ms = started.elapsed().as_millis(),
                "profile staging completed"
            );
            result?;
            append_hel_target_environment(profile.kind, &profile_stage, backend)?;
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
        )
    }

    /// Collect the dead worker's exit record and log tail for a session whose
    /// worker has become unreachable. Best-effort; returns None when the
    /// target no longer exists or has no diagnostics.
    pub fn diagnose_worker(&self, session_id: &str) -> Option<String> {
        self.diagnose_worker_controlled(session_id, &ProcessExecutor)
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
        let worker_root = match hel_targets::worker_root(&backend, session_id) {
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
        worker_last_words(executor, &backend, &worker_root)
    }

    /// A non-destructive liveness probe plus commands that replace a confirmed
    /// dead session worker without touching its durable relay files. The
    /// session manager runs both off its async actor.
    pub fn worker_recovery_plan(&self, session_id: &str) -> Result<WorkerRecoveryPlan> {
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
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
        let (launch, _, _) = worker_launch_config(
            session,
            profile,
            bundle,
            &backend,
            session_id,
            target.execution_policy(),
        )?;
        Ok(WorkerRecoveryPlan {
            target: hel_targets::target_recovery_plan(&backend, session_id)?,
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

    pub fn project_memory_sync_target(&self, session_id: &str) -> Result<ProjectMemorySyncTarget> {
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

fn worker_launch_config(
    session: &hel::hel_state::SessionRecord,
    profile: &hel::hel_config::HarnessProfile,
    bundle: Option<&ProjectBundle>,
    backend: &hel_targets::TargetLocator,
    session_id: &str,
    execution_policy: ExecutionPolicy,
) -> Result<(WorkerLaunchConfig, ProjectMemoryLaunchConfig, String)> {
    let target_profile_home = target_profile_home(backend, session_id, profile);
    let workspace = if let Some(project_directory) = &session.project_directory {
        (project_directory.to_string_lossy().into_owned(), Vec::new())
    } else {
        workspace_paths(
            backend,
            bundle.context("session bundle is missing")?,
            session_id,
        )?
    };
    let mut additional_directories = workspace.1.iter().map(PathBuf::from).collect::<Vec<_>>();
    additional_directories.extend(
        session
            .additional_mounts
            .iter()
            .map(|resource| resource.destination.clone()),
    );
    if profile.kind == hel::hel_config::HarnessKind::Deepseek && !additional_directories.is_empty()
    {
        bail!(
            "DeepSeek Harness ACP does not support multiple workspace roots; use a single-repository bundle"
        );
    }
    let (bridge_command, bridge_args) = bridge_launch(
        profile.kind,
        profile.executable.as_deref(),
        execution_policy,
    );
    let mut environment = profile.environment.clone();
    environment.insert(profile.home_env().into(), target_profile_home.clone());
    profile
        .kind
        .configure_execution_environment(execution_policy, &mut environment);
    configure_login_path_discovery(&mut environment, backend);
    let mut project_memory =
        project_memory_launch(session, bundle, &workspace, &target_profile_home)?;
    project_memory.mcp_delivery = project_memory_mcp_delivery(profile.kind, backend);
    if profile.kind == hel::hel_config::HarnessKind::Claude {
        environment.insert(
            "CLAUDE_CODE_PROJECT_DIR_NAME".into(),
            project_memory_replica_slug(&project_memory.project_key, session_id),
        );
    }
    apply_claude_setup_token(
        &mut environment,
        profile.kind,
        &hel::hel_credentials::claude_oauth_token_path(&session.last_profile),
    );
    Ok((
        WorkerLaunchConfig {
            session_id: session_id.to_string(),
            harness: profile.kind,
            bridge_command: PathBuf::from(bridge_command),
            bridge_args,
            environment,
            cwd: PathBuf::from(&workspace.0),
            additional_directories,
            native_session_id: session.native_session_id.clone(),
            project_memory: Some(project_memory.clone()),
            execution_policy,
        },
        project_memory,
        target_profile_home,
    ))
}

/// Hand a Claude worker the profile's long-lived setup token, when it has one.
///
/// Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN` ahead of the `/login`
/// credentials file, and a setup token does not rotate, so a container copy
/// cannot lose the single-use refresh race with the host. A profile that sets
/// the variable itself stays authoritative.
fn apply_claude_setup_token(
    environment: &mut std::collections::BTreeMap<String, String>,
    kind: hel::hel_config::HarnessKind,
    token_path: &Path,
) {
    use hel::hel_credentials::CLAUDE_OAUTH_TOKEN_ENV;

    if kind != hel::hel_config::HarnessKind::Claude
        || environment.contains_key(CLAUDE_OAUTH_TOKEN_ENV)
    {
        return;
    }
    match hel::hel_credentials::read_claude_oauth_token(token_path) {
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

fn configure_login_path_discovery(
    environment: &mut std::collections::BTreeMap<String, String>,
    backend: &hel_targets::TargetLocator,
) {
    environment.remove(DISCOVER_LOGIN_PATH_ENV);
    if !environment.contains_key("PATH")
        && matches!(
            backend,
            hel_targets::TargetLocator::LocalBare { .. }
                | hel_targets::TargetLocator::AwsEc2 { .. }
                | hel_targets::TargetLocator::SshBare { .. }
        )
    {
        environment.insert(DISCOVER_LOGIN_PATH_ENV.into(), "1".into());
    }
}

fn project_memory_launch(
    session: &hel::hel_state::SessionRecord,
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
            Some(hel::hel_state::TargetLocator::LocalBare { .. }) => {
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
    harness: hel::hel_config::HarnessKind,
    target: &hel_targets::TargetLocator,
) -> ProjectMemoryMcpDelivery {
    if harness == hel::hel_config::HarnessKind::Kimi
        && !matches!(target, hel_targets::TargetLocator::LocalBare { .. })
    {
        ProjectMemoryMcpDelivery::HarnessProfile
    } else {
        ProjectMemoryMcpDelivery::Acp
    }
}

fn configured_memory_identity(repository: &ProjectRepository) -> Result<RepositoryMemoryIdentity> {
    if let Some(source) = repository.github.as_deref() {
        let github = crate::hel_setup::github_repository_from_origin(source)
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
        canonical_root: hel::hel_local_git::main_worktree_root(root)
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
    let mut document = match std::fs::read(&path) {
        Ok(body) => serde_json::from_slice::<serde_json::Value>(&body)
            .with_context(|| format!("parse staged Kimi MCP configuration {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read staged Kimi MCP configuration {}", path.display()));
        }
    };
    let root = document.as_object_mut().with_context(|| {
        format!(
            "staged Kimi MCP configuration {} must contain a JSON object",
            path.display()
        )
    })?;
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
                "mj-project-memory",
                worker,
                memory_root
            ],
            "runtime_id": "local"
        })
    };
    servers.insert("mj-project-memory".into(), server);
    let mut body = serde_json::to_vec_pretty(&document)?;
    body.push(b'\n');
    atomic_write(&path, &body)
        .with_context(|| format!("write staged Kimi MCP configuration {}", path.display()))
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

fn packaged_worker_binary_path(directory: &Path, triple: &str) -> PathBuf {
    directory.join(format!("mj-worker-{triple}"))
}

/// File names a worker binary may carry when it sits beside the controller or
/// in a development sibling directory. The controller's own file name comes
/// first (after the 2.0 rename that is `mj`), then the legacy `hel` name that
/// older packages shipped, so both resolve without hardcoding one.
fn worker_sibling_names(controller: &Path) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut names = Vec::new();
    if let Some(own) = controller.file_name() {
        names.push(own.to_os_string());
    }
    let legacy = OsString::from("hel");
    if !names.contains(&legacy) {
        names.push(legacy);
    }
    names
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
        for name in &names {
            candidates.push((
                target_dir.join(triple).join(profile).join(name),
                "development musl sibling",
            ));
        }
    }
    // Worker installed in the controller's own directory (released layout,
    // including the legacy `hel` name).
    for name in &names {
        candidates.push((directory.join(name), "beside the running executable"));
    }
    candidates.into_iter().find(|(path, _)| is_file(path))
}

/// Find a worker source without downloading it.
///
/// Container provisioning resolves this after discovering the target
/// architecture. Doctor uses the same lookup with the selected container's
/// expected architecture, so it can recommend a fix without creating a
/// container or making a network request.
pub fn worker_binary_prerequisite_for_arch(arch: &str) -> Result<WorkerBinaryAvailability> {
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    worker_binary_prerequisite_for_current(arch, &current, &|path| path.is_file())
}

/// The lookup itself, with the controller's own path and the file probe passed
/// in so both can be exercised without the machine they describe.
fn worker_binary_prerequisite_for_current(
    arch: &str,
    current: &Path,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<WorkerBinaryAvailability> {
    let triple = format!("{arch}-unknown-linux-musl");
    if let Some(path) = hel::hel_config::env_override_os("WORKER_BINARY").map(PathBuf::from) {
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
    if let Some(directory) = hel::hel_config::env_override_os("WORKER_DIR").map(PathBuf::from) {
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
    // The native branches survive a replaced controller: they copy
    // /proc/self/exe, which still names the running image.
    if cfg!(all(target_os = "linux", target_env = "musl"))
        && ((arch == "x86_64" && cfg!(target_arch = "x86_64"))
            || (arch == "aarch64" && cfg!(target_arch = "aarch64")))
    {
        return Ok(WorkerBinaryAvailability::Local {
            path: stable_running_executable(current)?,
            source: "native musl mj binary".into(),
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
    if cfg!(target_os = "linux")
        && ((arch == "x86_64" && cfg!(target_arch = "x86_64"))
            || (arch == "aarch64" && cfg!(target_arch = "aarch64")))
    {
        return Ok(WorkerBinaryAvailability::Local {
            path: stable_running_executable(current)?,
            source: "native Linux mj binary".into(),
        });
    }
    if let Some(template) = hel::hel_config::env_override("WORKER_URL") {
        let expected = hel::hel_config::env_override("WORKER_SHA256")
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
fn template_architecture(template: &hel::hel_config::TargetTemplate) -> Option<&'static str> {
    use hel::hel_config::TargetTemplate as Template;
    let platform = match template {
        Template::LocalPodman { container }
        | Template::LocalDocker { container }
        | Template::AppleContainer { container }
        | Template::SshPodman { container, .. } => container.platform.as_deref()?,
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
fn preflight_architectures(template: &hel::hel_config::TargetTemplate) -> Vec<&'static str> {
    use hel::hel_config::TargetTemplate as Template;
    if let Some(arch) = template_architecture(template) {
        return vec![arch];
    }
    match template {
        Template::LocalBare
        | Template::LocalPodman { .. }
        | Template::LocalDocker { .. }
        | Template::AppleContainer { .. } => vec![std::env::consts::ARCH],
        Template::SshBare { .. } | Template::SshPodman { .. } | Template::AwsEc2 { .. } => {
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
pub(super) fn preflight_worker_binary(template: &hel::hel_config::TargetTemplate) -> Result<()> {
    let mut failure = None;
    for arch in preflight_architectures(template) {
        match worker_binary_prerequisite_for_arch(arch) {
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

fn stable_running_executable(current: &Path) -> Result<PathBuf> {
    if current.is_file() {
        return Ok(current.to_path_buf());
    }
    #[cfg(target_os = "linux")]
    {
        let proc_exe = PathBuf::from(format!("/proc/{}/exe", std::process::id()));
        let directory = data_dir().join("workers").join("running");
        let cached = directory.join(format!("hel-{}", std::process::id()));
        materialize_running_executable(current, &proc_exe, &cached)
    }
    #[cfg(not(target_os = "linux"))]
    bail!(
        "resolved Mjolnir controller executable is no longer readable: {}",
        current.display()
    )
}

#[cfg(target_os = "linux")]
fn materialize_running_executable(
    current: &Path,
    proc_exe: &Path,
    cached: &Path,
) -> Result<PathBuf> {
    if !proc_exe.is_file() {
        bail!(
            "resolved Mjolnir controller executable is no longer readable: {}",
            current.display()
        );
    }
    let parent = cached
        .parent()
        .context("worker executable cache has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create worker executable cache {}", parent.display()))?;
    std::fs::copy(proc_exe, cached).with_context(|| {
        format!(
            "copy running mj executable from {} after {} was replaced",
            proc_exe.display(),
            current.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cached, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(cached.to_path_buf())
}

pub(super) fn worker_binary_for(
    locator: &hel_targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let arch = target_architecture(locator, executor)?;
    match worker_binary_prerequisite_for_arch(arch)? {
        WorkerBinaryAvailability::Local { path, .. } => Ok(path),
        WorkerBinaryAvailability::Remote {
            url,
            sha256,
            triple,
        } => download_worker(&url, &sha256, &triple),
    }
}

fn target_architecture(
    locator: &hel_targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<&'static str> {
    let command = match locator {
        hel_targets::TargetLocator::LocalBare { .. } => CommandSpec::new("uname", ["-m"]),
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", container_id, "uname", "-m"])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", container_id, "uname", "-m"])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => {
            CommandSpec::new("container", ["exec", container_id, "uname", "-m"])
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => ssh_command_spec(ssh, ["uname", "-m"]),
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            ssh_command_spec(ssh, ["podman", "exec", container_id, "uname", "-m"])
        }
    }
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
    let directory = data_dir()
        .join("workers")
        .join(env!("CARGO_PKG_VERSION"))
        .join(triple);
    std::fs::create_dir_all(&directory)?;
    let destination = directory.join("hel");
    if destination.is_file() {
        let bytes = std::fs::read(&destination)?;
        if format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(expected_sha256) {
            return Ok(destination);
        }
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
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    std::io::Write::write_all(&mut temporary, &bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(destination)
}

fn validate_worker_sha256(expected_sha256: &str) -> Result<()> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("MJ_WORKER_SHA256 must be a 64-character hexadecimal digest");
    }
    Ok(())
}

fn workspace_paths(
    locator: &hel_targets::TargetLocator,
    bundle: &ProjectBundle,
    session_id: &str,
) -> Result<(String, Vec<String>)> {
    let root = match locator {
        hel_targets::TargetLocator::LocalBare { .. } => {
            bail!("local bare projects use their selected directory")
        }
        hel_targets::TargetLocator::LocalPodman { .. }
        | hel_targets::TargetLocator::LocalDocker { .. }
        | hel_targets::TargetLocator::AppleContainer { .. }
        | hel_targets::TargetLocator::SshPodman { .. } => "/workspace".to_string(),
        hel_targets::TargetLocator::AwsEc2 { workspace, .. }
        | hel_targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
    };
    if matches!(locator, hel_targets::TargetLocator::AwsEc2 { .. }) {
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

// Package versions for ACP bridges. Keep these in lockstep with the global npm installs in
// containers/Containerfile.agent-dev; bridge_pins_match_containerfile() below
// fails the build when they drift.
// Codex 0.148 reuses pending MCP startups during runtime reconciliation. Older
// releases could cancel the first project-memory startup while immediately
// replacing it with an equivalent connection, leaving a false failed-tool
// event at the beginning of every session.
const CODEX_ACP_FALLBACK_VERSION: &str = "1.8.0";

const CLAUDE_AGENT_ACP_FALLBACK_VERSION: &str = "0.73.0";

const DEEPSEEK_HARNESS_FALLBACK_VERSION: &str = "0.1.1-rc.2";

const DEEPSEEK_ACP_FALLBACK_VERSION: &str = "0.10.0";

pub(super) fn bridge_launch(
    harness: hel::hel_config::HarnessKind,
    executable: Option<&Path>,
    policy: hel::hel_config::ExecutionPolicy,
) -> (String, Vec<String>) {
    if let Some(executable) = executable {
        let args = harness
            .bridge_override_args(policy)
            .into_iter()
            .map(str::to_owned)
            .collect();
        return (executable.to_string_lossy().into_owned(), args);
    }
    match harness {
        hel::hel_config::HarnessKind::Codex => (
            "sh".into(),
            vec![
                "-c".into(),
                format!("if command -v codex-acp >/dev/null 2>&1 && [ \"$(codex-acp --version 2>/dev/null)\" = \"@agentclientprotocol/codex-acp {CODEX_ACP_FALLBACK_VERSION}\" ]; then exec codex-acp; fi; {}; exec npx -y @agentclientprotocol/codex-acp@{CODEX_ACP_FALLBACK_VERSION}", ensure_node_script()),
            ],
        ),
        hel::hel_config::HarnessKind::Claude => (
            "sh".into(),
            vec![
                "-c".into(),
                format!("if command -v claude-agent-acp >/dev/null 2>&1; then exec claude-agent-acp; fi; {}; exec npx -y @agentclientprotocol/claude-agent-acp@{CLAUDE_AGENT_ACP_FALLBACK_VERSION}", ensure_node_script()),
            ],
        ),
        hel::hel_config::HarnessKind::Kimi => (
            "sh".into(),
            vec![
                "-c".into(),
                "if command -v kimi >/dev/null 2>&1; then exec kimi acp; elif [ -x \"$HOME/.kimi-code/bin/kimi\" ]; then exec \"$HOME/.kimi-code/bin/kimi\" acp; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash && exec \"$HOME/.kimi-code/bin/kimi\" acp; else echo 'Mjolnir needs compatible Kimi Code or curl for its official installer; configure the profile executable or environment PATH when the tool is installed elsewhere' >&2; exit 127; fi".into(),
            ],
        ),
        hel::hel_config::HarnessKind::Grok => {
            let acp = hel::hel_config::HarnessKind::Grok
                .bridge_override_args(policy)
                .join(" ");
            (
                "sh".into(),
                vec![
                    "-c".into(),
                    format!(
                        "if command -v grok >/dev/null 2>&1; then exec grok {acp}; elif [ -x \"$GROK_HOME/bin/grok\" ]; then exec \"$GROK_HOME/bin/grok\" {acp}; elif [ -x \"$HOME/.grok/bin/grok\" ]; then exec \"$HOME/.grok/bin/grok\" {acp}; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://x.ai/cli/install.sh | bash && exec \"$HOME/.grok/bin/grok\" {acp}; else echo 'Mjolnir needs compatible Grok Build or curl for its official installer; configure the profile executable or environment PATH when the tool is installed elsewhere' >&2; exit 127; fi"
                    ),
                ],
            )
        }
        hel::hel_config::HarnessKind::Deepseek => (
            "sh".into(),
            vec![
                "-c".into(),
                format!(
                    "{}; if command -v dsh >/dev/null 2>&1 && command -v dsh-acp-server >/dev/null 2>&1; then exec dsh-acp-server; fi; echo 'Mjolnir needs @deepseek-ai/dsh@{DEEPSEEK_HARNESS_FALLBACK_VERSION} and dsh-acp-server@{DEEPSEEK_ACP_FALLBACK_VERSION} installed on PATH; configure the profile executable or environment PATH when they are installed elsewhere' >&2; exit 127",
                    ensure_node_22_script(),
                ),
            ],
        ),
    }
}

fn ensure_node_script() -> &'static str {
    "if ! command -v npx >/dev/null 2>&1; then if [ \"$(id -u)\" = 0 ]; then SUDO=''; elif command -v sudo >/dev/null 2>&1 && sudo -n true; then SUDO='sudo'; else echo 'Mjolnir needs Node/npx or passwordless sudo to install it; configure the profile executable or environment PATH when the tool is installed elsewhere' >&2; exit 127; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update && $SUDO apt-get install -y nodejs npm; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y nodejs npm; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y nodejs npm; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache nodejs npm; else echo 'Mjolnir cannot install Node on this image; bake npx or a compatible ACP bridge into it, or configure the profile executable or environment PATH' >&2; exit 127; fi; fi"
}

fn ensure_node_22_script() -> String {
    format!(
        "{}; if ! node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)'; then echo 'DeepSeek Harness requires Node.js 22 or newer' >&2; exit 127; fi",
        ensure_node_script()
    )
}

const MJ_CONTAINER_ENVIRONMENT: &str = "## Mjolnir disposable environment\n\nThis session runs in a disposable Mjolnir container. When the session closes, Mjolnir checkpoints everything in project workspace directories under `/workspace`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then removes the container.\n\nEverything outside `/workspace`, including installed packages, `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n";

pub(super) fn stage_profile(
    profile: &hel::hel_config::HarnessProfile,
    destination: &Path,
) -> Result<()> {
    let harness = profile.kind;
    let source = profile.home.as_path();
    std::fs::create_dir_all(destination)?;
    let allowlist: &[&str] = match harness {
        hel::hel_config::HarnessKind::Codex => &[
            "auth.json",
            "config.toml",
            "AGENTS.md",
            "instructions.md",
            "rules",
            "skills",
        ],
        hel::hel_config::HarnessKind::Claude => &[
            ".claude.json",
            ".credentials.json",
            "settings.json",
            "CLAUDE.md",
            "skills",
            "plugins",
        ],
        hel::hel_config::HarnessKind::Kimi => &[
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
        hel::hel_config::HarnessKind::Grok => &[
            "auth.json",
            "config.toml",
            "AGENTS.md",
            "agent_id",
            "skills",
            "plugins",
        ],
        hel::hel_config::HarnessKind::Deepseek => &[
            ".credentials.yaml",
            "settings.yaml",
            "AGENTS.md",
            "skills",
            ".agent-presets",
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

/// Add lifecycle guidance only for targets that Hel destroys as a whole.
fn append_hel_target_environment(
    harness: hel::hel_config::HarnessKind,
    destination: &Path,
    target: &hel_targets::TargetLocator,
) -> Result<()> {
    let environment = match target {
        hel_targets::TargetLocator::LocalPodman { .. }
        | hel_targets::TargetLocator::LocalDocker { .. }
        | hel_targets::TargetLocator::AppleContainer { .. }
        | hel_targets::TargetLocator::SshPodman { .. } => MJ_CONTAINER_ENVIRONMENT.to_owned(),
        hel_targets::TargetLocator::AwsEc2 { workspace, .. } => format!(
            "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/{workspace}`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/{workspace}`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n"
        ),
        hel_targets::TargetLocator::LocalBare { .. }
        | hel_targets::TargetLocator::SshBare { .. } => return Ok(()),
    };
    let instructions = match harness {
        hel::hel_config::HarnessKind::Codex => "AGENTS.md",
        hel::hel_config::HarnessKind::Claude => "CLAUDE.md",
        hel::hel_config::HarnessKind::Kimi => "AGENTS.md",
        hel::hel_config::HarnessKind::Grok => "AGENTS.md",
        hel::hel_config::HarnessKind::Deepseek => "AGENTS.md",
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
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("read staged profile entry metadata {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
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
            copy_profile_entry(&entry.path(), &destination.join(entry.file_name()))
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

#[allow(clippy::too_many_arguments)]
fn install_worker_files(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
    profile_home: &str,
    worker_binary: &Path,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
) -> Result<()> {
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => {
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
        hel_targets::TargetLocator::LocalPodman { container_id }
        | hel_targets::TargetLocator::LocalDocker { container_id }
        | hel_targets::TargetLocator::AppleContainer { container_id } => {
            let engine = match locator {
                hel_targets::TargetLocator::LocalPodman { .. } => "podman",
                hel_targets::TargetLocator::LocalDocker { .. } => "docker",
                hel_targets::TargetLocator::AppleContainer { .. } => "container",
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
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
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
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            // The worker binary is 10-30 MB and identical across sessions, so
            // keep it in a content-addressed cache on the remote host and copy
            // it over the wire only once per unique binary.
            let digest = hel::hel_worker_launch::worker_executable_digest(worker_binary)?;
            // Home-relative, not "~/": ssh_command_spec single-quotes every
            // argument, so a tilde would stay literal in the remote shell
            // while scp expands it, and the two sides would disagree. Both
            // ssh commands (cwd is the login home) and scp resolve a relative
            // path against the remote home.
            let cache_dir = format!(".cache/mjolnir/workers/{digest}");
            let cached_worker = format!("{cache_dir}/hel");
            let cached = matches!(
                executor.execute(
                    &ssh_command_spec(ssh, ["test", "-f", &cached_worker])
                        .purpose("probe cached remote Mjolnir worker"),
                ),
                Ok(output) if output.status == 0
            );
            if !cached {
                execute_checked(
                    executor,
                    ssh_command_spec(ssh, ["mkdir", "-p", &cache_dir])
                        .purpose("create remote worker cache"),
                )?;
                let partial = format!("{cache_dir}/hel.partial-{session_id}");
                execute_checked(
                    executor,
                    scp_command_spec(ssh, worker_binary, &partial, false)
                        .purpose("upload remote Podman worker binary"),
                )?;
                // Rename within the cache directory so the final path only
                // ever names a complete upload.
                execute_checked(
                    executor,
                    ssh_command_spec(ssh, ["mv", &partial, &cached_worker])
                        .purpose("publish cached remote Mjolnir worker"),
                )?;
            }
            let upload = format!(".cache/mjolnir/uploads/{session_id}");
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["mkdir", "-p", &upload])
                    .purpose("create remote upload staging"),
            )?;
            for (source, name) in [
                (launch_config, "launch.json"),
                (ownership, "ownership.json"),
            ] {
                execute_checked(
                    executor,
                    scp_command_spec(ssh, source, &format!("{upload}/{name}"), false)
                        .purpose("upload remote Podman worker file"),
                )?;
            }
            execute_checked(
                executor,
                scp_command_spec(ssh, profile_stage, &format!("{upload}/profile"), true)
                    .purpose("upload remote Podman profile allowlist"),
            )?;
            let remote = [
                vec![
                    "podman".into(),
                    "exec".into(),
                    container_id.clone(),
                    "mkdir".into(),
                    "-p".into(),
                    worker_root.into(),
                    profile_home.into(),
                ],
                vec![
                    "podman".into(),
                    "cp".into(),
                    cached_worker.clone(),
                    format!("{container_id}:{worker_root}/hel"),
                ],
                vec![
                    "podman".into(),
                    "cp".into(),
                    format!("{upload}/launch.json"),
                    format!("{container_id}:{worker_root}/launch.json"),
                ],
                vec![
                    "podman".into(),
                    "cp".into(),
                    format!("{upload}/ownership.json"),
                    format!("{container_id}:{worker_root}/ownership.json"),
                ],
                vec![
                    "podman".into(),
                    "cp".into(),
                    format!("{upload}/profile/."),
                    format!("{container_id}:{profile_home}"),
                ],
                vec![
                    "podman".into(),
                    "exec".into(),
                    container_id.clone(),
                    "chmod".into(),
                    "700".into(),
                    format!("{worker_root}/hel"),
                ],
                vec![
                    "podman".into(),
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
                    ssh_command_spec(ssh, args).purpose("install remote Podman worker"),
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
        ssh_command_spec(ssh, ["mkdir", "-p", worker_root, profile_home])
            .purpose("create SSH worker directories"),
    )?;
    for (source, remote, recursive) in [
        (worker_binary, format!("{worker_root}/hel"), false),
        (launch_config, format!("{worker_root}/launch.json"), false),
        (ownership, format!("{worker_root}/ownership.json"), false),
    ] {
        execute_checked(
            executor,
            scp_command_spec(ssh, source, &remote, recursive).purpose("upload SSH worker file"),
        )?;
    }
    let incoming_profile = format!("{profile_home}.incoming");
    execute_checked(
        executor,
        scp_command_spec(ssh, profile_stage, &incoming_profile, true)
            .purpose("upload SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        ssh_command_spec(
            ssh,
            ["cp", "-R", &format!("{incoming_profile}/."), profile_home],
        )
        .purpose("install SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        ssh_command_spec(ssh, ["rm", "-rf", "--", &incoming_profile])
            .purpose("remove SSH profile staging"),
    )?;
    execute_checked(
        executor,
        ssh_command_spec(ssh, ["chmod", "700", &format!("{worker_root}/hel")])
            .purpose("make SSH worker executable"),
    )?;
    execute_checked(
        executor,
        ssh_command_spec(ssh, ["chmod", "-R", "go-rwx", profile_home])
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
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<()> {
    let plan = installed_worker_binary_replacement_plan(locator, session_id, worker_binary)?;
    for command in plan.commands {
        execute_checked(executor, command)?;
    }
    Ok(())
}

fn installed_worker_binary_replacement_plan(
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<CommandPlan> {
    let worker_root = hel_targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/hel");
    let staged = format!("{worker_root}/hel.next");
    let commands = match locator {
        hel_targets::TargetLocator::LocalBare { .. } => vec![
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
        hel_targets::TargetLocator::LocalPodman { container_id }
        | hel_targets::TargetLocator::LocalDocker { container_id }
        | hel_targets::TargetLocator::AppleContainer { container_id } => {
            let engine = match locator {
                hel_targets::TargetLocator::LocalPodman { .. } => "podman",
                hel_targets::TargetLocator::LocalDocker { .. } => "docker",
                hel_targets::TargetLocator::AppleContainer { .. } => "container",
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
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => vec![
            scp_command_spec(ssh, worker_binary, &staged, false)
                .purpose("stage replacement Mjolnir worker"),
            ssh_command_spec(ssh, ["mv", "-f", "--", &staged, &installed])
                .purpose("replace installed Mjolnir worker"),
            ssh_command_spec(ssh, ["chmod", "700", &installed])
                .purpose("make replaced Mjolnir worker executable"),
        ],
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            let upload = format!(".cache/mjolnir/uploads/{session_id}-hel.next");
            vec![
                ssh_command_spec(ssh, ["mkdir", "-p", ".cache/mjolnir/uploads"])
                    .purpose("create remote replacement worker staging"),
                scp_command_spec(ssh, worker_binary, &upload, false)
                    .purpose("stage replacement Mjolnir worker"),
                ssh_command_spec(
                    ssh,
                    ["podman", "cp", &upload, &format!("{container_id}:{staged}")],
                )
                .purpose("stage replacement Mjolnir worker"),
                ssh_command_spec(
                    ssh,
                    [
                        "podman",
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
                ssh_command_spec(
                    ssh,
                    ["podman", "exec", container_id, "chmod", "700", &installed],
                )
                .purpose("make replaced Mjolnir worker executable"),
                ssh_command_spec(ssh, ["rm", "-f", "--", &upload])
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
    locator: &hel_targets::TargetLocator,
    path: &str,
    purpose: &str,
) -> CommandSpec {
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => CommandSpec::new("sha256sum", [path]),
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", container_id, "sha256sum", path])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", container_id, "sha256sum", path])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => {
            CommandSpec::new("container", ["exec", container_id, "sha256sum", path])
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, ["sha256sum", path])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            ssh_command_spec(ssh, ["podman", "exec", container_id, "sha256sum", path])
        }
    }
    .purpose(purpose)
}

fn worker_launch_refresh_plan(
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    launch: &WorkerLaunchConfig,
) -> Result<WorkerLaunchRefreshPlan> {
    let worker_root = hel_targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/launch.json");
    let staged = format!("{installed}.next");
    let staged_arg = hel_targets::join_remote_command(std::slice::from_ref(&staged));
    let installed_arg = hel_targets::join_remote_command(std::slice::from_ref(&installed));
    let script = format!("umask 077; cat > {staged_arg} && mv -f -- {staged_arg} {installed_arg}");
    let body = serde_json::to_vec_pretty(launch).context("serialize worker launch config")?;
    let expected_sha256 = format!("{:x}", Sha256::digest(&body));
    let replace = match locator {
        hel_targets::TargetLocator::LocalBare { .. } => CommandSpec::new("sh", ["-c", &script]),
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", "-i", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", "-i", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => CommandSpec::new(
            "container",
            ["exec", "-i", container_id, "sh", "-c", &script],
        ),
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, ["sh", "-c", &script])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => ssh_command_spec(
            ssh,
            ["podman", "exec", "-i", container_id, "sh", "-c", &script],
        ),
    }
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
    locator: &hel_targets::TargetLocator,
    session_id: &str,
) -> Result<Option<WorkerBinaryRefresh>> {
    let worker_root = hel_targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/hel");
    // Remote targets defer source selection to the recovery task: choosing the
    // binary needs the target's architecture, and probing it (plus hashing the
    // remote binary) is blocking ssh work that must not run on this UI/event
    // path. Building the refresh here stays cheap.
    if matches!(
        locator,
        hel_targets::TargetLocator::AwsEc2 { .. }
            | hel_targets::TargetLocator::SshBare { .. }
            | hel_targets::TargetLocator::SshPodman { .. }
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
    if !std::env::current_exe().is_ok_and(|path| path.is_file()) {
        return Ok(None);
    }
    let source = match worker_binary_prerequisite_for_arch(std::env::consts::ARCH) {
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
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    installed_digest: &CommandSpec,
    source: &Path,
) -> Result<bool> {
    let expected = hel::hel_worker_launch::worker_executable_digest(source)?;
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
    locator: &hel_targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    execute_checked(executor, stop_worker_command(locator, worker_root))?;
    Ok(())
}

/// Restore a stopped Podman target before signaling its worker. Checkpoint
/// recovery uses this instead of assuming every persisted target is running.
pub(super) fn stop_worker_after_target_recovery(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
) -> Result<()> {
    let target = hel_targets::target_recovery_plan(locator, session_id)?;
    hel_targets::ensure_recovery_target_running(executor, target.as_ref())
        .context("restore Mjolnir worker target")?;
    stop_worker(executor, locator, worker_root)
}

fn stop_worker_command(locator: &hel_targets::TargetLocator, worker_root: &str) -> CommandSpec {
    let script = hel_targets::stop_worker_daemon_script(worker_root);
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => CommandSpec::new("sh", ["-c", &script]),
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => {
            CommandSpec::new("container", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, ["sh", "-c", &script])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            ssh_command_spec(ssh, ["podman", "exec", container_id, "sh", "-c", &script])
        }
    }
    .purpose("stop Mjolnir worker daemon")
}

fn worker_liveness_command(locator: &hel_targets::TargetLocator, worker_root: &str) -> CommandSpec {
    let script = hel_targets::worker_daemon_liveness_script(worker_root);
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => CommandSpec::new("sh", ["-c", &script]),
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => {
            CommandSpec::new("container", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, ["sh", "-c", &script])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            ssh_command_spec(ssh, ["podman", "exec", container_id, "sh", "-c", &script])
        }
    }
    .purpose("probe Mjolnir worker daemon liveness")
}

pub(super) fn start_worker(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    execute_checked(executor, start_worker_command(locator, worker_root))?;
    Ok(())
}

fn start_worker_command(locator: &hel_targets::TargetLocator, worker_root: &str) -> CommandSpec {
    let binary = format!("{worker_root}/hel");
    let config = format!("{worker_root}/launch.json");
    // These files describe the worker's previous life. Clear them as part of
    // the launch, before the new daemon can be probed: a stale exit record
    // aborts startup, while a stale socket makes a recovering daemon look
    // ready and invites the reconnect actor to kill it as unresponsive.
    let clear_stale_runtime = format!(
        "rm -f {} {}; ",
        hel_targets::join_remote_command(&[format!("{worker_root}/worker-exit.json")]),
        hel_targets::join_remote_command(&[format!("{worker_root}/control.sock")]),
    );
    let detached_script = format!(
        "{clear_stale_runtime}nohup {} >{} 2>&1 </dev/null &",
        hel_targets::join_remote_command(&[
            binary.clone(),
            "worker".into(),
            "run".into(),
            "--root".into(),
            worker_root.into(),
            "--config".into(),
            config.clone(),
        ]),
        hel_targets::join_remote_command(&[format!("{worker_root}/worker.log")]),
    );
    // Redirect daemon output to worker.log in every launch mode; an
    // unexplained dead worker is undebuggable without it.
    let exec_script = format!(
        "{clear_stale_runtime}exec {} >{} 2>&1",
        hel_targets::join_remote_command(&[
            binary.clone(),
            "worker".into(),
            "run".into(),
            "--root".into(),
            worker_root.into(),
            "--config".into(),
            config.clone(),
        ]),
        hel_targets::join_remote_command(&[format!("{worker_root}/worker.log")]),
    );
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => {
            CommandSpec::new("sh", ["-c", &detached_script])
        }
        hel_targets::TargetLocator::LocalPodman { container_id } => CommandSpec::new(
            "podman",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        hel_targets::TargetLocator::LocalDocker { container_id } => CommandSpec::new(
            "docker",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        hel_targets::TargetLocator::AppleContainer { container_id } => CommandSpec::new(
            "container",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, ["sh", "-c", &detached_script])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => ssh_command_spec(
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
    locator: &hel_targets::TargetLocator,
    worker_root: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let binary = format!("{worker_root}/hel");
    let command = match locator {
        hel_targets::TargetLocator::LocalBare { .. } => {
            CommandSpec::new(binary.clone(), ["--version"])
        }
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", container_id, &binary, "--version"])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", container_id, &binary, "--version"])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => {
            CommandSpec::new("container", ["exec", container_id, &binary, "--version"])
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, [binary.as_str(), "--version"])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => ssh_command_spec(
            ssh,
            ["podman", "exec", container_id, binary.as_str(), "--version"],
        ),
    }
    .purpose("probe installed worker binary");
    let error = match executor.execute(&command) {
        Ok(output) if output.status == 0 => error,
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            error.context(format!(
                "worker binary {binary} fails to run in the target: {detail}; \
                 if this is a loader/glibc error, provide a musl worker \
                 (cargo build --release --target <arch>-unknown-linux-musl, \
                 or set MJ_WORKER_BINARY/MJ_WORKER_DIR)"
            ))
        }
        Err(probe_error) => error.context(format!("worker probe failed: {probe_error:#}")),
    };
    match worker_last_words(executor, locator, worker_root) {
        Some(last_words) => error.context(last_words),
        None => error,
    }
}

/// Fetch the dead worker's structured exit record and log tail from the
/// target, so unreachable-worker errors carry the root cause.
pub(super) fn worker_last_words(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    worker_root: &str,
) -> Option<String> {
    let script = format!(
        "if [ -f {root}/worker-exit.json ]; then echo '{marker}'; cat {root}/worker-exit.json; fi; if [ -f {root}/worker.log ]; then echo '--- worker.log (tail) ---'; tail -n 20 {root}/worker.log; fi",
        root = worker_root,
        marker = WORKER_EXIT_RECORD_MARKER
    );
    let command = match locator {
        hel_targets::TargetLocator::LocalBare { .. } => CommandSpec::new("sh", ["-c", &script]),
        hel_targets::TargetLocator::LocalPodman { container_id } => {
            CommandSpec::new("podman", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::LocalDocker { container_id } => {
            CommandSpec::new("docker", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AppleContainer { container_id } => {
            CommandSpec::new("container", ["exec", container_id, "sh", "-c", &script])
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            ssh_command_spec(ssh, ["sh", "-c", &script])
        }
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            ssh_command_spec(ssh, ["podman", "exec", container_id, "sh", "-c", &script])
        }
    }
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

    use anyhow::Result;

    use hel::hel_config::ExecutionPolicy;
    use hel::hel_targets::{self, CommandExecutor, CommandOutput, CommandSpec, SshTarget};

    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use std::path::{Path, PathBuf};

    #[test]
    fn a_stored_setup_token_reaches_only_claude_workers_that_do_not_set_their_own() {
        use hel::hel_config::HarnessKind;
        use hel::hel_credentials::{CLAUDE_OAUTH_TOKEN_ENV, write_claude_oauth_token};

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
    fn dev_checkout_prefers_the_musl_sibling_over_the_glibc_controller() {
        let controller = PathBuf::from("target/debug/mj");
        let musl = PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj");
        // Both the controller (glibc) and its musl sibling exist on disk.
        let present = [controller.clone(), musl.clone()];
        let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
            present.iter().any(|p| p == path)
        });
        assert_eq!(
            selected,
            Some((musl, "development musl sibling")),
            "the static musl sibling must win over the glibc controller itself"
        );
    }

    /// A configured container template for the preflight tests. Only the
    /// platform matters here; the rest is the smallest valid template.
    fn container_template(platform: Option<&str>) -> hel::hel_config::ContainerTemplate {
        hel::hel_config::ContainerTemplate {
            image: "example.invalid/mj-test:latest".into(),
            pull_policy: Default::default(),
            platform: platform.map(str::to_owned),
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
        }
    }

    fn ssh_connection() -> hel::hel_config::SshConnection {
        hel::hel_config::SshConnection {
            host: "builder".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: Vec::new(),
        }
    }

    #[test]
    fn preflight_reads_the_architecture_a_template_names() {
        use hel::hel_config::TargetTemplate;

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
        use hel::hel_config::TargetTemplate;

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
        use hel::hel_config::TargetTemplate;

        // Nothing in the configuration says what a remote machine runs, so the
        // preflight passes as long as one architecture could be served; the
        // real architecture is read from the live target during provisioning.
        for template in [
            TargetTemplate::SshBare {
                ssh: ssh_connection(),
                permissions: hel::hel_config::PermissionMode::Yolo,
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

        let error = worker_binary_prerequisite_for_current(FOREIGN_ARCH, &stale, &|path| {
            probed.borrow_mut().push(path.to_path_buf());
            false
        })
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
    /// empty.
    #[test]
    fn a_present_controller_still_looks_beside_itself() {
        let controller = PathBuf::from("/opt/brokk/mj");
        let probed = RefCell::new(Vec::new());

        let availability =
            worker_binary_prerequisite_for_current(FOREIGN_ARCH, &controller, &|path| {
                probed.borrow_mut().push(path.to_path_buf());
                path == controller
            })
            .unwrap();

        let probed = probed.into_inner();
        assert!(
            probed
                .iter()
                .any(|path| path.ends_with("mj-worker-riscv64-unknown-linux-musl")),
            "the packaged worker name must still be probed: {probed:?}"
        );
        assert!(matches!(
            availability,
            WorkerBinaryAvailability::Local { .. }
        ));

        // With nothing beside it either, a present controller still gets the
        // generic message; only a replaced one is told to restart.
        let root = PathBuf::from("/");
        let error =
            worker_binary_prerequisite_for_current(FOREIGN_ARCH, &root, &|path| path == root)
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
        let availability =
            worker_binary_prerequisite_for_current(FOREIGN_ARCH, &stale, &|path| path.is_file())
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
    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_running_executable_is_materialized_for_worker_upload() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let replaced = directory.path().join("hel (deleted)");
        let proc_exe = directory.path().join("proc-exe");
        let cached = directory.path().join("workers/running/hel-1");
        std::fs::write(&proc_exe, b"running executable").unwrap();

        assert_eq!(
            materialize_running_executable(&replaced, &proc_exe, &cached).unwrap(),
            cached
        );
        assert_eq!(std::fs::read(&cached).unwrap(), b"running executable");
        assert_eq!(
            std::fs::metadata(&cached).unwrap().permissions().mode() & 0o777,
            0o700
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
            hel_targets::TargetLocator::LocalBare {
                worker_root: "/worker/root".into(),
            },
            hel_targets::TargetLocator::LocalPodman {
                container_id: "container-1".into(),
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

        let locator = hel_targets::TargetLocator::SshBare {
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
        let container_id = hel_targets::resource_name(session).unwrap();
        let inspection = |status: &str| CommandOutput {
            status: 0,
            stdout: serde_json::to_vec(&serde_json::json!([{
                "Config": { "Labels": {
                    (hel_targets::MANAGED_LABEL): "true",
                    (hel_targets::SESSION_LABEL): session,
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
        let locator = hel_targets::TargetLocator::LocalPodman { container_id };

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
        locator: hel_targets::TargetLocator,
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
            locator: hel_targets::TargetLocator::SshPodman {
                ssh: SshTarget {
                    destination: "user@example.test".into(),
                    ssh_args: Vec::new(),
                },
                container_id: "container-1".into(),
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
            lines.iter().any(|line| line
                == &format!(
                    "scp {} user@example.test:{partial}",
                    fixture.worker_binary.display()
                )),
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
        let container_id = hel_targets::resource_name(session).unwrap();
        let locator = hel_targets::TargetLocator::LocalPodman {
            container_id: container_id.clone(),
        };
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        replace_installed_worker_binary(&executor, &locator, session, Path::new("/controller/hel"))
            .unwrap();

        let lines = rendered(&executor.commands.borrow());
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
            hel::hel_config::HarnessKind::Codex,
            None,
            ExecutionPolicy::Unconstrained,
        );
        assert_eq!(codex_command, "sh");
        assert_eq!(codex_arguments[0], "-c");
        assert!(codex_arguments[1].contains("@agentclientprotocol/codex-acp@1.8.0"));
        assert!(codex_arguments[1].contains("codex-acp --version"));

        let (claude_command, claude_arguments) = bridge_launch(
            hel::hel_config::HarnessKind::Claude,
            None,
            ExecutionPolicy::Unconstrained,
        );
        assert_eq!(claude_command, "sh");
        assert_eq!(claude_arguments[0], "-c");
        assert!(claude_arguments[1].contains("@agentclientprotocol/claude-agent-acp@0.73.0"));

        let (deepseek_command, deepseek_arguments) = bridge_launch(
            hel::hel_config::HarnessKind::Deepseek,
            None,
            ExecutionPolicy::Unconstrained,
        );
        assert_eq!(deepseek_command, "sh");
        assert_eq!(deepseek_arguments[0], "-c");
        assert!(deepseek_arguments[1].contains("@deepseek-ai/dsh@0.1.1-rc.2"));
        assert!(deepseek_arguments[1].contains("dsh-acp-server@0.10.0"));
        assert!(!deepseek_arguments[1].contains("npx -y -p @deepseek-ai/dsh"));
        assert!(deepseek_arguments[1].contains("exec dsh-acp-server"));
        assert!(deepseek_arguments[1].contains("Mjolnir needs @deepseek-ai/dsh"));
        assert!(!deepseek_arguments[1].contains("Hel"));
    }
    #[test]
    fn codex_execution_environment_follows_the_target_policy() {
        let mut podman_environment =
            BTreeMap::from([("INITIAL_AGENT_MODE".to_owned(), "read-only".to_owned())]);
        hel::hel_config::HarnessKind::Codex.configure_execution_environment(
            ExecutionPolicy::Unconstrained,
            &mut podman_environment,
        );
        assert_eq!(
            podman_environment
                .get("INITIAL_AGENT_MODE")
                .map(String::as_str),
            Some("agent-full-access")
        );

        let mut bare_environment =
            BTreeMap::from([("INITIAL_AGENT_MODE".to_owned(), "read-only".to_owned())]);
        hel::hel_config::HarnessKind::Codex.configure_execution_environment(
            ExecutionPolicy::ConfiguredApprovals,
            &mut bare_environment,
        );
        assert_eq!(
            bare_environment
                .get("INITIAL_AGENT_MODE")
                .map(String::as_str),
            Some("read-only"),
            "raw localhost must preserve the profile's configured mode"
        );
    }
    #[test]
    fn only_raw_targets_without_an_explicit_path_request_login_path_discovery() {
        let raw = hel_targets::TargetLocator::LocalBare {
            worker_root: "/worker".into(),
        };
        let managed = hel_targets::TargetLocator::LocalPodman {
            container_id: "container".into(),
        };

        let mut environment = BTreeMap::new();
        configure_login_path_discovery(&mut environment, &raw);
        assert_eq!(
            environment.get(DISCOVER_LOGIN_PATH_ENV).map(String::as_str),
            Some("1")
        );

        let mut explicit = BTreeMap::from([
            ("PATH".into(), "/configured/bin".into()),
            (DISCOVER_LOGIN_PATH_ENV.into(), "stale".into()),
        ]);
        configure_login_path_discovery(&mut explicit, &raw);
        assert_eq!(
            explicit.get("PATH").map(String::as_str),
            Some("/configured/bin")
        );
        assert!(!explicit.contains_key(DISCOVER_LOGIN_PATH_ENV));

        let mut managed_environment =
            BTreeMap::from([(DISCOVER_LOGIN_PATH_ENV.into(), "stale".into())]);
        configure_login_path_discovery(&mut managed_environment, &managed);
        assert!(!managed_environment.contains_key(DISCOVER_LOGIN_PATH_ENV));
    }
    #[test]
    fn grok_sandbox_environment_follows_the_target_policy() {
        let mut isolated = BTreeMap::from([("GROK_SANDBOX".to_owned(), "strict".to_owned())]);
        hel::hel_config::HarnessKind::Grok
            .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut isolated);
        assert_eq!(
            isolated.get("GROK_SANDBOX").map(String::as_str),
            Some("off")
        );

        let mut local = BTreeMap::from([("GROK_SANDBOX".to_owned(), "strict".to_owned())]);
        hel::hel_config::HarnessKind::Grok
            .configure_execution_environment(ExecutionPolicy::ConfiguredApprovals, &mut local);
        assert_eq!(
            local.get("GROK_SANDBOX").map(String::as_str),
            Some("strict"),
            "raw localhost must preserve the profile's configured sandbox"
        );
    }
    #[test]
    fn bridge_fallback_pins_match_the_agent_dev_containerfile() {
        const CONTAINERFILE: &str = include_str!("../../../containers/Containerfile.agent-dev");

        let codex = format!("codex-acp@{CODEX_ACP_FALLBACK_VERSION}");
        assert!(
            CONTAINERFILE.contains(&codex),
            "containers/Containerfile.agent-dev must install {codex}. The image and the \
                 bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container \
                 session and an npx session run different adapter versions."
        );

        let claude = format!("claude-agent-acp@{CLAUDE_AGENT_ACP_FALLBACK_VERSION}");
        assert!(
            CONTAINERFILE.contains(&claude),
            "containers/Containerfile.agent-dev must install {claude}. The image and the \
                 bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container \
                 session and an npx session run different adapter versions."
        );

        for package in [
            format!("@deepseek-ai/dsh@{DEEPSEEK_HARNESS_FALLBACK_VERSION}"),
            format!("dsh-acp-server@{DEEPSEEK_ACP_FALLBACK_VERSION}"),
        ] {
            assert!(
                CONTAINERFILE.contains(&package),
                "containers/Containerfile.agent-dev must install {package}"
            );
        }
    }
    #[test]
    fn kimi_default_bridge_is_non_login_and_uses_bash_for_the_official_installer() {
        let (command, arguments) = bridge_launch(
            hel::hel_config::HarnessKind::Kimi,
            None,
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
            hel::hel_config::HarnessKind::Grok,
            None,
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
        assert!(script.contains("Mjolnir needs Node/npx or passwordless sudo"));
        assert!(script.contains("Mjolnir cannot install Node on this image"));
        assert!(!script.contains("Hel"));
    }
    #[test]
    fn grok_default_bridge_adds_the_always_approve_flag_when_unrestricted() {
        let (_, arguments) = bridge_launch(
            hel::hel_config::HarnessKind::Grok,
            None,
            ExecutionPolicy::Unconstrained,
        );
        let script = &arguments[1];
        assert!(script.contains("exec grok agent --always-approve stdio"));
        assert!(script.contains("exec \"$GROK_HOME/bin/grok\" agent --always-approve stdio"));
        assert!(script.contains("exec \"$HOME/.grok/bin/grok\" agent --always-approve stdio"));
    }
    #[test]
    fn bridge_executable_override_carries_the_acp_subcommand_per_harness() {
        let executable = std::path::PathBuf::from("/opt/harness");
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            for (kind, expected) in [
                (hel::hel_config::HarnessKind::Codex, Vec::new()),
                (hel::hel_config::HarnessKind::Claude, Vec::new()),
                (hel::hel_config::HarnessKind::Kimi, vec!["acp"]),
                (
                    hel::hel_config::HarnessKind::Grok,
                    if policy.is_unconstrained() {
                        vec!["agent", "--always-approve", "stdio"]
                    } else {
                        vec!["agent", "stdio"]
                    },
                ),
                (hel::hel_config::HarnessKind::Deepseek, Vec::new()),
            ] {
                let (command, arguments) = bridge_launch(kind, Some(&executable), policy);
                assert_eq!(command, "/opt/harness");
                assert_eq!(arguments, expected, "{kind:?} policy: {policy:?}");
            }
        }
    }
    #[test]
    fn kimi_uses_runtime_aware_memory_delivery_only_on_staged_targets() {
        let local = hel_targets::TargetLocator::LocalBare {
            worker_root: "/worker".into(),
        };
        let podman = hel_targets::TargetLocator::LocalPodman {
            container_id: "container".into(),
        };

        assert_eq!(
            project_memory_mcp_delivery(hel::hel_config::HarnessKind::Kimi, &local),
            ProjectMemoryMcpDelivery::Acp
        );
        assert_eq!(
            project_memory_mcp_delivery(hel::hel_config::HarnessKind::Kimi, &podman),
            ProjectMemoryMcpDelivery::HarnessProfile
        );
        assert_eq!(
            project_memory_mcp_delivery(hel::hel_config::HarnessKind::Codex, &podman),
            ProjectMemoryMcpDelivery::Acp
        );
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
        let profile = hel::hel_config::HarnessProfile {
            kind: hel::hel_config::HarnessKind::Grok,
            home: home.path().to_path_buf(),
            executable: None,
            environment: BTreeMap::new(),
            context_window_bytes: None,
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
        let profile = hel::hel_config::HarnessProfile {
            kind: hel::hel_config::HarnessKind::Claude,
            home: home.path().to_path_buf(),
            executable: None,
            environment: BTreeMap::new(),
            context_window_bytes: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(staged.path().join(".claude.json")).unwrap(),
            identity
        );
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
        let profile = hel::hel_config::HarnessProfile {
            kind: hel::hel_config::HarnessKind::Kimi,
            home: home.path().to_path_buf(),
            executable: None,
            environment: BTreeMap::new(),
            context_window_bytes: None,
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
        let profile = hel::hel_config::HarnessProfile {
            kind: hel::hel_config::HarnessKind::Kimi,
            home: home.path().to_path_buf(),
            executable: None,
            environment: BTreeMap::new(),
            context_window_bytes: None,
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
            configured["mcpServers"]["mj-project-memory"],
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
        let server = &configured["mcpServers"]["mj-project-memory"];
        assert_eq!(server["command"], "sh");
        assert_eq!(server["runtime_id"], "local");
        assert_eq!(
            server["args"],
            serde_json::json!([
                "-c",
                "exec \"$HOME/$1\" worker memory-mcp --root \"$HOME/$2\"",
                "mj-project-memory",
                ".local/share/hel/workers/session/hel",
                ".local/share/hel/profiles/session/projects/project/memory"
            ])
        );
    }
    #[test]
    fn stage_deepseek_profile_copies_only_portable_configuration() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.yaml"),
            "version: 1\nrefs: {}\n",
        )
        .unwrap();
        std::fs::write(home.path().join("settings.yaml"), "models: {}\n").unwrap();
        std::fs::create_dir(home.path().join("sessions")).unwrap();
        std::fs::write(home.path().join("sessions/native-session"), "private state").unwrap();
        std::fs::create_dir(home.path().join("profiles")).unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = hel::hel_config::HarnessProfile {
            kind: hel::hel_config::HarnessKind::Deepseek,
            home: home.path().to_path_buf(),
            executable: None,
            environment: BTreeMap::new(),
            context_window_bytes: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        assert!(staged.path().join(".credentials.yaml").is_file());
        assert!(staged.path().join("settings.yaml").is_file());
        assert!(!staged.path().join("sessions").exists());
        assert!(!staged.path().join("profiles").exists());
    }
    #[test]
    fn disposable_container_guidance_reaches_each_harness_without_touching_home() {
        let target = hel_targets::TargetLocator::LocalPodman {
            container_id: "container".into(),
        };
        for (kind, instructions) in [
            (hel::hel_config::HarnessKind::Codex, "AGENTS.md"),
            (hel::hel_config::HarnessKind::Claude, "CLAUDE.md"),
            (hel::hel_config::HarnessKind::Kimi, "AGENTS.md"),
            (hel::hel_config::HarnessKind::Grok, "AGENTS.md"),
            (hel::hel_config::HarnessKind::Deepseek, "AGENTS.md"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let original = "# Controller instructions\n\nKeep this source unchanged.\n";
            let source_instructions = home.path().join(instructions);
            std::fs::write(&source_instructions, original).unwrap();
            let staged = tempfile::tempdir().unwrap();
            let profile = hel::hel_config::HarnessProfile {
                kind,
                home: home.path().to_path_buf(),
                executable: None,
                environment: std::collections::BTreeMap::new(),
                context_window_bytes: None,
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
        let profile = hel::hel_config::HarnessProfile {
            kind: hel::hel_config::HarnessKind::Kimi,
            home: home.path().to_path_buf(),
            executable: None,
            environment: std::collections::BTreeMap::new(),
            context_window_bytes: None,
        };

        stage_profile(&profile, staged.path()).unwrap();
        append_hel_target_environment(
            profile.kind,
            staged.path(),
            &hel_targets::TargetLocator::LocalPodman {
                container_id: "container".into(),
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
            hel::hel_config::HarnessKind::Codex,
            ec2.path(),
            &hel_targets::TargetLocator::AwsEc2 {
                profile: "profile".into(),
                region: "region".into(),
                instance_id: "instance".into(),
                ssh: hel_targets::SshTarget {
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
            "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/.local/share/hel/workspaces/session`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/.local/share/hel/workspaces/session`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n"
        );
        assert!(!guidance.contains("## Hel disposable environment"));

        let ssh_bare = tempfile::tempdir().unwrap();
        append_hel_target_environment(
            hel::hel_config::HarnessKind::Codex,
            ssh_bare.path(),
            &hel_targets::TargetLocator::SshBare {
                ssh: hel_targets::SshTarget {
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
    fn ssh_bare_locator(session_id: &str) -> hel_targets::TargetLocator {
        hel_targets::TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@host.test".into(),
                ssh_args: Vec::new(),
            },
            workspace: format!("/srv/mj/{session_id}"),
        }
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
        let current = hel::hel_worker_launch::worker_executable_digest(&source).unwrap();
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

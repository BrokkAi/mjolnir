use super::*;

impl Controller {
    /// Where this session's worker lives. This is decided from the session
    /// record and configuration alone, so a caller can name the worker root
    /// before anything is installed into it.
    pub(in crate::controller) fn worker_placement(
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

    pub(in crate::controller) fn prepare_worker_files(
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
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let (mut launch, project_memory, target_profile_home) =
            self.session_launch_config(session_id, backend)?;

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
            instance_id: Some(mj_core::config::instance_identity()),
        }
        .write(&ownership_path)?;
        let profile_stage = staging.path().join("profile");
        // Files are staged only into a home the session owns. The alternative
        // is `target_profile_home` being the user's own harness home, where
        // installing the stage would overwrite their configuration with the
        // daemon's copy and leave it there. The private-home decision is read
        // from the one place that makes it rather than restated here.
        if crate::controller::session_owns_profile_home(backend, session_id, profile) {
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
        // The build cache is an optimization: a failure here leaves the
        // session running the image's own Cargo.
        if session.build_cache.is_some()
            && let Err(error) = self.install_build_cache_shim(session, backend, executor)
        {
            tracing::warn!(
                session_id,
                "installing the mbx build cache failed: {error:#}"
            );
        }
        prepare_installed_managed_harness(executor, backend, worker_root, &launch)
    }

    /// Put the pinned mbx binary and its `cargo` shim in the session's `bin`
    /// directory, which the worker prepends to `PATH` for the harness, its
    /// terminals, and `bash -lc` shells. mbx invoked as `cargo` removes that
    /// directory from `PATH` and runs the image's real Cargo underneath.
    fn install_build_cache_shim(
        &self,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let worker_root = targets::worker_root(backend, &session.id)?;
        // The download is the one build-cache failure worth telling the user
        // about: it is fixable, and it is the only step that reaches the
        // network.
        let binary =
            crate::controller::mbx::binary_for(backend, executor).inspect_err(|error| {
                executor.notify_notice(&format!(
                    "The Rust build cache is unavailable: {error:#}; this session builds without it."
                ));
            })?;
        let configuration = self
            .config
            .targets
            .get(&session.target_template_id)
            .map(|template| {
                crate::controller::backend::backend_target(
                    template,
                    session.resource_allocation.as_ref(),
                    crate::controller::backend::ContainerOverrides::for_session(session),
                )
            })
            .transpose()?
            .and_then(|target| {
                crate::controller::mbx::host_configuration(
                    &target,
                    &self.config.build_cache,
                    executor,
                )
            });
        install_mbx_files(
            executor,
            backend,
            &session.id,
            &worker_root,
            &binary,
            configuration.as_deref(),
        )
    }

    /// Probe the installed binary and collect the dead worker's exit record
    /// and log tail after a session becomes unreachable. Best-effort; returns
    /// `None` when the target no longer exists or has no diagnostics.
    pub fn diagnose_worker(&self, session_id: &str) -> Option<String> {
        self.diagnose_worker_controlled(session_id, &crate::targets::ProcessExecutor)
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

    /// The launch config for a session, including what depends on its
    /// sub-agent role. The first launch and every relaunch use this, so a
    /// relaunched worker keeps its delegation tools and a relaunched child
    /// keeps its parent's workspace.
    fn session_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
    ) -> Result<(WorkerLaunchConfig, ProjectMemoryLaunchConfig, String)> {
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
        // A sub-agent child shares its parent's container, so it works in the
        // parent's workspace. The parent record is authoritative for that path.
        let (workspace_session_id, workspace_container) = match subagent.as_ref() {
            Some(child) => {
                let parent = self
                    .state
                    .sessions
                    .get(&child.parent_session_id)
                    .context("sub-agent parent session is missing")?;
                (parent.id.clone(), parent.container_workspace.clone())
            }
            None => (session_id.to_owned(), session.container_workspace.clone()),
        };
        let (mut launch, project_memory, target_profile_home) = worker_launch_config(
            session,
            profile,
            bundle,
            backend,
            &workspace_session_id,
            workspace_container.as_deref(),
            target,
        )?;
        launch.subagent_tools =
            subagent_tools_enabled(session, self.config.subagents.enabled, subagent.is_some());
        // Claude reads Mjolnir's delegation server from a configuration file in
        // its harness home, never over ACP, so a session running out of the
        // user's own home has no way to be given one. Leaving the flag set
        // would take Claude's own Agent and Task tools away without putting
        // anything in their place.
        if launch.subagent_tools
            && profile.kind == mj_core::config::HarnessKind::Claude
            && !crate::controller::session_owns_profile_home(backend, session_id, profile)
        {
            tracing::info!(
                session_id,
                "Mjolnir sub-agents need a harness home of their own; this Claude session runs \
                 out of the user's own home and keeps Claude's Agent and Task tools instead"
            );
            launch.subagent_tools = false;
        }
        // Capturing the working tree is only ever useful to a turn review, so
        // it is spent only on a session a review can run for: one whose
        // configuration names a reviewer, and that is not a child. A child
        // works in its parent's tree, and reviewing it would report the
        // parent's work as the child's.
        launch.review_capture =
            self.config.review.reviewer_profile().is_some() && subagent.is_none();
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
                parent.container_workspace.as_deref(),
                parent_target,
            )?;
            launch.cwd = if subagent.working_directory.as_os_str().is_empty() {
                parent_launch.cwd
            } else {
                parent_launch.cwd.join(&subagent.working_directory)
            };
            launch.additional_directories = parent_launch.additional_directories;
        }
        Ok((launch, project_memory, target_profile_home))
    }

    pub(in crate::controller) fn current_worker_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
    ) -> Result<WorkerLaunchConfig> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let (mut launch, _, _) = self.session_launch_config(session_id, backend)?;
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
                session.container_workspace.as_deref(),
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
pub(super) fn subagent_tools_enabled(
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

pub(super) fn worker_workspace_for_recovery(
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

pub(super) fn worker_launch_config(
    session: &mj_core::state::SessionRecord,
    profile: &mj_core::config::HarnessProfile,
    bundle: Option<&ProjectBundle>,
    backend: &targets::TargetLocator,
    workspace_session_id: &str,
    workspace_container: Option<&Path>,
    target: &mj_core::config::TargetTemplate,
) -> Result<(WorkerLaunchConfig, ProjectMemoryLaunchConfig, String)> {
    let session_id = session.id.as_str();
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
            workspace_container,
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
    let mut target_environment = target_environment;
    // The turn bounds are read by the worker process, which re-execs with a
    // cleared environment, so a value set for the daemon cannot reach it by
    // inheritance. Carry the two knobs explicitly when the daemon was started
    // with them, so shortening a timeout for a test works on every target and
    // not only on the container targets that can set it in configuration.
    // `RUST_LOG` travels the same way and for the same reason: a worker that
    // has gone quiet is diagnosed from its own log, and the log level cannot
    // be raised after the fact on a worker that re-execs with a cleared
    // environment.
    for name in [
        "MJ_TURN_STALL_TIMEOUT_MS",
        "MJ_TURN_TOOL_STALL_TIMEOUT_MS",
        "RUST_LOG",
    ] {
        if let Ok(value) = std::env::var(name) {
            target_environment.insert(name.to_owned(), value);
        }
    }
    // A worker process carries no other sign of which instance owns it, so
    // `pgrep`, `/proc/<pid>/environ` and a recovery scan cannot attribute one.
    // The worker re-execs with a cleared environment, so this has to travel in
    // the launch config rather than by inheritance.
    target_environment.insert(
        "MJ_INSTANCE".to_owned(),
        mj_core::config::instance_identity(),
    );
    // The build cache reaches the harness, its terminals, and the reviewer
    // sidecar, all of which run Cargo through the mbx shim.
    if let Some(build_cache) = &session.build_cache {
        target_environment.insert(
            "MBX_CACHE_DIR".into(),
            build_cache.directory.to_string_lossy().into_owned(),
        );
        if let Some(max_size) = &build_cache.max_size {
            target_environment.insert("MBX_GC_MAX_SIZE".into(), max_size.clone());
        }
        // The per-build summary and savings lines are for a human at a
        // terminal; in a harness session they only add noise to Cargo output.
        target_environment.insert("MBX_SUMMARY".into(), "off".into());
        target_environment.insert("MBX_SAVINGS".into(), "off".into());
    }
    let mut environment = target_environment.clone();
    environment.extend(profile.environment.clone());
    profile.kind.configure_home_environment(
        Path::new(&target_profile_home),
        backend.harness_host(),
        &mut environment,
    );
    profile
        .kind
        .configure_execution_environment(execution_policy, &mut environment)?;
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
            seed_image_environment: backend.container_engine().is_some(),
            run_mode: Default::default(),
            session_id: session_id.to_string(),
            subagent_tools: false,
            review_capture: false,
            harness: profile.kind,
            harness_home: PathBuf::from(&target_profile_home),
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

pub(super) fn harness_runtime_policy(backend: &targets::TargetLocator) -> HarnessRuntimePolicy {
    match backend {
        targets::TargetLocator::LocalBare { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => HarnessRuntimePolicy::Managed,
        _ => HarnessRuntimePolicy::Ambient,
    }
}

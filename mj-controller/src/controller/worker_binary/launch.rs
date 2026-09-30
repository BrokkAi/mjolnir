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
        // Every session runs from a staged copy of its profile, on every target,
        // so `target_profile_home` is always a home the session owns and the
        // stage is always installed there.
        let profile_stage = staging.path().join("profile");
        let started = Instant::now();
        let result = stage_profile(profile, &profile_stage);
        tracing::debug!(
            session_id,
            elapsed_ms = started.elapsed().as_millis(),
            "profile staging completed"
        );
        result?;
        stage_managed_skills(profile.kind, &profile_stage)?;
        stage_codex_catalog(
            &session.last_profile,
            profile,
            &profile_stage,
            &fetch_catalog_over_https,
            &SharedCatalogCache,
        )?;
        append_hel_target_environment(profile.kind, &profile_stage, backend)?;
        append_subagent_policy(
            profile.kind,
            &profile_stage,
            &launch.subagents,
            self.config.subagents.max_concurrent,
        )?;
        apply_staged_execution_setting(profile.kind, launch.execution_policy, &profile_stage)?;
        if profile.kind == mj_core::config::HarnessKind::Claude {
            if let Some(role) = launch.subagents.parent_role() {
                configure_claude_subagent_mcp(&profile_stage, worker_root, role)?;
            } else if launch.handback_tool {
                configure_claude_subagent_mcp(
                    &profile_stage,
                    worker_root,
                    mj_core::subagent::SubagentMcpRole::Child,
                )?;
            }
        }
        stage_memory_replica(
            &project_memory,
            Path::new(&target_profile_home),
            &profile_stage,
        )?;
        if project_memory.mcp_delivery == ProjectMemoryMcpDelivery::HarnessProfile {
            configure_kimi_project_memory_mcp(&profile_stage, worker_root, &project_memory)?;
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
        if session.build_cache.is_some() {
            self.install_build_cache_shim(session, backend, &launch, executor)
                .context("install shared machine build cache configuration")?;
        }
        prepare_installed_managed_harness(executor, backend, worker_root, &launch)
    }

    /// Put the pinned mbx binary and its `cargo` shim in the session's `bin`
    /// directory, which the worker prepends to `PATH` for the harness, its
    /// terminals, and `bash -lc` shells. mbx invoked as `cargo` removes that
    /// directory from `PATH` and runs the image's real Cargo underneath.
    pub(in crate::controller) fn install_build_cache_shim(
        &self,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        launch: &WorkerLaunchConfig,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let worker_root = targets::worker_root(backend, &session.id)?;
        // The download is the one build-cache failure worth telling the user
        // about: it is fixable, and it is the only step that reaches the
        // network.
        let binary =
            crate::controller::mbx::binary_for(backend, executor).inspect_err(|error| {
                executor.notify_notice(&format!(
                    "The Rust build cache could not be prepared: {error:#}."
                ));
            })?;
        let (configuration, config_roots) =
            self.build_cache_configuration(session, backend, launch, executor)?;
        install_mbx_files(
            executor,
            backend,
            &session.id,
            &worker_root,
            &binary,
            &configuration,
            &config_roots,
        )
    }

    pub(in crate::controller) fn prepare_build_cache_links(
        &self,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        launch: &WorkerLaunchConfig,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let (configuration, config_roots) =
            self.build_cache_configuration(session, backend, launch, executor)?;
        link_mbx_configuration(executor, backend, &configuration, &config_roots)
    }

    fn build_cache_configuration(
        &self,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        launch: &WorkerLaunchConfig,
        executor: &impl CommandExecutor,
    ) -> Result<(PathBuf, Vec<PathBuf>)> {
        let configuration = crate::controller::mbx::prepare_session_configuration(
            &self.config,
            backend,
            session
                .build_cache
                .as_ref()
                .context("session has no build cache")?,
            executor,
        )?;
        let config_roots = [&launch.target_environment, &launch.environment]
            .into_iter()
            .filter_map(|environment| environment.get("XDG_CONFIG_HOME").map(PathBuf::from))
            .collect::<std::collections::BTreeSet<_>>();
        Ok((configuration, config_roots.into_iter().collect()))
    }

    /// Probe the installed binary and the worker's recorded state after a
    /// session becomes unreachable. Returns `None` only when the session has
    /// no target to probe; a probe that fails says why.
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
        let probe = match probe_worker(executor, &backend, &worker_root) {
            Ok(probe) => probe.to_string(),
            Err(error) => format!("the worker could not be probed: {error:#}"),
        };
        Some(match binary_failure {
            Some(binary_failure) => format!("{binary_failure}; {probe}"),
            None => probe,
        })
    }

    /// A non-destructive liveness probe plus commands that replace a confirmed
    /// dead session worker without touching its durable relay files. The
    /// session manager runs both off its async actor.
    pub fn worker_recovery_plan(
        &self,
        session_id: &str,
        operation: Option<&mj_core::state::MoveOperation>,
    ) -> Result<WorkerRecoveryPlan> {
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let launch = self.worker_launch_config_for_move(session_id, &backend, operation)?;
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
        let target = session.target_runtime_settings(&self.config)?;
        let subagent = self.state.subagents.get(session_id);
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
            LaunchWorkspace {
                session_id: &workspace_session_id,
                container: workspace_container.as_deref(),
                parent_worktree: self.subagent_parent_worktree(session_id),
            },
            &target,
        )?;
        apply_jev_switch(&mut launch, self.config.jev.enabled);
        apply_continuation_switch(&mut launch, self.config.automatic_continuation_enabled());
        launch.subagents = session
            .subagents
            .clone()
            .unwrap_or_default()
            .for_launch(profile.kind, subagent.is_some());
        // Registration decided whether this child can be given the tool.
        launch.handback_tool = subagent.as_ref().is_some_and(|child| child.handback_tool);
        // Capturing the working tree is only ever useful to a turn review, so
        // it is spent only on a session a review can run for: one whose
        // configuration has an eligible reviewer, and that is not a child. A child
        // works in its parent's tree, and reviewing it would report the
        // parent's work as the child's.
        launch.review_capture =
            mj_core::review::settings::can_review(&self.config) && subagent.is_none();
        launch.bifrost_binary = mj_review::bifrost::configured_bifrost_binary();
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
            let parent_target = parent.target_runtime_settings(&self.config)?;
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
                LaunchWorkspace {
                    session_id: &parent.id,
                    container: parent.container_workspace.as_deref(),
                    parent_worktree: None,
                },
                &parent_target,
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
        let operation = crate::database::load_move_operation(session_id)?;
        self.worker_launch_config_for_move(session_id, backend, operation.as_ref())
    }

    fn worker_launch_config_for_move(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        operation: Option<&mj_core::state::MoveOperation>,
    ) -> Result<WorkerLaunchConfig> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let (mut launch, _, _) = self.session_launch_config(session_id, backend)?;
        if operation.is_some_and(|operation| {
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
        let launch = project_memory_launch(
            session,
            bundle,
            &workspace,
            &target_home,
            self.subagent_parent_worktree(session_id),
        )?;
        Ok(ProjectMemorySyncTarget {
            canonical_root: canonical_memory_root(&launch.project_key),
        })
    }
}

/// Carries `[jev] enabled = false` to the worker, which reads it from its
/// launch environment, and keeps the Jev key out of the worker and the
/// harness so nothing can reach the service.
pub(super) fn apply_jev_switch(launch: &mut WorkerLaunchConfig, enabled: bool) {
    if enabled {
        return;
    }
    for environment in [&mut launch.target_environment, &mut launch.environment] {
        environment.remove("TYPESAFE_API_KEY");
        environment.insert(
            mj_core::jev::DISABLED_ENVIRONMENT.to_owned(),
            "1".to_owned(),
        );
    }
}

/// Tell the worker when nobody will act on a `Continue` verdict, so it does
/// not park the assessment as deferred. Travels like the Jev switch.
pub(super) fn apply_continuation_switch(launch: &mut WorkerLaunchConfig, enabled: bool) {
    if enabled {
        return;
    }
    for environment in [&mut launch.target_environment, &mut launch.environment] {
        environment.insert(
            mj_core::jev::CONTINUATION_DISABLED_ENVIRONMENT.to_owned(),
            "1".to_owned(),
        );
    }
}

/// Whether this session gets Mjolnir's delegation tools in place of its
/// harness's own. The session's stored choice governs; `None` means native
/// sub-agents, so a session created before the per-session choice existed (or
/// through `mj new` with neither flag given) gets its harness's own
/// sub-agents. A child never gets them, and only Claude and Codex can receive
/// them at all.
#[cfg(test)]
pub(super) fn subagent_tools_enabled(
    session: &mj_core::state::SessionRecord,
    is_child: bool,
) -> bool {
    session
        .subagents
        .clone()
        .unwrap_or_default()
        .for_launch(session.harness_kind, is_child)
        .uses_mjolnir()
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

pub(super) struct LaunchWorkspace<'a> {
    pub session_id: &'a str,
    pub container: Option<&'a Path>,
    pub parent_worktree: Option<&'a mj_core::state::ManagedWorktree>,
}

pub(super) fn worker_launch_config(
    session: &mj_core::state::SessionRecord,
    profile: &mj_core::config::HarnessProfile,
    bundle: Option<&ProjectBundle>,
    backend: &targets::TargetLocator,
    worker_workspace: LaunchWorkspace<'_>,
    target: &mj_core::state::TargetRuntimeSettings,
) -> Result<(WorkerLaunchConfig, ProjectMemoryLaunchConfig, String)> {
    let session_id = session.id.as_str();
    let execution_policy = profile
        .kind
        .effective_execution_policy(target.execution_policy);
    let target_profile_home = target_profile_home(backend, session_id, profile);
    let workspace = if let Some(project_directory) = &session.project_directory {
        (project_directory.to_string_lossy().into_owned(), Vec::new())
    } else {
        workspace_paths(
            backend,
            bundle.context("session bundle is missing")?,
            worker_workspace.session_id,
            worker_workspace.container,
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
    let mut target_environment = target.environment.clone();
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
    // Resolve the daemon's local key before launching remote or container
    // workers, whose home directories do not contain its secrets file.
    if let Some(key) = mj_core::activity::verdict::api_key() {
        target_environment.insert("TYPESAFE_API_KEY".to_owned(), key);
    }
    // The build cache reaches the harness, its terminals, and the reviewer
    // sidecar, all of which run Cargo through the mbx shim.
    if let Some(build_cache) = &session.build_cache {
        target_environment.insert(
            "MBX_CACHE_DIR".into(),
            build_cache.directory.to_string_lossy().into_owned(),
        );
        target_environment.insert(
            "MJ_MBX_CONFIG_DIR".into(),
            mj_core::config::build_cache_configuration_directory(&build_cache.directory)
                .to_string_lossy()
                .into_owned(),
        );
        // Compiler symlinks name this worker's private executable. Sharing
        // them lets another container replace them with an unreachable path.
        target_environment.insert(
            "MBX_SHIMS_DIR".into(),
            Path::new(&targets::worker_root(backend, session_id)?)
                .join("mbx-shims")
                .to_string_lossy()
                .into_owned(),
        );
        // The per-build summary and savings lines are for a human at a
        // terminal; in a harness session they only add noise to Cargo output.
        target_environment.insert("MBX_SUMMARY".into(), "off".into());
        target_environment.insert("MBX_SAVINGS".into(), "off".into());
    }
    let mut environment = target_environment.clone();
    environment.extend(profile.environment.resolved().clone());
    profile
        .kind
        .configure_home_environment(Path::new(&target_profile_home), &mut environment);
    profile
        .kind
        .configure_execution_environment(execution_policy, &mut environment)?;
    let mut project_memory = project_memory_launch(
        session,
        bundle,
        &workspace,
        &target_profile_home,
        worker_workspace.parent_worktree,
    )?;
    project_memory.mcp_delivery = project_memory_mcp_delivery(profile.kind, backend);
    project_memory.history_socket =
        Some(Path::new(&targets::worker_root(backend, &session.id)?).join("control.sock"));
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
    let excluded_environment =
        exclude_harness_environment(&session.last_profile, profile, &mut environment);
    Ok((
        WorkerLaunchConfig {
            goal_resume_request: None,
            target_environment,
            seed_image_environment: backend.container_engine().is_some(),
            run_mode: Default::default(),
            expected_runtime_identity: session.expected_runtime_identity.clone(),
            session_id: session_id.to_string(),
            subagents: mj_core::subagent::SubagentPolicy::Native,
            handback_tool: false,
            review_capture: false,
            bifrost_binary: None,
            harness: profile.kind,
            harness_home: PathBuf::from(&target_profile_home),
            // The staged home mirrors the profile home, so the marker's path
            // within the profile home is the one the worker must check and
            // the credential sync must write. Kimi's is nested
            // (`credentials/kimi-code.json`); its file name alone named a file
            // at the top of the staged home that Kimi never reads.
            authentication_marker: profile
                .authentication_marker()
                .strip_prefix(&profile.home)
                .ok()
                .map(|name| name.to_string_lossy().into_owned()),
            bridge_command: PathBuf::from(bridge_command),
            bridge_args,
            harness_runtime: harness_runtime_policy(backend),
            environment,
            excluded_environment,
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

/// Leave out of a launch every variable the profile's harness must never see,
/// and name them for the worker, which removes them again once it has added
/// the target's own login environment.
///
/// Saying so once per profile is enough: the launch config is rebuilt for
/// every recovery check, and the variables do not change between them.
pub(super) fn exclude_harness_environment(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    environment: &mut std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    static REPORTED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let before = environment.clone();
    let excluded = profile.exclude_harness_environment(environment);
    let removed = excluded
        .iter()
        .filter(|name| before.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !removed.is_empty()
        && REPORTED
            .lock()
            .map(|mut reported| reported.insert(profile_id.to_owned()))
            .unwrap_or(true)
    {
        tracing::info!(
            profile_id,
            removed = removed.join(", "),
            "left API key settings out of the harness environment: this Codex profile signs in with ChatGPT and must not fall back to an API key"
        );
    }
    excluded
}

pub(super) fn harness_runtime_policy(backend: &targets::TargetLocator) -> HarnessRuntimePolicy {
    match backend {
        targets::TargetLocator::LocalBare { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => HarnessRuntimePolicy::Managed,
        _ => HarnessRuntimePolicy::Ambient,
    }
}

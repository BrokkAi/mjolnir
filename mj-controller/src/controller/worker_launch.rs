//! The controller's side of launching a worker: what a session's worker is
//! given to run with, and the staging that puts it on the target.

use super::worker_binary::launch::*;
use super::worker_binary::*;
use super::*;
use mj_core::targets::SshTrafficClass;
use mj_core::worker_build::worker_build_id;
use mj_core::worker_launch::{
    ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery, WorkerLaunchConfig, WorkerOwnership,
};
use mj_session_types::{ProjectMemorySyncTarget, WorkerRecoveryPlan};

impl Controller {
    pub(crate) fn aws_credential_source(
        &self,
        session_id: &str,
    ) -> Result<Option<mj_core::aws_credentials::AwsCredentialSource>> {
        let (profile, backend, environment) = self.aws_session_inputs(session_id)?;
        crate::aws_credentials::source(profile, &backend, &environment)
    }

    /// [`Self::aws_credential_source`] for the credential sync: the same for a
    /// profile without a role, and for one with a role every session that may
    /// hold Mjolnir's credentials stays managed (see
    /// [`crate::aws_credentials::sync_source`]).
    pub(crate) fn aws_credential_sync_source(
        &self,
        session_id: &str,
    ) -> Result<Option<mj_core::aws_credentials::AwsCredentialSource>> {
        let (profile, backend, environment) = self.aws_session_inputs(session_id)?;
        crate::aws_credentials::sync_source(profile, &backend, &environment)
    }

    /// A notice for the person when the profile's `[profiles.<id>.aws]` role
    /// cannot apply to this session, and why: its target is raw local or EC2,
    /// or sets its own AWS credentials, or the profile is not a Bedrock one.
    pub(crate) fn aws_role_notice(&self, session_id: &str) -> Result<Option<String>> {
        let (profile, backend, environment) = self.aws_session_inputs(session_id)?;
        let profile_id = &self.state.sessions[session_id].last_profile;
        Ok(crate::aws_credentials::unused_role_notice(
            profile_id,
            profile,
            crate::aws_credentials::SessionHost::from(&backend),
            &environment,
        ))
    }

    /// What decides whether this session receives synced AWS credentials.
    fn aws_session_inputs(
        &self,
        session_id: &str,
    ) -> Result<(
        &mj_core::config::HarnessProfile,
        targets::TargetLocator,
        std::collections::BTreeMap<String, String>,
    )> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session is missing")?;
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let locator = session
            .target
            .as_ref()
            .context("session target is missing")?;
        let backend = backend_locator(locator, session, &self.config)?;
        let runtime = session.target_runtime_settings(&self.config)?;
        Ok((profile, backend, runtime.environment.clone()))
    }

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
        if let Some(owner) = crate::worker_lifecycle::current(session_id) {
            owner.verify_target(locator)?;
        }
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
        history: LaunchHistory<'_>,
    ) -> Result<()> {
        self.prepare_worker_files_for(
            session_id,
            backend,
            worker_root,
            executor,
            WorkerFilePreparation::Install,
            history,
        )
        .map(|_| ())
    }

    pub(in crate::controller) fn prepare_worker_files_for(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
        preparation: WorkerFilePreparation,
        history: LaunchHistory<'_>,
    ) -> Result<Option<mj_core::credentials::LoginFingerprint>> {
        let (launch, staged_login) = self.stage_worker_files_for(
            session_id,
            backend,
            worker_root,
            executor,
            preparation,
            history,
        )?;
        if let Some(launch) = launch {
            prepare_installed_managed_harness(executor, backend, worker_root, &launch)?;
        }
        Ok(staged_login)
    }

    /// Return the pending harness preflight so a new checkout can finish first,
    /// and the login identity when refreshing the profile for an existing worker.
    pub(in crate::controller) fn stage_worker_files_for(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
        preparation: WorkerFilePreparation,
        history: LaunchHistory<'_>,
    ) -> Result<(
        Option<WorkerLaunchConfig>,
        Option<mj_core::credentials::LoginFingerprint>,
    )> {
        let _timing =
            crate::launch_timing::LaunchPhaseTimer::new(session_id, "prepare worker files");
        let owner = crate::worker_lifecycle::require(session_id)?;
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let target = session
            .target
            .as_ref()
            .context("worker preparation has no committed placement")?;
        owner.verify_target(target)?;
        crate::database::admit_environment_worker(session_id, target)?;
        // A busy worker can outlive a daemon upgrade. Reusing its history
        // does not imply that its executable can read this launch format.
        let upgrade_retained_worker = if preparation == WorkerFilePreparation::RefreshProfile {
            let executable = mj_core::target_path::join(Path::new(worker_root), Path::new("hel"));
            let output = execute_checked(
                executor,
                targets::command_on_locator(
                    SshTrafficClass::Control,
                    backend,
                    session_id,
                    vec![
                        mj_core::target_path::text(&executable),
                        "worker".into(),
                        "build-id".into(),
                    ],
                    "read retained worker build",
                )?,
            )?;
            let build = std::str::from_utf8(&output.stdout)
                .context("decode retained worker build")?
                .trim();
            ensure!(
                mj_core::worker_build::is_worker_build(build),
                "retained worker returned an invalid build identity"
            );
            build != worker_build_id()
        } else {
            false
        };
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        profile.ensure_ready(&session.last_profile)?;
        let (mut launch, project_memory, target_profile_home) =
            self.session_launch_config(session_id, backend, history)?;

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
            no_github_credentials: session.no_github_credentials,
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
        let staging_timing =
            crate::launch_timing::LaunchPhaseTimer::new(session_id, "stage harness profile");
        let result = stage_profile_with_catalog(
            &session.last_profile,
            profile,
            &profile_stage,
            &fetch_catalog_over_https,
            &SharedCatalogCache,
        );
        drop(staging_timing);
        result?;
        if let Some(notice) = self.aws_role_notice(session_id)? {
            executor.notify_notice(&notice);
        }
        if let Some(source) = self.aws_credential_source(session_id)? {
            let credentials = crate::aws_assume_role::obtain(session_id, &source, executor)?;
            credentials.install(&profile_stage, Path::new(&target_profile_home))?;
        }
        stage_claude_keychain_login(
            profile,
            mj_core::config::HarnessHost::current(),
            &profile_stage,
            claude_environment_login_present(profile, &launch.environment),
            executor,
        )?;
        stage_managed_skills(
            profile.kind,
            &profile_stage,
            session
                .target
                .as_ref()
                .context("session target is missing")?
                .skills_scope(),
        )?;
        append_hel_target_environment(profile.kind, &profile_stage, backend)?;
        // Edits to attachment settings apply on the next container creation;
        // describe this container's creation snapshot while it is retained.
        let attachments = mj_core::state::applied_container_settings(session).additional_mounts;
        let storage = targets::inspect_container_storage(backend, attachments, executor)?;
        append_container_storage_guidance(
            profile.kind,
            &profile_stage,
            backend,
            session.container_workspace.as_deref(),
            storage.managed_temporary_volume,
            &storage.attachments,
        )?;
        append_subagent_policy(
            profile.kind,
            &profile_stage,
            &launch.subagents,
            self.config.subagents.max_concurrent,
            launch.subtle_subagents,
        )?;
        apply_staged_execution_setting(profile.kind, launch.execution_policy, &profile_stage)?;
        if let Some(reviewer) = &launch.reviewer_launch
            && mj_core::worker_launch::ReviewMcpDelivery::for_harness(profile.kind)
                == mj_core::worker_launch::ReviewMcpDelivery::HarnessProfile
        {
            super::reviewer::configure_staged_review_mcp(
                profile.kind,
                &profile_stage,
                &reviewer.mcp_servers,
            )?;
        }
        // The `mj-agents` registration is not staged here: the worker writes
        // it on every start, because an upgrade or recovery replaces the
        // launch configuration without restaging this profile.
        if profile.kind == mj_core::config::HarnessKind::Claude {
            configure_claude_mailbox_hook(&profile_stage, worker_root)?;
        }
        let mut shared_paths: Vec<&Path> = shared_profile_paths(profile.kind);
        // A session without project memory gets no replica, no baseline and
        // no history server: nothing of the workspace's memory reaches it.
        if let Some(project_memory) = &project_memory {
            shared_paths.extend(stage_memory_replica(
                project_memory,
                Path::new(&target_profile_home),
                &profile_stage,
            )?);
            if project_memory.mcp_delivery == ProjectMemoryMcpDelivery::HarnessProfile {
                configure_kimi_history_mcp(
                    &profile_stage,
                    worker_root,
                    project_memory.history_socket.as_deref(),
                )?;
            }
        }
        if mj_core::session_instructions::delivered_by_staging(profile.kind) {
            let index = if let Some(memory) = &project_memory {
                let index =
                    staged_memory_index(memory, Path::new(&target_profile_home), &profile_stage)
                        .map(|contents| contents.unwrap_or_default())
                        .map_err(|error| format!("{error:#}"));
                if let Err(reason) = &index {
                    executor.notify_notice(&format!(
                        "Warning: Project memory index {} unavailable: {reason}",
                        memory
                            .root
                            .join(mj_core::project_memory::MEMORY_INDEX)
                            .display()
                    ));
                }
                index
            } else {
                Ok(String::new())
            };
            append_composed_session_instructions(
                &launch,
                &profile_stage,
                index.as_deref().map_err(String::as_str),
            )?;
        }
        // The notes above can make instructions the profile alone could
        // deliver too long, so the staged file is checked before it travels.
        profile.ensure_staged_instructions_deliverable(&session.last_profile, &profile_stage)?;
        let shared_login = shared_profile_login(profile, backend);
        if preparation != WorkerFilePreparation::Install {
            // A shared symlink stays live across boot. Only a copied login
            // identifies the credentials staged for this specific launch.
            let staged_login = if preparation == WorkerFilePreparation::RefreshProfileOnly
                && profile.auth_scheme().uses_native_login_file()
                && shared_login.is_none()
            {
                mj_core::credentials::LoginFingerprint::read_file(
                    profile.kind,
                    &mj_core::config::harness_authentication_marker(profile.kind, &profile_stage),
                )?
            } else {
                None
            };
            clear_profile_configuration(
                executor,
                backend,
                session_id,
                &target_profile_home,
                profile.kind,
            )?;
            install_worker_profile_files(
                executor,
                backend,
                session_id,
                worker_root,
                &target_profile_home,
                StagedWorkerProfile {
                    launch: &launch_path,
                    ownership: &ownership_path,
                    profile: &profile_stage,
                    shared_login: shared_login.as_ref(),
                    shared_paths: &shared_paths,
                },
            )?;
            if upgrade_retained_worker {
                let worker = WorkerInstall::for_target(backend, executor)?;
                install_worker_binary(executor, backend, session_id, &worker, "hel")?;
                return Ok((Some(launch), staged_login));
            }
            return Ok((None, staged_login));
        }
        let worker = WorkerInstall::for_target(backend, executor)?;
        install_worker_files(
            executor,
            backend,
            session_id,
            worker_root,
            &target_profile_home,
            &worker,
            &launch_path,
            &ownership_path,
            &profile_stage,
            shared_login.as_ref(),
            &shared_paths,
        )?;
        if session.build_cache.is_some() {
            self.install_build_cache_shim(session, backend, &launch, executor)
                .context("install shared machine build cache configuration")?;
        }
        Ok((Some(launch), None))
    }

    /// Refresh and verify the shared-cache copy, then install marked launchers
    /// in the session's `bin` directory on the worker PATH.
    pub(in crate::controller) fn install_build_cache_shim(
        &self,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        launch: &WorkerLaunchConfig,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let Some(cache) = session.build_cache.as_ref() else {
            return Ok(());
        };
        let worker_root = targets::worker_root(backend, &session.id)?;
        let config_roots = build_cache_config_roots(launch);
        let configuration = mj_core::config::build_cache_configuration_directory(&cache.directory);
        if link_legacy_mbx_configuration(
            executor,
            backend,
            &worker_root,
            &configuration,
            &config_roots,
        )? {
            return Ok(());
        }
        let binary = match crate::controller::mbx::sync_mbx_binary_for_container(
            backend,
            &cache.directory,
            executor,
        ) {
            Ok(crate::controller::mbx::CachedMbxSync::Ready(binary)) => binary,
            Ok(crate::controller::mbx::CachedMbxSync::Unavailable(reason)) => {
                disable_mbx_cache_shims(&session.id, backend, &worker_root, executor, &reason);
                return Ok(());
            }
            Err(error) => {
                disable_mbx_cache_shims(
                    &session.id,
                    backend,
                    &worker_root,
                    executor,
                    &format!("{error:#}"),
                );
                return Ok(());
            }
        };
        if let Err(error) = verify_mbx_binary(executor, backend, &binary.path, &binary.version) {
            disable_mbx_cache_shims(
                &session.id,
                backend,
                &worker_root,
                executor,
                &format!("{error:#}"),
            );
            return Ok(());
        }
        let (configuration, config_roots) =
            self.build_cache_configuration(session, backend, launch, executor)?;
        install_mbx_shims(
            executor,
            backend,
            &worker_root,
            &binary.path,
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
        self.install_build_cache_shim(session, backend, launch, executor)
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
        let config_roots = build_cache_config_roots(launch);
        Ok((configuration, config_roots))
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
        let probe = match diagnose_worker_read_only(executor, &backend, &worker_root) {
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
            source_target: *self.state.sessions[session_id]
                .target
                .clone()
                .context("session target is missing")?,
            target: targets::target_recovery_plan(&backend, session_id)?,
            workspace,
            liveness_probe: targets::WorkerControlPlan::new(
                format!("probe the worker of session {session_id}"),
                vec![worker_liveness_command(&backend, &worker_root)],
                &backend,
                session_id,
            )?,
            exit_record: Some(worker_exit_record_command(&backend, &worker_root)),
            binary_refresh: worker_binary_refresh_plan(&backend, session_id)?,
            launch_refresh: Some(worker_launch_refresh_plan(&backend, session_id, &launch)?),
            restart: targets::WorkerControlPlan::new(
                format!("restart Mjolnir worker for session {session_id}"),
                vec![
                    stop_worker_command(&backend, &worker_root),
                    start_worker_command(&backend, &worker_root).into(),
                ],
                &backend,
                session_id,
            )?,
        })
    }

    /// The launch config for a session, including what depends on its
    /// sub-agent role. The first launch and every relaunch use this, so a
    /// relaunched worker keeps its delegation tools and a relaunched child
    /// keeps its parent's workspace.
    pub(in crate::controller) fn session_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        history: LaunchHistory<'_>,
    ) -> Result<(
        WorkerLaunchConfig,
        Option<ProjectMemoryLaunchConfig>,
        String,
    )> {
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
        let checkout = self.state.checkout(session_id)?;
        let bundle = checkout
            .project_directory()
            .is_none()
            .then(|| session.project_bundle(&self.config))
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
        let (mut launch, project_memory, target_profile_home) = worker_launch_config_with_checkout(
            session,
            &checkout,
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
        launch.reviewer_launch = if subagent.is_some() {
            crate::database::load_subagent(session_id)?
                .context("sub-agent launch record is missing")?
                .reviewer_launch
        } else {
            None
        };
        if let Some(reviewer) = &launch.reviewer_launch {
            launch.bridge_command = reviewer.bridge_command.clone();
            launch.bridge_args = reviewer.bridge_args.clone();
            launch.execution_policy = reviewer.execution_policy;
            launch.environment.extend(reviewer.environment.clone());
            launch.excluded_environment = reviewer.excluded_environment.clone();
        }
        // Read by the target-side MCP server from launch.json beside its socket,
        // independently of how each harness starts or registers that server.
        for name in [
            mj_core::subagent::CHILD_REPORT_DIR_ENV,
            mj_core::subagent::CHILD_REPORT_LIMIT_ENV,
        ] {
            launch.environment.remove(name);
        }
        if subagent.is_some()
            && let Some(report_dir) = crate::database::load_subagent_report(session_id)?.report_dir
        {
            launch
                .environment
                .insert(mj_core::subagent::CHILD_REPORT_DIR_ENV.into(), report_dir);
            launch.environment.insert(
                mj_core::subagent::CHILD_REPORT_LIMIT_ENV.into(),
                mj_core::subagent::max_child_report_bytes(self.config.subagents.max_concurrent)
                    .to_string(),
            );
        }
        if subagent.is_some() {
            launch.environment.insert(
                mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV.into(),
                "1".into(),
            );
            launch
                .environment
                .remove(mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV);
        } else {
            launch
                .environment
                .remove(mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV);
            // This controller grants the message-only server to every
            // top-level session; the worker trusts only this marker.
            launch.environment.insert(
                mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV.into(),
                "1".into(),
            );
        }
        apply_jev_switch(&mut launch, self.config.jev.enabled);
        launch.agent_mailboxes_enabled = true;
        launch.subagents = session
            .subagents
            .clone()
            .unwrap_or_default()
            .for_launch(profile.kind, subagent.is_some());
        // Only a delegating parent's spawn tool reads this. Other sessions keep
        // the default, so their launch files do not change.
        let yolo = session.is_yolo_target(&self.config);
        launch.subtle_subagents = !launch.subagents.uses_mjolnir()
            || self
                .config
                .subagents
                .offers_subtle_for(&launch.subagents, yolo);
        launch.oracle =
            oracle_launch_enabled(session, subagent.is_some(), &self.config.subagents, yolo);
        // Registration decided whether this child can be given the tool.
        launch.handback_tool = subagent.as_ref().is_some_and(|child| child.handback_tool);
        launch.subagent_kind = subagent.as_ref().map(|child| child.kind);

        // The child's harness opens on its spawn model instead of switching
        // to it from the profile default before the first turn (issue 1217).
        launch.initial_model = subagent.as_ref().and_then(|child| child.model.clone());
        if let Some(child) = subagent
            .as_ref()
            .filter(|child| child.kind != mj_core::subagent::SubagentKind::Reviewer)
        {
            let group =
                mj_core::continuation::GeneratedPrompt::subagent_spawn_id(&child.request_key);
            let steps = crate::database::load_startup_group(session_id, &group)?;
            // The assignment is delivered at most once per session history.
            // A restore deletes the worker's command receipts, so the history
            // decides: once the assignment has started, the conversation
            // already holds it and the settings it was sent with.
            let prompt_id = crate::database::startup_step_id(&group, "prompt");
            let assignment_started = prompt_started(session_id, &history, &prompt_id)?;
            if !assignment_started {
                launch.initial_commands = steps
                    .into_iter()
                    .map(|step| {
                        let (command, optional) = match serde_json::from_str::<
                            crate::database::StartupStep,
                        >(&step.step_json)?
                        {
                            crate::database::StartupStep::Configure {
                                key,
                                value,
                                optional,
                            } => (
                                mj_core::relay::RelayCommand::SetConfig { key, value },
                                optional,
                            ),
                            crate::database::StartupStep::ApiPrompt { text } => (
                                mj_core::relay::RelayCommand::Prompt {
                                    prompt: vec![
                                        agent_client_protocol::v1::schema::ContentBlock::Text(
                                            agent_client_protocol::v1::schema::TextContent::new(
                                                text,
                                            ),
                                        ),
                                    ],
                                },
                                false,
                            ),
                            _ => anyhow::bail!("sub-agent startup contains a non-input step"),
                        };
                        Ok(mj_core::worker_launch::InitialWorkerCommand {
                            command_id: step.command_id,
                            command,
                            optional,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
        }
        // Top-level sessions and ordinary sub-agents may request review. A
        // reviewer or the oracle never does. The worker derives the review
        // tools and the push gate from this one value. An ordinary sub-agent
        // takes its parent's effective review choice.
        launch.review_trigger = if subagent
            .as_ref()
            .is_none_or(|child| child.kind == mj_core::subagent::SubagentKind::Worker)
        {
            if let Err(missing) = self.state.effective_review(session_id) {
                tracing::warn!(session_id, missing = %missing,
                    "review choice cannot be resolved through the parent chain; launching without review tools");
            }
            self.state
                .effective_review_config(&self.config.review, session_id)
                .trigger()
        } else {
            Default::default()
        };
        // Older workers still read this launch field. Push capture does not
        // need working-tree baselines at startup.
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
            let parent_checkout = self.state.checkout(&parent.id)?;
            let parent_bundle = parent_checkout
                .project_directory()
                .is_none()
                .then(|| parent.project_bundle(&self.config))
                .flatten();
            let (parent_launch, _, _) = worker_launch_config_with_checkout(
                parent,
                &parent_checkout,
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
                mj_core::target_path::join(&parent_launch.cwd, &subagent.working_directory)
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
        let (mut launch, _, _) =
            self.session_launch_config(session_id, backend, LaunchHistory::Stored)?;
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
                && session.target.as_deref() == operation.source_target.as_ref()
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

    /// Where the session's project memory is synchronized, or `None` for a
    /// session that has none.
    pub fn project_memory_sync_target(
        &self,
        session_id: &str,
    ) -> Result<Option<ProjectMemorySyncTarget>> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.no_project_memory {
            return Ok(None);
        }
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
        let checkout = self.state.checkout(session_id)?;
        let bundle = checkout
            .project_directory()
            .is_none()
            .then(|| session.project_bundle(&self.config))
            .flatten();
        let workspace = if let Some(project_directory) = checkout.project_directory() {
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
        Ok(Some(ProjectMemorySyncTarget {
            canonical_root: canonical_memory_root(&launch.project_key),
        }))
    }
}

/// Plans recovery from the configuration and records as they are stored now.
pub struct DurableRecoveryPlanner;

impl crate::session_manager::RecoveryPlanner for DurableRecoveryPlanner {
    fn replan_after_move(
        &self,
        session_id: &str,
        operation: &mj_core::state::MoveOperation,
    ) -> Result<WorkerRecoveryPlan> {
        Controller::load()?.worker_recovery_plan(session_id, Some(operation))
    }
}

use super::*;

/// How long creating a parent's report root on its target may take.
const REPORT_ROOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl RuntimeState {
    pub(super) async fn start_create_session(
        self: &Arc<Self>,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        self.start_create_session_inner(request, CreateSessionControl::default(), None)
            .await
    }

    /// Register and start a child worker on its parent's existing target.
    pub async fn start_subagent_session(
        self: &Arc<Self>,
        request: crate::controller::RegisterSubagentRequest,
    ) -> Result<mj_core::subagent::SubagentRecord> {
        let _upgrade_work = crate::upgrade::activity("subagent admission")?;
        let (relation, needs_provisioning) = blocking(move || {
            let mut controller = Controller::load()?;
            if let Some(existing) = crate::database::lookup_subagent_request(
                &request.parent_session_id,
                &request.request_key,
            )? {
                let needs_provisioning = controller
                    .state
                    .sessions
                    .get(&existing.child_session_id)
                    .is_some_and(|record| {
                        record.state == mj_core::state::SessionState::Provisioning
                    });
                return Ok((existing, needs_provisioning));
            }
            let mut request = request;
            // The parent's report root is made before the child exists, so the
            // child's first prompt can name its own directory under it. A
            // request run again after a restart finds the same root.
            let executor = CancellableProcessExecutor::with_timeout(REPORT_ROOT_TIMEOUT);
            request.report_root = Some(
                controller
                    .prepare_subagent_report_root(&request.parent_session_id, &executor)
                    .context("create the sub-agent report directory")?,
            );
            controller
                .register_subagent(request)
                .map(|relation| (relation, true))
        })
        .await?;
        if !needs_provisioning {
            return Ok(relation);
        }
        let session_id = relation.child_session_id.clone();
        let parent_session_id = relation.parent_session_id.clone();
        let started = self.start_or_join_lifecycle_controlled(
            session_id.clone(),
            LifecycleKind::Create,
            None,
            Some(relation.request_key.clone()),
            None,
            move |state, session_id, cancelled| async move {
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for sub-agent startup")??;
                // The lifecycle owner now excludes competing resume/close work.
                // Recovery may already have settled this registration before
                // admission; replay never reinstalls an existing worker.
                if controller
                    .state
                    .sessions
                    .get(&session_id)
                    .is_none_or(|record| record.state != mj_core::state::SessionState::Provisioning)
                {
                    return Ok(DaemonLifecycleResult::Done);
                }
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state.clone(),
                    session_id.clone(),
                );
                if let Err(error) = controller
                    .provision_subagent_session_controlled(&session_id, &executor)
                    .await
                {
                    drop(controller);
                    state.reload_controller().await?;
                    if let Err(prompt_error) = state
                        .ensure_parent_wait_prompt(&parent_session_id)
                        .await
                    {
                        tracing::warn!(
                            child_session_id = %session_id,
                            parent_session_id,
                            error = format!("{prompt_error:#}"),
                            "could not reconcile the parent's sub-agent wait prompt after provisioning failure"
                        );
                    }
                    return Err(error);
                }
                Ok(DaemonLifecycleResult::Done)
            },
        );
        if let Err(error) = started {
            self.reload_controller().await?;
            if let Err(prompt_error) = self
                .ensure_parent_wait_prompt(&relation.parent_session_id)
                .await
            {
                tracing::warn!(
                    child_session_id = %relation.child_session_id,
                    parent_session_id = %relation.parent_session_id,
                    error = format!("{prompt_error:#}"),
                    "could not reconcile the parent's sub-agent wait prompt after provisioning failure"
                );
            }
            return Err(error);
        }
        self.reload_controller().await?;
        Ok(relation)
    }

    pub async fn start_create_session_controlled(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: CreateSessionControl,
        publication: tokio::sync::oneshot::Receiver<std::result::Result<(), String>>,
    ) -> Result<RegisteredSession> {
        self.start_create_session_inner(request, control, Some(publication))
            .await
    }

    pub(super) async fn start_create_session_inner(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: CreateSessionControl,
        publication: Option<tokio::sync::oneshot::Receiver<std::result::Result<(), String>>>,
    ) -> Result<RegisteredSession> {
        let mut request = request;
        let parent = request.profile_id.clone();
        let supplied = request.subagents.clone();
        let (config, policy) = blocking(move || {
            let controller = Controller::load()?;
            let profile = controller
                .config
                .enabled_profile(&parent)
                .context("parent profile unavailable")?;
            let policy = supplied.unwrap_or_else(|| profile.subagents.clone());
            Ok((controller.config, policy))
        })
        .await?;
        crate::controller::profile_config::validate_session_subagent_policy(
            &config,
            &request.profile_id,
            &policy,
        )
        .await?;
        if !request.bundle_id.is_empty() {
            crate::controller::config_only_controller(config.clone())
                .validate_github_bundle_installations(&request.bundle_id)
                .await
                .map_err(crate::controller::GithubBundleSelectionError::into_anyhow)?;
        }
        // Discovery is restartable preparation, not admitted lifecycle work.
        let _upgrade_work = crate::upgrade::activity("session admission")?;
        request.subagents = Some(policy);
        let path_cancelled = control.cancelled.clone();
        let registered = blocking(move || {
            let mut controller = Controller::load()?;
            let path_executor = crate::targets::CancellableProcessExecutor::new(path_cancelled)
                .with_deadline(Duration::from_secs(30));
            let project_directory = request
                .project_directory
                .as_deref()
                .map(|path| {
                    controller.resolve_project_directory(
                        &request.target_template_id,
                        path,
                        &path_executor,
                    )
                })
                .transpose()?;
            let session_id = controller.register_session_with_resources(
                &request.profile_id,
                &request.bundle_id,
                &request.target_template_id,
                request.title,
                SessionLaunchOptions {
                    create_managed_worktree: request.create_managed_worktree,
                    at: request.at,
                    branch: request.branch,
                    base: request.base,
                    subagents: request.subagents,
                    review: request.review,
                    initial_prompt: request.initial_prompt,
                    workspace_id: request.workspace_id,
                    additional_mounts: request.additional_mounts,
                    resource_allocation: request.resource_allocation,
                    project_directory,
                    session_title_override: request.session_title_override,
                },
            )?;
            // Every surface creates sessions through here, so the dashboard,
            // the phone, `mj new`, and `mj acp` all leave a default pair
            // behind for the next caller that names none. A preference that
            // cannot be written does not undo a session that was created.
            if let Err(error) = mj_core::go::GoPreferences::remember_first_pair(
                &mj_core::go::GoPreferences::path(),
                &request.profile_id,
                &request.target_template_id,
            ) {
                tracing::warn!(%error, "could not save the default profile and target");
            }
            let session = controller
                .state
                .sessions
                .get(&session_id)
                .expect("newly registered session exists")
                .clone();
            let remembered_container_size = controller
                .config
                .targets
                .get(&request.target_template_id)
                .and_then(mj_core::config::container_size_host)
                .and_then(|host| {
                    controller
                        .state
                        .container_sizes
                        .get(host)
                        .copied()
                        .map(|size| (host.to_owned(), size))
                });
            Ok(RegisteredSession {
                session,
                remembered_container_size,
            })
        })
        .await?;
        let session_id = registered.session.id.clone();
        self.start_or_join_lifecycle_controlled(
            session_id,
            LifecycleKind::Create,
            None,
            None,
            Some(control.clone()),
            move |state, session_id, cancelled| async move {
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for daemon create task")??;
                let publication_error = if let Some(publication) = publication {
                    let published = tokio::select! {
                        result = publication => result.context("session publication owner stopped")
                            .and_then(|result| result.map_err(anyhow::Error::msg)),
                        () = async {
                            while !cancelled.load(Ordering::Acquire) {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                        } => Err(anyhow!("session creation cancelled before publication")),
                    };
                    published.err()
                } else {
                    None
                };
                if publication_error.is_some() {
                    control.request_cancel();
                }
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state,
                    session_id.clone(),
                );
                let provision = controller
                    .provision_session_controlled_with_commit(&session_id, &executor, || {
                        ensure!(
                            control.grant_commit(),
                            "session creation cancelled before commit"
                        );
                        Ok(())
                    })
                    .await;
                if let Some(error) = publication_error {
                    return match provision {
                        Ok(()) => Err(error),
                        Err(rollback) => {
                            Err(error.context(format!("discard unpublished session: {rollback:#}")))
                        }
                    };
                }
                provision?;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        self.reload_controller().await?;
        Ok(registered)
    }

    pub async fn wait_create_session(&self, session_id: &str) -> Result<()> {
        let result = {
            let lifecycle_owner = self.owner();
            let lifecycle = &lifecycle_owner.lifecycle;
            let active = lifecycle
                .get(session_id)
                .with_context(|| format!("no create operation exists for session {session_id}"))?;
            ensure!(
                active.kind == LifecycleKind::Create,
                "session {session_id} is no longer being created"
            );
            active.result.clone()
        };
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match outcome? {
            DaemonLifecycleResult::Done => Ok(()),
            DaemonLifecycleResult::Move(_) | DaemonLifecycleResult::Park(_) => {
                unreachable!("cleanup cannot return a move outcome")
            }
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("session creation cannot schedule target cleanup")
            }
        }
    }
}

#[cfg(test)]
mod delegation_replay_tests {
    use crate::controller::test_support::{IsolatedTest, test_name};

    #[tokio::test]
    async fn multi_model_creation_is_refused_before_registration_or_provisioning() {
        const CHILD: &str = "MJ_TEST_MULTI_MODEL_CREATION_REFUSAL";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "multi_model_creation_is_refused_before_registration_or_provisioning",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "multi-model-creation-refusal")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut config = mj_core::config::Config::default();
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: mj_core::config::data_dir().join("missing-profile-home"),
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
                subagents: mj_core::subagent::SubagentPolicy::Native,
            },
        );
        config.save().unwrap();
        let workspace = crate::database::create_workspace("legacy delegation").unwrap();
        let mut legacy = crate::daemon::tests::runtime_test_session(
            "legacy-parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        legacy.subagents = Some(mj_core::subagent::SubagentPolicy::AllModels);
        crate::database::save_session(&legacy).unwrap();
        let runtime = crate::daemon::tests::test_runtime_state();
        let request = mj_client::daemon::CreateSessionRequest {
            create_managed_worktree: None,
            at: None,
            branch: None,
            base: None,
            subagents: Some(mj_core::subagent::SubagentPolicy::AllModels),
            review: None,
            initial_prompt: None,
            workspace_id: workspace.id,
            profile_id: "codex".into(),
            bundle_id: "missing-bundle".into(),
            project_directory: None,
            target_template_id: "missing-target".into(),
            additional_mounts: Vec::new(),
            resource_allocation: None,
            title: "must not register".into(),
            session_title_override: None,
        };
        let error = runtime
            .start_create_session(request)
            .await
            .expect_err("a refusal");
        let refusal = mj_core::refusal::Refusal::of(&error).expect("a request refusal");
        assert!(refusal.message().contains("no longer available"));
        assert!(runtime.active_lifecycles().is_empty());
        let state = crate::database::load_state().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[&legacy.id], legacy);
    }

    #[tokio::test]
    async fn replayed_spawn_does_not_reprovision_an_existing_child() {
        const CHILD: &str = "MJ_TEST_SPAWN_REPLAY";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "replayed_spawn_does_not_reprovision_an_existing_child",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "concurrency-sweep-spawn")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("spawn replay").unwrap();
        let parent = crate::daemon::tests::runtime_test_session(
            "parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        crate::database::save_session(&parent).unwrap();
        for (index, status) in [
            mj_core::state::SessionState::Running,
            mj_core::state::SessionState::Parked,
            mj_core::state::SessionState::Stopped,
        ]
        .into_iter()
        .enumerate()
        {
            let child_id = format!("child-{index}");
            let child =
                crate::daemon::tests::runtime_test_session(&child_id, &workspace.id, status);
            let relation = crate::daemon::tests::runtime_test_subagent(&child_id, "parent");
            crate::database::save_subagent_session(&child, &relation).unwrap();
            let runtime = crate::daemon::tests::test_runtime_state();
            let replayed = runtime
                .start_subagent_session(crate::controller::RegisterSubagentRequest {
                    parent_session_id: "parent".into(),
                    task_name: "must reuse original".into(),
                    profile_id: "missing-profile".into(),
                    model: None,
                    effort: None,
                    working_directory: Default::default(),
                    initial_prompt: "must not resend".into(),
                    request_key: relation.request_key.clone(),
                    report_root: None,
                })
                .await
                .unwrap();
            assert_eq!(replayed, relation);
            assert!(runtime.active_lifecycles().is_empty());
            assert_eq!(
                crate::database::load_session_record(&child_id)
                    .unwrap()
                    .unwrap()
                    .state,
                status
            );
        }
    }
}

use super::*;

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
        let relation = blocking(move || {
            let mut controller = Controller::load()?;
            controller.register_subagent(request)
        })
        .await?;
        let session_id = relation.child_session_id.clone();
        self.start_or_join_lifecycle_controlled(
            session_id.clone(),
            LifecycleKind::Create,
            None,
            Some(relation.request_key.clone()),
            None,
            move |state, session_id, cancelled| async move {
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for sub-agent startup")??;
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state,
                    session_id.clone(),
                );
                controller
                    .provision_subagent_session_controlled(&session_id, &executor)
                    .await?;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
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
        let _upgrade_work = crate::upgrade::activity("session admission")?;
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
                    launch_base: request.launch_base,
                    mjolnir_subagents: request.mjolnir_subagents,
                    initial_prompt: request.initial_prompt,
                    workspace_id: request.workspace_id,
                    additional_mounts: request.additional_mounts,
                    resource_allocation: request.resource_allocation,
                    project_directory,
                    session_title_override: request.session_title_override,
                },
            )?;
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
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
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
            DaemonLifecycleResult::Move(_) => unreachable!("cleanup cannot return a move outcome"),
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("session creation cannot schedule target cleanup")
            }
        }
    }
}

use super::views::{LifecycleStallProgress, StalledLifecycle};
use super::*;
use mj_core::subagent::SubagentPolicy;

#[derive(Clone, Copy)]
pub(super) enum CreateDelegationDefault {
    Mjolnir,
    Harness,
}

/// How long creating a parent's report root on its target may take.
const REPORT_ROOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Bounds one reviewer readiness wait that runs with no Create to end it.
///
/// The clock runs only while no Create is running, so it starts when the wait
/// began or when the Create finished, whichever is later. A running Create is
/// bounded by its own stall check instead, which lets a slow Create take as
/// long as it keeps making progress.
pub(super) struct PostCreateWait {
    what: &'static str,
    since: Option<tokio::time::Instant>,
}

impl PostCreateWait {
    pub(super) fn new(what: &'static str) -> Self {
        Self {
            what,
            since: Some(tokio::time::Instant::now()),
        }
    }

    fn create_running(&mut self) {
        self.since = None;
    }

    /// The harness startup bound the other readiness waits share, so a
    /// reviewer is given as long as any other session to open.
    fn check(&mut self, child: &str) -> Result<()> {
        let since = *self.since.get_or_insert_with(tokio::time::Instant::now);
        let bound = mj_controller::controller::NATIVE_SESSION_STARTUP_TIMEOUT;
        anyhow::ensure!(
            since.elapsed() < bound,
            "the reviewer child {child} did not {} within {} s of its Create finishing",
            self.what,
            bound.as_secs()
        );
        Ok(())
    }
}

impl RuntimeState {
    pub(super) async fn start_create_session(
        self: &Arc<Self>,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        self.start_create_session_with_delegation_default(request, CreateDelegationDefault::Mjolnir)
            .await
    }

    pub(super) async fn start_create_session_with_delegation_default(
        self: &Arc<Self>,
        mut request: CreateSessionRequest,
        delegation_default: CreateDelegationDefault,
    ) -> Result<RegisteredSession> {
        let (primary_selection, provider_reservation, delegation_notice) = if request
            .profile_id
            .is_empty()
        {
            let config = self.owner().controller().config.clone();
            let execution_policy = if config
                .targets
                .get(&request.target_template_id)
                .is_some_and(|target| target.is_yolo())
            {
                mj_core::config::ExecutionPolicy::Unconstrained
            } else {
                mj_core::config::ExecutionPolicy::ConfiguredApprovals
            };
            let backend: Arc<dyn crate::server::api::SubagentBackend> = self
                .quota_report_backend()
                .ok_or_else(|| anyhow::anyhow!("session selection backend is unavailable"))?;
            let default_empty_title = mj_core::state::default_session_title(
                request.project_directory.as_deref(),
                &request.bundle_id,
                "",
            );
            let apply_chosen_profile = |request: &mut CreateSessionRequest, profile_id: &str| {
                if request.title.trim().is_empty()
                    || request.title.ends_with(" ()")
                    || request.title == default_empty_title
                {
                    request.title = mj_core::state::default_session_title(
                        request.project_directory.as_deref(),
                        &request.bundle_id,
                        profile_id,
                    );
                }
                request.profile_id = profile_id.to_owned();
            };
            if let Some(provider) = request.provider.clone() {
                let selected = crate::server::api::resolve_provider_session_selection(
                    &backend,
                    &config,
                    &provider,
                    execution_policy,
                    request.subagents.as_ref(),
                )
                .await
                .map_err(|failure| anyhow::anyhow!(failure.message))?;
                apply_chosen_profile(&mut request, &selected.profile_id);
                let mut notice = None;
                if matches!(
                    request.subagents,
                    Some(mj_core::subagent::SubagentPolicy::Native)
                ) && let Some(profile) = config.enabled_profile(&selected.profile_id)
                    && !profile.kind.has_native_subagents()
                {
                    request.subagents = Some(mj_core::subagent::SubagentPolicy::None);
                    notice = Some(format!(
                        "Disabled native sub-agents for this session: selected profile {} ({}) does not support native sub-agents.",
                        selected.profile_id,
                        profile.kind.display_name(),
                    ));
                }
                (None, Some(selected.reservation), notice)
            } else {
                anyhow::ensure!(
                    config.subagents.primary_configured(),
                    "Auto (Primary) requires at least one entry in [[subagents.primary]]"
                );
                let selected = crate::server::api::resolve_primary_session_selection(
                    &backend,
                    None,
                    config.subagents.primary_choices(),
                    execution_policy,
                )
                .await
                .map_err(|failure| anyhow::anyhow!(failure.message))?;
                apply_chosen_profile(&mut request, &selected.profile_id);
                (Some((selected, backend)), None, None)
            }
        } else {
            (None, None, None)
        };
        let (registered, _, warning) = self
            .start_create_session_inner(
                request,
                LifecycleControl::default(),
                None,
                delegation_default,
            )
            .await?;
        if let Some(reservation) = provider_reservation {
            reservation.commit(Some(registered.session.id.clone()));
        }
        if let Some(notice) = delegation_notice {
            self.push_notice(&registered.session.id, notice);
        }
        if let Some((selected, backend)) = primary_selection {
            selected
                .reservation
                .commit(Some(registered.session.id.clone()));
            if let Err(error) = backend
                .start_followup(
                    registered.session.id.clone(),
                    crate::server::api::StartFollowup {
                        model: Some(selected.model.clone()),
                        effort: selected.effort.clone(),
                        ..Default::default()
                    },
                )
                .await
            {
                tracing::warn!(session_id = %registered.session.id, model = %selected.model, effort = ?selected.effort, %error, "failed to queue selected primary model/effort after creating session");
                self.push_notice(
                    &registered.session.id,
                    format!("Session created on profile {}, but applying primary model {} failed ({error:#}). Select the model manually in the session.", registered.session.last_profile, selected.model),
                );
            }
        }
        if let Some(warning) = warning {
            self.push_notice(&registered.session.id, warning);
        }
        Ok(registered)
    }

    pub(crate) async fn start_orchestrator_session(
        self: &Arc<Self>,
        parent_id: String,
        request: mj_core::subagent::SubagentToolRequest,
        prepared: Option<mj_controller::database::PreparedDelegation>,
    ) -> Result<mj_core::subagent::SubagentRecord> {
        use mj_controller::controller::{OrchestratorPlacement, RegisterSubagentRequest};
        use mj_core::subagent::{SubagentKind, SubagentTaskType, SubagentToolAction};

        let SubagentToolAction::SpawnOrchestrator {
            task_name,
            instructions,
            target,
            repos,
            no_github_credentials,
        } = request.action
        else {
            anyhow::bail!("expected an orchestrator spawn");
        };
        let request_id = request.request_id;
        let existing = blocking({
            let parent_id = parent_id.clone();
            let request_id = request_id.clone();
            move || {
                mj_controller::database::lookup_subagent_request(
                    &parent_id,
                    &SubagentKind::Worker.request_key(&request_id),
                )
            }
        })
        .await?;
        if let Some(relation) = existing {
            self.finish_subagent_registration(
                &relation,
                self.session_record(&relation.child_session_id)
                    .is_none_or(|session| {
                        session.state == mj_core::state::SessionState::Provisioning
                    }),
                false,
            )
            .await?;
            return Ok(relation);
        }

        let projection = self.runtime_publication()?;
        let parent = projection
            .records
            .get(&parent_id)
            .context("unknown parent session")?;
        let policy = parent
            .subagents
            .as_ref()
            .filter(|policy| policy.orchestrators_enabled())
            .context("orchestrators are not enabled for this session")?;
        ensure!(
            !projection.subagents.contains_key(&parent_id),
            "only top-level sessions can spawn orchestrators"
        );
        ensure!(
            parent.state.can_receive_child_work(),
            "parent is no longer accepting child work"
        );
        ensure!(
            !task_name.trim().is_empty(),
            "sub-agent task name cannot be empty"
        );
        ensure!(
            !instructions.trim().is_empty(),
            "sub-agent instructions cannot be empty"
        );

        let target_id = target.unwrap_or_else(|| parent.target_template_id.clone());
        let config = self.with_config(Clone::clone);
        let template = config.ensure_target_enabled(&target_id)?;
        let bare = mj_core::config::is_bare_project_target(template);
        let bare_error = || {
            let container = config
                .enabled_targets()
                .find(|(_, target)| mj_core::config::is_container_target(target))
                .map(|(id, _)| format!("use container target {id:?}"))
                .unwrap_or_else(|| "configure and choose a container target".to_owned());
            anyhow!(
                "Bare target {target_id:?} requires an existing project directory. Please {container} for repository URLs"
            )
        };
        if let Some(repos) = &repos {
            ensure!(
                !repos.is_empty(),
                "repos must contain at least one repository URL"
            );
            for url in repos {
                mj_core::remote_git::validate_network_url(url).context("invalid repository URL")?;
            }
        }

        let directory = if bare {
            ensure!(repos.is_none(), bare_error());
            Some(
                parent
                    .project_directory
                    .clone()
                    .filter(|_| target_id == parent.target_template_id)
                    .ok_or_else(bare_error)?,
            )
        } else {
            None
        };
        let (bundle_id, project) = if let Some(repos) = repos {
            let created = self.create_bundle_from_sources(repos, false).await?;
            let id = created.bundle_id;
            let project = blocking({
                let id = id.clone();
                move || {
                    mj_controller::database::read_project_catalog()?
                        .projects
                        .into_iter()
                        .find(|entry| entry.bundle_id == id)
                        .map(|entry| Arc::new(entry.project))
                        .context("created project is missing from the catalog")
                }
            })
            .await?;
            (id, project)
        } else {
            let project = parent.project.clone()
                .filter(|project| !project.primary_is_plain_directory())
                .context("parent has no cloneable project bundle. Supply repos with repository URLs on a container target")?;
            ensure!(
                bare || project.bundle.repositories.iter().all(|repo| {
                    repo.github.is_some() || project.network_sources.contains_key(&repo.id)
                }),
                "parent has no cloneable project bundle. Supply repos with repository URLs"
            );
            (parent.bundle_id.clone(), project)
        };
        let allocation = crate::server_runtime::snapshot::target_container_sizing(
            template,
            &projection.metadata,
            crate::server_runtime::snapshot::host_capacity_for_target(
                &projection.metadata,
                &target_id,
            ),
        )
        .map(
            |sizing| mj_core::state::SessionResourceAllocation::Container {
                cpus: sizing.default.cpus,
                memory_bytes: sizing.default.memory_bytes,
            },
        );
        if let Some(allocation) = &allocation {
            validate_minimum_container_allocation(allocation)?;
        }
        let durable = prepared.is_some();
        let prepared = prepared.and_then(|effect| effect.spawn);
        let (selection, reservation) = if let Some(prepared) = prepared {
            (prepared, None)
        } else {
            let backend: Arc<dyn crate::server::api::SubagentBackend> = self
                .quota_report_backend()
                .context("orchestrator placement backend is unavailable")?;
            let selected = crate::server::api::resolve_subagent_policy_selection(
                &backend,
                &parent.last_profile,
                policy,
                SubagentTaskType::Orchestrator,
                template.execution_policy(),
            )
            .await
            .map_err(|failure| anyhow!(failure.message))?;
            let selection = mj_controller::database::PreparedSpawn {
                profile_id: selected.profile_id,
                model: selected.model,
                effort: selected.effort,
                fast_mode: selected.fast_mode,
                prompt: instructions,
            };
            let selection = if durable {
                blocking({
                    let parent_id = parent_id.clone();
                    let request_id = request_id.clone();
                    move || {
                        mj_controller::database::prepare_delegation_spawn(
                            parent_id, request_id, selection,
                        )
                    }
                })
                .await?
            } else {
                selection
            };
            (selection, Some(selected.reservation))
        };
        let capacity = self
            .admit_container_launch(&target_id, allocation.clone(), None, BTreeSet::new(), None)
            .await?;
        let _upgrade_work = mj_controller::upgrade::activity("orchestrator admission")?;
        let manager = self.session_manager.clone();
        let relation = blocking(move || {
            let mut controller = Controller::load()?;
            let directory = directory
                .as_deref()
                .map(|directory| {
                    controller.resolve_project_directory(
                        &target_id,
                        directory,
                        &CancellableProcessExecutor::with_timeout(REPORT_ROOT_TIMEOUT),
                    )
                })
                .transpose()?;
            controller.register_orchestrator_with_manager(
                RegisterSubagentRequest {
                    kind: SubagentKind::Worker,
                    orchestrator: true,
                    reviewer_launch: None,
                    parent_session_id: parent_id,
                    task_name,
                    profile_id: selection.profile_id,
                    model: Some(selection.model),
                    effort: selection.effort,
                    fast_mode: selection.fast_mode,
                    working_directory: Default::default(),
                    initial_prompt: selection.prompt,
                    request_key: request_id,
                    report_root: None,
                    category: SubagentTaskType::Orchestrator.id().to_owned(),
                    no_github_credentials,
                },
                OrchestratorPlacement {
                    target_id,
                    bundle_id,
                    project,
                    directory,
                    allocation,
                },
                &manager,
            )
        })
        .await?;
        if let Some(reservation) = reservation {
            reservation.commit(Some(relation.child_session_id.clone()));
        }
        drop(capacity);
        self.finish_subagent_registration(&relation, true, true)
            .await?;
        Ok(relation)
    }

    /// Register and start a child worker on its parent's existing target.
    ///
    /// Once the registration has committed, any failure before the worker's
    /// own lifecycle takes over leaves a `Provisioning` child that nothing
    /// owns. One match on that stretch moves such a child to `Error`, so the
    /// parent sees a failed child and the oracle's one-live-oracle rule frees
    /// its slot.
    pub async fn start_subagent_session(
        self: &Arc<Self>,
        request: mj_controller::controller::RegisterSubagentRequest,
    ) -> Result<mj_core::subagent::SubagentRecord> {
        let _upgrade_work = mj_controller::upgrade::activity("subagent admission")?;
        let manager = self.session_manager.clone();
        let (relation, needs_provisioning, registered_here) = blocking(move || {
            let mut controller = Controller::load()?;
            if let Some(existing) = mj_controller::database::lookup_subagent_request(
                &request.parent_session_id,
                &request.kind.request_key(&request.request_key),
            )? {
                let needs_provisioning = controller
                    .state
                    .sessions
                    .get(&existing.child_session_id)
                    .is_some_and(|record| {
                        record.state == mj_core::state::SessionState::Provisioning
                    });
                if needs_provisioning {
                    mj_controller::controller::recover_registered_subagent_startup(
                        &existing,
                        request.fast_mode,
                    )?;
                }
                return Ok((existing, needs_provisioning, false));
            }
            let mut request = request;
            // The parent's report root is made before the child exists, so the
            // child's first prompt can name its own directory under it. A
            // request run again after a restart finds the same root.
            let executor = CancellableProcessExecutor::with_timeout(REPORT_ROOT_TIMEOUT);
            if request.kind != mj_core::subagent::SubagentKind::Reviewer {
                request.report_root = Some(
                    controller
                        .prepare_subagent_report_root(&request.parent_session_id, &executor)
                        .context("create the sub-agent report directory")?,
                );
            }
            controller
                .register_subagent_with_manager(request, &manager)
                .map(|relation| (relation, true, true))
        })
        .await?;
        self.finish_subagent_registration(&relation, needs_provisioning, registered_here)
            .await?;
        Ok(relation)
    }

    async fn finish_subagent_registration(
        self: &Arc<Self>,
        relation: &mj_core::subagent::SubagentRecord,
        needs_provisioning: bool,
        registered_here: bool,
    ) -> Result<()> {
        if let Err(error) = self
            .start_registered_subagent(relation, needs_provisioning)
            .await
        {
            if registered_here {
                self.fail_unstarted_subagent(&relation.child_session_id, &error)
                    .await;
            }
            if let Err(reload_error) = self.reload_controller().await {
                tracing::warn!(
                    child_session_id = %relation.child_session_id,
                    error = format!("{reload_error:#}"),
                    "could not reload the controller after a sub-agent failed to start"
                );
            }
            if let Err(prompt_error) = self
                .reconcile_parent_reports(&relation.parent_session_id)
                .await
            {
                tracing::warn!(
                    child_session_id = %relation.child_session_id,
                    parent_session_id = %relation.parent_session_id,
                    error = format!("{prompt_error:#}"),
                    "could not enqueue the parent's finished child reports after provisioning failure"
                );
            }
            return Err(error);
        }
        self.reload_controller().await?;
        Ok(())
    }

    /// Everything a registered child needs before its own lifecycle owns it:
    /// publishing the registration, queueing its first delivery, and admitting
    /// the lifecycle that provisions it. The lifecycle records its own
    /// failures.
    async fn start_registered_subagent(
        self: &Arc<Self>,
        relation: &mj_core::subagent::SubagentRecord,
        needs_provisioning: bool,
    ) -> Result<()> {
        self.reload_controller().await?;
        if relation.kind != mj_core::subagent::SubagentKind::Reviewer {
            let id = relation.child_session_id.clone();
            if let Some(delivery) =
                blocking(move || mj_controller::database::next_startup_delivery(&id)).await?
            {
                self.start_persisted_startup_delivery(delivery, &CancellationToken::new());
            }
        }
        if !needs_provisioning {
            return Ok(());
        }
        self.start_create_lifecycle(
            relation.child_session_id.clone(),
            LifecycleControl::default(),
            CreateInputs {
                child: Some(relation.clone()),
                publication: None,
                remember_launch_choices: false,
            },
            None,
        )
        .await
    }

    /// Record, through the ordinary lifecycle write, that a child registered by
    /// this call failed before its lifecycle started. A child that has already
    /// left `Provisioning` is not touched.
    async fn fail_unstarted_subagent(
        self: &Arc<Self>,
        child_session_id: &str,
        error: &anyhow::Error,
    ) {
        let cause = format!("sub-agent startup failed: {error:#}");
        let written = blocking({
            let child_session_id = child_session_id.to_owned();
            move || Controller::load()?.fail_interrupted_lifecycle(&child_session_id, &cause)
        })
        .await;
        if let Err(write_error) = written {
            tracing::warn!(
                child_session_id,
                error = format!("{write_error:#}"),
                "could not record a sub-agent that failed to start"
            );
        }
    }

    pub(crate) async fn start_reviewer_child(self: &Arc<Self>, child: &str) -> Result<()> {
        let id = child.to_owned();
        let relation = blocking(move || mj_controller::database::load_subagent(&id))
            .await?
            .context("reviewer child is missing")?;
        let launch = relation
            .reviewer_launch
            .as_ref()
            .context("reviewer launch configuration is missing")?
            .clone();
        self.start_subagent_session(mj_controller::controller::RegisterSubagentRequest {
            no_github_credentials: false,
            kind: relation.kind,
            orchestrator: relation.orchestrator,
            reviewer_launch: relation.reviewer_launch.clone(),
            parent_session_id: relation.parent_session_id.clone(),
            task_name: relation.task_name.clone(),
            profile_id: relation.profile_id.clone(),
            model: relation.model.clone(),
            effort: relation.effort.clone(),
            fast_mode: launch.fast_mode.unwrap_or(false),
            working_directory: relation.working_directory.clone(),
            initial_prompt: relation.initial_prompt.clone(),
            request_key: relation.request_key.clone(),
            report_root: None,
            category: match launch.role {
                mj_core::worker_launch::ReviewChildRole::Reviewer => "reviewer",
                mj_core::worker_launch::ReviewChildRole::BriefWriter => "brief_writer",
            }
            .to_owned(),
        })
        .await?;
        // No total deadline here: one counted queue time and cancelled an
        // accepted Create mid-provision, which left the child without a worker
        // root (#1438). While the Create runs, each wait ends with its
        // outcome: the child is ready, its record says the Create failed or
        // it was closed, or the Create stalled for `LIFECYCLE_STALL_BOUND` and
        // was cancelled. Once no Create is running (it finished, or the
        // reviewer was restored without one) nothing else ends a wait, so
        // `PostCreateWait` bounds it.
        let mut waiting = PostCreateWait::new("open its session");
        let handle = loop {
            self.reviewer_child_can_still_start(child, &mut waiting)?;
            // `None` is the normal wait for the actor; a stopped session
            // manager is a real failure and ends the start now.
            if let Some(handle) = self.session_manager.find_session(child.to_owned()).await? {
                break handle;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let mut waiting = PostCreateWait::new("report its capabilities");
        loop {
            let view = handle.view();
            if view.connected
                && view
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.operational.agent_capabilities.is_some())
            {
                break;
            }
            if let Some(error) = view.error {
                anyhow::bail!("{error:?}");
            }
            self.reviewer_child_can_still_start(child, &mut waiting)?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        use crate::server_runtime::api::ExportRuntime;
        Arc::clone(self)
            .queue_startup(
                child.to_owned(),
                relation.request_key.clone(),
                crate::server::api::StartFollowup {
                    model: relation.model,
                    effort: relation.effort,
                    fast_mode: launch.fast_mode.unwrap_or(false),
                    prompt: Some(relation.initial_prompt),
                },
            )
            .await?;
        // The startup delivery fails the step when the session reaches a state
        // that cannot take it; `PostCreateWait` bounds a harness that never
        // answers.
        let mut waiting = PostCreateWait::new("take its first prompt");
        loop {
            match self.startup_status(child.to_owned()).await? {
                Some(crate::server::api::StartStatus::Submitted { .. }) => return Ok(()),
                Some(crate::server::api::StartStatus::Failed { message }) => {
                    anyhow::bail!("{message}")
                }
                _ => {
                    self.reviewer_child_can_still_start(child, &mut waiting)?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Fail unless the reviewer child's record is still one a Create can
    /// finish: missing, closed, failed or destroyed children never become
    /// ready, so waiting for them would never end. Also fail, and cancel, a
    /// Create that has stalled (see [`LIFECYCLE_STALL_BOUND`]), and a readiness
    /// wait that has gone on too long with no Create running.
    fn reviewer_child_can_still_start(
        &self,
        child: &str,
        waiting: &mut PostCreateWait,
    ) -> Result<()> {
        self.check_create_progress(child, LIFECYCLE_STALL_BOUND, waiting)?;
        let Some(record) = self.session_record(child) else {
            anyhow::bail!("reviewer child {child} no longer exists");
        };
        match record.state {
            // `StartupCleanup` is provisioning retrying a worker start that
            // failed before publishing its socket; the retry can still end
            // in `Running`.
            mj_core::state::SessionState::Provisioning
            | mj_core::state::SessionState::StartupCleanup
            | mj_core::state::SessionState::Running
            | mj_core::state::SessionState::Disconnected
            | mj_core::state::SessionState::Checkpointing => Ok(()),
            state => match record.last_error {
                Some(cause) => anyhow::bail!("reviewer child {child} is {state:?}: {cause}"),
                None => anyhow::bail!("reviewer child {child} is {state:?}"),
            },
        }
    }

    /// Fail when the child's running Create has gone `bound` without a
    /// provisioning stage change or transfer progress, and cancel that Create in
    /// the same decision, through the cancellation close and destroy use, so
    /// it stops holding its lifecycle admission. The stage signal is the one
    /// the lifecycle already keeps for its progress display. Waiting for a
    /// slot is a stage of its own and is never a stall; admission ends it and
    /// so restarts the clock.
    ///
    /// With no Create running, `waiting` bounds the readiness wait instead.
    pub(super) fn check_create_progress(
        &self,
        child: &str,
        bound: Duration,
        waiting: &mut PostCreateWait,
    ) -> Result<()> {
        match self.cancel_stalled_lifecycle(child, LifecycleKind::Create, bound) {
            LifecycleStallProgress::NotRunning => waiting.check(child),
            LifecycleStallProgress::Running(_) => {
                waiting.create_running();
                Ok(())
            }
            LifecycleStallProgress::Stalled {
                idle,
                stages,
                stream,
                phase,
            } => {
                let stage = if stages.is_empty() {
                    "between stages".to_owned()
                } else {
                    stages.join(" + ")
                };
                let outcome = match phase {
                    StalledLifecycle::Cancelled => "its Create was cancelled",
                    StalledLifecycle::AlreadyCancelling => "its Create was already being cancelled",
                    StalledLifecycle::Committed => "its Create had committed and was left running",
                };
                tracing::warn!(session_id = child, idle_secs = idle.as_secs(), %stage, ?stream, ?phase, "reviewer child Create stalled");
                let detail = match stream {
                    Some(stream) => format!("{stream} moved no bytes for {} s", idle.as_secs()),
                    None => format!(
                        "no stage started or finished for {} s (stalled in: {stage})",
                        idle.as_secs()
                    ),
                };
                anyhow::bail!(
                    "the reviewer child {child} stalled while provisioning: {detail}; {outcome}"
                )
            }
        }
    }

    pub async fn start_create_session_controlled(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: LifecycleControl,
        publication: tokio::sync::oneshot::Receiver<std::result::Result<(), String>>,
    ) -> Result<(RegisteredSession, Option<&'static str>, Option<String>)> {
        self.start_create_session_inner(
            request,
            control,
            Some(publication),
            CreateDelegationDefault::Mjolnir,
        )
        .await
    }

    pub(super) async fn start_create_session_inner(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: LifecycleControl,
        publication: Option<tokio::sync::oneshot::Receiver<std::result::Result<(), String>>>,
        delegation_default: CreateDelegationDefault,
    ) -> Result<(RegisteredSession, Option<&'static str>, Option<String>)> {
        let mut admission_timing =
            mj_controller::launch_timing::LaunchPhaseTimer::new("", "admit and register session");
        tracing::info!(profile_id = %request.profile_id, target_id = %request.target_template_id,
            "session launch admission started");
        let parent = request.profile_id.clone();
        let (config, kind) = blocking(move || {
            let controller = Controller::load()?;
            let kind = controller.config.named_profile(&parent)?.kind;
            Ok((controller.config, kind))
        })
        .await?;
        let implicit_mjolnir = request.subagents.is_none()
            && matches!(delegation_default, CreateDelegationDefault::Mjolnir);
        let policy = request
            .subagents
            .unwrap_or_else(|| match delegation_default {
                CreateDelegationDefault::Mjolnir => SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
                CreateDelegationDefault::Harness => SubagentPolicy::default_for(kind),
            });
        let validation_timing =
            mj_controller::launch_timing::LaunchPhaseTimer::new("", "validate subagent choices");
        let validation =
            mj_controller::controller::profile_config::validate_session_subagent_policy(
                self.profile_catalog
                    .get()
                    .expect("catalog installed before serving"),
                &config,
                &request.profile_id,
                &policy,
                &request.target_template_id,
            )
            .await;
        let (policy, delegation_notice) = match validation {
            Err(error)
                if implicit_mjolnir
                    && let Some(refusal) = mj_core::refusal::Refusal::of(&error)
                    && refusal.code() == Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE) =>
            {
                let fallback = SubagentPolicy::default_for(kind);
                let mode = if fallback.is_native() {
                    "native"
                } else {
                    "none"
                };
                let notice = format!(
                    "Mjolnir delegation is unavailable. Using sub-agent mode {mode} for this session. {}",
                    refusal.message()
                );
                (fallback, Some(notice))
            }
            result => {
                result?;
                (policy, None)
            }
        };
        drop(validation_timing);
        if let Some(allocation) = request.resource_allocation.as_ref() {
            validate_minimum_container_allocation(allocation)?;
        }
        if !request.bundle_id.is_empty() {
            mj_controller::controller::config_only_controller(config.clone())
                .validate_github_bundle_installations(&request.bundle_id)
                .await
                .map_err(mj_controller::controller::GithubBundleSelectionError::into_anyhow)?;
        }
        let _container_admission = self
            .admit_container_launch(
                &request.target_template_id,
                request.resource_allocation.clone(),
                None,
                BTreeSet::new(),
                None,
            )
            .await?;
        // Discovery is restartable preparation, not admitted lifecycle work.
        let _upgrade_work = mj_controller::upgrade::activity("session admission")?;
        let skip_remembering_subagent_policy = delegation_notice.is_some();
        let path_cancelled = control.cancelled.clone();
        let remember_launch_choices = request.remember_launch_choices;
        let preferences_owner = self.clone();
        let (registered, review_notice, oracle_notice) = blocking(move || {
            let mut controller = Controller::load()?;
            let path_executor =
                mj_controller::targets::CancellableProcessExecutor::new(path_cancelled)
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
            let registration = controller.register_session_reporting_review(
                &request.profile_id,
                &request.bundle_id,
                &request.target_template_id,
                request.title,
                SessionLaunchOptions {
                    remember_launch_choices: request.remember_launch_choices,
                    skip_remembering_subagent_policy,
                    remember_container_size: request.remember_container_size,
                    create_managed_worktree: request.create_managed_worktree,
                    at: request.at,
                    branch: request.branch,
                    base: request.base,
                    no_later_history: request.no_later_history,
                    subagents: Some(policy),
                    oracle: request.oracle,
                    oracle_choice: request.oracle_choice,
                    review: request.review,
                    no_project_memory: request.no_project_memory,
                    no_github_events: request.no_github_events,
                    no_github_credentials: request.no_github_credentials,
                    notifications_ignored: request.notifications_ignored,
                    initial_prompt: request.initial_prompt,
                    workspace_id: request.workspace_id,
                    additional_mounts: request.additional_mounts,
                    resource_allocation: request.resource_allocation,
                    project_directory,
                    session_title_override: request.session_title_override,
                },
            )?;
            let session_id = registration.id;
            // Only interactive launches seed defaults. A failed preference write
            // does not undo a session that was created.
            if request.remember_launch_choices
                && let Err(error) = mj_core::go::GoPreferences::remember_first_pair(
                    &mj_core::go::GoPreferences::path(),
                    &request.profile_id,
                    &request.target_template_id,
                )
            {
                tracing::warn!(%error, "could not save the default profile and target");
            }
            if request.remember_launch_choices {
                let _preferences_update = preferences_owner
                    .recent_launch_update
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let recent = mj_core::go::RecentLaunch {
                    profile_id: request.profile_id.clone(),
                    target_id: request.target_template_id.clone(),
                    bundle_id: controller.state.sessions[&session_id].bundle_id.clone(),
                    project_directory: controller.state.sessions[&session_id]
                        .project_directory
                        .clone(),
                };
                if let Err(error) = mj_core::go::GoPreferences::remember_recent(
                    &mj_core::go::GoPreferences::path(),
                    recent.clone(),
                ) {
                    tracing::warn!(%error, "could not save recent interactive launch choices");
                } else {
                    preferences_owner.recent_launch.send_replace(Some(recent));
                }
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
            Ok((
                RegisteredSession {
                    session,
                    remembered_container_size,
                },
                registration.review_notice,
                registration.oracle_notice,
            ))
        })
        .await?;
        admission_timing.set_session_id(&registered.session.id);
        drop(admission_timing);
        let warning = [
            delegation_notice,
            oracle_notice,
            _container_admission.warning.clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
        let warning = (!warning.is_empty()).then_some(warning);
        drop(_container_admission);
        let session_id = registered.session.id.clone();
        self.start_create_lifecycle(
            session_id,
            control,
            CreateInputs {
                child: None,
                publication,
                remember_launch_choices,
            },
            None,
        )
        .await?;
        let publishing = mj_controller::launch_timing::LaunchPhaseTimer::new(
            &registered.session.id,
            "publish registered session view",
        );
        self.reload_controller().await?;
        drop(publishing);
        Ok((registered, review_notice, warning))
    }

    /// Admit the Create lifecycle of a registered session, top-level or
    /// sub-agent: the one create body. A create is resumable: shutdown
    /// interrupts it before it commits instead of cancelling it, it holds no
    /// handoff admission while it provisions, and the next daemon finishes it
    /// from the durable create intent.
    ///
    /// `recovery_operation` is the intent's operation id when a restarted
    /// daemon re-admits an interrupted create; that run first retires what
    /// the interrupted attempt built. A sub-agent create is always admitted
    /// under its own request key, so a replayed spawn joins it instead of
    /// starting a second one.
    pub(super) async fn start_create_lifecycle(
        self: &Arc<Self>,
        session_id: String,
        control: LifecycleControl,
        inputs: CreateInputs,
        recovery_operation: Option<String>,
    ) -> Result<()> {
        let recovering = recovery_operation.is_some();
        let scheduled_at = std::time::Instant::now();
        let automatic = recovering || inputs.child.is_some();
        let request_key = inputs
            .child
            .as_ref()
            .map(|child| child.request_key.clone())
            .or_else(|| recovery_operation.map(|operation| format!("recovery:{operation}")));
        let mut start =
            super::lifecycle::LifecycleStart::request(String::new(), None, Some(control.clone()));
        start.request_key = request_key;
        if automatic {
            start = start.automatic();
        }
        self.admit_lifecycle(
            session_id,
            LifecycleKind::Create,
            start,
            move |state, session_id, cancelled, _session_admission| async move {
                state
                    .run_admitted_create(
                        session_id,
                        cancelled,
                        control,
                        inputs,
                        recovering,
                        scheduled_at,
                    )
                    .await
            },
        )
        .await?;
        Ok(())
    }

    pub(super) async fn start_failed_launch_retry(
        self: &Arc<Self>,
        record: SessionRecord,
        request_key: String,
        control: LifecycleControl,
    ) -> Result<StartedLifecycle> {
        let (accepted, acceptance) = tokio::sync::watch::channel(None);
        let scheduled_at = std::time::Instant::now();
        self.admit_prepared_lifecycle(
            record.id.clone(),
            LifecycleKind::Create,
            super::lifecycle::LifecycleStart::request(request_key, None, Some(control.clone())),
            Some(acceptance),
            move |state, session_id, cancelled, _session_admission| async move {
                let capacity = state
                    .admit_container_launch(
                        &record.target_template_id,
                        record.resource_allocation.clone(),
                        Some(&session_id),
                        BTreeSet::from([session_id.clone()]),
                        None,
                    )
                    .await?;
                {
                    let _handoff = mj_controller::upgrade::activity_unless_draining("failed launch retry admission")?;
                    ensure!(!cancelled.load(Ordering::Acquire), "launch retry cancelled before admission");
                    let Some((operation_id, first_prompt_unsent)) = blocking(move || {
                        // Retry dismisses settled groups, so check for an unsent
                        // first prompt before it does. This only affects the notice.
                        let first_prompt_unsent = match mj_controller::database::load_latest_startup_group(&record.id) {
                            Ok(steps) => steps.iter().any(|step| {
                                step.phase == "failed"
                                    && step.accepted_ordinal.is_none()
                                    && matches!(
                                        serde_json::from_str::<StartupStep>(&step.step_json),
                                        Ok(StartupStep::ApiPrompt { .. })
                                    )
                            }),
                            Err(error) => {
                                tracing::warn!(session_id = %record.id, %error, "could not read startup steps for the retry notice");
                                false
                            }
                        };
                        mj_controller::database::begin_failed_launch_retry(record)
                            .map(|retry| retry.map(|operation_id| (operation_id, first_prompt_unsent)))
                    }).await? else {
                        return Err(Refusal::precondition(
                            "This session changed while Retry launch was waiting. Check its current status.",
                        ).into());
                    };
                    tracing::info!(%session_id, %operation_id, "accepted retry of failed session launch");
                    let notice = if first_prompt_unsent {
                        let instance = mj_core::config::instance_name()
                            .map_or_else(String::new, |name| format!(" --instance={name}"));
                        format!(
                            "Relaunching the stored launch request. Your first prompt is still unsent. In the dashboard, type or paste it again. From the shell, resend your original prompt file with `mj{instance} message --session {session_id} --prompt-file ORIGINAL_PROMPT_FILE` or pass your original prompt text as an argument to `mj{instance} message --session {session_id}`."
                        )
                    } else {
                        "Relaunching the stored launch request. Prompts that failed with the previous launch are not resent.".to_owned()
                    };
                    state.push_public_notice(&session_id, notice);
                    accepted.send_replace(Some(Ok(())));
                }
                state.reload_controller().await?;
                drop(capacity);
                state.run_admitted_create(
                    session_id,
                    cancelled,
                    control,
                    CreateInputs { child: None, publication: None, remember_launch_choices: false },
                    true,
                    scheduled_at,
                ).await
            },
        ).await
    }

    async fn run_admitted_create(
        self: Arc<Self>,
        session_id: String,
        cancelled: Arc<AtomicBool>,
        control: LifecycleControl,
        inputs: CreateInputs,
        recovering: bool,
        scheduled_at: std::time::Instant,
    ) -> Result<DaemonLifecycleResult> {
        let state = self;
        tracing::info!(%session_id, elapsed_ms = scheduled_at.elapsed().as_millis(),
            "session launch lifecycle scheduled");
        let _total =
            mj_controller::launch_timing::LaunchPhaseTimer::new(&session_id, "create lifecycle");
        let loading =
            mj_controller::launch_timing::LaunchPhaseTimer::new(&session_id, "load controller");
        let mut controller = mj_core::runtime::spawn_blocking(Controller::load)
            .await
            .context("load controller for daemon create task")??;
        drop(loading);
        // Recovery may already have settled this registration before
        // admission; replay never provisions a finished session. A
        // recovering create is checked by its reset instead.
        if !recovering
            && controller
                .state
                .sessions
                .get(&session_id)
                .is_none_or(|record| record.state != mj_core::state::SessionState::Provisioning)
        {
            return Ok(DaemonLifecycleResult::Done);
        }
        let CreateInputs {
            child,
            publication,
            remember_launch_choices,
        } = inputs;
        let parent = child.map(|relation| relation.parent_session_id);
        let publication_timing =
            mj_controller::launch_timing::LaunchPhaseTimer::new(&session_id, "await publication");
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
        drop(publication_timing);
        if publication_error.is_some() {
            control.request_cancel("session publication failed");
        }
        let executor = DaemonStageReportingExecutor::new(
            CancellableProcessExecutor::new(cancelled),
            state.clone(),
            session_id.clone(),
        );
        // The commit and the write that marks the session running
        // are the one stretch a handoff waits for: a daemon replaced
        // between them would redo a create that had just succeeded.
        let commit_admission = Mutex::new(None);
        let commit = || {
            let admission = match mj_controller::upgrade::activity("session create commit") {
                Ok(admission) => admission,
                Err(error) => {
                    // The daemon is being replaced: the next one owns
                    // this create, which has not committed.
                    control.cancel_for_shutdown();
                    return Err(error);
                }
            };
            ensure!(
                control.grant_commit(),
                "session creation cancelled before commit"
            );
            *commit_admission
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(admission);
            Ok(())
        };
        // A create stopped before the phone was told it exists is
        // discarded by the ordinary rollback, not left for the next
        // daemon to provision for a client that was never answered.
        let published = publication_error.is_none();
        let interrupted = || published && control.stopped_for_shutdown();
        let provision = controller
            .run_create(
                &session_id,
                &executor,
                remember_launch_choices,
                recovering,
                commit,
                &interrupted,
            )
            .await;
        drop(commit_admission);
        if let Some(parent) = parent
            && provision.is_err()
        {
            drop(controller);
            state.reload_controller().await?;
            if let Err(prompt_error) = state.reconcile_parent_reports(&parent).await {
                tracing::warn!(
                    child_session_id = %session_id,
                    parent_session_id = parent,
                    error = format!("{prompt_error:#}"),
                    "could not enqueue the parent's finished child reports after provisioning failure"
                );
            }
        }
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
    }

    /// Re-admit the creates a previous daemon's shutdown interrupted. Runs at
    /// startup before clients are served, so a client that reconnects to wait
    /// for its session finds the create in the lifecycle registry. Returns the
    /// sessions it owns, which the unowned-lifecycle reconciliation must skip.
    ///
    /// An intent whose session is no longer creating (finished or failed by
    /// something that did not settle the intent, such as an older daemon) is
    /// discarded. Capacity is not admitted again: the original create passed
    /// that gate, and refusing here would strand a session the person already
    /// has.
    pub(super) async fn recover_creates(
        self: &Arc<Self>,
        intents: Vec<mj_controller::database::SessionCreateIntent>,
    ) -> Result<BTreeSet<String>> {
        let mut owned = BTreeSet::new();
        for intent in intents {
            let creating = self
                .owner()
                .controller()
                .state
                .sessions
                .get(&intent.session_id)
                .is_some_and(|record| !mj_controller::database::create_intent_is_stale(record));
            if !creating {
                mj_controller::database::discard_session_create_intent(
                    &intent.session_id,
                    &intent.operation_id,
                )?;
                tracing::info!(session_id = %intent.session_id, "discarded a create intent whose session is no longer creating");
                continue;
            }
            tracing::info!(session_id = %intent.session_id, "resuming a session creation interrupted by a daemon restart");
            let inputs = match mj_controller::database::load_subagent(&intent.session_id)? {
                Some(relation) => {
                    self.session_manager
                        .restore_starting(intent.session_id.clone())
                        .await?;
                    CreateInputs {
                        child: Some(relation),
                        publication: None,
                        remember_launch_choices: intent.remember_launch_choices,
                    }
                }
                None => CreateInputs {
                    child: None,
                    publication: None,
                    remember_launch_choices: intent.remember_launch_choices,
                },
            };
            self.start_create_lifecycle(
                intent.session_id.clone(),
                LifecycleControl::default(),
                inputs,
                Some(intent.operation_id),
            )
            .await?;
            owned.insert(intent.session_id);
        }
        Ok(owned)
    }

    /// How a wait for a session's creation ends, answered from durable state
    /// alone (the create intent row and the session record), so every daemon
    /// gives the same answer and a client that loses its daemon only asks
    /// again. While the intent row exists the create is unfinished and the
    /// wait sleeps until the daemon publishes a change to session records.
    pub(crate) async fn wait_create_outcome(&self, session_id: &str) -> CreateWait {
        // Subscribed before the first read, so a change between the read and
        // the sleep still wakes it.
        let mut changes = self.revisions.subscribe();
        loop {
            let id = session_id.to_owned();
            match blocking(move || mj_controller::database::session_create_end(&id)).await {
                Ok(mj_controller::database::CreateEnd::Finished) => return CreateWait::Finished,
                Ok(mj_controller::database::CreateEnd::Failed(reason)) => {
                    return CreateWait::Failed(anyhow::anyhow!(reason));
                }
                Ok(mj_controller::database::CreateEnd::Unfinished) => {}
                Err(error) => return CreateWait::Failed(error),
            }
            if changes.changed().await.is_err() {
                return CreateWait::Failed(anyhow::anyhow!("the daemon is shutting down"));
            }
        }
    }
}

pub(super) struct CreateInputs {
    pub(super) publication: Option<Publication>,
    pub(super) remember_launch_choices: bool,
    pub(super) child: Option<mj_core::subagent::SubagentRecord>,
}

/// Tells a create that its phone was told the session exists.
pub(super) type Publication = tokio::sync::oneshot::Receiver<std::result::Result<(), String>>;

/// How a wait for a session's creation ended.
#[derive(Debug)]
pub(crate) enum CreateWait {
    Finished,
    Failed(anyhow::Error),
}

fn validate_minimum_container_allocation(
    allocation: &mj_core::state::SessionResourceAllocation,
) -> Result<()> {
    allocation
        .validate_minimum_useful_container()
        .map_err(|error| {
            anyhow::Error::new(
                mj_core::refusal::Refusal::unusable(error.to_string())
                    .with_code("container-size-too-small"),
            )
        })
}

#[cfg(test)]
mod capacity_admission_tests {
    use super::*;

    #[test]
    fn undersized_container_is_a_user_facing_refusal() {
        let allocation = mj_core::state::SessionResourceAllocation::Container {
            cpus: 1,
            memory_bytes: 2 << 30,
        };

        let error = validate_minimum_container_allocation(&allocation).unwrap_err();
        let refusal = mj_core::refusal::Refusal::of(&error).expect("user-facing refusal");

        assert_eq!(refusal.code(), Some("container-size-too-small"));
        assert!(refusal.message().contains("at least 2 CPUs"));
    }
}

#[cfg(test)]
mod delegation_replay_tests {
    use super::{CreateDelegationDefault, CreateWait, LifecycleControl};
    use mj_controller::controller::test_support::{IsolatedTest, test_name};

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    async fn create_resolves_omitted_delegation_before_persisting_session_and_preferences() {
        use mj_core::config::{
            Config, HarnessKind, HarnessProfile, SubagentChoice, TargetTemplate,
        };
        use mj_core::subagent::SubagentPolicy;

        const CHILD: &str = "MJ_TEST_CREATE_DEFAULT_DELEGATION";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "create_resolves_omitted_delegation_before_persisting_session_and_preferences",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "create-default-delegation")
            .isolated_store(root.path())
            .run();
            return;
        }

        let _writer = mj_controller::database::install_isolated_test_writer();
        let project = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        for (name, harness) in [
            ("codex", HarnessKind::Codex),
            ("dsh", HarnessKind::Dsh),
            ("kimi", HarnessKind::Kimi),
        ] {
            config.profiles.insert(
                name.into(),
                HarnessProfile::new(harness, mj_core::config::data_dir().join(name)),
            );
        }
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        config.save().unwrap();
        let workspace = mj_controller::database::create_workspace("create delegation").unwrap();
        let runtime = crate::daemon::tests::test_runtime_state_loading_the_store();
        let catalog =
            mj_controller::profile_catalog::ProfileCatalog::with_probe(std::sync::Arc::new(|_| {
                Box::pin(async {
                    Ok(mj_core::worker_launch::ProfileConfig {
                        model: Some("available-model".into()),
                        models: vec![mj_core::acp::SessionConfigChoice {
                            value: "available-model".into(),
                            name: "Available Model".into(),
                            description: None,
                        }],
                        efforts: Vec::new(),
                        observed_at: chrono::Utc::now().timestamp(),
                    })
                })
            }));
        runtime
            .profile_catalog
            .set(catalog)
            .unwrap_or_else(|_| panic!("install test catalog"));
        let request = |profile_id: &str, subagents, remember_launch_choices| {
            mj_client::daemon::CreateSessionRequest {
                oracle: Some(false),
                oracle_choice: None,
                remember_launch_choices,
                remember_container_size: false,
                create_managed_worktree: Some(false),
                at: None,
                branch: None,
                base: None,
                no_later_history: false,
                subagents,
                review: None,
                no_project_memory: false,
                no_github_events: false,
                no_github_credentials: false,
                notifications_ignored: false,
                initial_prompt: None,
                workspace_id: workspace.id.clone(),
                profile_id: profile_id.into(),
                provider: None,
                bundle_id: mj_core::config::raw_project_context_id(
                    &project.path().to_string_lossy(),
                ),
                project_directory: Some(project.path().to_owned()),
                target_template_id: "localhost".into(),
                additional_mounts: Vec::new(),
                resource_allocation: None,
                title: "create delegation".into(),
                session_title_override: None,
            }
        };
        let mjolnir = SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        };
        let cases = [
            (
                "codex",
                None,
                None,
                SubagentPolicy::Native,
                Some(("native", "[subagents.straightforward]")),
                None,
            ),
            (
                "dsh",
                None,
                None,
                SubagentPolicy::None,
                Some(("none", "[subagents.straightforward]")),
                None,
            ),
            (
                "kimi",
                Some("available-model"),
                None,
                SubagentPolicy::Native,
                Some(("native", "Kimi")),
                None,
            ),
            (
                "codex",
                Some("available-model"),
                None,
                mjolnir.clone(),
                None,
                Some(mjolnir.clone()),
            ),
            (
                "codex",
                Some("unavailable-model"),
                None,
                SubagentPolicy::Native,
                Some(("native", "unavailable-model")),
                Some(mjolnir),
            ),
            (
                "codex",
                Some("available-model"),
                Some(SubagentPolicy::None),
                SubagentPolicy::None,
                None,
                Some(SubagentPolicy::None),
            ),
        ];
        for (profile, model, supplied, effective, fallback, remembered) in cases {
            config.subagents.straightforward = model
                .into_iter()
                .map(|model| SubagentChoice {
                    model: model.into(),
                    effort: None,
                })
                .collect();
            config.save().unwrap();
            let (publish, publication) = tokio::sync::oneshot::channel();
            let control = LifecycleControl::default();
            let (registered, _, notice) = runtime
                .start_create_session_controlled(
                    request(profile, supplied, true),
                    control.clone(),
                    publication,
                )
                .await
                .unwrap();
            let state = mj_controller::database::load_state().unwrap();
            assert_eq!(registered.session.subagents.as_ref(), Some(&effective));
            assert_eq!(
                state.sessions[&registered.session.id].subagents.as_ref(),
                Some(&effective)
            );
            assert_eq!(
                state.last_subagent_policies.get(profile),
                remembered.as_ref()
            );
            assert_eq!(state.last_oracle_choices.get(profile), Some(&false));
            if let Some((mode, reason)) = fallback {
                let notice = notice.expect("an implicit fallback reports its mode and reason");
                assert!(notice.contains(&format!("mode {mode}")), "{notice}");
                assert!(notice.contains(reason), "{notice}");
            } else {
                assert_eq!(notice, None);
            }
            control.request_cancel("create delegation test cancelled");
            drop(publish);
            assert!(matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    runtime.wait_create_outcome(&registered.session.id)
                )
                .await
                .expect("cancelled creation must settle its durable intent"),
                CreateWait::Failed(_)
            ));
        }

        // The Wiki restore path asks for the historical harness policy.
        // Keep the same usable Straightforward choice in place, so inheriting
        // fresh Create defaults here would be visible in the persisted record.
        let (publish, publication) = tokio::sync::oneshot::channel();
        let control = LifecycleControl::default();
        let (registered, _, notice) = runtime
            .start_create_session_inner(
                request("codex", None, false),
                control.clone(),
                Some(publication),
                CreateDelegationDefault::Harness,
            )
            .await
            .unwrap();
        assert_eq!(notice, None);
        assert_eq!(
            mj_controller::database::load_state().unwrap().sessions[&registered.session.id]
                .subagents,
            Some(SubagentPolicy::Native)
        );
        control.request_cancel("archive delegation test cancelled");
        drop(publish);
        assert!(matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                runtime.wait_create_outcome(&registered.session.id)
            )
            .await
            .expect("cancelled creation must settle its durable intent"),
            CreateWait::Failed(_)
        ));
    }

    // Hard-won: #1562: a cancelled create stayed unfinished when cached eligibility omitted its durable record.
    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    async fn only_interactive_create_remembers_default_recent_and_subagents() {
        use mj_core::go::{GoPreferences, RecentLaunch};
        use mj_core::subagent::SubagentPolicy;
        const CHILD: &str = "MJ_TEST_INTERACTIVE_LAUNCH_CHOICES";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "only_interactive_create_remembers_default_recent_and_subagents",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "interactive-launch-choices")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();
        let project = tempfile::tempdir().unwrap();
        let mut config = mj_core::config::Config::default();
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                legacy_subagents: None,
                ..mj_core::config::HarnessProfile::new(
                    mj_core::config::HarnessKind::Codex,
                    mj_core::config::data_dir().join("unused-profile-home"),
                )
            },
        );
        config.targets.insert(
            "localhost".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        config.save().unwrap();
        let workspace = mj_controller::database::create_workspace("launch choices").unwrap();
        let runtime = crate::daemon::tests::test_runtime_state_loading_the_store();
        runtime.profile_catalog.get_or_init(|| {
            mj_controller::profile_catalog::ProfileCatalog::new(
                tokio_util::sync::CancellationToken::new(),
            )
        });
        let path = GoPreferences::path();
        let mut stale_candidate = None;
        for remember in [false, true, false] {
            let before = std::fs::read(&path).ok();
            let recent_before = runtime
                .runtime_publication()
                .unwrap()
                .metadata
                .recent_launch;
            let policy_before = mj_controller::database::load_state()
                .unwrap()
                .last_subagent_policy_for("codex");
            let request = mj_client::daemon::CreateSessionRequest {
                oracle: None,
                oracle_choice: None,
                remember_launch_choices: remember,
                remember_container_size: false,
                create_managed_worktree: Some(false),
                at: None,
                branch: None,
                base: None,
                no_later_history: false,
                subagents: Some(SubagentPolicy::Native),
                review: None,
                no_project_memory: false,
                no_github_events: false,
                no_github_credentials: false,
                notifications_ignored: false,
                initial_prompt: None,
                workspace_id: workspace.id.clone(),
                profile_id: "codex".into(),
                provider: None,
                bundle_id: mj_core::config::raw_project_context_id(
                    &project.path().to_string_lossy(),
                ),
                project_directory: Some(project.path().to_owned()),
                target_template_id: "localhost".into(),
                additional_mounts: Vec::new(),
                resource_allocation: None,
                title: "launch choices".into(),
                session_title_override: None,
            };
            let (publish, publication) = tokio::sync::oneshot::channel();
            let control = super::LifecycleControl::default();
            let (registered, _, _) = runtime
                .start_create_session_controlled(request, control.clone(), publication)
                .await
                .unwrap();
            stale_candidate = Some(registered.session.clone());
            if remember {
                let saved = GoPreferences::load(&path).unwrap();
                let default = saved.default.unwrap();
                assert_eq!(
                    (default.profile_id.as_str(), default.target_id.as_str()),
                    ("codex", "localhost")
                );
                assert_eq!(
                    saved.recent,
                    Some(RecentLaunch {
                        profile_id: "codex".into(),
                        target_id: "localhost".into(),
                        bundle_id: registered.session.bundle_id.clone(),
                        project_directory: registered.session.project_directory.clone(),
                    })
                );
                assert_eq!(
                    mj_controller::database::load_state()
                        .unwrap()
                        .last_subagent_policy_for("codex"),
                    SubagentPolicy::Native
                );
                assert_eq!(
                    runtime
                        .runtime_publication()
                        .unwrap()
                        .metadata
                        .recent_launch,
                    saved.recent
                );
            } else {
                assert_eq!(std::fs::read(&path).ok(), before);
                assert_eq!(
                    runtime
                        .runtime_publication()
                        .unwrap()
                        .metadata
                        .recent_launch,
                    recent_before
                );
                assert_eq!(
                    mj_controller::database::load_state()
                        .unwrap()
                        .last_subagent_policy_for("codex"),
                    policy_before
                );
            }
            control.request_cancel("create test cancelled");
            drop(publish);
            assert!(matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    runtime.wait_create_outcome(&registered.session.id)
                )
                .await
                .expect("cancelled creation must settle its durable intent"),
                super::CreateWait::Failed(_)
            ));
        }
        // Failure settlement reads the store even when the runtime's
        // publication has not observed a registered create yet.
        let mut pending = stale_candidate.unwrap();
        pending.id = "77777777777777777777777777777777".into();
        mj_controller::database::save_new_session(&pending, None, false, false).unwrap();
        let stale = crate::daemon::tests::test_runtime_state();
        assert!(
            !stale
                .owner()
                .controller()
                .state
                .sessions
                .contains_key(&pending.id)
        );
        stale
            .fail_unfinished_provisioning(
                &pending.id,
                "cancelled before admission with a stale publication",
            )
            .await;
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stale.wait_create_outcome(&pending.id),
        )
        .await
        .expect("stale publication must not retain a create intent")
        {
            super::CreateWait::Failed(error) => {
                assert!(error.to_string().contains("stale publication"), "{error:#}")
            }
            super::CreateWait::Finished => panic!("cancelled provisioning was not successful"),
        }
    }

    #[tokio::test]
    async fn unconfigured_mjolnir_creation_is_refused_before_registration_or_provisioning() {
        const CHILD: &str = "MJ_TEST_MULTI_MODEL_CREATION_REFUSAL";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "unconfigured_mjolnir_creation_is_refused_before_registration_or_provisioning",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "multi-model-creation-refusal")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();
        let mut config = mj_core::config::Config::default();
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                legacy_subagents: None,
                ..mj_core::config::HarnessProfile::new(
                    mj_core::config::HarnessKind::Codex,
                    mj_core::config::data_dir().join("missing-profile-home"),
                )
            },
        );
        config.save().unwrap();
        let workspace = mj_controller::database::create_workspace("legacy delegation").unwrap();
        let mut legacy = crate::daemon::tests::runtime_test_session(
            "legacy-parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        legacy.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        });
        mj_controller::database::save_session(&legacy).unwrap();
        let runtime = crate::daemon::tests::test_runtime_state();
        runtime.profile_catalog.get_or_init(|| {
            mj_controller::profile_catalog::ProfileCatalog::new(
                tokio_util::sync::CancellationToken::new(),
            )
        });
        let request = mj_client::daemon::CreateSessionRequest {
            oracle: None,
            oracle_choice: None,
            remember_launch_choices: false,
            remember_container_size: false,
            create_managed_worktree: None,
            at: None,
            branch: None,
            base: None,
            no_later_history: false,
            subagents: Some(mj_core::subagent::SubagentPolicy::Mjolnir {
                orchestrators: false,
                straightforward: None,
                subtle: None,
            }),
            review: None,
            no_project_memory: false,
            no_github_events: false,
            no_github_credentials: false,
            notifications_ignored: false,
            initial_prompt: None,
            workspace_id: workspace.id,
            profile_id: "codex".into(),
            provider: None,
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
        assert!(refusal.message().contains("[subagents.straightforward]"));
        assert!(refusal.message().contains("--subagent-straightforward"));
        assert_eq!(
            refusal.code(),
            Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
        );
        assert!(runtime.active_lifecycles().is_empty());
        let state = mj_controller::database::load_state().unwrap();
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
        let _writer = mj_controller::database::install_isolated_test_writer();
        let workspace = mj_controller::database::create_workspace("spawn replay").unwrap();
        let parent = crate::daemon::tests::runtime_test_session(
            "parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        mj_controller::database::save_session(&parent).unwrap();
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
            mj_controller::database::save_subagent_session(&child, &relation).unwrap();
            let runtime = crate::daemon::tests::test_runtime_state();
            runtime.profile_catalog.get_or_init(|| {
                mj_controller::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                )
            });
            let replayed = runtime
                .start_subagent_session(mj_controller::controller::RegisterSubagentRequest {
                    no_github_credentials: false,
                    kind: mj_core::subagent::SubagentKind::Worker,
                    orchestrator: false,
                    reviewer_launch: None,
                    parent_session_id: "parent".into(),
                    task_name: "must reuse original".into(),
                    profile_id: "missing-profile".into(),
                    model: None,
                    effort: None,
                    fast_mode: false,
                    working_directory: Default::default(),
                    initial_prompt: "must not resend".into(),
                    request_key: relation.request_key.clone(),
                    report_root: None,
                    category: "straightforward".into(),
                })
                .await
                .unwrap();
            assert_eq!(replayed, relation);
            assert!(runtime.active_lifecycles().is_empty());
            assert_eq!(
                mj_controller::database::load_session_record(&child_id)
                    .unwrap()
                    .unwrap()
                    .state,
                status
            );
        }
    }
}

use super::views::{CreateProgress, StalledCreate};
use super::*;

/// How long creating a parent's report root on its target may take.
const REPORT_ROOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a reviewer child's Create may go with no provisioning stage
/// starting or finishing before its start is failed. It bounds idleness, not
/// the whole Create, so a slow Create that keeps progressing is never failed.
///
/// The longest single stage measured on 2026-11-06 under a load average of
/// about 200 on morannon was about 190 s (a `mkdir`); ten minutes is about
/// three times that worst case. The bound exists because the Create's
/// executor and `CommandSpec` have no per-command timeout, so a remote command
/// that hangs on a live connection would otherwise wait for ever (#1438).
pub(super) const CREATE_STALL_BOUND: std::time::Duration = std::time::Duration::from_secs(10 * 60);

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
        let bound = crate::controller::NATIVE_SESSION_STARTUP_TIMEOUT;
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
        let (registered, _, warning) = self
            .start_create_session_inner(request, LifecycleControl::default(), None)
            .await?;
        if let Some(warning) = warning {
            self.push_notice(&registered.session.id, warning);
        }
        Ok(registered)
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
        request: crate::controller::RegisterSubagentRequest,
    ) -> Result<mj_core::subagent::SubagentRecord> {
        let _upgrade_work = crate::upgrade::activity("subagent admission")?;
        let (relation, needs_provisioning, registered_here) = blocking(move || {
            let mut controller = Controller::load()?;
            if let Some(existing) = crate::database::lookup_subagent_request(
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
                    crate::controller::recover_registered_subagent_startup(
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
                .register_subagent(request)
                .map(|relation| (relation, true, true))
        })
        .await?;
        if let Err(error) = self
            .start_registered_subagent(&relation, needs_provisioning)
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
        Ok(relation)
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
                blocking(move || crate::database::next_startup_delivery(&id)).await?
            {
                self.start_persisted_startup_delivery(delivery, &CancellationToken::new());
            }
        }
        if !needs_provisioning {
            return Ok(());
        }
        let session_id = relation.child_session_id.clone();
        let parent_session_id = relation.parent_session_id.clone();
        let control = LifecycleControl::default();
        self.start_or_join_lifecycle_controlled(
            session_id.clone(),
            LifecycleKind::Create,
            None,
            Some(relation.request_key.clone()),
            Some(control.clone()),
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
                    .provision_subagent_session_controlled(&session_id, &executor, || {
                        control.stopped_for_shutdown()
                    })
                    .await
                {
                    drop(controller);
                    state.reload_controller().await?;
                    if let Err(prompt_error) = state
                        .reconcile_parent_reports(&parent_session_id)
                        .await
                    {
                        tracing::warn!(
                            child_session_id = %session_id,
                            parent_session_id,
                            error = format!("{prompt_error:#}"),
                            "could not enqueue the parent's finished child reports after provisioning failure"
                        );
                    }
                    return Err(error);
                }
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        Ok(())
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
        let relation = blocking(move || crate::database::load_subagent(&id))
            .await?
            .context("reviewer child is missing")?;
        let launch = relation
            .reviewer_launch
            .as_ref()
            .context("reviewer launch configuration is missing")?
            .clone();
        self.start_subagent_session(crate::controller::RegisterSubagentRequest {
            no_github_credentials: false,
            kind: relation.kind,
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
        })
        .await?;
        // No total deadline here: one counted queue time and cancelled an
        // accepted Create mid-provision, which left the child without a worker
        // root (#1438). While the Create runs, each wait ends with its
        // outcome: the child is ready, its record says the Create failed or
        // it was closed, or the Create stalled for `CREATE_STALL_BOUND` and
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
    /// Create that has stalled (see [`CREATE_STALL_BOUND`]), and a readiness
    /// wait that has gone on too long with no Create running.
    fn reviewer_child_can_still_start(
        &self,
        child: &str,
        waiting: &mut PostCreateWait,
    ) -> Result<()> {
        self.check_create_progress(child, CREATE_STALL_BOUND, waiting)?;
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
    /// provisioning stage starting or finishing, and cancel that Create in
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
        match self.cancel_stalled_create(child, bound) {
            CreateProgress::NotRunning => waiting.check(child),
            CreateProgress::Running => {
                waiting.create_running();
                Ok(())
            }
            CreateProgress::Stalled {
                idle,
                stages,
                phase,
            } => {
                let stage = if stages.is_empty() {
                    "between stages".to_owned()
                } else {
                    stages.join(" + ")
                };
                let outcome = match phase {
                    StalledCreate::Cancelled => "its Create was cancelled",
                    StalledCreate::AlreadyCancelling => "its Create was already being cancelled",
                    StalledCreate::Committed => "its Create had committed and was left running",
                };
                tracing::warn!(session_id = child, idle_secs = idle.as_secs(), %stage, ?phase, "reviewer child Create stalled");
                anyhow::bail!(
                    "the reviewer child {child} stalled while provisioning: no stage started or finished for {} s (stalled in: {stage}); {outcome}",
                    idle.as_secs()
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
        self.start_create_session_inner(request, control, Some(publication))
            .await
    }

    pub(super) async fn start_create_session_inner(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: LifecycleControl,
        publication: Option<tokio::sync::oneshot::Receiver<std::result::Result<(), String>>>,
    ) -> Result<(RegisteredSession, Option<&'static str>, Option<String>)> {
        let mut admission_timing =
            crate::launch_timing::LaunchPhaseTimer::new("", "admit and register session");
        tracing::info!(profile_id = %request.profile_id, target_id = %request.target_template_id,
            "session launch admission started");
        let mut request = request;
        let parent = request.profile_id.clone();
        let supplied = request.subagents.clone();
        let (config, policy) = blocking(move || {
            let controller = Controller::load()?;
            let profile = controller
                .config
                .enabled_profile(&parent)
                .context("parent profile unavailable")?;
            let policy = supplied
                .unwrap_or_else(|| mj_core::subagent::SubagentPolicy::default_for(profile.kind));
            Ok((controller.config, policy))
        })
        .await?;
        crate::controller::profile_config::validate_session_subagent_policy(
            &config,
            &request.profile_id,
            &policy,
        )
        .await?;
        if let Some(allocation) = request.resource_allocation.as_ref() {
            validate_minimum_container_allocation(allocation)?;
        }
        if !request.bundle_id.is_empty() {
            crate::controller::config_only_controller(config.clone())
                .validate_github_bundle_installations(&request.bundle_id)
                .await
                .map_err(crate::controller::GithubBundleSelectionError::into_anyhow)?;
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
        let _upgrade_work = crate::upgrade::activity("session admission")?;
        request.subagents = Some(policy);
        let path_cancelled = control.cancelled.clone();
        let remember_launch_choices = request.remember_launch_choices;
        let preferences_owner = self.clone();
        let (registered, review_notice, oracle_notice) = blocking(move || {
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
            let registration = controller.register_session_reporting_review(
                &request.profile_id,
                &request.bundle_id,
                &request.target_template_id,
                request.title,
                SessionLaunchOptions {
                    remember_launch_choices: request.remember_launch_choices,
                    remember_container_size: request.remember_container_size,
                    create_managed_worktree: request.create_managed_worktree,
                    at: request.at,
                    branch: request.branch,
                    base: request.base,
                    no_later_history: request.no_later_history,
                    subagents: request.subagents,
                    oracle: request.oracle,
                    oracle_choice: request.oracle_choice,
                    review: request.review,
                    no_project_memory: request.no_project_memory,
                    no_github_events: request.no_github_events,
                    no_github_credentials: request.no_github_credentials,
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
        let capacity_warning = [oracle_notice, _container_admission.warning.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        let capacity_warning = (!capacity_warning.is_empty()).then_some(capacity_warning);
        drop(_container_admission);
        let session_id = registered.session.id.clone();
        let scheduled_at = std::time::Instant::now();
        self.start_or_join_lifecycle_controlled(
            session_id,
            LifecycleKind::Create,
            None,
            None,
            Some(control.clone()),
            move |state, session_id, cancelled| async move {
                tracing::info!(%session_id, elapsed_ms = scheduled_at.elapsed().as_millis(),
                    "session launch lifecycle scheduled");
                let _total =
                    crate::launch_timing::LaunchPhaseTimer::new(&session_id, "create lifecycle");
                let loading =
                    crate::launch_timing::LaunchPhaseTimer::new(&session_id, "load controller");
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for daemon create task")??;
                drop(loading);
                let publication_timing =
                    crate::launch_timing::LaunchPhaseTimer::new(&session_id, "await publication");
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
                    state,
                    session_id.clone(),
                );
                let provision = controller
                    .provision_session_controlled_with_commit(
                        &session_id,
                        &executor,
                        || {
                            ensure!(
                                control.grant_commit(),
                                "session creation cancelled before commit"
                            );
                            Ok(())
                        },
                        remember_launch_choices,
                    )
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
        let publishing = crate::launch_timing::LaunchPhaseTimer::new(
            &registered.session.id,
            "publish registered session view",
        );
        self.reload_controller().await?;
        drop(publishing);
        Ok((registered, review_notice, capacity_warning))
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
    use crate::controller::test_support::{IsolatedTest, test_name};

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
        let _writer = crate::database::install_isolated_test_writer();
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
        let workspace = crate::database::create_workspace("launch choices").unwrap();
        let runtime = crate::daemon::tests::test_runtime_state();
        let path = GoPreferences::path();
        for remember in [false, true, false] {
            let before = std::fs::read(&path).ok();
            let recent_before = runtime
                .runtime_publication()
                .unwrap()
                .metadata
                .recent_launch;
            let policy_before = crate::database::load_state()
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
                initial_prompt: None,
                workspace_id: workspace.id.clone(),
                profile_id: "codex".into(),
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
                    crate::database::load_state()
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
                    crate::database::load_state()
                        .unwrap()
                        .last_subagent_policy_for("codex"),
                    policy_before
                );
            }
            control.request_cancel("create test cancelled");
            drop(publish);
            assert!(
                runtime
                    .wait_create_session(&registered.session.id)
                    .await
                    .is_err()
            );
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
        let _writer = crate::database::install_isolated_test_writer();
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
        let workspace = crate::database::create_workspace("legacy delegation").unwrap();
        let mut legacy = crate::daemon::tests::runtime_test_session(
            "legacy-parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        legacy.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        });
        crate::database::save_session(&legacy).unwrap();
        let runtime = crate::daemon::tests::test_runtime_state();
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
                straightforward: None,
                subtle: None,
            }),
            review: None,
            no_project_memory: false,
            no_github_events: false,
            no_github_credentials: false,
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
        assert!(refusal.message().contains("[subagents.straightforward]"));
        assert!(refusal.message().contains("--subagent-straightforward"));
        assert_eq!(
            refusal.code(),
            Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
        );
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
                    no_github_credentials: false,
                    kind: mj_core::subagent::SubagentKind::Worker,
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

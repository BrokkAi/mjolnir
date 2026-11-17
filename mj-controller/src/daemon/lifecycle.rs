use super::*;
use crate::recovery_gate::SessionAdmission;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LifecycleOrigin {
    Person,
    Automatic,
}

pub(super) struct LifecycleStart {
    pub(super) origin: LifecycleOrigin,
    pub(super) resume_workspace_id: Option<String>,
    pub(super) request_key: Option<String>,
    /// The control the caller keeps to cancel or commit the operation; a new
    /// one when the caller does not need it.
    pub(super) control: Option<LifecycleControl>,
    pub(super) phase: LifecyclePhase,
    pub(super) move_operation_id: Option<String>,
    /// Admit this operation only while no handoff drains, under this label.
    /// Work nobody asked for, which the next daemon starts again, uses it;
    /// anything a person asked for holds the handoff instead.
    pub(super) deferrable_admission: Option<&'static str>,
    /// The requester was already answered "accepted" and holds handoff
    /// admission for this operation, as an accepted destroy does (#1191). A
    /// draining daemon answers a queued request "not started" so its client
    /// retries; it must not say that to this one. It queues and preempts
    /// through the drain, and the stop or handoff waits for it under the
    /// bound of the admission it holds.
    pub(super) holds_through_drain: bool,
}

impl LifecycleStart {
    pub(super) fn automatic(mut self) -> Self {
        self.origin = LifecycleOrigin::Automatic;
        self
    }

    pub(super) fn holding_through_drain(mut self) -> Self {
        self.holds_through_drain = true;
        self
    }

    pub(super) fn request(
        key: String,
        workspace: Option<String>,
        control: Option<LifecycleControl>,
    ) -> Self {
        Self {
            origin: LifecycleOrigin::Person,
            resume_workspace_id: workspace,
            request_key: Some(key),
            control,
            phase: LifecyclePhase::Executing,
            move_operation_id: None,
            deferrable_admission: None,
            holds_through_drain: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdmissionMode {
    /// Identical selections join running work; a completed attempt may retry.
    FreshRequest,
    /// Durable caller keys also identify a previously completed request.
    Replay,
}

struct LifecycleAdmissionOptions {
    queued: bool,
    mode: AdmissionMode,
    acceptance: Option<LifecycleAcceptance>,
}

impl RuntimeState {
    pub(super) async fn start_or_join_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_for_workspace(session_id, kind, None, work)
            .await
    }

    pub(super) async fn start_automatic_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let mut start = LifecycleStart::request(String::new(), None, None);
        start.request_key = None;
        start.origin = LifecycleOrigin::Automatic;
        self.admit_lifecycle(session_id, kind, start, work).await
    }

    pub(super) async fn start_or_join_lifecycle_for_workspace<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_with_key(session_id, kind, resume_workspace_id, None, work)
            .await
    }

    pub(super) async fn start_or_join_lifecycle_with_key<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        request_key: Option<String>,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_controlled(
            session_id,
            kind,
            resume_workspace_id,
            request_key,
            None,
            work,
        )
        .await
    }

    pub(super) async fn start_or_join_lifecycle_controlled<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        request_key: Option<String>,
        control: Option<LifecycleControl>,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.admit_lifecycle(
            session_id,
            kind,
            LifecycleStart {
                origin: if matches!(
                    kind,
                    LifecycleKind::Cleanup
                        | LifecycleKind::StartupCleanup
                        | LifecycleKind::Park
                        | LifecycleKind::Unpark
                        | LifecycleKind::StopSubagent
                ) {
                    LifecycleOrigin::Automatic
                } else {
                    LifecycleOrigin::Person
                },
                resume_workspace_id,
                request_key,
                control,
                phase: LifecyclePhase::Executing,
                move_operation_id: None,
                deferrable_admission: None,
                holds_through_drain: false,
            },
            work,
        )
        .await
    }

    pub(super) async fn admit_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        start: LifecycleStart,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.admit_lifecycle_mode(session_id, kind, start, None, AdmissionMode::Replay, work)
            .await
            .map(|started| started.result)
    }

    pub(super) async fn admit_prepared_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        start: LifecycleStart,
        acceptance: Option<LifecycleAcceptance>,
        work: F,
    ) -> Result<StartedLifecycle>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.admit_lifecycle_mode(
            session_id,
            kind,
            start,
            acceptance,
            AdmissionMode::FreshRequest,
            work,
        )
        .await
    }

    // A typed future breaks the supervision cycle: a completed lifecycle may
    // start its deferred cleanup through this same admission entry.
    fn admit_lifecycle_mode<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        start: LifecycleStart,
        acceptance: Option<LifecycleAcceptance>,
        mode: AdmissionMode,
        work: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<StartedLifecycle>> + Send + '_>>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        Box::pin(async move {
            let mut revisions = self.revisions.subscribe();
            let ticket = Arc::new(());
            let _queued = LifecycleIntent {
                state: self.clone(),
                session_id: session_id.clone(),
                ticket: ticket.clone(),
            };
            let preempt = start.origin == LifecycleOrigin::Person
                && matches!(
                    kind,
                    LifecycleKind::ForceDestroy | LifecycleKind::StopSubagent
                );
            {
                let mut owner = self.owner();
                owner.ensure_available()?;
                if start.origin == LifecycleOrigin::Automatic {
                    ensure!(
                        !owner.lifecycle_intents.contains_key(&session_id),
                        "a person's lifecycle request is waiting for session {session_id}"
                    );
                }
                let queue = owner
                    .lifecycle_intents
                    .entry(session_id.clone())
                    .or_default();
                if preempt {
                    // Destroy is the escape hatch, ahead of ordinary FIFO intents.
                    queue.push_front(ticket.clone());
                } else {
                    queue.push_back(ticket.clone());
                }
            }
            let yields_to_drain = !start.holds_through_drain;
            let mut previous = String::new();
            let mut waited = false;
            loop {
                if let Some(control) = &start.control {
                    ensure!(
                        !control.cancelled.load(Ordering::Acquire),
                        "operation cancelled while waiting for lifecycle admission"
                    );
                }
                let waiting = {
                    let mut owner = self.owner();
                    owner.ensure_available()?;
                    let first = owner
                        .lifecycle_intents
                        .get(&session_id)
                        .and_then(|queue| queue.front())
                        .is_some_and(|front| Arc::ptr_eq(front, &ticket));
                    let child_close = kind == LifecycleKind::Suspend
                        && owner.controller().state.subagents.contains_key(&session_id);
                    if let Some(active) = owner
                        .lifecycle
                        .get_mut(&session_id)
                        .filter(|active| active.is_running())
                    {
                        let identical = active
                            .check_request(
                                &session_id,
                                kind,
                                start.request_key.as_deref(),
                                start.resume_workspace_id.as_deref(),
                                child_close,
                            )
                            .is_ok();
                        if identical {
                            return self.admit_lifecycle_locked(
                                &mut owner,
                                session_id,
                                kind,
                                start,
                                LifecycleAdmissionOptions {
                                    queued: waited && yields_to_drain,
                                    mode,
                                    acceptance,
                                },
                                work,
                            );
                        } else {
                            ensure!(
                                start.origin == LifecycleOrigin::Person,
                                "another lifecycle operation is already running for session {session_id}"
                            );
                            if first && preempt {
                                (
                                    None,
                                    None,
                                    Some(super::resume::LifecyclePreemption::begin(
                                        active,
                                        &session_id,
                                    )),
                                )
                            } else {
                                if active.origin == LifecycleOrigin::Automatic
                                    && !active.control.is_committed()
                                {
                                    active.request_cancel(
                                        &session_id,
                                        "person's lifecycle request superseded automatic work",
                                    );
                                }
                                (
                                    Some(active.result.clone()),
                                    Some((active.operation_id.clone(), active.kind)),
                                    None,
                                )
                            }
                        }
                    } else if first {
                        return self.admit_lifecycle_locked(
                            &mut owner,
                            session_id,
                            kind,
                            start,
                            LifecycleAdmissionOptions {
                                queued: waited && yields_to_drain,
                                mode,
                                acceptance,
                            },
                            work,
                        );
                    } else {
                        (None, None, None)
                    }
                };
                let (result, holder, preemption) = waiting;
                if let Some(preemption) = preemption {
                    waited = true;
                    tokio::select! {
                        biased;
                        () = crate::upgrade::gate().wait_until_draining(), if yields_to_drain => {
                            return Err(Refusal::precondition("daemon upgrade handoff is waiting; the queued lifecycle request was not started")
                                .with_code(crate::upgrade::HANDOFF_REFUSAL_CODE).into());
                        }
                        result = preemption.wait(self, &session_id) => { result?; }
                    }
                    continue;
                }
                waited = true;
                if let Some((operation_id, kind)) = holder {
                    use crate::upgrade::Describe;
                    let detail = LifecycleHold {
                        state: Arc::downgrade(self),
                        session_id: session_id.clone(),
                        operation_id,
                        kind,
                    }
                    .describe();
                    let notice = format!(
                        "Waiting for {}{}",
                        detail.subject,
                        detail
                            .stage
                            .map(|stage| format!(": {stage}"))
                            .unwrap_or_default()
                    );
                    if notice != previous {
                        self.push_notice(&session_id, notice.clone());
                        previous = notice;
                    }
                }
                let cancellation = async {
                    match &start.control {
                        Some(control) => {
                            crate::recovery_gate::wait_until_cancelled(&control.cancelled).await
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    biased;
                    () = crate::upgrade::gate().wait_until_draining(), if yields_to_drain => {
                        return Err(Refusal::precondition("daemon upgrade handoff is waiting; the queued lifecycle request was not started")
                            .with_code(crate::upgrade::HANDOFF_REFUSAL_CODE).into());
                    }
                    _ = async { if let Some(result) = result { let _ = Self::wait_lifecycle_result(result).await; } else { std::future::pending::<()>().await; } } => {}
                    changed = revisions.changed() => { changed?; }
                    () = cancellation => {}
                }
            }
        })
    }

    fn admit_lifecycle_locked<F, Fut>(
        self: &Arc<Self>,
        lifecycle_owner: &mut super::owner::RuntimeStateOwner,
        session_id: String,
        kind: LifecycleKind,
        start: LifecycleStart,
        options: LifecycleAdmissionOptions,
        work: F,
    ) -> Result<StartedLifecycle>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let LifecycleAdmissionOptions {
            queued,
            mode,
            acceptance,
        } = options;
        let LifecycleStart {
            origin,
            resume_workspace_id,
            request_key,
            control,
            mut phase,
            move_operation_id,
            deferrable_admission,
            holds_through_drain: _,
        } = start;
        let mut work = Some(work);
        let mut superseded_restart_intent = None;
        let (result, receipt, acceptance) = {
            lifecycle_owner.ensure_available()?;
            ensure!(
                matches!(
                    kind,
                    LifecycleKind::Move | LifecycleKind::ForceDestroy | LifecycleKind::StopSubagent
                ) || !crate::controller::move_session::move_has_pending_queue(&session_id),
                "Move queue admission is incomplete; retry Move on the same destination before another lifecycle operation"
            );
            if kind == LifecycleKind::Move
                && lifecycle_owner
                    .committed()
                    .and_then(|state| state.moves.get(&session_id))
                    .is_some_and(|operation| {
                        move_operation_id.as_ref() == Some(&operation.operation_id)
                            && operation.queue_admission_started
                            && !operation.queue_admission_finished
                    })
            {
                phase = LifecyclePhase::MovingDestination;
            }

            let durable = durable_session_state(lifecycle_owner.controller(), &session_id);
            // Every close of a child has the same effect. Startup recovery
            // has no caller key, while a replay has its durable MCP key; they
            // must join the same owner under this lock rather than compete.
            let child_close = kind == LifecycleKind::Suspend
                && lifecycle_owner
                    .controller()
                    .state
                    .subagents
                    .contains_key(&session_id);
            let lifecycle = &mut lifecycle_owner.lifecycle;
            // Completion releases ownership under this same lock. Retained
            // results answer identical retries; they cannot block a fresh
            // request just because its original waiter disappeared.
            let completed_other_request = lifecycle.get(&session_id).is_some_and(|active| {
                !active.is_running()
                    && (mode == AdmissionMode::FreshRequest
                        || active.kind != kind
                        || active.request_key != request_key
                        || kind == LifecycleKind::UpgradeContainer)
            });
            if completed_other_request {
                ensure!(
                    !lifecycle
                        .get(&session_id)
                        .is_some_and(|active| active.kind == LifecycleKind::UpgradeContainer
                            && active.restart_intent.is_some()
                            && active.control.is_committed())
                        || matches!(
                            kind,
                            LifecycleKind::UpgradeContainer
                                | LifecycleKind::ForceDestroy
                                | LifecycleKind::DestroyStopped
                        ),
                    "an interrupted container upgrade retains its source and storage; retry that upgrade or explicitly destroy the session"
                );
                superseded_restart_intent = lifecycle.remove(&session_id).and_then(|active| {
                    (kind != LifecycleKind::UpgradeContainer)
                        .then_some(active.restart_intent)
                        .flatten()
                });
            }
            if let Some(active) = lifecycle.get(&session_id) {
                active.check_request(
                    &session_id,
                    kind,
                    request_key.as_deref(),
                    resume_workspace_id.as_deref(),
                    child_close,
                )?;
                (
                    active.result.clone(),
                    LifecycleReceipt {
                        session_id: session_id.clone(),
                        operation_id: active.operation_id.clone(),
                    },
                    active.acceptance.clone(),
                )
            } else {
                // A destroy is waited for by a stop or handoff, up to a bound;
                // other lifecycles hold the handoff until they finish.
                // The operation's own id is also what a failure reports as its
                // reference, so a person holding "the suspension did not finish"
                // can find the daemon-log entry that says why.
                let operation_reference = new_command_id("lifecycle")?;
                // The wait notice asks the lifecycle registry for the stage
                // when it renders; the gate keeps no copy.
                let describer: Arc<dyn crate::upgrade::Describe> = Arc::new(LifecycleHold {
                    state: Arc::downgrade(self),
                    session_id: session_id.clone(),
                    operation_id: operation_reference.clone(),
                    kind,
                });
                // A create is safe to stop and starts again under the next
                // daemon from its durable create intent, so it does not hold
                // the handoff: the handoff stops it. Only its short commit
                // write takes admission, under its own label.
                let resumable = kind == LifecycleKind::Create;
                let admit = || -> Result<crate::upgrade::Work> {
                    Ok(if let Some(label) = deferrable_admission {
                        crate::upgrade::activity_unless_draining_described(label, describer)?
                    } else if kind == LifecycleKind::StartupCleanup {
                        crate::upgrade::activity_unless_draining_described(
                            "failed startup cleanup",
                            describer,
                        )?
                    } else if kind == LifecycleKind::ForceDestroy {
                        if queued {
                            crate::upgrade::destroy_activity_unless_draining(&session_id)?
                        } else {
                            crate::upgrade::destroy_activity(&session_id)?
                        }
                    } else if kind == LifecycleKind::Unpark {
                        crate::upgrade::activity_unless_draining_described(
                            "subagent unpark admission",
                            describer,
                        )?
                    } else {
                        let label = if child_close {
                            "subagent close admission"
                        } else {
                            "session lifecycle"
                        };
                        if queued {
                            crate::upgrade::activity_unless_draining_described(label, describer)?
                        } else {
                            crate::upgrade::activity_described(label, describer)?
                        }
                    })
                };
                let _queued_create_admission = if resumable && queued {
                    Some(crate::upgrade::activity_unless_draining(
                        "queued create admission",
                    )?)
                } else {
                    None
                };
                let admitted = if resumable { None } else { Some(admit()?) };
                let upgrade_work = Arc::new(Mutex::new(admitted));
                let control = control.unwrap_or_default();
                // Decided under the owner lock, before anyone else can see the
                // control, so a cancel can never be accepted first.
                if lifecycle_starts_committed(kind, durable)
                    // Automatic teardown completes an already accepted stop;
                    // there is no rollback once it owns the environment.
                    || (origin == LifecycleOrigin::Automatic && kind.is_teardown())
                    || (kind == LifecycleKind::UpgradeContainer
                        && matches!(phase, LifecyclePhase::RetiringContainer))
                {
                    control.grant_commit();
                }
                let cancelled = control.cancelled.clone();
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                let acceptance =
                    acceptance.unwrap_or_else(|| tokio::sync::watch::channel(Some(Ok(()))).1);
                lifecycle.insert(
                    session_id.clone(),
                    ActiveLifecycle {
                        origin,
                        move_operation_id: move_operation_id.clone(),
                        restart_intent: None,
                        upgrade_work: upgrade_work.clone(),
                        phase,
                        operation_id: operation_reference.clone(),
                        control,
                        kind,
                        started_at_epoch_seconds: epoch_seconds(),
                        active_stages: BTreeMap::new(),
                        last_stage_change: tokio::time::Instant::now(),
                        resume_workspace_id,
                        resume_destination: None,
                        notice: None,
                        request_key,
                        _move_guard: (kind == LifecycleKind::Move)
                            .then(|| MoveMutationGuard::reserve(&session_id))
                            .transpose()?,
                        result: result_rx.clone(),
                        acceptance: acceptance.clone(),
                    },
                );
                if mode == AdmissionMode::FreshRequest
                    && matches!(kind, LifecycleKind::Suspend | LifecycleKind::ForceStop)
                {
                    lifecycle_owner.close_requested.insert(session_id.clone());
                }
                self.publish_revision();
                let state = Arc::clone(self);
                let operation_session_id = session_id.clone();
                let receipt = LifecycleReceipt {
                    session_id: session_id.clone(),
                    operation_id: operation_reference.clone(),
                };
                let operation = work.take().expect("new lifecycle operation has work");
                let completed_channel = result_rx.clone();
                tokio::spawn(async move {
                    let _upgrade_work = LifecycleAdmission(upgrade_work);
                    let operation_state = state.clone();
                    let operation_id = operation_session_id.clone();
                    // The operation waits synchronously for target commands,
                    // archive copies and similar I/O. It runs on its own
                    // blocking-pool thread so those waits never hold one of
                    // the async workers that serve every other request. This
                    // task keeps the admission and publishes the result; the
                    // daemon's `blocking` helper is not used because it would
                    // add an admission hold of its own.
                    let cleanup_state = state.clone();
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled.clone()),
                        operation_state.clone(),
                        operation_id.clone(),
                    );
                    let foreground = async {
                        if kind == LifecycleKind::Unpark {
                            executor.begin_resumable_work()?;
                        }
                        operation_state
                            .recovery_observer
                            .gate
                            .enter_foreground(&operation_id, kind.label(), &cancelled, &executor)
                            .await
                    }
                    .await;
                    let mut foreground_hold = None;
                    let mut result = match foreground {
                        // A Move cancelled before it owned the session did no
                        // source work. It ends as it does when the cancel
                        // lands in the startup delivery wait below: cancelled,
                        // not failed.
                        Err(_)
                            if kind == LifecycleKind::Move && cancelled.load(Ordering::Acquire) =>
                        {
                            Ok(DaemonLifecycleResult::Done)
                        }
                        Err(error) => Err(LifecycleFailure::of(&error)),
                        Ok(admission) => {
                            foreground_hold = Some(admission.clone());
                            let session_admission = SessionAdmission::Foreground(admission);
                            match mj_core::runtime::spawn_off_async_workers(async move {
                        if let Some(intent_id) = superseded_restart_intent {
                            if matches!(
                                kind,
                                LifecycleKind::ForceDestroy | LifecycleKind::DestroyStopped
                            ) && let Some(plan) = crate::database::load_session_restarts()?
                                .into_iter()
                                .find(|intent| {
                                    intent.session_id == operation_id
                                        && intent.operation_id == intent_id
                                })
                                .and_then(|intent| intent.container_upgrade)
                                && let Err(error) = plan.retire_backup_for_destroy(&operation_id, &ProcessExecutor)
                            {
                                if kind != LifecycleKind::ForceDestroy {
                                    return Err(error);
                                }
                                tracing::warn!(session_id = %operation_id, error = format!("{error:#}"), "Destroy could not remove retained container upgrade resources");
                                cleanup_state.push_public_notice(&operation_id, format!("Cleanup of retained container upgrade resources failed during Destroy for session {operation_id}. Resources may remain; details are in the daemon logs."));
                            }
                            cleanup_state
                                .release_restart_intent(&operation_id, &intent_id)
                                .await?;
                        }
                        // Move was already published under the lifecycle owner.
                        // Any earlier startup submit must settle before source
                        // sealing; later startup submits see that same owner.
                        let _startup_delivery = if kind == LifecycleKind::Move {
                            let admission = operation_state
                                .startup_delivery_gate(&operation_id)
                                .lock_owned();
                            tokio::pin!(admission);
                            loop {
                                if cancelled.load(Ordering::Acquire) {
                                    return Ok(DaemonLifecycleResult::Done);
                                }
                                tokio::select! {
                                    guard = &mut admission => break Some(guard),
                                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                                }
                            }
                        } else {
                            None
                        };
                        operation(operation_state, operation_id, cancelled, session_admission).await
                    })
                    .await
                    {
                        Ok(Ok(result)) => result.map_err(|error| LifecycleFailure::of(&error)),
                        Ok(Err(shutdown)) => Err(LifecycleFailure::internal(format!(
                            "daemon lifecycle stopped: {shutdown:#}"
                        ))),
                        Err(error) => Err(LifecycleFailure::internal(format!(
                            "daemon lifecycle task failed: {error}"
                        ))),
                            }
                        }
                    };
                    if matches!(
                        kind,
                        LifecycleKind::Restart | LifecycleKind::UpgradeContainer
                    ) {
                        // End cancellation and decide durable release under the
                        // same owner lock, before another lifecycle can enter.
                        let release_intent = {
                            let mut owner = state.owner();
                            let active = owner
                                .lifecycle
                                .get_mut(&operation_session_id)
                                .expect("Restart owns its lifecycle until completion");
                            let user_cancelled =
                                matches!(active.phase, LifecyclePhase::CancellingRestart);
                            active.phase = LifecyclePhase::CompletingRestart;
                            user_cancelled
                                .then(|| active.restart_intent.clone())
                                .flatten()
                        };
                        if let Some(intent_id) = release_intent
                            && let Err(error) = state
                                .release_restart_intent(&operation_session_id, &intent_id)
                                .await
                        {
                            result = Err(LifecycleFailure::of(&error));
                        }
                    }
                    // Cancelling a pending upgrade changes no environment.
                    // Once its durable intent is released, cancellation is a
                    // successful outcome rather than a session failure.
                    if kind == LifecycleKind::UpgradeContainer
                        && state
                            .owner()
                            .lifecycle
                            .get(&operation_session_id)
                            .is_some_and(|active| {
                                active.control.cancel_accepted() && active.restart_intent.is_none()
                            })
                    {
                        result = Ok(DaemonLifecycleResult::Done);
                    }
                    let handed_off_lifecycle = matches!(
                        kind,
                        LifecycleKind::Suspend
                            | LifecycleKind::Restart
                            | LifecycleKind::UpgradeContainer
                            | LifecycleKind::Create
                    ) && state
                        .owner()
                        .lifecycle
                        .get(&operation_session_id)
                        .is_some_and(|active| active.control.stopped_for_shutdown());
                    if let Err(error) = state.reload_controller().await {
                        let reload_error = format!(
                            "reload daemon state after lifecycle operation for {operation_session_id}: {error:#}"
                        );
                        if result.is_ok() {
                            result = Err(LifecycleFailure::internal(reload_error));
                        } else {
                            tracing::warn!(
                                session_id = %operation_session_id,
                                error = reload_error,
                                "lifecycle failed and its durable state could not be reloaded"
                            );
                        }
                    }
                    // A create or resume that failed owns its record. If it did
                    // not get as far as rolling that record back, the stored
                    // error is applied here, because nothing else will.
                    if let Err(failure) = &result
                        && !handed_off_lifecycle
                        && matches!(
                            kind,
                            LifecycleKind::Create
                                | LifecycleKind::Resume
                                | LifecycleKind::Restart
                                | LifecycleKind::UpgradeContainer
                        )
                    {
                        state
                            .fail_unfinished_provisioning(&operation_session_id, &failure.detail)
                            .await;
                    }
                    // A close is accepted asynchronously: its caller has been
                    // answered and the CLI has already printed `closing <id>`
                    // and exited. A failure puts the record back in the state
                    // it had, so without this the person is never told (#1081).
                    if let Err(failure) = &result
                        && !handed_off_lifecycle
                        && matches!(
                            kind,
                            LifecycleKind::Suspend
                                | LifecycleKind::ForceStop
                                | LifecycleKind::Restart
                                | LifecycleKind::UpgradeContainer
                        )
                    {
                        if matches!(kind, LifecycleKind::Suspend | LifecycleKind::ForceStop) {
                            state
                                .record_failed_close(
                                    &operation_session_id,
                                    &operation_reference,
                                    failure,
                                )
                                .await;
                        } else {
                            state
                                .record_lifecycle_failure(
                                    &operation_session_id,
                                    &operation_reference,
                                    failure,
                                    if kind == LifecycleKind::UpgradeContainer {
                                        mj_core::state::CONTAINER_UPGRADE_FAILURE_PREFIX
                                    } else {
                                        mj_core::state::CLOSE_FAILURE_PREFIX
                                    },
                                )
                                .await;
                        }
                    }
                    if let Err(failure) = &result
                        && matches!(
                            kind,
                            LifecycleKind::ForceDestroy | LifecycleKind::DestroyStopped
                        )
                    {
                        state
                            .record_lifecycle_failure(
                                &operation_session_id,
                                &operation_reference,
                                failure,
                                mj_core::state::DESTRUCTION_FAILURE_PREFIX,
                            )
                            .await;
                    }
                    state.note_lifecycle_outcome(&operation_session_id);
                    if let Err(error) =
                        reach_test_hook("lifecycle_reservation_before_result_publication").await
                    {
                        result = Err(LifecycleFailure::internal(format!(
                            "test lifecycle publication hook failed: {error:#}"
                        )));
                    }
                    let deferred_cleanup =
                        matches!(result, Ok(DaemonLifecycleResult::DeferredCleanup));
                    if handed_off_lifecycle {
                        tracing::info!(session_id = %operation_session_id, ?kind, "lifecycle operation yielded to the replacement daemon");
                    } else if let Err(error) = &result {
                        let cancel_cause = state
                            .owner()
                            .lifecycle
                            .get(&operation_session_id)
                            .and_then(|active| active.control.cancel_cause());
                        tracing::warn!(session_id = %operation_session_id, ?kind, reference = %operation_reference, ?cancel_cause, %error, "lifecycle operation failed");
                    }
                    // Serialize final Move refusal disposition against any late
                    // startup enqueue, including cancellation before source work.
                    let _startup_enqueue = if kind == LifecycleKind::Move {
                        Some(state.startup_enqueue.lock().await)
                    } else {
                        None
                    };
                    let restore_failed_move = kind == LifecycleKind::Move
                        && state
                            .owner()
                            .lifecycle
                            .get(&operation_session_id)
                            .is_some_and(|active| {
                                !active.control.stopped_for_shutdown()
                                    && (result.is_err() || active.control.cancel_accepted())
                            });
                    if restore_failed_move && let Some(move_origin) = move_operation_id.clone() {
                        let id = operation_session_id.clone();
                        let reason = match &result {
                            Err(error) => {
                                format!("Move failed before destination delivery: {}", error.detail)
                            }
                            Ok(_) => "Move was cancelled before destination delivery".to_owned(),
                        };
                        let persisted = reason.clone();
                        match blocking(move || {
                            crate::database::reject_pending_move_startup_prompts(&id, &move_origin, &persisted)
                        })
                        .await
                        {
                            Ok(()) => state.push_notice(
                                &operation_session_id,
                                format!(
                                    "Move did not deliver queued input; unsent prompts remain saved for draft recovery: {reason}"
                                ),
                            ),
                            Err(error) => {
                                tracing::error!(session_id = %operation_session_id, %error, "Move prompt disposition remains saved")
                            }
                        }
                    }
                    {
                        let mut owner = state.owner();
                        owner.complete_lifecycle(
                            &operation_session_id,
                            &operation_reference,
                            result.clone(),
                        );
                        // Waiters and state readers cross the same completion
                        // boundary; the channel is notification, not ownership.
                        result_tx.send_replace(Some(result));
                    }
                    drop(_startup_enqueue);
                    drop(foreground_hold);
                    // Hand off under daemon ownership even if the requesting
                    // client disconnects. The completed close remains visible
                    // until the cleanup replaces it in the lifecycle map.
                    if deferred_cleanup
                        && let Err(error) = state
                            .start_deferred_cleanup(operation_session_id.clone())
                            .await
                    {
                        tracing::warn!(session_id = %operation_session_id, %error, "could not start retained cleanup");
                        state.push_notice(&operation_session_id, "Container cleanup could not start; retry cleanup from the stopped session.");
                        state
                            .owner()
                            .lifecycle
                            .retain(|_, active| !active.result.same_channel(&completed_channel));
                    }
                    state.publish_revision();
                });
                (result_rx, receipt, acceptance)
            }
        };
        Ok(StartedLifecycle {
            receipt,
            result,
            acceptance,
        })
    }

    pub(super) async fn wait_lifecycle_result(
        mut result: LifecycleWatch,
    ) -> Result<DaemonLifecycleResult> {
        loop {
            if let Some(result) = result.borrow_and_update().clone() {
                return result.map_err(LifecycleFailure::into_error);
            }
            result
                .changed()
                .await
                .context("daemon lifecycle operation stopped without a result")?;
        }
    }

    pub(super) async fn run_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<DaemonLifecycleResult>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>, SessionAdmission) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let result = self.start_or_join_lifecycle(session_id, kind, work).await?;
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        outcome
    }

    pub(super) fn remove_completed_lifecycle(&self, channel: &LifecycleWatch) {
        self.owner().lifecycle.retain(|_, active| {
            !active.result.same_channel(channel)
                || active.is_visible()
                || (active.restart_intent.is_some()
                    && matches!(active.phase, LifecyclePhase::Completed(Err(_))))
        });
    }
}

/// Names a lifecycle's hold on the upgrade gate. It reads the session, kind and
/// stage from the lifecycle registry each time, so it can never disagree with it.
struct LifecycleHold {
    state: std::sync::Weak<RuntimeState>,
    session_id: String,
    operation_id: String,
    kind: LifecycleKind,
}

impl crate::upgrade::Describe for LifecycleHold {
    fn describe(&self) -> crate::upgrade::HoldDetail {
        let stages = self
            .state
            .upgrade()
            .map(|state| state.lifecycle_stage_labels(&self.session_id, &self.operation_id))
            .unwrap_or_default();
        crate::upgrade::HoldDetail {
            subject: format!(
                "{} {}",
                self.kind.label(),
                mj_core::state::short_id(&self.session_id)
            ),
            stage: (!stages.is_empty()).then(|| stages.join(" + ")),
        }
    }
}

struct LifecycleAdmission(Arc<Mutex<Option<crate::upgrade::Work>>>);
impl Drop for LifecycleAdmission {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// Arrival order lives with the lifecycle registry. Dropping a cancelled
/// request removes its intent and wakes its successor.
struct LifecycleIntent {
    state: Arc<RuntimeState>,
    session_id: String,
    ticket: Arc<()>,
}
impl Drop for LifecycleIntent {
    fn drop(&mut self) {
        let mut owner = self.state.owner();
        if let Some(queue) = owner.lifecycle_intents.get_mut(&self.session_id) {
            queue.retain(|ticket| !Arc::ptr_eq(ticket, &self.ticket));
            if queue.is_empty() {
                owner.lifecycle_intents.remove(&self.session_id);
            }
        }
        self.state.publish_revision();
    }
}

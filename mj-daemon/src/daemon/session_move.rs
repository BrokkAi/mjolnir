//! Daemon-owned move admission, supervision, and restart reconciliation.

use super::*;
use mj_core::state::MoveOperation;

pub(super) struct MoveRecoveryOwners {
    pub(super) moves: BTreeSet<String>,
    pub(super) closes: BTreeSet<String>,
}

pub(super) fn public_quota_failure(error: &anyhow::Error, reference: &str) -> String {
    // General configuration refusals can contain parser source lines. Only
    // the Move safety check's explicit code certifies journal-safe text.
    Refusal::of(error)
        .filter(|refusal| {
            refusal.code()
                == Some(mj_controller::controller::move_session::QUOTA_MOVE_SAFETY_REFUSAL)
        })
        .map(|refusal| refusal.message().to_owned())
        .unwrap_or_else(|| format!("details are in the daemon log under reference {reference}"))
}

fn validate_prepared_move_environment(prepared: bool, durable: bool) -> Result<()> {
    ensure!(
        prepared == durable,
        "the Move's environment decision changed after preparation; prepare and confirm the Move again"
    );
    Ok(())
}

impl RuntimeState {
    async fn reserve_container_capacity_for_move(
        self: &Arc<Self>,
        request: &MoveSessionRequest,
    ) -> Result<Option<capacity::ContainerCapacityLease>> {
        let selection = &request.preparation.selection;
        let (target_id, allocation, source, in_place) = {
            let owner = self.owner();
            let controller = owner.controller();
            let source = controller
                .state
                .sessions
                .get(&selection.session_id)
                .with_context(|| format!("unknown session {:?}", selection.session_id))?;
            let target_id = selection
                .target_template_id
                .as_deref()
                .unwrap_or(&source.target_template_id)
                .to_owned();
            let previous = owner
                .committed()
                .and_then(|committed| committed.moves.get(&source.id));
            let in_place = mj_controller::controller::move_session::move_in_place_for_record(
                source,
                selection,
                &controller.config.targets,
                previous,
            )
            .map_err(|error| anyhow::Error::new(Refusal::precondition(error.to_string())))?;
            let allocation = if selection.clear_resource_allocation {
                None
            } else {
                selection
                    .resource_allocation
                    .clone()
                    .or_else(|| source.resource_allocation.clone())
            };
            let source_commitment = if mj_core::state::session_owns_container(source) {
                self.capacity_feed()
                    .and_then(|feed| feed.target_for(&source.target_template_id))
                    .filter(|target| {
                        target.kind == mj_controller::targets::DeploymentCapacityKind::Host
                    })
                    .map(|target| capacity::ContainerCapacityCommitment {
                        target_ids: target.target_ids,
                        allocation: mj_core::state::counted_container_allocation(source).cloned(),
                    })
            } else {
                None
            };
            (target_id, allocation, source_commitment, in_place)
        };
        validate_prepared_move_environment(request.preparation.in_place, in_place)
            .map_err(|error| anyhow::Error::new(Refusal::precondition(error.to_string())))?;
        // An in-place Move replaces only the harness in the existing
        // environment, so it creates no additional container commitment.
        if in_place {
            return Ok(None);
        }
        self.admit_container_launch(
            &target_id,
            allocation,
            Some(&selection.session_id),
            BTreeSet::from([selection.session_id.clone()]),
            source,
        )
        .await
        .map(capacity::ContainerAdmission::into_lease)
    }

    pub async fn prepare_move_session(
        self: &Arc<Self>,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        self.prepare_move_session_for(selection, None).await
    }

    async fn prepare_move_session_for(
        self: &Arc<Self>,
        selection: MoveSelection,
        quota: Option<&mj_core::continuation::QuotaRecovery>,
    ) -> Result<MovePreparation> {
        let (source_harness, source_active, source_has_request_queue) = {
            let controller_owner = self.owner();
            let controller = controller_owner.controller();
            let source = controller
                .state
                .sessions
                .get(&selection.session_id)
                .context("Move session is missing")?;
            (
                source.harness_kind,
                matches!(
                    source.state,
                    SessionState::Running | SessionState::Disconnected
                ),
                controller.source_has_request_queue(&source.id),
            )
        };
        let snapshot = if source_active {
            mj_controller::controller::move_session::refresh_move_source(
                &self.session_manager,
                &selection.session_id,
            )
            .await?
        } else {
            None
        };
        let catalog = self
            .profile_catalog
            .get()
            .expect("catalog installed before serving")
            .clone();
        let mut preparation = blocking(move || {
            let controller = Controller::load()?;
            mj_core::runtime::block_on(controller.prepare_move_session_controlled(
                &catalog,
                selection,
                &ProcessExecutor,
            ))?
        })
        .await?;
        if preparation.container_upgrade.is_some() {
            return Ok(preparation);
        }
        preparation.source_unavailable = source_active
            && snapshot
                .as_ref()
                .is_none_or(|snapshot| !snapshot.operational.native_session_is_ready());
        preparation.active |= preparation.source_unavailable;
        if let Some(snapshot) = snapshot {
            // Old workers cannot freeze the child set, so their stop hook
            // requires a warning even when no child is recorded yet.
            preparation.stops_subagents |= source_has_request_queue
                && snapshot
                    .operational
                    .relay_protocol_version
                    .is_some_and(|version| {
                        !mj_controller::controller::move_session::worker_drains_subagent_requests(
                            version,
                        )
                    });
            let mut operational = snapshot.operational;
            operational.queued_prompts.clear();
            operational.checkpoint_barrier = None;
            let purpose = if quota.is_some() && preparation.in_place && !preparation.cross_harness {
                mj_core::activity::MovePurpose::QuotaRetry
            } else {
                mj_core::activity::MovePurpose::Manual
            };
            preparation.active |= !operational.safe_to_move(source_harness, purpose);
            if let Some(quota) = quota
                && purpose == mj_core::activity::MovePurpose::QuotaRetry
            {
                ensure!(
                    preparation.source_profile_id == quota.profile_id
                        && operational.continuation.user_command_id.as_ref()
                            == Some(&quota.user_command_id)
                        && operational.continuation.completed_command_id.as_ref()
                            == Some(&quota.completed_command_id),
                    "quota Move source frontier changed during preparation"
                );
                preparation.fingerprint =
                    mj_controller::controller::move_session::quota_move_fingerprint(
                        &preparation.selection.session_id,
                        &preparation.fingerprint,
                        &operational,
                    )?;
            }
        }
        Ok(preparation)
    }

    pub(crate) async fn prepare_quota_move(
        self: &Arc<Self>,
        session_id: &str,
        profile_id: &str,
        quota: &mj_core::continuation::QuotaRecovery,
    ) -> Result<MoveSessionRequest> {
        let preparation = self
            .prepare_move_session_for(
                MoveSelection {
                    oracle: None,
                    oracle_choice: None,
                    session_id: session_id.to_owned(),
                    profile_id: Some(profile_id.to_owned()),
                    target_template_id: None,
                    consent: Default::default(),
                    skip_keep_paths: Vec::new(),
                    clear_resource_allocation: false,
                    additional_mounts: None,
                    resource_allocation: None,
                    subagents: None,
                },
                Some(quota),
            )
            .await?;
        let blockers: Vec<_> = [
            (!preparation.in_place, "the environment would change"),
            (
                preparation.cross_harness,
                "the native conversation cannot be kept",
            ),
            (
                preparation.active,
                "the session has active work or its worker is unavailable",
            ),
            (
                preparation.stops_subagents,
                "the Move cannot guarantee that sub-agents stay running",
            ),
            (
                preparation.conversion.is_some(),
                "the workspace requires conversion",
            ),
        ]
        .into_iter()
        .filter_map(|(blocked, reason)| blocked.then_some(reason))
        .collect();
        if !blockers.is_empty() {
            return Err(Refusal::precondition(format!(
                "Cannot switch profiles automatically: {}",
                blockers.join("; ")
            ))
            .with_code(mj_controller::controller::move_session::QUOTA_MOVE_SAFETY_REFUSAL)
            .into());
        }
        Ok(MoveSessionRequest {
            preparation,
            queue: Some(mj_core::state::ResumeQueueDisposition::Start),
            acknowledge_interruption: false,
        })
    }

    pub(crate) async fn start_quota_move(
        self: &Arc<Self>,
        request: MoveSessionRequest,
        reason: String,
        continuation: mj_core::state::QuotaMoveContinuation,
        admission: mj_controller::upgrade::Work,
    ) -> Result<MoveOutcome> {
        let started = self
            .admit_move_session(request, None, Some((reason, continuation)))
            .await?;
        drop(admission);
        let channel = started.result.clone();
        let result = Self::wait_lifecycle_result(started.result).await;
        self.remove_completed_lifecycle(&channel);
        match result? {
            DaemonLifecycleResult::Move(outcome) => Ok(outcome),
            DaemonLifecycleResult::Done => bail!("quota Move was cancelled before it started"),
            _ => bail!("quota Move returned an unrelated lifecycle result"),
        }
    }

    /// Reserve lifecycle ownership before a web request is acknowledged.
    pub(crate) async fn start_move_session(
        self: &Arc<Self>,
        request: MoveSessionRequest,
        control: LifecycleControl,
    ) -> Result<StartedLifecycle> {
        self.admit_move_session(request, Some(control), None).await
    }

    async fn admit_move_session(
        self: &Arc<Self>,
        request: MoveSessionRequest,
        control: Option<LifecycleControl>,
        quota_move: Option<(String, mj_core::state::QuotaMoveContinuation)>,
    ) -> Result<StartedLifecycle> {
        let selection = request.preparation.selection.clone();
        let operation_id = request.preparation.operation_id.clone();
        // Preparation resolves inherited settings. Compare those settings,
        // never just the verb or a freshly generated preparation identifier.
        let key = serde_json::to_string(&(
            &selection,
            request.queue,
            request.acknowledge_interruption,
            &quota_move,
        ))?;
        let session_id = selection.session_id.clone();
        let (accepted, acceptance) = tokio::sync::watch::channel(None);
        let result = self
            .admit_prepared_lifecycle(
                session_id.clone(),
                LifecycleKind::Move,
                super::lifecycle::LifecycleStart {
                    origin: if quota_move.is_some() {
                        super::lifecycle::LifecycleOrigin::Automatic
                    } else {
                        super::lifecycle::LifecycleOrigin::Person
                    },
                    resume_workspace_id: None,
                    request_key: Some(key),
                    control,
                    phase: LifecyclePhase::Executing,
                    move_operation_id: Some(operation_id.clone()),
                    deferrable_admission: None,
                    holds_through_drain: false,
                },
                Some(acceptance),
                move |state, session_id, control, session_admission| async move {
                    let result = async {
                        let _capacity_lease =
                            state.reserve_container_capacity_for_move(&request).await?;
                        let catalog = state
                            .profile_catalog
                            .get()
                            .expect("catalog installed before serving")
                            .clone();
                        state
                            .clone()
                            .run_move_controller_work(
                                session_id.clone(),
                                control,
                                |mut controller, executor, manager| async move {
                                    controller
                                        .move_session_managed_controlled_with_admission(
                                            &session_admission,
                                            &catalog,
                                            request,
                                            quota_move,
                                            &executor,
                                            &manager,
                                            move || {
                                                accepted.send_replace(Some(Ok(())));
                                            },
                                        )
                                        .await
                                },
                            )
                            .await
                    }
                    .await;
                    if result.is_err() {
                        state
                            .tell_live_parent_about_stopped_subagents(&session_id)
                            .await;
                    }
                    result.map(DaemonLifecycleResult::Move)
                },
            )
            .await?;
        self.set_lifecycle_resume_destination(
            &session_id,
            selection.profile_id.clone().unwrap_or_default(),
            selection.target_template_id.clone().unwrap_or_default(),
        );
        Ok(result)
    }

    pub async fn move_session(
        self: &Arc<Self>,
        request: MoveSessionRequest,
    ) -> Result<MoveOutcome> {
        let selection = request.preparation.selection.clone();
        let operation_id = request.preparation.operation_id.clone();
        let session_id = selection.session_id.clone();
        let mut result = self.admit_move_session(request, None, None).await?;
        let channel = result.result.clone();
        // Resource/preparation refusals still precede acceptance on the
        // legacy wait-for-outcome surface, just as on the receipt API.
        if let Err(error) = result.confirm_acceptance().await {
            self.remove_completed_lifecycle(&channel);
            return Err(error);
        }
        let result = Self::wait_lifecycle_result(result.result).await;
        self.remove_completed_lifecycle(&channel);
        match result {
            Ok(DaemonLifecycleResult::Move(outcome)) => Ok(outcome),
            Ok(_) => bail!("move returned an unrelated lifecycle result"),
            Err(error) => Ok(MoveOutcome {
                operation_id, session_id, profile_id: selection.profile_id.unwrap_or_default(),
                target_template_id: selection.target_template_id.unwrap_or_default(), outcome: "failed".into(),
                error: Some(format!("{error:#}")), recovery: Some("Inspect session status and prepare Move again; any verified checkpoint is retained.".into()),
            }),
        }
    }

    pub(super) async fn recover_moves(
        self: &Arc<Self>,
        operations: Vec<MoveOperation>,
        close_operations: &BTreeSet<String>,
        retired_sessions: &BTreeSet<String>,
    ) -> Result<MoveRecoveryOwners> {
        let mut owned = BTreeSet::new();
        let mut closes = BTreeSet::new();
        // At daemon startup (before live Move admission opens), no direct
        // transfer from a previous daemon process is still running, including
        // for any Move that crashed mid-Transferring. Passing an empty active
        // set prunes every dead pre-crash user immediately so live-safe
        // background reconciliation revokes their EC2 ingress rules while
        // preserving any new Moves admitted after startup.
        mj_controller::controller::move_session::recovery::spawn_ec2_grant_reconcile_background(
            BTreeSet::new(),
        );
        // Schedule non-admitted background revocation for any ephemeral Move
        // credentials left on non-active moves without blocking startup on SSH.
        if operations
            .iter()
            .any(|op| !op.is_active() && !op.granted_keys.is_empty())
        {
            mj_controller::controller::move_session::recovery::spawn_move_key_cleanup_background();
        }
        // Recover any pre-149 LegacyMoveOperation rows across an upgrade:
        // - Fully resolved legacy moves are deleted immediately (local SQLite).
        // - Retryable inactive (Failed/Cancelled) moves retaining a handoff,
        //   checkpoint, or prepared destination are converted to current
        //   MoveOperation rows locally so retryability and archive retention survive.
        // - Active or unfinished-queue moves are admitted as supervised
        //   background lifecycle tasks (holds_through_drain: false) so remote
        //   cleanup, worker probes, and relay connections never block startup.
        if let Ok(legacy_ops) = mj_controller::database::load_legacy_move_operations() {
            for (session_id, legacy_op) in legacy_ops {
                if retired_sessions.contains(&session_id) {
                    continue;
                }
                if legacy_op.is_resolved() {
                    let _ = mj_controller::database::delete_move_operation(&session_id);
                    continue;
                }
                if !legacy_op.needs_recovery() {
                    let converted = legacy_op.into_retryable_move_operation(&session_id);
                    if let Err(error) = mj_controller::database::save_move_operation(&converted) {
                        tracing::warn!(
                            session_id = %session_id,
                            operation_id = %legacy_op.operation_id,
                            error = %error,
                            "could not migrate retryable legacy Move row; preserving legacy row"
                        );
                    }
                    continue;
                }
                let op_id = legacy_op.operation_id.clone();
                let key = format!("legacy-recovery:{op_id}");
                let id_for_task = session_id.clone();
                let op_for_task = legacy_op.clone();
                let result = self
                    .admit_lifecycle(
                        session_id.clone(),
                        LifecycleKind::Move,
                        super::lifecycle::LifecycleStart {
                            origin: super::lifecycle::LifecycleOrigin::Automatic,
                            resume_workspace_id: None,
                            request_key: Some(key),
                            control: None,
                            phase: LifecyclePhase::Executing,
                            move_operation_id: Some(op_id.clone()),
                            deferrable_admission: None,
                            holds_through_drain: false,
                        },
                        move |state, session_id, control, _session_admission| async move {
                            let notice_state = state.clone();
                            let outcome = state
                                .run_move_controller_work(
                                    session_id.clone(),
                                    control,
                                    |mut controller, executor, _manager| async move {
                                        let notice = controller
                                            .recover_legacy_move_operation(
                                                &id_for_task,
                                                &op_for_task,
                                                &executor,
                                            )
                                            .await?;
                                        let _ = mj_controller::database::delete_move_operation(
                                            &id_for_task,
                                        );
                                        Ok(mj_core::state::MoveOutcome {
                                            operation_id: op_for_task.operation_id,
                                            session_id: id_for_task,
                                            profile_id: op_for_task.source_profile_id,
                                            target_template_id: op_for_task
                                                .source_target_template_id,
                                            outcome: "completed".into(),
                                            error: None,
                                            recovery: Some(notice),
                                        })
                                    },
                                )
                                .await?;
                            if let Some(notice) = outcome.recovery.clone() {
                                notice_state.push_notice(&session_id, notice);
                            }
                            Ok(DaemonLifecycleResult::Move(outcome))
                        },
                    )
                    .await?;
                owned.insert(session_id.clone());
                let state = self.clone();
                mj_core::runtime::spawn(async move {
                    let channel = result.clone();
                    if let Err(error) = Self::wait_lifecycle_result(result).await {
                        state.push_notice(
                            &session_id,
                            format!("Legacy Move {op_id} recovery failed: {error:#}"),
                        );
                    }
                    state.remove_completed_lifecycle(&channel);
                });
            }
        }
        for operation in &operations {
            mj_controller::controller::move_session::restore_move_queue_hold(operation);
        }
        for operation in operations {
            // Historical records cannot own a later lifecycle or revive an
            // intent preserved by an older daemon after terminal publication.
            if operation.is_resolved() {
                continue;
            }
            let state = self
                .owner()
                .controller()
                .state
                .sessions
                .get(&operation.selection.session_id)
                .map(|session| session.state);
            let owner = mj_controller::database::move_recovery_owner(
                state,
                close_operations.contains(&operation.operation_id),
            );
            // Cancellation alone grants no teardown authority.
            if owner == Some(mj_controller::database::MoveRecoveryOwner::Close) {
                closes.insert(operation.selection.session_id.clone());
                continue;
            }
            if !operation.is_active() && owner.is_none() {
                continue;
            }
            let id = operation.selection.session_id.clone();
            let key = format!("recovery:{}", operation.operation_id);
            let phase = if operation.recovery_session.is_some() {
                LifecyclePhase::MovingDestination
            } else {
                LifecyclePhase::Executing
            };
            let result = self
                .admit_lifecycle(
                    id.clone(),
                    LifecycleKind::Move,
                    super::lifecycle::LifecycleStart {
                        origin: super::lifecycle::LifecycleOrigin::Automatic,
                        resume_workspace_id: None,
                        request_key: Some(key),
                        control: None,
                        phase,
                        move_operation_id: Some(operation.operation_id.clone()),
                        deferrable_admission: None,
                        holds_through_drain: false,
                    },
                    move |state, session_id, control, session_admission| async move {
                        state
                            .run_move_controller_work(
                                session_id,
                                control,
                                |mut controller, executor, manager| async move {
                                    controller
                                        .recover_move_managed_controlled(
                                            &session_admission,
                                            operation,
                                            &executor,
                                            &manager,
                                        )
                                        .await
                                },
                            )
                            .await
                            .map(DaemonLifecycleResult::Move)
                    },
                )
                .await?;
            owned.insert(id.clone());
            let state = self.clone();
            mj_core::runtime::spawn(async move {
                let channel = result.clone();
                match Self::wait_lifecycle_result(result).await {
                    Ok(DaemonLifecycleResult::Move(outcome))
                        if !matches!(outcome.outcome.as_str(), "completed" | "interrupted") =>
                    {
                        state.push_notice(
                            &id,
                            outcome
                                .error
                                .unwrap_or_else(|| "Move recovery needs attention".into()),
                        );
                    }
                    Err(error) => {
                        state.push_notice(&id, format!("Move recovery failed: {error:#}"))
                    }
                    _ => {}
                }
                state.remove_completed_lifecycle(&channel);
            });
        }
        Ok(MoveRecoveryOwners {
            moves: owned,
            closes,
        })
    }

    pub(super) async fn resume_move_destination_cleanups(self: &Arc<Self>, immediately: bool) {
        let ids = {
            let owner = self.owner();
            owner
                .committed()
                .into_iter()
                .flat_map(|committed| committed.moves.values())
                .filter(|op| {
                    op.prepared_destination.as_ref().is_some_and(|d| {
                        matches!(
                            d.state,
                            mj_core::state::PreparedDestinationState::CleanupPending { .. }
                        )
                    })
                })
                .filter(|op| {
                    !owner
                        .lifecycle
                        .get(&op.selection.session_id)
                        .is_some_and(|active| active.is_running())
                })
                .filter(|op| {
                    immediately
                        || chrono::DateTime::parse_from_rfc3339(&op.updated_at)
                            .map(|at| {
                                (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds()
                                    >= 30
                            })
                            .unwrap_or(true)
                })
                .map(|op| op.selection.session_id.clone())
                .collect::<Vec<_>>()
        };
        for id in ids {
            let result = self
                .start_automatic_lifecycle(
                    id.clone(),
                    LifecycleKind::Cleanup,
                    |state, id, control, _session_admission| async move {
                        state
                        .run_move_controller_work(
                            id.clone(),
                            control,
                            move |controller, executor, _manager| async move {
                                let mut operation = mj_controller::database::load_move_operation(&id)?
                                    .context("Move cleanup intent missing")?;
                                executor.begin_resumable_work()?;
                                let result = controller
                                    .cleanup_prepared_move_destination(&mut operation, &executor);
                                operation.updated_at = chrono::Utc::now().to_rfc3339();
                                if result.is_ok() {
                                    mj_controller::controller::move_session::record_finished_move_recovery(
                                        &controller.state,
                                        &mut operation,
                                    )?;
                                } else {
                                    mj_controller::database::save_move_operation(&operation)?;
                                }
                                executor.end_resumable_work("Move control transition")?;
                                result?;
                                Ok(MoveOutcome {
                                    operation_id: operation.operation_id,
                                    session_id: id,
                                    profile_id: operation.selection.profile_id.unwrap_or_default(),
                                    target_template_id: operation
                                        .selection
                                        .target_template_id
                                        .unwrap_or_default(),
                                    outcome: "cleaned_up".into(),
                                    error: None,
                                    recovery: None,
                                })
                            },
                        )
                        .await
                        .map(DaemonLifecycleResult::Move)
                    },
                )
                .await;
            match result {
                Ok(result) => {
                    let state = self.clone();
                    mj_core::runtime::spawn(async move {
                        let channel = result.clone();
                        if let Err(error) = Self::wait_lifecycle_result(result).await {
                            tracing::warn!(session_id=%id, %error, "EC2 Move destination cleanup remains pending; retry in 30s");
                            state.push_notice(
                                &id,
                                format!(
                                    "EC2 destination cleanup will retry automatically: {error:#}"
                                ),
                            );
                        }
                        state.remove_completed_lifecycle(&channel);
                    });
                }
                Err(error) => {
                    tracing::warn!(session_id=%id, %error, "EC2 Move destination cleanup could not start")
                }
            }
        }
    }

    /// The controller half of every Move lifecycle, started or recovered.
    ///
    /// It takes no upgrade admission of its own; the lifecycle's hold is the
    /// only one. That is what lets the slow path work as intended: it
    /// releases the lifecycle's hold around each resumable workspace copy
    /// (`begin_resumable_work`), so a handoff does not wait for a copy,
    /// the closing daemon cancels it, and the next daemon resumes the Move
    /// from its durable phase. An in-place Move also releases the hold while
    /// draining durable delegation requests before sealing the source.
    ///
    /// The Move's controller future holds a standard mutex guard across an
    /// await, so it is not `Send` and cannot be the lifecycle future itself.
    /// It is built and awaited on a blocking-pool thread of its own.
    pub(super) async fn run_move_controller_work<W, Fut>(
        self: Arc<Self>,
        session_id: String,
        control: LifecycleControl,
        work: W,
    ) -> Result<MoveOutcome>
    where
        W: FnOnce(
                Controller,
                DaemonStageReportingExecutor<CancellableProcessExecutor>,
                SessionManagerControl,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = Result<MoveOutcome>>,
    {
        mj_core::runtime::spawn_blocking(move || {
            let loading = std::time::Instant::now();
            let controller = (self.controller_loader)()?;
            tracing::info!(
                %session_id,
                phase = "load controller state",
                elapsed_ms = loading.elapsed().as_millis() as u64,
                "move phase finished"
            );
            let manager = self.session_manager.clone();
            let executor = DaemonStageReportingExecutor::new(control.executor(), self, session_id);
            let result = mj_core::runtime::block_on(Box::pin(work(controller, executor, manager)));
            mj_controller::mailbox_outbox::notify_mailbox_outbox_changed();
            result?
        })
        .await
        .context("Move controller task failed")?
    }
}

#[cfg(all(test, unix))]
mod admission_tests {
    use super::*;
    use mj_controller::controller::test_support::{IsolatedTest, test_name};

    const BOUND: Duration = Duration::from_secs(5);

    fn metadata() -> DaemonMetadata {
        DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            token: "right-token".into(),
            started_at: "now".into(),
            build_version: "test".into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        }
    }

    fn outcome(status: &str) -> MoveOutcome {
        MoveOutcome {
            operation_id: "move-operation".into(),
            session_id: "moving-session".into(),
            profile_id: String::new(),
            target_template_id: String::new(),
            outcome: status.into(),
            error: None,
            recovery: None,
        }
    }

    // Hard-won: 03034132f: a client-controlled in-place flag could skip capacity admission.
    #[test]
    fn forged_move_environment_decision_is_refused() {
        assert!(validate_prepared_move_environment(true, false).is_err());
        assert!(validate_prepared_move_environment(false, true).is_err());
        assert!(validate_prepared_move_environment(true, true).is_ok());
        assert!(validate_prepared_move_environment(false, false).is_ok());
    }

    /// A command that records its process id and waits for `release`.
    fn held_command(directory: &Path, purpose: &str) -> CommandSpec {
        CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                format!(
                    "echo $$ > '{0}/pid.part' && mv '{0}/pid.part' '{0}/pid' && while [ ! -e '{0}/release' ]; do sleep 0.05; done",
                    directory.display()
                ),
            ],
        )
        .purpose(purpose)
    }

    /// Releases a held command when a test ends, so a failing assertion does
    /// not leave the runtime waiting for it forever.
    struct ReleaseOnDrop(PathBuf);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }

    async fn wait_for_file(path: &Path) {
        let deadline = std::time::Instant::now() + BOUND;
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn process_is_running(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn handoff(state: &Arc<RuntimeState>) -> DaemonReply {
        super::super::actions::handle_action(
            DaemonAction::PrepareUpgrade,
            &metadata(),
            state,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    /// A slow-path Move holds no handoff admission while it copies a
    /// workspace: a handoff completes without waiting for the copy, the stop
    /// that follows kills the copy, and the Move reports that it was
    /// interrupted for the next daemon to resume, as `finish_move_result`
    /// does for a Move with a workspace transfer.
    // Hard-won: 571e8aa0: a handoff waited through a resumable Move copy because duplicate admission stayed held.
    // Hard-won: 5ca468627: reading the pid before its write finished hung panic cleanup.
    // Hard-won: #1562: finishing resumable work during draining was recorded as failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_handoff_does_not_wait_for_a_slow_path_move_copy() {
        const NAME: &str = "a_handoff_does_not_wait_for_a_slow_path_move_copy";
        const CHILD: &str = "MJ_TEST_SLOW_MOVE_HANDOFF";
        if std::env::var_os(CHILD).is_none() {
            for mode in ["copy", "draining-lapse", "draining-close"] {
                let root = tempfile::tempdir().unwrap();
                IsolatedTest::new(test_name(module_path!(), NAME))
                    .env(CHILD, mode)
                    .env("MJ_INSTANCE", "slow-move-handoff")
                    .isolated_store(root.path())
                    .run();
            }
            return;
        }
        let mode = std::env::var(CHILD).unwrap();
        let copy_finished = Arc::new(tokio::sync::Notify::new());
        let finished = copy_finished.clone();
        let directory = tempfile::tempdir().unwrap();
        let _release = ReleaseOnDrop(directory.path().join("release"));
        let copy = held_command(directory.path(), "copy Move workspace");
        let state = super::super::tests::test_runtime_state();
        let result = state
            .start_automatic_lifecycle(
                "moving-session".into(),
                LifecycleKind::Move,
                move |state, session_id, control, _session_admission| async move {
                    state
                        .run_move_controller_work(
                            session_id,
                            control,
                            |_, executor, _| async move {
                                executor.begin_resumable_work()?;
                                let copied = executor.execute(&copy);
                                finished.notify_one();
                                let resumed =
                                    executor.end_resumable_work("Move control transition");
                                if copied.is_ok() && resumed.is_ok() {
                                    return Ok(outcome("completed"));
                                }
                                ensure!(
                                    !mj_controller::upgrade::gate().is_open(),
                                    "the copy stopped without a handoff"
                                );
                                Ok(outcome("interrupted"))
                            },
                        )
                        .await
                        .map(DaemonLifecycleResult::Move)
                },
            )
            .await
            .unwrap();
        wait_for_file(&directory.path().join("pid")).await;
        let pid: i32 = std::fs::read_to_string(directory.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let labels = mj_controller::upgrade::active_labels();
        assert!(
            !labels
                .iter()
                .any(|label| label == "session lifecycle" || label == "database operation"),
            "the copy holds handoff admission: {labels:?}"
        );
        if mode != "copy" {
            let other = mj_controller::upgrade::activity("other bounded control").unwrap();
            assert!(matches!(handoff(&state).await, DaemonReply::UpgradePending));
            assert!(mj_controller::upgrade::gate().is_open());
            std::fs::write(directory.path().join("release"), b"").unwrap();
            tokio::time::timeout(BOUND, copy_finished.notified())
                .await
                .unwrap();
            tokio::time::timeout(
                BOUND,
                mj_controller::upgrade::gate().wait_for_resumable_waiter_for_test(),
            )
            .await
            .unwrap();
            assert!(
                result.borrow().is_none(),
                "drain refusal must not complete the Move as failure"
            );
            assert_eq!(
                mj_controller::upgrade::active_labels(),
                vec!["other bounded control"]
            );
            if mode == "draining-lapse" {
                mj_controller::upgrade::gate().expire_drain_for_test();
                drop(other);
                match tokio::time::timeout(BOUND, RuntimeState::wait_lifecycle_result(result))
                    .await
                    .unwrap()
                    .unwrap()
                {
                    DaemonLifecycleResult::Move(outcome) => {
                        assert_eq!(outcome.outcome, "completed")
                    }
                    _ => panic!("Move must finish normally after drain lapse"),
                }
                assert!(mj_controller::upgrade::gate().is_open());
            } else {
                drop(other);
                assert!(matches!(handoff(&state).await, DaemonReply::Done));
                match tokio::time::timeout(BOUND, RuntimeState::wait_lifecycle_result(result))
                    .await
                    .unwrap()
                    .unwrap()
                {
                    DaemonLifecycleResult::Move(outcome) => {
                        assert_eq!(outcome.outcome, "interrupted")
                    }
                    _ => panic!("closed gate must hand the durable Move to the next daemon"),
                }
            }
            assert!(
                !process_is_running(pid),
                "the completed copy cannot outlive the test"
            );
            return;
        }
        let deadline = std::time::Instant::now() + BOUND;
        while !matches!(handoff(&state).await, DaemonReply::Done) {
            assert!(
                std::time::Instant::now() < deadline,
                "the handoff waited for the copy: {:?}",
                mj_controller::upgrade::active_labels()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            process_is_running(pid),
            "the handoff itself cancels nothing"
        );

        // The closing daemon cancels the lifecycles it leaves behind.
        let stopping = std::time::Instant::now();
        state.cancel_and_wait_lifecycles().await.unwrap();
        assert!(stopping.elapsed() < BOUND);
        match RuntimeState::wait_lifecycle_result(result).await.unwrap() {
            DaemonLifecycleResult::Move(outcome) => assert_eq!(outcome.outcome, "interrupted"),
            _ => panic!("a Move lifecycle returns a Move outcome"),
        }
        let deadline = std::time::Instant::now() + BOUND;
        while process_is_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled copy kept running"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!directory.path().join("release").exists());
    }

    /// An in-place Move copies nothing and keeps its admission for its whole
    /// run, so a handoff waits for it and then proceeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_handoff_waits_for_a_fast_path_move() {
        const NAME: &str = "a_handoff_waits_for_a_fast_path_move";
        const CHILD: &str = "MJ_TEST_FAST_MOVE_HANDOFF";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), NAME))
                .env(CHILD, "1")
                .env("MJ_INSTANCE", "fast-move-handoff")
                .isolated_store(root.path())
                .run();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let _release = ReleaseOnDrop(directory.path().join("release"));
        let swap = held_command(directory.path(), "swap the harness in place");
        let state = super::super::tests::test_runtime_state();
        let result = state
            .start_automatic_lifecycle(
                "moving-session".into(),
                LifecycleKind::Move,
                move |state, session_id, control, _session_admission| async move {
                    state
                        .run_move_controller_work(
                            session_id,
                            control,
                            |_, executor, _| async move {
                                executor.execute(&swap)?;
                                Ok(outcome("completed"))
                            },
                        )
                        .await
                        .map(DaemonLifecycleResult::Move)
                },
            )
            .await
            .unwrap();
        wait_for_file(&directory.path().join("pid")).await;
        assert!(
            mj_controller::upgrade::active_labels()
                .iter()
                .any(|label| label == "session lifecycle")
        );
        assert!(matches!(handoff(&state).await, DaemonReply::UpgradePending));
        std::fs::write(directory.path().join("release"), b"").unwrap();
        match RuntimeState::wait_lifecycle_result(result).await.unwrap() {
            DaemonLifecycleResult::Move(outcome) => assert_eq!(outcome.outcome, "completed"),
            _ => panic!("a Move lifecycle returns a Move outcome"),
        }
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            if matches!(handoff(&state).await, DaemonReply::Done) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the finished Move still holds the handoff"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // Hard-won: #1639: committed LifecycleControl::interrupt() set the flag without waking shutdown_stopped().
    #[tokio::test]
    async fn committed_lifecycle_interrupt_wakes_shutdown_stopped() {
        let control = Arc::new(super::super::LifecycleControl::default());
        assert!(control.grant_commit());
        let waiter = {
            let control = control.clone();
            mj_core::runtime::spawn(async move {
                control.shutdown_stopped().await;
            })
        };
        tokio::task::yield_now().await;
        control.interrupt();
        assert!(control.stopped_for_shutdown());
        tokio::time::timeout(BOUND, waiter)
            .await
            .expect("interrupt() must wake shutdown_stopped() for committed lifecycles")
            .unwrap();
    }
}

#[cfg(all(test, unix))]
impl RuntimeState {
    pub(crate) fn move_test_runtime(
        controller: Controller,
        manager: SessionManagerControl,
    ) -> Arc<RuntimeState> {
        let recovery = manager.session_gate.clone();
        let upgrades =
            crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(manager.clone(), &recovery);
        let state = Arc::new(RuntimeState::new_with_controller_loader(
            manager,
            controller,
            recovery.clone(),
            upgrades.observer(),
            Vec::new(),
            Controller::load,
        ));
        state.profile_catalog.get_or_init(|| {
            mj_controller::profile_catalog::ProfileCatalog::new(
                tokio_util::sync::CancellationToken::new(),
            )
        });
        state
    }
}

//! Daemon-owned move admission, supervision, and restart reconciliation.

use super::*;
use mj_core::state::MoveOperation;

pub(super) fn load_controller_for_resume(request: &ResumeSessionRequest) -> Result<Controller> {
    let mut controller = Controller::load()?;
    if let Some(operation) = crate::database::load_move_operation(&request.session_id)?
        && matches!(
            operation.phase,
            mj_core::state::MovePhase::Failed | mj_core::state::MovePhase::Cancelled
        )
        && !operation.queue_admission_started
        && operation.source_profile_id == request.profile_id
        && operation.source_target_template_id == request.target_template_id
    {
        // None in ordinary Resume means inheritance. Restore that inheritance
        // baseline first so a partial conversion cannot change source sizing.
        let record = controller
            .state
            .sessions
            .get_mut(&request.session_id)
            .context("Move recovery session is missing")?;
        record.resource_allocation = operation.source_resource_allocation;
        record.additional_mounts = operation.source_additional_mounts;
        if let Some(previous) = operation.recovery_session {
            record.container_cpus = previous.container_cpus;
            record.container_memory = previous.container_memory;
        }
        crate::database::save_resumed_session(record, None)?;
    }
    Ok(controller)
}

impl RuntimeState {
    pub async fn prepare_move_session(
        self: &Arc<Self>,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        let (source_harness, source_active) = {
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
            )
        };
        let snapshot = if source_active {
            crate::controller::move_session::refresh_move_source(
                &self.session_manager,
                &selection.session_id,
            )
            .await?
        } else {
            None
        };
        let mut preparation = blocking(move || {
            let controller = Controller::load()?;
            mj_core::runtime::block_on(
                controller.prepare_move_session_controlled(selection, &ProcessExecutor),
            )?
        })
        .await?;
        preparation.source_unavailable = source_active
            && snapshot
                .as_ref()
                .is_none_or(|snapshot| !snapshot.operational.native_session_is_ready());
        preparation.active |= preparation.source_unavailable;
        if let Some(snapshot) = snapshot {
            let mut operational = snapshot.operational;
            operational.queued_prompts.clear();
            operational.checkpoint_barrier = None;
            preparation.active |= !operational.safe_to_replace(source_harness);
        }
        Ok(preparation)
    }

    /// Reserve lifecycle ownership before a web request is acknowledged.
    pub(crate) fn start_move_session(self: &Arc<Self>, request: MoveSessionRequest) -> Result<()> {
        self.admit_move_session(request).map(|_| ())
    }

    fn admit_move_session(self: &Arc<Self>, request: MoveSessionRequest) -> Result<LifecycleWatch> {
        let selection = request.preparation.selection.clone();
        let operation_id = request.preparation.operation_id.clone();
        // Preparation resolves inherited settings. Compare those settings,
        // never just the verb or a freshly generated preparation identifier.
        let key =
            serde_json::to_string(&(&selection, request.queue, request.acknowledge_interruption))?;
        let session_id = selection.session_id.clone();
        let result = self.admit_lifecycle(
            session_id.clone(),
            LifecycleKind::Move,
            super::lifecycle::LifecycleStart {
                resume_workspace_id: None,
                request_key: Some(key),
                create_control: None,
                phase: LifecyclePhase::Executing,
                move_operation_id: Some(operation_id.clone()),
            },
            move |state, session_id, cancelled| async move {
                let result = state
                    .clone()
                    .run_move_controller_work(
                        session_id.clone(),
                        cancelled,
                        |mut controller, executor, manager| async move {
                            controller
                                .move_session_managed_controlled(request, &executor, &manager)
                                .await
                        },
                    )
                    .await
                    .map(DaemonLifecycleResult::Move);
                if result.is_err() {
                    state
                        .tell_live_parent_about_stopped_subagents(&session_id)
                        .await;
                }
                result
            },
        )?;
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
        let result = self.admit_move_session(request)?;
        let channel = result.clone();
        let result = Self::wait_lifecycle_result(result).await;
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

    pub(super) fn recover_moves(
        self: &Arc<Self>,
        operations: Vec<MoveOperation>,
    ) -> Result<BTreeSet<String>> {
        let mut owned = BTreeSet::new();
        for operation in &operations {
            crate::controller::move_session::restore_move_queue_hold(operation);
        }
        for operation in operations.into_iter().filter(|op| {
            op.is_active()
                || self
                    .owner()
                    .controller()
                    .state
                    .sessions
                    .get(&op.selection.session_id)
                    .is_some_and(|session| {
                        matches!(
                            session.state,
                            SessionState::Closing | SessionState::Destroying
                        )
                    })
        }) {
            let id = operation.selection.session_id.clone();
            let key = format!("recovery:{}", operation.operation_id);
            let phase = if operation.recovery_session.is_some() {
                LifecyclePhase::MovingDestination
            } else {
                LifecyclePhase::Executing
            };
            let result = self.admit_lifecycle(
                id.clone(),
                LifecycleKind::Move,
                super::lifecycle::LifecycleStart {
                    resume_workspace_id: None,
                    request_key: Some(key),
                    create_control: None,
                    phase,
                    move_operation_id: None,
                },
                move |state, session_id, cancelled| async move {
                    state
                        .run_move_controller_work(
                            session_id,
                            cancelled,
                            |mut controller, executor, manager| async move {
                                controller
                                    .recover_move_managed_controlled(operation, &executor, &manager)
                                    .await
                            },
                        )
                        .await
                        .map(DaemonLifecycleResult::Move)
                },
            )?;
            owned.insert(id.clone());
            let state = self.clone();
            tokio::spawn(async move {
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
        Ok(owned)
    }

    pub(super) fn resume_move_destination_cleanups(self: &Arc<Self>, immediately: bool) {
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
            let result = self.start_or_join_lifecycle(
                id.clone(),
                LifecycleKind::Cleanup,
                |state, id, cancelled| async move {
                    state
                        .run_move_controller_work(
                            id.clone(),
                            cancelled,
                            move |controller, executor, _manager| async move {
                                let mut operation = crate::database::load_move_operation(&id)?
                                    .context("Move cleanup intent missing")?;
                                executor.begin_resumable_move_work()?;
                                let result = controller
                                    .cleanup_prepared_move_destination(&mut operation, &executor);
                                operation.updated_at = chrono::Utc::now().to_rfc3339();
                                if result.is_ok() {
                                    crate::controller::move_session::record_finished_move_recovery(
                                        &controller.state,
                                        &mut operation,
                                    )?;
                                } else {
                                    crate::database::save_move_operation(&operation)?;
                                }
                                executor.end_resumable_move_work()?;
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
            );
            match result {
                Ok(result) => {
                    let state = self.clone();
                    tokio::spawn(async move {
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
    /// (`begin_resumable_move_work`), so a handoff does not wait for a copy,
    /// the closing daemon cancels it, and the next daemon resumes the Move
    /// from its durable phase. An in-place Move copies nothing and never
    /// releases the hold, so a handoff waits for it to finish.
    ///
    /// The Move's controller future holds a standard mutex guard across an
    /// await, so it is not `Send` and cannot be the lifecycle future itself.
    /// It is built and awaited on a blocking-pool thread of its own.
    pub(super) async fn run_move_controller_work<W, Fut>(
        self: Arc<Self>,
        session_id: String,
        cancelled: Arc<AtomicBool>,
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
        tokio::task::spawn_blocking(move || {
            let reserving = std::time::Instant::now();
            let _reservation =
                reserve_recovery_or_cancel(&self.recovery_observer, &session_id, &cancelled)?;
            tracing::info!(
                %session_id,
                phase = "recovery reservation",
                elapsed_ms = reserving.elapsed().as_millis() as u64,
                "move phase finished"
            );
            let loading = std::time::Instant::now();
            let controller = (self.controller_loader)()?;
            tracing::info!(
                %session_id,
                phase = "load controller state",
                elapsed_ms = loading.elapsed().as_millis() as u64,
                "move phase finished"
            );
            let manager = self.session_manager.clone();
            let executor = DaemonStageReportingExecutor::new(
                CancellableProcessExecutor::new(cancelled),
                self,
                session_id,
            );
            mj_core::runtime::block_on(Box::pin(work(controller, executor, manager)))?
        })
        .await
        .context("Move controller task failed")?
    }
}

#[cfg(all(test, unix))]
mod admission_tests {
    use super::*;
    use crate::controller::test_support::{IsolatedTest, test_name};

    const BOUND: Duration = Duration::from_secs(5);

    fn metadata() -> DaemonMetadata {
        DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            token: "right-token".into(),
            started_at: "now".into(),
            build_version: "test".into(),
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

    /// A command that records its process id and waits for `release`.
    fn held_command(directory: &Path, purpose: &str) -> CommandSpec {
        CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                format!(
                    "echo $$ > '{0}/pid'; while [ ! -e '{0}/release' ]; do sleep 0.05; done",
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_handoff_does_not_wait_for_a_slow_path_move_copy() {
        const NAME: &str = "a_handoff_does_not_wait_for_a_slow_path_move_copy";
        const CHILD: &str = "MJ_TEST_SLOW_MOVE_HANDOFF";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), NAME))
                .env(CHILD, "1")
                .env("MJ_INSTANCE", "slow-move-handoff")
                .isolated_store(root.path())
                .run();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let _release = ReleaseOnDrop(directory.path().join("release"));
        let copy = held_command(directory.path(), "copy Move workspace");
        let state = super::super::tests::test_runtime_state();
        let result = state
            .start_or_join_lifecycle(
                "moving-session".into(),
                LifecycleKind::Move,
                move |state, session_id, cancelled| async move {
                    state
                        .run_move_controller_work(
                            session_id,
                            cancelled,
                            |_, executor, _| async move {
                                executor.begin_resumable_move_work()?;
                                let copied = executor.execute(&copy);
                                let resumed = executor.end_resumable_move_work();
                                if copied.is_ok() && resumed.is_ok() {
                                    return Ok(outcome("completed"));
                                }
                                ensure!(
                                    !crate::upgrade::gate().is_open(),
                                    "the copy stopped without a handoff"
                                );
                                Ok(outcome("interrupted"))
                            },
                        )
                        .await
                        .map(DaemonLifecycleResult::Move)
                },
            )
            .unwrap();
        wait_for_file(&directory.path().join("pid")).await;
        let pid: i32 = std::fs::read_to_string(directory.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let labels = crate::upgrade::active_labels();
        assert!(
            !labels
                .iter()
                .any(|label| label == "session lifecycle" || label == "database operation"),
            "the copy holds handoff admission: {labels:?}"
        );
        let deadline = std::time::Instant::now() + BOUND;
        while !matches!(handoff(&state).await, DaemonReply::Done) {
            assert!(
                std::time::Instant::now() < deadline,
                "the handoff waited for the copy: {:?}",
                crate::upgrade::active_labels()
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
            .start_or_join_lifecycle(
                "moving-session".into(),
                LifecycleKind::Move,
                move |state, session_id, cancelled| async move {
                    state
                        .run_move_controller_work(
                            session_id,
                            cancelled,
                            |_, executor, _| async move {
                                executor.execute(&swap)?;
                                Ok(outcome("completed"))
                            },
                        )
                        .await
                        .map(DaemonLifecycleResult::Move)
                },
            )
            .unwrap();
        wait_for_file(&directory.path().join("pid")).await;
        assert!(
            crate::upgrade::active_labels()
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
}

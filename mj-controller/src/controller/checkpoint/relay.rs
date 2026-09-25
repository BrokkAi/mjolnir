use super::*;

impl Controller {
    pub(in crate::controller) async fn prepare_move_source_checkpoint(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        operation: &mut mj_core::state::MoveOperation,
    ) -> Result<()> {
        let snapshot =
            crate::controller::move_session::refresh_move_source(manager, session_id).await?;
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.operational.checkpoint_only)
        {
            operation.source_checkpoint_only = true;
            crate::database::save_move_operation(operation)?;
            return Ok(());
        }
        if snapshot.as_ref().is_some_and(|snapshot| {
            matches!(
                snapshot.operational.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
        }) {
            return Ok(());
        }
        if !operation.source_checkpoint_only
            && snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.native_session_is_ready())
        {
            return Ok(());
        }
        ensure!(
            !executor.cancellation_requested() && !operation.cancellation_requested,
            "Move cancelled before source recovery"
        );
        operation.source_checkpoint_only = true;
        crate::database::save_move_operation(operation)?;
        executor.notify_notice("Recovering source data without starting its old harness");
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        let connection = self
            .restart_worker_with_installed_binary(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: Some(&launch),
                    prepared: false,
                    messages: &RESTART_FOR_CHECKPOINT,
                },
            )
            .await?;
        adopt_restarted_checkpoint_relay(session_id, Some(manager), connection)
            .await?
            .release();
        Ok(())
    }

    /// Reach the session worker for a checkpoint, restarting it when the proxy
    /// cannot complete hello. A previous Stop can leave the daemon dead; failing
    /// that first connect without a bounce never gets to the barrier retry.
    pub(super) async fn open_checkpoint_relay(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        target: InstalledWorkerRestart<'_>,
        restart_if_unreachable: bool,
    ) -> Result<(ControllerRelayLease, bool)> {
        let project_memory = match self.project_memory_sync_target(session_id) {
            Ok(target) => Some(target),
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "project memory will not be synchronized during checkpoint reconnect"
                );
                None
            }
        };
        match connect_checkpoint_relay(
            session_id,
            manager,
            target.reconnect,
            project_memory.clone(),
        )
        .await
        {
            Ok(relay) => Ok((relay, false)),
            Err(error) if worker_connect_needs_restart(&error) && restart_if_unreachable => {
                tracing::warn!(
                    session_id,
                    "checkpoint could not reach the worker; restarting it: {error:#}"
                );
                let mut connection = self
                    .restart_worker_for_checkpoint(
                        session_id,
                        executor,
                        target.backend,
                        target.worker_root,
                        target.reconnect,
                    )
                    .await?;
                connection.set_project_memory_target(project_memory);
                let relay =
                    adopt_restarted_checkpoint_relay(session_id, manager, connection).await?;
                Ok((relay, true))
            }
            Err(error) if worker_connect_needs_restart(&error) => {
                Err(error.context(CheckpointDeferred::background_work()))
            }
            Err(error) => Err(error).context("connect to the session worker for checkpoint"),
        }
    }

    /// Kill a worker whose ACP turn will not finish, install the current
    /// binary, and reconnect. Restart recovery interrupts the in-flight prompt
    /// so a later BeginCheckpoint can be admitted.
    /// Start the worker again without its harness, so a checkpoint can be
    /// taken from the relay journal and the files on the target when the
    /// harness does not come back (R4-3). The same mode a Move uses to
    /// recover a source whose harness cannot start.
    pub(super) async fn restart_worker_for_checkpoint_only(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        backend: &targets::TargetLocator,
        worker_root: &str,
        reconnect: &targets::CommandSpec,
    ) -> Result<StandaloneSession> {
        let mut launch = self.current_worker_launch_config(session_id, backend)?;
        launch.run_mode = mj_core::worker_launch::WorkerRunMode::CheckpointOnly;
        self.restart_worker_with_installed_binary(
            session_id,
            executor,
            InstalledWorkerRestart {
                backend,
                worker_root,
                reconnect,
                launch: Some(&launch),
                prepared: false,
                messages: &RESTART_FOR_CHECKPOINT,
            },
        )
        .await
    }

    pub(super) async fn restart_worker_for_checkpoint(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        backend: &targets::TargetLocator,
        worker_root: &str,
        reconnect: &targets::CommandSpec,
    ) -> Result<StandaloneSession> {
        self.restart_worker_with_installed_binary(
            session_id,
            executor,
            InstalledWorkerRestart {
                backend,
                worker_root,
                reconnect,
                launch: None,
                prepared: false,
                messages: &RESTART_FOR_CHECKPOINT,
            },
        )
        .await
    }
}

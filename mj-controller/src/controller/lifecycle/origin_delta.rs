use super::*;
use crate::targets::CommandSpec;
use mj_core::worker_launch::WorkerRunMode;
use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest};

pub(super) struct OriginDeltaSuspend {
    backend: targets::TargetLocator,
    root: PathBuf,
    checkout: PathBuf,
    child_sessions: Vec<String>,
    operation: String,
    request_recorded: bool,
    accepted: bool,
}

impl OriginDeltaSuspend {
    pub(super) fn load(controller: &Controller, session_id: &str) -> Result<Self> {
        let checkout = controller.state.checkout(session_id)?;
        let checkout = checkout
            .managed_worktree()
            .context("managed clone is missing")?;
        let (backend, root) = controller.worker_placement(session_id)?;
        let root = PathBuf::from(root);
        let accepted = wire::read::<String>(&root, wire::ACCEPTED)?;
        let request = wire::read::<SuspendRequest>(&root, wire::REQUEST)?;
        let request_recorded = accepted.as_ref().is_some_and(|operation| {
            request
                .as_ref()
                .is_some_and(|request| &request.operation == operation)
        });
        Ok(Self {
            backend,
            root,
            checkout: checkout.worktree_root.clone(),
            child_sessions: controller.state.active_child_session_ids(session_id),
            request_recorded,
            accepted: accepted.is_some(),
            operation: match accepted {
                Some(operation) => operation,
                None => new_command_id("origin-delta-suspend")?,
            },
        })
    }

    pub(super) fn accepted(&self) -> bool {
        self.accepted
    }

    pub(super) fn accept(&mut self) -> Result<()> {
        self.operation = new_command_id("origin-delta-suspend")?;
        wire::write(&self.root, wire::ACCEPTED, &self.operation)?;
        self.request_recorded = false;
        self.accepted = true;
        Ok(())
    }

    pub(super) fn request_recorded(&self) -> bool {
        self.request_recorded
    }

    pub(super) fn restart_reason(&self) -> Result<Option<String>> {
        Ok(wire::read::<SuspendReceipt>(&self.root, wire::RESTART)?
            .filter(|receipt| receipt.operation == self.operation)
            .and_then(|receipt| receipt.error))
    }

    pub(super) fn record_restart(&self, error: &anyhow::Error) -> Result<()> {
        wire::write(
            &self.root,
            wire::RESTART,
            &SuspendReceipt {
                operation: self.operation.clone(),
                error: Some(format!("{error:#}")),
            },
        )
    }

    pub(super) fn clear_after_restart(&self) -> Result<()> {
        for name in [
            wire::ACCEPTED,
            wire::REQUEST,
            wire::RECEIPT,
            wire::PROGRESS,
            wire::RESTART,
        ] {
            match std::fs::remove_file(self.root.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove suspend record {name}"));
                }
            }
        }
        Ok(())
    }

    fn receipt(&self) -> Result<Option<SuspendReceipt>> {
        Ok(wire::read::<SuspendReceipt>(&self.root, wire::RECEIPT)?
            .filter(|receipt| receipt.operation == self.operation))
    }

    pub(super) fn request_stop(&mut self, executor: &(impl CommandExecutor + Sync)) -> Result<()> {
        if self.receipt()?.is_some() {
            return Ok(());
        }
        if let Some(request) = wire::read::<SuspendRequest>(&self.root, wire::REQUEST)?
            && request.operation == self.operation
            && !request.worker.is_alive()?
        {
            return Ok(());
        }
        let mut args = vec![
            "worker".to_owned(),
            "quiesce".to_owned(),
            "--root".to_owned(),
            self.root.to_string_lossy().into_owned(),
            "--operation".to_owned(),
            self.operation.clone(),
            "--checkout".to_owned(),
            self.checkout.to_string_lossy().into_owned(),
        ];
        for id in &self.child_sessions {
            args.extend(["--child-session".to_owned(), id.clone()]);
        }
        let command = CommandSpec::new(self.root.join("hel").to_string_lossy(), args)
            .purpose("request managed clone process quiescence");
        mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))?;
        self.request_recorded = true;
        Ok(())
    }

    pub(super) async fn wait_for_stop(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<SuspendReceipt> {
        executor.begin_resumable_work()?;
        let outcome = async {
            let worker = wire::read::<SuspendRequest>(&self.root, wire::REQUEST)?
                .context("worker did not record the suspend request")?
                .worker;
            let mut cancellation_recorded = self.restart_reason()?.is_some();
            let mut displayed_notice = String::new();
            loop {
                ensure!(
                    !executor.shutdown_interrupted(),
                    "suspend process wait was handed off"
                );
                if executor.cancellation_requested() && !cancellation_recorded {
                    self.record_restart(&anyhow::anyhow!("suspend was cancelled"))?;
                    cancellation_recorded = true;
                }
                // A receipt can precede the worker's last cleanup tasks.
                if !worker.is_alive()? {
                    return Ok(self.receipt()?.unwrap_or_else(|| SuspendReceipt {
                        operation: self.operation.clone(),
                        error: Some(
                            "the worker exited without confirming checkout quiescence".to_owned(),
                        ),
                    }));
                }
                let mut notice = if cancellation_recorded {
                    "Restarting after the previous agent exits".to_owned()
                } else {
                    "Waiting for the session's processes to stop".to_owned()
                };
                if let Some(progress) =
                    wire::read::<wire::SuspendProgress>(&self.root, wire::PROGRESS)?
                    && progress.operation == self.operation
                    && !progress.remaining_pids.is_empty()
                {
                    let pids = progress
                        .remaining_pids
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>();
                    notice.push_str(&format!(" (PIDs {})", pids.join(", ")));
                }
                if displayed_notice != notice {
                    executor.notify_notice(&notice);
                    displayed_notice = notice;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        .await;
        executor.end_resumable_work("record suspend quiescence outcome")?;
        outcome
    }

    pub(super) async fn start_worker(
        &self,
        controller: &Controller,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        run_mode: WorkerRunMode,
    ) -> Result<super::super::checkpoint::ControllerRelayLease> {
        use super::super::checkpoint::adopt_restarted_checkpoint_relay;
        use super::super::worker_restart::{InstalledWorkerRestart, RESTART_FOR_CHECKPOINT};

        let reconnect = targets::reconnect_plan(&self.backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is missing")?;
        let mut launch = controller.current_worker_launch_config(session_id, &self.backend)?;
        launch.run_mode = run_mode;
        let worker_root = self.root.to_str().context("worker root is not UTF-8")?;
        let mut restart = InstalledWorkerRestart {
            backend: &self.backend,
            worker_root,
            reconnect: &reconnect,
            launch: Some(&launch),
            prepared: false,
            messages: &RESTART_FOR_CHECKPOINT,
            swap_admission: None,
            discard_queued_prompts: None,
        };
        // Quiescence proved the original worker exited. If a worker has since
        // booted in the requested mode, observe it through the existing launch
        // owner, including when the daemon exited before recording readiness.
        let installed =
            mj_core::worker_launch::WorkerLaunchConfig::read(&self.root.join("launch.json"))?;
        let same_mode = installed.run_mode == run_mode;
        let existing = mj_core::runtime::off_async_worker(|| {
            super::super::worker_binary::probe_worker_liveness(
                executor,
                &self.backend,
                session_id,
                worker_root,
            )
        })?;
        ensure!(
            existing.status == 0,
            "cannot verify the retained worker's liveness"
        );
        let connection =
            if same_mode && matches!(existing.stdout.as_slice(), b"alive\n" | b"starting\n") {
                let pending = crate::database::load_worker_restart(session_id)?;
                executor.begin_resumable_work()?;
                let result = controller
                    .observe_started_worker(session_id, executor, restart)
                    .await;
                executor.end_resumable_work("reattach to the retained suspend worker")?;
                let connection = result?;
                if let Some(pending) = pending {
                    crate::database::finish_worker_restart(session_id, &pending.operation_id)?;
                }
                connection
            } else {
                restart.swap_admission = Some(crate::upgrade::activity_unless_draining(
                    "start managed clone suspend worker",
                )?);
                let result = controller
                    .restart_worker_with_installed_binary(session_id, executor, restart)
                    .await;
                executor.end_resumable_work("record managed clone suspend worker readiness")?;
                result?
            };
        adopt_restarted_checkpoint_relay(admission, session_id, manager, connection).await
    }
}

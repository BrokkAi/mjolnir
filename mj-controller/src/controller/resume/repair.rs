//! Resume never restores an archive over a retained environment.
use super::*;
use crate::controller::worker_binary::{WorkerInstall, prepare_managed_harness_for_upgrade};
use crate::controller::worker_restart::{InstalledWorkerRestart, WorkerRestartMessages};

const REPAIR: WorkerRestartMessages = WorkerRestartMessages {
    stop: "stop the failed session worker for repair",
    replace: "install the worker for repair",
    start: "start the repaired session worker",
    connect: "reconnect the retained session journal",
    project_memory: "project memory sync unavailable after repair",
    native_session: "wait for the retained native session",
};

impl Controller {
    pub(super) async fn repair_existing_environment(
        &mut self,
        session_id: &str,
        live_worker: bool,
        discard_queue: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        executor.notify_notice(
            "Resuming in the existing environment; current files and sub-agents are kept.",
        );
        let result = async {
            // Preparation and journal replay can resume from this retained
            // placement after daemon handoff; they must not hold admission.
            executor.begin_resumable_work()?;
            let (backend, root) = self.worker_placement(session_id)?;
            let reconnect = targets::reconnect_plan(&backend, session_id)?
                .commands
                .into_iter()
                .next()
                .context("missing reconnect command")?;
            let attached = if live_worker {
                // A lost handshake cannot prove death. Only the worker's terminal
                // preparation failure permits replacing an otherwise live worker:
                // that boot has no coordinator and can never dispatch ACP work.
                match self
                    .observe_started_worker(
                        session_id,
                        executor,
                        InstalledWorkerRestart {
                            backend: &backend,
                            worker_root: &root,
                            reconnect: &reconnect,
                            launch: None,
                            prepared: true,
                            messages: &REPAIR,
                        },
                    )
                    .await
                {
                    Ok(relay) => Some(relay),
                    Err(error)
                        if error
                            .downcast_ref::<super::super::readiness::HarnessPreparationFailure>()
                            .is_some() =>
                    {
                        executor.notify_notice(&format!(
                            "The existing worker's harness preparation failed: {}. Restarting that worker in the same environment.",
                            super::super::worker_binary::failure_line(&error)
                        ));
                        if let Some(intent) = crate::database::load_worker_restart(session_id)?
                            && self.state.sessions[session_id].target.as_deref()
                                == Some(&intent.target)
                        {
                            crate::database::finish_worker_restart(
                                session_id,
                                &intent.operation_id,
                            )?;
                        }
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let mut relay = if let Some(relay) = attached {
                relay
            } else {
                let launch = self.current_worker_launch_config(session_id, &backend)?;
                let worker = WorkerInstall::for_target(&backend, executor)?;
                prepare_managed_harness_for_upgrade(
                    executor, &backend, session_id, &worker, &launch,
                )?;
                self.restart_worker_with_installed_binary(
                    session_id,
                    executor,
                    InstalledWorkerRestart {
                        backend: &backend,
                        worker_root: &root,
                        reconnect: &reconnect,
                        launch: Some(&launch),
                        prepared: false,
                        messages: &REPAIR,
                    },
                )
                .await?
            };
            executor.end_resumable_work("resume repair publication")?;
            if discard_queue {
                relay
                    .submit(
                        new_command_id("resume-clear-queue")?,
                        RelayCommand::ClearQueuedPrompts,
                    )
                    .await?;
            }
            let snapshot = relay.sync().await?;
            self.mark_worker_connected(
                session_id,
                relay.snapshot().operational.native_session_id.clone(),
            )?;
            if let Some(intent) = crate::database::load_worker_restart(session_id)?
                && self.state.sessions[session_id].target.as_deref() == Some(&intent.target)
            {
                crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
            }
            Ok(snapshot.materialized)
        }
        .await;
        if let Err(error) = &result {
            let record = self
                .state
                .sessions
                .get_mut(session_id)
                .context("repair session disappeared")?;
            record.state = SessionState::Error;
            record.updated_at = now();
            record.last_error = Some(format!(
                "Resume repair failed: {error:#}. The environment was kept. Retry Resume when the target is reachable."
            ));
            crate::database::save_lifecycle_session(record)?;
        }
        result
    }
}

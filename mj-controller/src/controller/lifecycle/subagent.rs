//! A child owns its conversation and worker, while its parent owns the workspace.

use super::*;
use crate::controller::checkpoint::{
    BarrierBusyPolicy, CHECKPOINT_BARRIER_TIMEOUT, connect_checkpoint_relay,
    wait_for_checkpoint_barrier,
};

impl Controller {
    /// Persist the child's final relay events before removing its private
    /// worker state. No repository or native-session archive is needed: a
    /// closed child is not resumed independently of its parent.
    pub(crate) async fn suspend_subagent_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        before_close: Option<BeforeClose>,
    ) -> Result<bool> {
        crate::worker_lifecycle::run(session_id, "close sub-agent", executor, async {
            crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
            ensure!(
                self.state.subagents.contains_key(session_id),
                "session {session_id} is not a sub-agent"
            );
            let previous = self
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?
                .clone();
            let live = previous.target.is_some()
                && matches!(
                    previous.state,
                    SessionState::Running
                        | SessionState::Disconnected
                        | SessionState::Checkpointing
                        | SessionState::Closing
                );
            let mut relay = if live {
                let record = self.state.sessions.get_mut(session_id).unwrap();
                apply_close_checkpoint_started(record, now());
                self.persist_session_transition_or_restore(
                    session_id,
                    &previous,
                    "persist child close intent before synchronizing its conversation",
                )?;
                let backend =
                    backend_locator(previous.target.as_ref().unwrap(), &previous, &self.config)?;
                let reconnect = targets::reconnect_plan(&backend, session_id)?
                    .commands
                    .into_iter()
                    .next()
                    .context("reconnect plan is empty")?;
                Some(connect_checkpoint_relay(session_id, manager, &reconnect, None).await?)
            } else {
                None
            };
            let mut barrier = None;
            if let Some(relay) = &mut relay {
                let snapshot = relay.connection_mut().sync().await?;
                if !matches!(
                    snapshot.operational.execution,
                    RelayExecutionState::Closing | RelayExecutionState::Closed
                ) {
                    let command_id = if let Some(existing) = snapshot.operational.checkpoint_barrier
                    {
                        existing
                    } else {
                        let id = new_command_id("child-close-barrier")?;
                        relay
                            .connection_mut()
                            .submit(
                                id.clone(),
                                RelayCommand::BeginCheckpoint {
                                    reason: Some(
                                        "close sub-agent without archiving its parent's workspace"
                                            .into(),
                                    ),
                                },
                            )
                            .await?;
                        id
                    };
                    let ready = wait_for_checkpoint_barrier(
                        relay.connection_mut(),
                        session_id,
                        &command_id,
                        CHECKPOINT_BARRIER_TIMEOUT,
                        BarrierBusyPolicy::InterruptWhileRunning,
                        previous.harness_kind,
                    )
                    .await?;
                    let cursor = ready
                        .operational
                        .checkpoint_ready
                        .context("sub-agent close barrier has no ready cursor")?;
                    barrier = Some((command_id, cursor));
                }
            }
            if let Some(before_close) = before_close
                && let Err(error) = before_close.await
            {
                if barrier.is_some() {
                    if let Some(relay) = &mut relay {
                        relay.cancel_abandoned_barrier().await.context(
                            "release the barrier of a sub-agent whose close did not commit",
                        )?;
                    }
                    let closing = self.state.sessions[session_id].clone();
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = state_after_unsealed_close(&previous);
                    record.updated_at = now();
                    self.persist_session_transition_or_restore(
                        session_id,
                        &closing,
                        "restore a sub-agent whose close did not commit",
                    )?;
                }
                return Err(error);
            }
            if let Some(relay) = &mut relay {
                if let Some((command_id, cursor)) = barrier {
                    relay
                        .connection_mut()
                        .submit(
                            new_command_id("child-close")?,
                            RelayCommand::Close {
                                barrier_command_id: command_id,
                                expected: cursor,
                            },
                        )
                        .await?;
                }
                // A retry can find an accepted Close whose barrier was not
                // released before the previous controller disappeared.
                let snapshot = relay.connection_mut().sync().await?;
                if let Some(barrier_command_id) = snapshot.operational.checkpoint_barrier {
                    relay
                        .connection_mut()
                        .submit(
                            new_command_id("child-close-complete")?,
                            RelayCommand::CompleteCheckpoint { barrier_command_id },
                        )
                        .await?;
                }
                wait_for_relay_closed(relay.connection_mut()).await?;
            }
            // Destroying means the final conversation is durable. Recovery
            // can finish cleanup even if the worker or its files are gone.
            let before_teardown = self.state.sessions[session_id].clone();
            let record = self.state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Destroying;
            record.updated_at = now();
            self.persist_session_transition_or_restore(
                session_id,
                &before_teardown,
                "persist child teardown after synchronizing its final conversation",
            )?;
            let session = self.state.sessions[session_id].clone();
            let outcome = self.stop_target_and_settle(
                session_id,
                &session,
                executor,
                &crate::database::save_lifecycle_session,
            );
            if let Some(relay) = relay {
                relay.release();
            }
            outcome
        })
        .await
    }
}

//! Session-only handoff for a Move that owns the existing workspace separately.
use super::*;
use crate::controller::checkpoint::{
    CheckpointExportPolicy, LatchExclusivity, wait_for_relay_closed,
};
use mj_checkpoint::checkpoint::CheckpointRepositoryCapture;

impl Controller {
    pub(in crate::controller) async fn seal_move_handoff(
        &mut self,
        id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        operation: &mut MoveOperation,
        preparation: Option<&MovePreparation>,
        source_relay: Option<super::super::checkpoint::ControllerRelayLease>,
    ) -> Result<()> {
        let previous = self.state.sessions[id].clone();
        let mut layout = self.session_export_layout(id, executor)?;
        for repository in &mut layout.repositories {
            repository.capture = CheckpointRepositoryCapture::MetadataOnly;
        }
        executor.notify_notice("Saving session handoff; workspace files stay outside the handoff");
        let mut latched = Box::pin(self.checkpoint_session_latched_with_layout(
            id,
            executor,
            Some(manager),
            LatchExclusivity::HoldThroughClose,
            CheckpointExportPolicy::Always,
            false,
            Some(&operation.operation_id),
            layout,
            source_relay,
        ))
        .await?;
        operation.handoff = Some(latched.artifact.metadata.clone());
        operation.checkpoint = previous.checkpoint.clone();
        operation.updated_at = now();
        crate::database::save_move_operation(operation)?;
        if let Err(error) = self.validate_move_checkpoint(operation, preparation, executor) {
            latched.relay.cancel_abandoned_barrier().await?;
            return Err(error);
        }
        // The Move owns this artifact. In particular, do not replace the last
        // full recovery checkpoint or advance its projection-retention floor.
        let record = self.state.sessions.get_mut(id).unwrap();
        record.state = SessionState::Closing;
        record.native_session_id = Some(latched.artifact.native_session_id.clone());
        record.updated_at = now();
        crate::database::save_lifecycle_session(record)?;
        let result = async {
            let barrier = latched.barrier_command_id.clone();
            latched
                .relay
                .connection_mut()
                .submit(
                    new_command_id("move-seal")?,
                    RelayCommand::Close {
                        barrier_command_id: barrier.clone(),
                        expected: latched.cursor.clone(),
                    },
                )
                .await?;
            latched
                .relay
                .connection_mut()
                .submit(
                    new_command_id("move-seal-complete")?,
                    RelayCommand::CompleteCheckpoint {
                        barrier_command_id: barrier,
                    },
                )
                .await?;
            wait_for_relay_closed(latched.relay.connection_mut()).await
        }
        .await;
        latched.relay.release();
        if let Err(error) = result {
            // Closing is durable: recovery asks the relay whether sealing was
            // accepted instead of guessing from a missing acknowledgement.
            self.state.sessions.get_mut(id).unwrap().last_error = Some(format!("{error:#}"));
            crate::database::save_lifecycle_session(&self.state.sessions[id])?;
            return Err(error);
        }
        let mut sealed = previous;
        sealed.state = SessionState::Closing;
        operation.recovery_session = Some(sealed);
        crate::database::save_move_operation(operation)?;
        Ok(())
    }
}

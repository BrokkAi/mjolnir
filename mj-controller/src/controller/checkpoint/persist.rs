use super::*;

impl Controller {
    pub(in crate::controller) fn persist_checkpoint_transition_or_restore(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        context: &'static str,
    ) -> Result<()> {
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            previous,
            context,
            &crate::database::save_checkpointed_session,
        )
    }

    pub(in crate::controller) fn persist_failed_checkpoint_state_or_restore(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        primary: anyhow::Error,
    ) -> anyhow::Error {
        match self.persist_session_state(session_id) {
            Ok(()) => primary,
            Err(error) => self.restore_prior_session_after_persistence_failure(
                session_id,
                previous,
                primary.context(format!(
                    "failed to persist the checkpoint rollback state: {error:#}"
                )),
            ),
        }
    }

    /// Capture an archive for bundle export while the target remains live.
    pub async fn checkpoint_for_bundle_export(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
    ) -> Result<CheckpointMetadata> {
        self.checkpoint_for_bundle_export_controlled(admission, session_id, &ProcessExecutor)
            .await
    }

    pub(super) async fn checkpoint_for_bundle_export_controlled(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointMetadata> {
        self.checkpoint_for_bundle_export_with_manager(admission, session_id, executor, None)
            .await
    }
}

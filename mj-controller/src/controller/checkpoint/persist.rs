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
}

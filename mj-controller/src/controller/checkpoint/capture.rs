use super::*;

impl Controller {
    pub(super) async fn checkpoint_session_controlled_with_manager(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
    ) -> Result<CheckpointMetadata> {
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        previous.validate_configuration(&self.config)?;
        ensure!(
            !matches!(
                previous.state,
                SessionState::Closing | SessionState::Destroying
            ),
            "session {session_id} is already closing; resume that close instead of starting an ordinary checkpoint"
        );
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Checkpointing;
        record.updated_at = now();
        record.last_checkpoint_error = None;
        self.persist_session_transition_or_restore(
            session_id,
            &previous,
            "persist checkpointing state before creating a checkpoint",
        )?;

        match self
            .checkpoint_session_latched(
                session_id,
                executor,
                manager,
                LatchExclusivity::ReleaseAfterLatch,
                CheckpointExportPolicy::Always,
            )
            .await
        {
            Ok(latched) => {
                let artifact = latched.artifact.clone();
                if let Err(error) = mj_core::test_hooks::reach_test_hook(
                    "checkpoint_archive_before_database_publication",
                ) {
                    latched.abandon(session_id).await;
                    return Err(remove_uninstalled_checkpoint(
                        &artifact.metadata.archive_path,
                        error,
                    ));
                }
                {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::Running;
                    record.native_session_id = Some(artifact.native_session_id.clone());
                    record.checkpoint = Some(artifact.metadata.clone());
                    record.updated_at = now();
                    record.last_error = None;
                    record.last_checkpoint_error = None;
                }
                let persist_started = Instant::now();
                if let Err(error) = self.persist_checkpoint_transition_or_restore(
                    session_id,
                    &previous,
                    "persist verified checkpoint before releasing relay history",
                ) {
                    latched.abandon(session_id).await;
                    return Err(error);
                }
                tracing::info!(
                    session_id,
                    persist_ms = persist_started.elapsed().as_millis() as u64,
                    "checkpoint metadata persisted"
                );
                prune_replaced_checkpoint(previous.checkpoint.as_ref(), &artifact.metadata);
                release_projection_behind_checkpoint(session_id, &artifact.metadata);
                if let Err(error) = latched.complete().await {
                    // Only journal retention is at stake. A barrier that is
                    // still open cannot dangle: the actor retries a failed
                    // submission over a fresh connection, and the worker
                    // cancels barriers whose submitting connection dropped.
                    // The next checkpoint moves the recovery floor again.
                    tracing::warn!(
                        session_id,
                        "verified checkpoint was saved, but the relay could not be told to release the history it covers: {error:#}"
                    );
                }
                Ok(artifact.metadata)
            }
            Err(error) => {
                // A deferred checkpoint says the agent was working, not that
                // anything failed. Recording it would leave a warning on the
                // session row until the next successful copy, so the caller is
                // told and the row is left alone.
                let deferred = checkpoint_was_deferred(&error);
                if let Some(record) = self.state.sessions.get_mut(session_id) {
                    record.state = if previous.state == SessionState::Checkpointing {
                        SessionState::Running
                    } else {
                        previous.state
                    };
                    record.updated_at = now();
                    if !deferred {
                        record.last_checkpoint_error = Some(format!("{error:#}"));
                    }
                }
                Err(self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error))
            }
        }
    }

    /// Create, checksum, and durably install a recovery archive before
    /// allowing the relay to garbage-collect through its event frontier.
    pub async fn create_recovery_checkpoint_managed_controlled(
        &self,
        session_id: &str,
        manager: &SessionManagerControl,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointArtifact> {
        self.create_recovery_checkpoint_with_manager(session_id, Some(manager), executor)
            .await
    }

    pub(super) async fn create_recovery_checkpoint_with_manager(
        &self,
        session_id: &str,
        manager: Option<&SessionManagerControl>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointArtifact> {
        let previous_checkpoint = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .checkpoint
            .clone();
        let latched = self
            .checkpoint_session_latched_with_recovery_stage(
                session_id,
                executor,
                manager,
                LatchExclusivity::ReleaseAfterLatch,
                CheckpointExportPolicy::Always,
                true,
            )
            .await?;
        let artifact = latched.artifact.clone();
        let verification = {
            let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            verify_checkpoint_artifact(session_id, &artifact)
        };
        if let Err(error) = verification {
            latched.abandon(session_id).await;
            return Err(remove_uninstalled_checkpoint(
                &artifact.metadata.archive_path,
                error.context("final recovery checkpoint verification"),
            ));
        }
        if let Err(error) =
            mj_core::test_hooks::reach_test_hook("checkpoint_archive_before_database_publication")
        {
            latched.abandon(session_id).await;
            return Err(remove_uninstalled_checkpoint(
                &artifact.metadata.archive_path,
                error,
            ));
        }
        let persist_started = Instant::now();
        if let Err(error) = crate::database::record_recovery_success(
            session_id,
            &artifact.native_session_id,
            &artifact.metadata,
        ) {
            latched.abandon(session_id).await;
            return Err(error
                .context("persist verified recovery checkpoint before releasing relay history"));
        }
        tracing::info!(
            session_id,
            persist_ms = persist_started.elapsed().as_millis() as u64,
            "recovery checkpoint metadata persisted"
        );
        if let Err(error) = latched.complete().await {
            // Only journal retention is at stake. A barrier that is still open
            // cannot dangle: the actor retries a failed submission over a fresh
            // connection, and the worker cancels barriers whose submitting
            // connection dropped. The next checkpoint moves the floor again.
            tracing::warn!(
                session_id,
                "recovery checkpoint was saved, but the relay could not be told to release the history it covers: {error:#}"
            );
        }
        prune_replaced_checkpoint(previous_checkpoint.as_ref(), &artifact.metadata);
        release_projection_behind_checkpoint(session_id, &artifact.metadata);
        Ok(artifact)
    }
}

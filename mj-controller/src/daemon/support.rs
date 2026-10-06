use super::*;

/// Log one finished worker upgrade, and tell the surfaces about the one that
/// changed something.
pub(super) async fn report_worker_upgrade(
    state: &RuntimeState,
    result: &crate::worker_upgrade::WorkerUpgradeResult,
) {
    use crate::controller::WorkerUpgradeOutcome;

    let session_id = &result.session_id;
    if result.cancelled {
        tracing::debug!(%session_id, "worker upgrade was preempted");
        return;
    }
    match &result.outcome {
        Ok(WorkerUpgradeOutcome::Upgraded { build }) => {
            tracing::info!(%session_id, %build, "replaced the session worker with the current build");
            let name = state
                .owner()
                .controller()
                .state
                .sessions
                .get(session_id)
                .map_or_else(
                    || session_id.clone(),
                    |session| session.display_title().to_owned(),
                );
            state.push_notice(session_id, format!("Upgraded the worker for {name}."));
        }
        Ok(WorkerUpgradeOutcome::AlreadyCurrent { build }) => {
            tracing::debug!(%session_id, %build, "session worker already runs the current build");
        }
        Ok(WorkerUpgradeOutcome::Deferred) => {
            tracing::debug!(%session_id, "worker upgrade deferred: the session is working");
        }
        Err(error) => {
            tracing::warn!(%session_id, %error, "could not upgrade the session worker");
            if let Some(failure) = &result.preparation_failure {
                state
                    .fail_harness_preparation(
                        session_id.clone(),
                        failure.clone(),
                        result.observed_updated_at.clone(),
                    )
                    .await;
            }
        }
    }
}

/// Children of `parent_session_id` whose session is still active, in the
/// order they should be stopped before the parent. A child that already
/// stopped needs nothing and would make `force_stop` fail on it.
pub(super) fn active_child_session_ids(
    state: &mj_core::state::State,
    parent_session_id: &str,
) -> Vec<String> {
    state
        .subagents
        .values()
        .filter(|child| child.parent_session_id == parent_session_id)
        .filter(|child| {
            state
                .sessions
                .get(&child.child_session_id)
                .is_some_and(|session| session.state.is_active())
        })
        .map(|child| child.child_session_id.clone())
        .collect()
}

pub(super) struct DaemonStageReportingExecutor<E> {
    pub(super) inner: E,
    pub(super) state: Arc<RuntimeState>,
    pub(super) session_id: String,
    operation_id: Option<String>,
}

impl<E> DaemonStageReportingExecutor<E> {
    pub(super) fn new(inner: E, state: Arc<RuntimeState>, session_id: String) -> Self {
        let operation_id = state
            .owner()
            .lifecycle
            .get(&session_id)
            .map(|active| active.operation_id.clone());
        Self {
            inner,
            state,
            session_id,
            operation_id,
        }
    }
}

impl<E: CommandExecutor> CommandExecutor for DaemonStageReportingExecutor<E> {
    fn execute_cleanup(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .as_ref()
            .map(|stage| ProvisionStageGuard::new(self, stage.clone()));
        self.inner.execute_cleanup(command)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .as_ref()
            .map(|stage| ProvisionStageGuard::new(self, stage.clone()));
        let started = std::time::Instant::now();
        let result = self.inner.execute(command);
        tracing::info!(
            session_id = %self.session_id,
            stage = command
                .stage
                .as_ref()
                .map(ProvisionStage::label)
                .unwrap_or_else(|| "command".to_owned()),
            purpose = %command.purpose,
            duration_ms = started.elapsed().as_millis(),
            succeeded = result.as_ref().is_ok_and(|output| output.status == 0),
            "session command stage finished"
        );
        result
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .as_ref()
            .map(|stage| ProvisionStageGuard::new(self, stage.clone()));
        let started = std::time::Instant::now();
        let result = self.inner.execute_with_stdin(command, input);
        tracing::info!(
            session_id = %self.session_id,
            stage = command
                .stage
                .as_ref()
                .map(ProvisionStage::label)
                .unwrap_or_else(|| "command".to_owned()),
            purpose = %command.purpose,
            duration_ms = started.elapsed().as_millis(),
            succeeded = result.as_ref().is_ok_and(|output| output.status == 0),
            "session streaming command stage finished"
        );
        result
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        if let Some(id) = &self.operation_id {
            self.state
                .change_lifecycle_stage(&self.session_id, id, stage, true);
        }
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        if let Some(id) = &self.operation_id {
            self.state
                .change_lifecycle_stage(&self.session_id, id, stage, false);
        }
    }

    fn begin_resumable_move_work(&self) -> Result<()> {
        let owner = self.state.owner();
        let active = owner
            .lifecycle
            .get(&self.session_id)
            .context("Move lifecycle missing")?;
        ensure!(
            self.operation_id.as_ref() == Some(&active.operation_id),
            "Move lifecycle changed"
        );
        active
            .upgrade_work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(())
    }

    fn end_resumable_move_work(&self) -> Result<()> {
        let work = crate::upgrade::activity("Move control transition")?;
        let owner = self.state.owner();
        let active = owner
            .lifecycle
            .get(&self.session_id)
            .context("Move lifecycle missing")?;
        ensure!(
            self.operation_id.as_ref() == Some(&active.operation_id),
            "Move lifecycle changed"
        );
        *active
            .upgrade_work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(work);
        Ok(())
    }

    fn before_move_source_stop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(self.state.stop_subagents_for_suspend(&self.session_id))
    }

    fn reserve_move_destination(&self) {
        if let Some(id) = &self.operation_id {
            self.state.reserve_move_destination(&self.session_id, id);
        }
    }

    fn notify_notice(&self, notice: &str) {
        if let Some(id) = &self.operation_id {
            self.state
                .set_lifecycle_notice(&self.session_id, id, notice);
        }
    }
}

pub(super) fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn random_hex<const N: usize>() -> Result<String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("generate daemon secret: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn write_metadata(path: &Path, metadata: &DaemonMetadata) -> Result<()> {
    let parent = path
        .parent()
        .context("daemon metadata path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create daemon data directory {}", parent.display()))?;
    let temporary = parent.join(format!(".daemon.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(metadata)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(&body)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
        .with_context(|| format!("publish daemon metadata {}", path.display()))?;
    Ok(())
}

use super::*;

/// Publication is deliberately synchronous: I/O settles in the coordinators.
pub(super) fn publish_background_results(
    state: &Arc<RuntimeState>,
    recovery: impl Iterator<Item = crate::recovery::RecoveryResult>,
    upgrades: impl Iterator<Item = crate::worker_upgrade::WorkerUpgradeResult>,
) {
    for result in recovery {
        report_recovery(&result);
    }
    for result in upgrades {
        report_worker_upgrade(state, &result);
    }
}

/// Records already reach the owner through the committed publication feed.
/// Reporting a finished copy must never join the configuration I/O owner.
pub(super) fn report_recovery(result: &crate::recovery::RecoveryResult) {
    if let Err(error) = &result.outcome {
        if result.deferred {
            tracing::info!(session_id = %result.session_id, %error, "recovery copy deferred: agent is working");
        } else if result.cancelled {
            tracing::info!(session_id = %result.session_id, %error, reason = "superseded by suspend", "daemon recovery checkpoint cancelled");
        } else {
            tracing::warn!(session_id = %result.session_id, %error, "daemon recovery checkpoint failed");
        }
    }
}

/// Log one finished worker upgrade, and tell the surfaces about the one that
/// changed something.
pub(super) fn report_worker_upgrade(
    state: &Arc<RuntimeState>,
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
                    |session| session.listed_title().to_owned(),
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
            if let Some(cause) = &result.applied_preparation_failure {
                state.push_notice(session_id, cause.clone());
            }
        }
    }
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

    fn execute_cleanup_with_timeout(
        &self,
        command: &CommandSpec,
        timeout: std::time::Duration,
    ) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .as_ref()
            .map(|stage| ProvisionStageGuard::new(self, stage.clone()));
        self.inner.execute_cleanup_with_timeout(command, timeout)
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

    fn shutdown_interrupted(&self) -> bool {
        self.state
            .owner()
            .lifecycle
            .get(&self.session_id)
            .is_some_and(|active| {
                self.operation_id.as_ref() == Some(&active.operation_id)
                    && active.control.stopped_for_shutdown()
            })
    }

    fn begin_resumable_work(&self) -> Result<()> {
        let owner = self.state.owner();
        let active = owner
            .lifecycle
            .get(&self.session_id)
            .context("resumable lifecycle missing")?;
        ensure!(
            self.operation_id.as_ref() == Some(&active.operation_id),
            "resumable lifecycle changed"
        );
        active
            .upgrade_work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(())
    }

    fn end_resumable_work(&self, label: &'static str) -> Result<()> {
        let work = crate::upgrade::activity_unless_draining(label)?;
        let owner = self.state.owner();
        let active = owner
            .lifecycle
            .get(&self.session_id)
            .context("resumable lifecycle missing")?;
        ensure!(
            self.operation_id.as_ref() == Some(&active.operation_id),
            "resumable lifecycle changed"
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

#[cfg(test)]
mod publication_tests {
    use super::*;
    use crate::daemon::tests::{test_metadata, test_runtime_state};

    // Hard-won: #1499: result publication awaited configuration ownership on the serving loop.
    #[tokio::test]
    async fn result_publication_does_not_wait_for_configuration() {
        const CHILD: &str = "MJ_RESULT_PUBLICATION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                "daemon::support::publication_tests::result_publication_does_not_wait_for_configuration",
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "result-publication-test")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut state = test_runtime_state();
        Arc::get_mut(&mut state).unwrap().committed =
            Some(crate::database::subscribe_committed_state().unwrap());
        let config_owner = state.config_mutation.lock().await;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = test_metadata(address);
        let cancellation = CancellationToken::new();
        let server_cancel = cancellation.clone();
        let serving = state.clone();
        let server = tokio::spawn(async move {
            // The same synchronous drain used by the real serving tick must
            // finish before this task can accept and answer a connection.
            publish_background_results(
                &serving,
                [(false, true), (true, false), (false, false), (false, false)]
                    .into_iter()
                    .enumerate()
                    .map(
                        |(index, (cancelled, deferred))| crate::recovery::RecoveryResult {
                            session_id: "finished-copy".into(),
                            expected_target: mj_core::state::TargetLocator::LocalBare {
                                worker_root: "unused".into(),
                            },
                            outcome: if index == 3 {
                                Ok(crate::controller::CheckpointArtifact {
                                    metadata: mj_core::state::CheckpointMetadata {
                                        archive_path: "unused.zip".into(),
                                        sha256: "verified".into(),
                                        created_at: "2026-11-11T15:00:00Z".into(),
                                        event_frontier: 1,
                                    },
                                    native_session_id: "native".into(),
                                    event_frontier_digest: "verified".into(),
                                })
                            } else {
                                Err("copy completed without an artifact".into())
                            },
                            cancelled,
                            deferred,
                        },
                    ),
                [crate::worker_upgrade::WorkerUpgradeResult {
                    session_id: "finished-upgrade".into(),
                    outcome: Err("harness preparation failed".into()),
                    applied_preparation_failure: Some("durably failed".into()),
                    cancelled: false,
                }]
                .into_iter(),
            );
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, metadata, serving, server_cancel)
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = TcpStream::connect(address).await.unwrap();
            for (request_id, action) in [(1, DaemonAction::Ping), (2, DaemonAction::Stop)] {
                write_frame(
                    &mut stream,
                    &RequestEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        request_id,
                        token: "right-token".into(),
                        action,
                    },
                )
                .await
                .unwrap();
                let reply = read_frame::<ResponseEnvelope>(&mut stream)
                    .await
                    .unwrap()
                    .result
                    .unwrap();
                if request_id == 1 {
                    assert!(matches!(reply, DaemonReply::Pong));
                } else {
                    assert!(matches!(reply, DaemonReply::Done));
                }
            }
            assert!(cancellation.is_cancelled());
            drop(stream);
            server.await.unwrap();
        })
        .await
        .expect("publication blocked request/stop behind configuration mutation");
        drop(config_owner);
    }
}

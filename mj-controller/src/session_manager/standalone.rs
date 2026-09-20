use super::*;

pub struct StandaloneSession {
    pub(super) client: RelayClient,
    pub(super) materialized: MaterializedSession,
    pub(super) operational: RelayOperationalState,
    pub(super) latest_credential_sync_signal: Option<CredentialSyncSignal>,
    pub(super) project_memory: Option<ProjectMemorySyncTarget>,
    pub(super) subagent_requests: Vec<mj_core::subagent::SubagentToolRequest>,
    pub(super) subagent_results: Vec<mj_core::subagent::SubagentToolResult>,
    history_jobs: tokio::task::JoinSet<mj_core::history::HistoryResult>,
    history_active: std::collections::BTreeSet<String>,
}

impl StandaloneSession {
    pub fn set_project_memory_target(&mut self, target: Option<ProjectMemorySyncTarget>) {
        self.project_memory = target;
    }

    pub async fn connect(target: &RelaySessionTarget) -> Result<Self> {
        // Reach the worker before reading the projection. A stored session can
        // be tens of megabytes, and the reconnect loop would otherwise pay that
        // whole synchronous read on every attempt against a worker that is down.
        let mut client = RelayClient::connect(&target.spec, &target.session_id).await?;
        let operational = client.status().await?;
        let materialized = load_projection(&target.session_id).await?;
        let mut connection = Self {
            client,
            materialized,
            operational,
            latest_credential_sync_signal: None,
            project_memory: target.project_memory.clone(),
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            history_jobs: tokio::task::JoinSet::new(),
            history_active: Default::default(),
        };
        connection.sync_in_place().await?;
        Ok(connection)
    }

    pub async fn connect_command(spec: &CommandSpec, session_id: &str) -> Result<Self> {
        Self::connect(&RelaySessionTarget {
            session_id: session_id.to_owned(),
            spec: spec.clone(),
            worker_recovery: None,
            project_memory: None,
        })
        .await
    }

    /// Protocol negotiated with the worker behind this connection. Lifecycle
    /// operations use it to avoid sending a newly introduced command to an
    /// older worker that cannot decode it.
    pub fn protocol_version(&self) -> u32 {
        self.client.protocol_version()
    }

    pub(super) async fn detach(self) -> Result<()> {
        self.client.detach().await
    }

    pub async fn sync(&mut self) -> Result<ManagedSessionSnapshot> {
        self.sync_in_place().await?;
        Ok(self.snapshot())
    }

    pub(super) async fn sync_in_place(&mut self) -> Result<bool> {
        self.sync_history().await?;
        let original_ordinal = self.materialized.applied_event_ordinal;
        let original_digest = self.materialized.applied_event_digest.clone();
        let original_operational = self.operational.clone();
        let mut repaired = false;
        let mut repaired_frontiers = std::collections::HashSet::new();
        loop {
            let after_ordinal = self.materialized.applied_event_ordinal;
            match self.catch_up_fixed_frontier().await {
                Ok(()) => break,
                Err(error) if error.downcast_ref::<ProjectionAdvancedError>().is_some() => {
                    let durable = load_projection(&self.materialized.session_id).await?;
                    if durable.applied_event_ordinal <= after_ordinal {
                        return Err(error);
                    }
                    self.materialized = durable;
                    continue;
                }
                Err(error) if relay_desynchronized(&error) => {
                    self.repair_projection()
                        .await
                        .with_context(|| {
                            format!(
                                "controller projection for {} cannot catch up from ordinal {after_ordinal}: {error:#}",
                                self.materialized.session_id
                            )
                        })?;
                    repaired = true;
                    // Repair rebuilds from the same durable checkpoint every
                    // time. If catching up from that frontier still desyncs — as
                    // it does when relay history is unreadable past the
                    // checkpoint — repairing again lands on the same frontier and
                    // would loop forever. Fail loudly on the second visit instead
                    // of hanging; recovery got everything the checkpoint covers.
                    let frontier = self.materialized.applied_event_ordinal;
                    if !repaired_frontiers.insert(frontier) {
                        bail!(
                            "controller projection for {} cannot catch up: relay history is \
                             unreadable and rebuilding from checkpoint frontier {frontier} does \
                             not get past it",
                            self.materialized.session_id
                        );
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let previous_requests = self.subagent_requests.clone();
        let previous_results = self.subagent_results.clone();
        (self.subagent_requests, self.subagent_results) = self.client.subagent_requests().await?;
        let changed = repaired
            || self.materialized.applied_event_ordinal != original_ordinal
            || self.materialized.applied_event_digest != original_digest
            || self.operational != original_operational
            || self.subagent_requests != previous_requests
            || self.subagent_results != previous_results;
        Ok(changed)
    }

    /// Poll only bounded messages here. Disk searches run independently of this
    /// actor, and the owned JoinSet cancels them when the connection is retired.
    async fn sync_history(&mut self) -> Result<()> {
        while let Some(completed) = self.history_jobs.try_join_next() {
            match completed {
                Ok(result) => {
                    self.history_active.remove(&result.request_id);
                    self.client.complete_history_request(result).await?;
                }
                Err(error) => {
                    tracing::error!(%error, "history task failed; pending requests will retry");
                    self.history_jobs.abort_all();
                    while let Some(result) = self.history_jobs.join_next().await {
                        if let Err(error) = result
                            && !error.is_cancelled()
                        {
                            tracing::error!(%error, "history task failed during cleanup");
                        }
                    }
                    self.history_active.clear();
                }
            }
        }
        for request in self.client.history_requests().await? {
            if self.history_active.len() >= mj_core::history::MAX_PENDING {
                break;
            }
            if self.history_active.insert(request.request_id.clone()) {
                self.history_jobs
                    .spawn(crate::sessionwiki::history::execute(request));
            }
        }
        Ok(())
    }

    /// Apply relay pages through the exact frontier captured by the first
    /// response, then acknowledge that frontier once. Every projection page is
    /// independently durable; delaying the relay's GC watermark avoids one
    /// snapshot fsync per transport-sized page without risking redelivery.
    pub(super) async fn catch_up_fixed_frontier(&mut self) -> Result<()> {
        let after = RelayCursor {
            ordinal: self.materialized.applied_event_ordinal,
            digest: self.materialized.applied_event_digest.clone(),
        };
        let catch_up = self
            .client
            .begin_catch_up(after.ordinal, &after.digest)
            .await?;
        let mut cursor = self.apply_event_page(catch_up.first_page).await?;
        let mut pages_remaining = catch_up.frontier.ordinal.saturating_sub(cursor.ordinal);
        while cursor.ordinal < catch_up.frontier.ordinal {
            ensure!(
                pages_remaining > 0,
                "relay catch-up exceeded its fixed page bound"
            );
            pages_remaining -= 1;
            let page = self
                .client
                .next_catch_up_page(&cursor, &catch_up.frontier)
                .await?;
            cursor = self.apply_event_page(page).await?;
        }
        ensure!(
            cursor == catch_up.frontier,
            "controller projection did not reach the captured relay frontier"
        );
        if cursor.ordinal > 0 {
            let acknowledged = self
                .client
                .acknowledge(cursor.ordinal, &cursor.digest)
                .await?;
            ensure!(
                acknowledged == cursor,
                "relay acknowledged cursor {}:{} instead of {}:{}",
                acknowledged.ordinal,
                acknowledged.digest,
                cursor.ordinal,
                cursor.digest,
            );
        }
        let mut operational = catch_up.state;
        operational.acknowledged_through = cursor.ordinal;
        operational.acknowledged_digest = cursor.digest;
        self.operational = operational;
        Ok(())
    }

    pub(super) async fn repair_projection(&mut self) -> Result<()> {
        let state = crate::database::load_state()?;
        let record = state
            .sessions
            .get(&self.materialized.session_id)
            .context("controller session disappeared while repairing its projection")?;
        let Some(checkpoint) = record.checkpoint.as_ref() else {
            let replacement = MaterializedSession::empty(&self.materialized.session_id);
            self.client
                .attach(
                    replacement.applied_event_ordinal,
                    &replacement.applied_event_digest,
                )
                .await
                .context("relay cannot rebuild the projection from its genesis")?;
            save_materialized_session(&replacement)?;
            self.materialized = replacement;
            return Ok(());
        };
        let checkpoint_path = checkpoint.archive_path.clone();
        let archive = tokio::task::spawn_blocking(move || {
            verify_archive_streaming(&checkpoint_path).with_context(|| {
                format!(
                    "verify projection repair checkpoint {}",
                    checkpoint_path.display()
                )
            })
        })
        .await
        .context("projection repair archive verification task failed")??;
        ensure!(
            archive.archive_sha256 == checkpoint.sha256,
            "projection repair checkpoint checksum does not match controller metadata"
        );
        ensure!(
            archive.manifest.session.id == self.materialized.session_id,
            "projection repair checkpoint belongs to session {}, not {}",
            archive.manifest.session.id,
            self.materialized.session_id
        );
        let canonical = archive.canonical_session;
        ensure!(
            canonical.event_frontier == checkpoint.event_frontier,
            "projection repair checkpoint metadata frontier {} does not match archive frontier {}",
            checkpoint.event_frontier,
            canonical.event_frontier
        );

        // Prove that the relay recognizes this exact event-chain cursor before
        // replacing any controller state. A matching ordinal alone is not a
        // repair proof.
        self.client
            .attach(canonical.event_frontier, &canonical.event_frontier_digest)
            .await
            .context("relay rejected the verified checkpoint repair cursor")?;
        let replacement =
            materialized_session_from_canonical(&self.materialized.session_id, &canonical)?;
        save_materialized_session(&replacement)?;
        self.materialized = replacement;
        Ok(())
    }

    pub fn snapshot(&self) -> ManagedSessionSnapshot {
        ManagedSessionSnapshot {
            window: mj_core::state::ProjectionWindow::of(&self.materialized),
            materialized: self.materialized.clone(),
            operational: self.operational.clone(),
            latest_credential_sync_signal: self.latest_credential_sync_signal.clone(),
            worker_build: self.client.worker_build().map(str::to_owned),
            subagent_requests: self.subagent_requests.clone(),
            subagent_results: self.subagent_results.clone(),
        }
    }

    pub async fn complete_subagent_request(
        &mut self,
        result: mj_core::subagent::SubagentToolResult,
    ) -> Result<()> {
        self.client.complete_subagent_request(result).await?;
        (self.subagent_requests, self.subagent_results) = self.client.subagent_requests().await?;
        Ok(())
    }

    /// Hands one command to the relay and returns the ordinal it accepted it
    /// at, without catching the local projection up to it.
    ///
    /// Callers that need the projection current call [`Self::sync`] after.
    /// Keeping the two apart matters on the prompt path: the catch-up is the
    /// expensive half, and a caller waiting to hear that the relay took the
    /// command should not wait for it. It also stops a failed catch-up from
    /// looking like a failed submission to a caller that would retry.
    pub async fn submit_accepted(
        &mut self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<u64> {
        self.client.submit(command_id, command).await
    }

    pub async fn submit(&mut self, command_id: String, command: RelayCommand) -> Result<u64> {
        let ordinal = self.submit_accepted(command_id, command).await?;
        self.sync_in_place().await?;
        Ok(ordinal)
    }

    pub async fn respond_elicitation(
        &mut self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        self.client
            .respond_elicitation(elicitation_id, response)
            .await?;
        self.sync_in_place().await?;
        Ok(())
    }

    pub async fn stop_background_task(&mut self, background_task_id: String) -> Result<()> {
        self.client.stop_background_task(background_task_id).await?;
        self.sync_in_place().await?;
        Ok(())
    }

    /// Persist relay-private context for the next real prompt. It never
    /// contributes an event to the canonical projection.
    pub async fn install_prompt_context(&mut self, text: String) -> Result<()> {
        self.client.install_prompt_context(text).await
    }

    /// Apply one relay transport page in bounded durable chunks. A transport
    /// page can contain thousands of events, but SQLite has one global writer;
    /// regularly releasing it lets other session actors keep their views
    /// current. The relay GC watermark advances only after the complete page.
    pub(super) async fn apply_event_page(&mut self, page: RelayEventPage) -> Result<RelayCursor> {
        for event in &page.events {
            if let mj_core::relay::RelayObservation::CommandQueued {
                command: RelayCommand::Prompt { prompt },
                ..
            } = &event.observation
            {
                for reference in mj_core::attachment::references(prompt)? {
                    if let Err(error) = self.client.cache_attachment(&reference).await {
                        // History remains readable even if a blob was lost. A
                        // later submission still verifies every image before
                        // admission, and must report missing data to the user.
                        tracing::warn!(
                            session_id = %self.materialized.session_id,
                            attachment = %reference.sha256,
                            %error,
                            "could not cache image attachment during replay"
                        );
                    }
                }
            }
        }

        let RelayEventPage {
            events,
            through_ordinal,
            through_digest,
        } = page;
        let event_count = events.len();
        let transaction_count = event_count.div_ceil(PROJECTION_TRANSACTION_EVENT_BUDGET);
        let started = Instant::now();
        for events in events.chunks(PROJECTION_TRANSACTION_EVENT_BUDGET) {
            let session_id = self.materialized.session_id.clone();
            let events = events.to_vec();
            let projection = self.materialized.clone();
            // Projection is CPU work and its durable page uses synchronous
            // SQLite. Keep both off the async actor runtime so independent
            // sessions stay responsive during each bounded catch-up chunk.
            let (projection, credential_sync_signal) = tokio::task::spawn_blocking(
                move || -> Result<(MaterializedSession, Option<CredentialSyncSignal>)> {
                    // The in-memory projection advances on a working copy and
                    // is published only once its page is durable.
                    let mut projection = projection;
                    let mut projection_index = ProjectionIndex::new(&projection);
                    let mut credential_sync_signal = None;
                    let mut prepared = Vec::with_capacity(events.len());
                    for event in &events {
                        let mutation =
                            project_relay_event_indexed(&projection, &projection_index, event)?
                                .mutation;
                        prepared.push((
                            event.ordinal,
                            event.previous_digest.clone(),
                            event.digest.clone(),
                            mutation.clone(),
                        ));
                        apply_committed_projection_event_indexed(
                            &mut projection,
                            &mut projection_index,
                            event,
                            mutation,
                        )?;
                        if let Some(reason) = relay_event_credential_sync_reason(event) {
                            credential_sync_signal = Some(CredentialSyncSignal {
                                ordinal: event.ordinal,
                                reason,
                            });
                        }
                    }
                    drop(projection_index);
                    apply_projection_page(&session_id, move |committed| {
                        for (ordinal, previous_digest, digest, mutation) in prepared {
                            match committed.apply(ordinal, &previous_digest, &digest, &mutation)? {
                                ProjectionApplyOutcome::Applied => {}
                                ProjectionApplyOutcome::AlreadyApplied => {
                                    return Err(ProjectionAdvancedError {
                                        event_ordinal: ordinal,
                                    }
                                    .into());
                                }
                            }
                        }
                        Ok((projection, credential_sync_signal))
                    })
                },
            )
            .await
            .context("relay projection page task failed")??;
            self.materialized = projection;
            if let Some(signal) = credential_sync_signal {
                self.latest_credential_sync_signal = Some(signal);
            }
        }
        if transaction_count > 1 {
            tracing::debug!(
                session_id = self.materialized.session_id,
                event_count,
                transaction_count,
                elapsed_ms = started.elapsed().as_millis(),
                "applied a large relay page in bounded projection transactions"
            );
        }
        let delivered_through = self.materialized.applied_event_ordinal;
        ensure!(
            delivered_through == through_ordinal,
            "relay page claimed frontier {} but delivered through {delivered_through}",
            through_ordinal
        );
        ensure!(
            self.materialized.applied_event_digest == through_digest,
            "relay page digest does not match its claimed frontier"
        );
        Ok(RelayCursor {
            ordinal: delivered_through,
            digest: self.materialized.applied_event_digest.clone(),
        })
    }

    /// Reconcile this worker's project-memory replica at an explicit durable
    /// boundary. Normal relay attachment and polling must never perform this
    /// filesystem work: a degraded target could otherwise turn reconnects
    /// into an unbounded queue of timed-out snapshot writes.
    pub async fn sync_project_memory(&mut self) -> Result<()> {
        let Some(target) = self.project_memory.clone() else {
            return Ok(());
        };
        if !self.client.supports_project_memory_sync() {
            tracing::warn!(
                session_id = self.materialized.session_id,
                "worker protocol predates project-memory synchronization; preserving memory through checkpoints only"
            );
            self.project_memory = None;
            return Ok(());
        }
        let (baseline, replica) = match self.client.project_memory_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error)
                if error
                    .downcast_ref::<RelayRejected>()
                    .is_some_and(|rejected| {
                        rejected.0.code == mj_core::relay::RelayErrorCode::InvalidState
                    }) =>
            {
                tracing::warn!(
                    session_id = self.materialized.session_id,
                    "worker has no project-memory endpoint; preserving memory through checkpoints only"
                );
                self.project_memory = None;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let canonical_root = target.canonical_root;
        let session_id = self.materialized.session_id.clone();
        let (reconciliation, worker_install_needed) = tokio::task::spawn_blocking(move || {
            let reconciliation = mj_core::project_memory::reconcile_into_canonical(
                &canonical_root,
                &baseline,
                &replica,
                &session_id,
            )?;
            let worker_install_needed =
                reconciliation.merged != baseline || reconciliation.merged != replica;
            Ok::<_, anyhow::Error>((reconciliation, worker_install_needed))
        })
        .await
        .context("project memory reconciliation task failed")??;
        for conflict in &reconciliation.conflicts {
            tracing::warn!(session_id = self.materialized.session_id, %conflict, "project memory conflict preserved");
        }
        if worker_install_needed {
            self.client
                .install_project_memory_snapshot(reconciliation.merged)
                .await?;
        }
        Ok(())
    }
}

pub(super) fn relay_desynchronized(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<RelayRejected>()
            .is_some_and(RelayRejected::is_desynchronized)
    })
}

pub(super) fn projection_integrity_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ProjectionIntegrityError>().is_some())
}

/// A stopped actor and the manager that resolves its live replacement.
///
/// This fixture and its constructor are compiled unconditionally and hidden
/// from the documentation because the chat crate's tests need them, and a
/// `#[cfg(test)]` item is invisible to another crate.
#[cfg(test)]
pub(super) struct ReplacementSessionTestFixture {
    pub(super) stopped: ManagedSessionHandle,
    pub(super) control: SessionManagerControl,
    pub(super) submitted: mpsc::UnboundedReceiver<RelayCommand>,
}

/// A stopped actor and a manager that resolves its live replacement. Chat
/// tests use this hand-written actor instead of mocking the session manager
/// protocol.
#[cfg(test)]
pub(super) fn replacement_session_test_fixture(
    session_id: &str,
    accepted_ordinal: u64,
) -> ReplacementSessionTestFixture {
    let (stopped_commands, stopped_commands_rx) = mpsc::channel(1);
    drop(stopped_commands_rx);
    let (stopped_releases, stopped_releases_rx) = mpsc::unbounded_channel();
    drop(stopped_releases_rx);
    let (stopped_view_tx, stopped_view) = watch::channel(ManagedSessionView::default());
    drop(stopped_view_tx);
    let stopped = ManagedSessionHandle {
        session_id: session_id.to_owned(),
        commands: stopped_commands,
        releases: stopped_releases,
        view: stopped_view,
    };

    let (commands, mut commands_rx) = mpsc::channel(4);
    let (releases, _releases_rx) = mpsc::unbounded_channel();
    let (view_tx, view) = watch::channel(ManagedSessionView::default());
    let replacement = ManagedSessionHandle {
        session_id: session_id.to_owned(),
        commands,
        releases,
        view,
    };
    let actor_session_id = session_id.to_owned();
    let (submitted_tx, submitted) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _view_tx = view_tx;
        while let Some(command) = commands_rx.recv().await {
            match command {
                ActorCommand::Submit { command, reply, .. } => {
                    // Tests can drop the optional observer when they only
                    // care about acceptance/reconnection.
                    let _ = submitted_tx.send(command);
                    let _ = reply.send(Ok(accepted_ordinal));
                }
                ActorCommand::Sync { reply } => {
                    let _ = reply.send(Ok(()));
                }
                command => command.reject(&actor_session_id, "unsupported test operation"),
            }
        }
    });

    let (manager_commands, mut manager_commands_rx) = mpsc::channel(4);
    let manager_replacement = replacement.clone();
    tokio::spawn(async move {
        while let Some(ManagerCommand::Session {
            session_id: requested,
            reply,
        }) = manager_commands_rx.recv().await
        {
            let resolved =
                (requested == manager_replacement.session_id).then(|| manager_replacement.clone());
            let _ = reply.send(resolved);
        }
    });
    ReplacementSessionTestFixture {
        stopped,
        submitted,
        control: SessionManagerControl {
            commands: manager_commands,
        },
    }
}

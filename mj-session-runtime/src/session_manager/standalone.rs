use super::*;
use crate::project_memory_merge::{
    CanonicalSync, ProjectMemoryWorker, WorkerInstallOutcome, install_merged_tree,
};
use mj_core::config::ConfigLoadError;
use mj_core::guardian::{
    DaemonGuardianFailure, GUARDIAN_DISABLED_REASON, GUARDIAN_REVIEW_TIMEOUT,
    GuardianReviewOutcome, GuardianReviewRequest, GuardianReviewResult,
    MAX_PENDING_GUARDIAN_REVIEWS, Outcome, WOLF_STAGE1_ALLOW_THRESHOLD, WolfStage1SkipReason,
};
use mj_core::mailbox::{MailboxEvent, MailboxEventBody};
use mj_core::project_memory::ProjectMemorySnapshot;
use mj_core::state::MaterializedExecutionState;
use mj_core::transcript::TranscriptItem;
use mj_import::utility_llm::{GuardianUtilityOutcome, review_guardian_with_utility};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::{future::Future, pin::Pin};
use tokio::task::JoinError;

pub enum GuardianStage1Verdict {
    Scored {
        unsafe_prob: f64,
        tokens: usize,
        elapsed_ms: u64,
    },
    Escalated(WolfStage1SkipReason),
    Overflow {
        tokens: usize,
    },
    Unavailable {
        reason: String,
    },
}

pub trait GuardianStage1: Send + Sync {
    fn classify<'a>(
        &'a self,
        request: &'a GuardianReviewRequest,
    ) -> Pin<Box<dyn Future<Output = GuardianStage1Verdict> + Send + 'a>>;
}

// Acknowledge retries worker journal GC after transient failures, even when
// the projection watermark has not changed (mj-worker relay/requests.rs).
const ACK_GC_RETRY_INTERVAL: Duration = Duration::from_secs(60);

pub struct StandaloneSession {
    pub(super) last_ack_sent: Option<Instant>,
    pub client: RelayClient,
    pub materialized: MaterializedSession,
    pub window: mj_core::state::ProjectionWindow,
    pub operational: mj_core::snapshot_map::SharedSnapshot<RelayOperationalState>,
    pub latest_credential_sync_signal: Option<CredentialSyncSignal>,
    pub project_memory: Option<ProjectMemorySyncTarget>,
    pub subagent_requests:
        mj_core::snapshot_map::SharedSnapshot<Vec<mj_core::subagent::SubagentToolRequest>>,
    pub subagent_completions:
        mj_core::snapshot_map::SharedSnapshot<Vec<mj_core::subagent::SubagentCompletion>>,
    subagent_queue: Option<super::subagent_queue::SubagentQueueObserver>,
    subagent_queue_error: Option<String>,
    pub(super) history_jobs: tokio::task::JoinSet<mj_core::history::HistoryResult>,
    operator_presence_revision: Option<u64>,
    pub(super) history_active: std::collections::BTreeMap<String, tokio::task::AbortHandle>,
    guardian_jobs: tokio::task::JoinSet<GuardianReviewResult>,
    guardian_active: std::collections::BTreeSet<String>,
}

pub(super) enum ConnectionActivity {
    RelayChanged { guardian: bool },
    GuardianCompleted(std::result::Result<GuardianReviewResult, JoinError>),
}

impl StandaloneSession {
    pub async fn cpu_usage(&mut self) -> Result<Option<mj_core::cpu_usage::SessionCpuUsage>> {
        self.client.cpu_usage().await
    }

    pub fn set_project_memory_target(&mut self, target: Option<ProjectMemorySyncTarget>) {
        self.project_memory = target;
    }

    pub async fn connect(target: &RelaySessionTarget) -> Result<Self> {
        Self::connect_inner(target, None, None).await
    }

    pub(super) async fn connect_inner(
        target: &RelaySessionTarget,
        prepared: Option<RelayClient>,
        baseline: Option<mj_core::relay::LiveSessionBaseline>,
    ) -> Result<Self> {
        // Reach the worker before reading the projection. A stored session can
        // be tens of megabytes, and the reconnect loop would otherwise pay that
        // whole synchronous read on every attempt against a worker that is down.
        let mut client = match prepared {
            Some(client) => client,
            None => RelayClient::connect(&target.spec, &target.session_id).await?,
        };
        let session = target.session_id.clone();
        let protocol = client.protocol_version();
        mj_core::runtime::spawn_blocking(move || {
            crate::database::record_operator_away_worker_protocol(session, protocol)
        })
        .await
        .context("record negotiated worker protocol")??;
        let operational = client.status().await?;
        let (materialized, window) = load_projection(&target.session_id).await?;
        let mut connection = Self {
            last_ack_sent: None,
            client,
            materialized,
            window,
            operational: operational.into(),
            latest_credential_sync_signal: None,
            project_memory: target.project_memory.clone(),
            subagent_requests: Default::default(),
            subagent_completions: Default::default(),
            subagent_queue: None,
            subagent_queue_error: None,
            history_jobs: tokio::task::JoinSet::new(),
            operator_presence_revision: None,
            history_active: Default::default(),
            guardian_jobs: Default::default(),
            guardian_active: Default::default(),
        };
        if let Some(baseline) = baseline {
            ensure!(
                baseline.state.session_id == target.session_id,
                "live recovery cut belongs to another session"
            );
            Self::cache_baseline_attachments(&mut connection.client, &baseline).await?;
            let projection =
                mj_transcript::projection::materialized_session_from_live_baseline(&baseline)?;
            crate::database::install_live_session_baseline(&projection, baseline)?;
            (connection.materialized, connection.window) =
                load_projection(&target.session_id).await?;
        }
        let loaded_ordinal = connection.materialized.applied_event_ordinal;
        connection.sync_in_place().await?;
        if connection.materialized.applied_event_ordinal == loaded_ordinal {
            // An already-current projection can predate title publication.
            // Backfill against the exact materialized frontier on attachment.
            let session = connection.materialized.session_id.clone();
            let digest = connection.materialized.applied_event_digest.clone();
            let title = connection.window.resolved_title(&connection.materialized);
            mj_core::runtime::spawn_blocking(move || {
                crate::database::set_session_acp_title_at_projection(
                    session,
                    loaded_ordinal,
                    digest,
                    title,
                )
            })
            .await
            .context("backfill relay title task failed")??;
        }
        if let Some((legacy_pending, pinned_operations)) =
            connection.client.command_retention_status().await?
            && (legacy_pending || !pinned_operations.is_empty())
        {
            let session = target.session_id.clone();
            // Inspect only the owners this worker reported. A concurrent new
            // Move has a different identity and cannot be cleared by this read.
            let (legacy, completed) = mj_core::runtime::spawn_blocking(move || -> Result<_> {
                let operation = crate::database::load_move_operation(&session)?;
                let pending = operation.as_ref().filter(|operation| {
                    operation.queue_admission_started && !operation.queue_admission_finished
                });
                let completed = pinned_operations
                    .into_iter()
                    .filter(|id| pending.is_none_or(|operation| operation.operation_id != *id))
                    .collect::<Vec<_>>();
                if !legacy_pending {
                    return Ok((None, completed));
                }
                let legacy = (|| -> Result<_> {
                    let Some(operation) = pending.filter(|operation| {
                        operation.queue == mj_core::state::ResumeQueueDisposition::Start
                    }) else {
                        return Ok((None, Default::default()));
                    };
                    let checkpoint = operation.restore_artifact()
                        .context("pending Move queue archive is missing")?;
                    let verified = verify_archive_streaming(&checkpoint.archive_path)?;
                    anyhow::ensure!(
                        verified.archive_sha256 == checkpoint.sha256
                            && verified.manifest.session.id == session,
                        "pending Move queue checkpoint verification failed"
                    );
                    Ok((Some(operation.operation_id.clone()),
                        operation.queue_command_ids(&verified.canonical_session)))
                })();
                let legacy = match legacy {
                    Ok(legacy) => Some(legacy),
                    Err(error) => {
                        // Keep the worker's legacy receipt hold intact. Queue
                        // replay reports this error; ordinary attachment must
                        // still expose the live destination and its status.
                        tracing::warn!(session_id = %session,
                            "could not reconcile pending Move receipts; retaining legacy protection: {error:#}");
                        None
                    }
                };
                Ok((legacy, completed))
            })
            .await
            .context("reconcile command retention owners")??;
            // Catch-up has validated this store against the worker before any
            // receipt protection changes. Existing pins and the legacy hold
            // protect receipts during that ACK; stale backups never release them.
            if let Some((operation, ids)) = legacy {
                connection
                    .client
                    .update_command_retention(operation, ids, true)
                    .await?;
            }
            for operation in completed {
                connection
                    .client
                    .update_command_retention(Some(operation), Default::default(), false)
                    .await?;
            }
        }
        Ok(connection)
    }

    pub async fn cache_baseline_attachments(
        client: &mut RelayClient,
        baseline: &mj_core::relay::LiveSessionBaseline,
    ) -> Result<()> {
        let mut images = Vec::new();
        let mut files = Vec::new();
        for queued in baseline
            .queue
            .iter()
            .chain(&baseline.delivered_initial_prompts)
        {
            if let Some(prompt) = queued.command.prompt_blocks() {
                images.extend(mj_core::attachment::references(&prompt)?);
                files.extend(mj_core::attachment::file_references(&prompt)?);
            }
        }
        client.cache_attachments(&images, &files).await
    }

    async fn cache_event_attachments(
        &mut self,
        events: &[mj_core::relay::RelayEvent],
    ) -> Result<()> {
        let mut images = Vec::new();
        let mut files = Vec::new();
        for event in events {
            if let mj_core::relay::RelayObservation::CommandQueued { command, .. } =
                &event.observation
                && let Some(prompt) = command.prompt_blocks()
            {
                images.extend(mj_core::attachment::references(&prompt)?);
                files.extend(mj_core::attachment::file_references(&prompt)?);
            }
        }
        self.client.cache_attachments(&images, &files).await
    }

    pub async fn prepare_adoption(spec: &CommandSpec, session_id: &str) -> Result<RelayClient> {
        RelayClient::connect(spec, session_id).await
    }

    /// Probe only durable cursors; no history is acknowledged during preparation.
    pub async fn adoption_needs_cut(client: &mut RelayClient, session_id: &str) -> Result<bool> {
        let session = session_id.to_owned();
        let cursor = mj_core::runtime::spawn_blocking(move || {
            crate::database::materialized_event_frontier(&session)
        })
        .await
        .context("read adoption cursor")??
        .unwrap_or((0, mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into()));
        match client.attach(cursor.0, &cursor.1).await {
            Ok(_) => return Ok(false),
            Err(error) if relay_desynchronized(&error) => {
                // A competing adopter may have committed and ACKed since our read.
                let session = session_id.to_owned();
                let current = mj_core::runtime::spawn_blocking(move || {
                    crate::database::materialized_event_frontier(&session)
                })
                .await
                .context("reload adoption cursor")??;
                if let Some(current) = current.filter(|current| current.0 > 0) {
                    client.attach(current.0, &current.1).await?;
                    return Ok(false);
                }
                if client.status().await?.retained_event_ordinal() == 0 {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        if client.protocol_version() < mj_core::relay::RELAY_LIVE_BASELINE_PROTOCOL {
            return Err(crate::worker_client::LiveBaselineUnsupported.into());
        }
        Ok(true)
    }

    #[cfg(test)]
    pub async fn connect_adopting(spec: &CommandSpec, session_id: &str) -> Result<Self> {
        let mut client = Self::prepare_adoption(spec, session_id).await?;
        let cut = if Self::adoption_needs_cut(&mut client, session_id).await? {
            Some(client.live_session_baseline().await?)
        } else {
            None
        };
        Self::connect_prepared_adopting(spec, session_id, client, cut).await
    }

    #[cfg(test)]
    async fn connect_prepared_adopting(
        spec: &CommandSpec,
        session_id: &str,
        client: RelayClient,
        baseline: Option<mj_core::relay::LiveSessionBaseline>,
    ) -> Result<Self> {
        Self::connect_inner(
            &RelaySessionTarget {
                session_id: session_id.to_owned(),
                spec: spec.clone(),
                worker_recovery: None,
                project_memory: None,
            },
            Some(client),
            baseline,
        )
        .await
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

    pub async fn attachment_present(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<bool> {
        self.client.attachment_present(reference).await
    }

    pub async fn ensure_file_attachment(
        &mut self,
        reference: &mj_core::attachment::FileAttachmentRef,
    ) -> Result<()> {
        self.client.ensure_file_attachment(reference).await
    }

    pub async fn reserve_idle(&mut self, command_id: String) -> Result<bool> {
        self.client.reserve_idle(command_id).await
    }

    pub async fn detach(self) -> Result<()> {
        self.client.detach().await
    }

    pub async fn sync(&mut self) -> Result<ManagedSessionSnapshot> {
        self.sync_in_place().await?;
        Ok(self.snapshot())
    }

    /// Whether the actor should follow this worker at the fast cadence.
    ///
    /// True while the worker owns work whose events stream in, while it
    /// classifies a turn that just ended (`mj prompt` returns on that
    /// verdict and automatic continuation acts on it, and it lands a moment
    /// after the turn goes quiet), or while a history search this
    /// connection runs still has a result to deliver on its next sync. A
    /// classification that is still pending well after its turn ended is left
    /// to the slow sweep, so no worker state can hold the fast cadence forever.
    /// Whether this session's worker reports changes as they happen, so a
    /// quiet session need not be polled.
    pub fn waits_for_changes(&self) -> bool {
        self.subagent_queue
            .as_ref()
            .is_some_and(|queue| queue.waits_for_changes())
    }

    /// Wait for relay observations or a completed review without borrowing
    /// the control client.
    pub(super) async fn wait_for_activity(&mut self) -> ConnectionActivity {
        tokio::select! {
            Some(result) = self.guardian_jobs.join_next(), if !self.guardian_jobs.is_empty() => ConnectionActivity::GuardianCompleted(result),
            guardian = async {
                match self.subagent_queue.as_mut() {
                    Some(queue) => queue.relay_changed().await,
                    None => std::future::pending().await,
                }
            } => ConnectionActivity::RelayChanged { guardian },
        }
    }

    /// Only the actor starts replication, after control sync succeeds. Lifecycle
    /// connections never acquire their own background dialer.
    pub async fn start_observing(&mut self, target: &RelaySessionTarget) {
        // The worker serves control while preparing, but cannot serve its
        // delegation queue yet. Its published preparation state owns readiness.
        if self.operational.execution == mj_core::relay::RelayExecutionState::Closed
            || matches!(
                self.operational.harness_preparation,
                Some(
                    mj_core::relay::HarnessPreparation::Preparing { .. }
                        | mj_core::relay::HarnessPreparation::Failed { .. }
                )
            )
        {
            self.stop_observing().await;
            return;
        }
        self.subagent_queue
            .get_or_insert_with(|| super::subagent_queue::SubagentQueueObserver::spawn(target));
    }

    /// Join cancellation before granting the lifecycle lease: its owner may
    /// stop or replace the worker as soon as it receives the connection.
    pub async fn stop_observing(&mut self) {
        self.cancel_guardian_jobs();
        if let Some(queue) = self.subagent_queue.take() {
            queue.stop().await;
        }
    }

    pub fn follows_closely(&self) -> bool {
        const FRESH_CLASSIFICATION_MS: i64 = 60_000;
        let now = mj_core::clock::epoch_millis();
        !self.history_active.is_empty()
            || !self.guardian_active.is_empty()
            || self.operational.has_work_in_flight()
            || self
                .operational
                .assessment
                .as_ref()
                .is_some_and(|assessment| {
                    assessment.current()
                        && assessment.needs_classification(now)
                        && now.saturating_sub(assessment.completed_at_ms) < FRESH_CLASSIFICATION_MS
                })
    }

    pub async fn sync_in_place(&mut self) -> Result<bool> {
        self.sync_reads(None, false)
            .await
            .map(|(changed, _)| changed)
    }

    pub(super) async fn sync_reads(
        &mut self,
        actor: Option<Option<&Arc<dyn GuardianStage1>>>,
        cpu: bool,
    ) -> Result<(
        bool,
        Option<Result<Option<mj_core::cpu_usage::SessionCpuUsage>>>,
    )> {
        use mj_core::relay::{RelayRequest, RelayResponsePayload};
        let mut cpu_reply = None;
        if self.protocol_version() >= mj_core::relay::RELAY_OPERATOR_AWAY_PROTOCOL {
            let presence =
                mj_core::runtime::spawn_blocking(crate::database::load_operator_presence)
                    .await
                    .context("read operator away setting")??;
            if self.operator_presence_revision != Some(presence.revision) {
                self.client.set_operator_away(presence).await?;
                self.operator_presence_revision = Some(presence.revision);
            }
        }
        self.flush_history().await?;
        let original_ordinal = self.materialized.applied_event_ordinal;
        let original_digest = self.materialized.applied_event_digest.clone();
        let original_operational = self.operational.clone();
        let mut repaired = false;
        let mut repaired_frontiers = std::collections::HashSet::new();
        let mut first_pass = true;
        loop {
            let after_ordinal = self.materialized.applied_event_ordinal;
            let after_digest = self.materialized.applied_event_digest.clone();
            let attach = self
                .client
                .attach_request(after_ordinal, after_digest.clone());
            let mut requests = Vec::new();
            if first_pass && RelayRequest::HistoryRequests.supported_at(self.protocol_version()) {
                requests.push(RelayRequest::HistoryRequests);
            }
            requests.push(attach.clone());
            if first_pass
                && actor.is_some()
                && RelayRequest::GuardianRequests.supported_at(self.protocol_version())
            {
                requests.push(RelayRequest::GuardianRequests);
            }
            if first_pass && cpu && RelayRequest::CpuUsage.supported_at(self.protocol_version()) {
                requests.push(RelayRequest::CpuUsage);
            }
            first_pass = false;
            let methods: Vec<_> = requests.iter().map(RelayRequest::method_name).collect();
            let mut replies: std::collections::BTreeMap<_, _> = methods
                .into_iter()
                .zip(self.client.call_many(requests).await?)
                .collect();
            if let Some(reply) = replies.remove("history_requests") {
                match reply? {
                    RelayResponsePayload::HistoryRequests { requests } => {
                        self.apply_history(requests)
                    }
                    _ => bail!("unexpected history queue response"),
                }
            }
            if let Some(reply) = replies.remove("guardian_requests") {
                match reply? {
                    RelayResponsePayload::GuardianRequests { requests } => {
                        self.apply_guardian(requests, actor.flatten())
                    }
                    _ => bail!("unexpected guardian queue response"),
                }
            }
            if let Some(reply) = replies.remove("cpu_usage") {
                cpu_reply = Some(reply.and_then(|reply| match reply {
                    RelayResponsePayload::CpuUsage { usage } => Ok(usage),
                    _ => bail!("unexpected CPU response"),
                }));
            }
            let catch_up = replies
                .remove("attach")
                .expect("attach reply")
                .and_then(|reply| self.client.decode_attachment(attach, reply))
                .and_then(|first| {
                    RelayClient::catch_up_from_attachment(after_ordinal, after_digest, first)
                });
            let result = match catch_up {
                Ok(catch_up) => self.apply_catch_up(catch_up).await,
                Err(error) => Err(error),
            };
            let error = match result {
                Ok(()) => break,
                Err(error) => error,
            };
            let advancement = error.downcast_ref::<ProjectionAdvancedError>();
            if advancement.is_none() && !relay_desynchronized(&error) {
                return Err(error);
            }
            // Another connection can commit and ACK beyond our cursor. Try
            // its durable projection first, then attach from that exact cursor.
            let (durable, window) = load_projection(&self.materialized.session_id).await?;
            if let Some(ProjectionAdvancedError { required }) = advancement
                && (durable.applied_event_ordinal <= after_ordinal
                    || durable.applied_event_ordinal < required.ordinal
                    || (durable.applied_event_ordinal == required.ordinal
                        && durable.applied_event_digest != required.digest))
            {
                return Err(ProjectionIntegrityError(format!(
                    "durable projection for {} is at {}:{} and does not support advancement to {}:{}",
                    self.materialized.session_id,
                    durable.applied_event_ordinal,
                    durable.applied_event_digest,
                    required.ordinal,
                    required.digest,
                ))
                .into());
            }
            if durable.applied_event_ordinal > after_ordinal {
                self.materialized = durable;
                self.window = window;
                continue;
            }
            let behind_retention = after_ordinal < self.operational.retained_event_ordinal();
            self.repair_projection()
                .await
                .with_context(|| {
                    if behind_retention {
                        format!("controller store is behind the worker's retained journal for {} at ordinal {after_ordinal}: {error:#}", self.materialized.session_id)
                    } else {
                        format!("controller projection for {} cannot catch up from ordinal {after_ordinal}: {error:#}", self.materialized.session_id)
                    }
                })?;
            repaired = true;
            // Repair rebuilds from the same durable checkpoint every time.
            // If catch-up still desyncs there, another repair cannot help.
            let frontier = self.materialized.applied_event_ordinal;
            if !repaired_frontiers.insert(frontier) {
                bail!(
                    "controller projection for {} cannot catch up: relay history is \
                     unreadable and rebuilding from checkpoint frontier {frontier} does \
                     not get past it",
                    self.materialized.session_id
                );
            }
        }
        if let Some(queue) = &self.subagent_queue
            && let Some(digest) = self.client.known_state_digest()
        {
            queue.note_synced(self.operational.latest_ordinal, digest);
        }
        let previous_requests = self.subagent_requests.clone();
        let previous_results = self.subagent_completions.clone();
        let previous_error = self.subagent_queue_error.clone();
        if let Some(queue) = &self.subagent_queue {
            (
                self.subagent_requests,
                self.subagent_completions,
                self.subagent_queue_error,
            ) = queue.observe(self.follows_closely());
        }
        let changed = repaired
            || self.materialized.applied_event_ordinal != original_ordinal
            || self.materialized.applied_event_digest != original_digest
            || self.operational != original_operational
            || self.subagent_requests != previous_requests
            || self.subagent_completions != previous_results;
        let changed = changed || self.subagent_queue_error != previous_error;
        Ok((changed, cpu_reply))
    }

    /// Poll only bounded messages here. Disk searches run independently of this
    /// actor, and the owned JoinSet cancels them when the connection is retired.
    async fn flush_history(&mut self) -> Result<()> {
        while let Some(completed) = self.history_jobs.try_join_next() {
            match completed {
                Ok(result) => {
                    self.history_active.remove(&result.request_id);
                    self.client.complete_history_request(result).await?;
                }
                // Aborted below because the worker no longer lists the request.
                Err(error) if error.is_cancelled() => continue,
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
        Ok(())
    }

    fn apply_history(&mut self, pending: Vec<mj_core::history::HistoryRequest>) {
        // The worker drops a request when its caller gives up or it expires.
        // Stop that work now, before it waits for a permit or runs a query.
        let listed: std::collections::BTreeSet<&str> = pending
            .iter()
            .map(|request| request.request_id.as_str())
            .collect();
        self.history_active.retain(|request_id, task| {
            let keep = listed.contains(request_id.as_str());
            if !keep {
                task.abort();
            }
            keep
        });
        for request in pending {
            if self.history_active.len() >= mj_core::history::MAX_PENDING {
                break;
            }
            if !self.history_active.contains_key(&request.request_id) {
                let request_id = request.request_id.clone();
                let task = self
                    .history_jobs
                    .spawn(crate::sessionwiki::history::execute(
                        request,
                        Some(self.materialized.session_id.clone()),
                    ));
                self.history_active.insert(request_id, task);
            }
        }
    }

    pub(super) fn cancel_guardian_jobs(&mut self) {
        // Dropping the JoinSet aborts the tasks without waiting for a model.
        self.guardian_jobs = Default::default();
        self.guardian_active.clear();
    }

    fn apply_guardian(
        &mut self,
        requests: Vec<mj_core::guardian::GuardianReviewRequest>,
        classifier: Option<&Arc<dyn GuardianStage1>>,
    ) {
        for request in requests {
            if self.guardian_active.len() >= MAX_PENDING_GUARDIAN_REVIEWS {
                break;
            }
            if self.guardian_active.insert(request.request_id.clone()) {
                self.guardian_jobs
                    .spawn(execute_guardian_review(request, classifier.cloned()));
            }
        }
    }

    pub(super) async fn deliver_guardian_review(
        &mut self,
        result: std::result::Result<GuardianReviewResult, JoinError>,
    ) -> Result<()> {
        match result {
            Ok(result) => {
                self.guardian_active.remove(&result.request_id);
                self.client.complete_guardian_request(result).await
            }
            Err(error) => {
                tracing::error!(%error, "guardian review task failed; pending requests will retry");
                self.cancel_guardian_jobs();
                Ok(())
            }
        }
    }

    /// Apply relay pages through the exact frontier captured by the first
    /// response, then acknowledge that frontier once. Every projection page is
    /// independently durable; delaying the relay's GC watermark avoids one
    /// snapshot fsync per transport-sized page without risking redelivery.
    pub async fn catch_up_fixed_frontier(&mut self) -> Result<()> {
        let after = RelayCursor {
            ordinal: self.materialized.applied_event_ordinal,
            digest: self.materialized.applied_event_digest.clone(),
        };
        let catch_up = self
            .client
            .begin_catch_up(after.ordinal, &after.digest)
            .await?;
        self.apply_catch_up(catch_up).await
    }

    async fn apply_catch_up(&mut self, catch_up: crate::worker_client::RelayCatchUp) -> Result<()> {
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
        if cursor.ordinal > 0
            && (catch_up.state.acknowledged_through != cursor.ordinal
                || catch_up.state.acknowledged_digest != cursor.digest
                || self
                    .last_ack_sent
                    .is_none_or(|at| at.elapsed() >= ACK_GC_RETRY_INTERVAL))
        {
            let acknowledged = self
                .client
                .acknowledge(cursor.ordinal, &cursor.digest)
                .await?;
            self.last_ack_sent = Some(Instant::now());
            // The reply carries the worker's global watermark. A newer ACK
            // requires reloading the projection before publishing any state.
            if acknowledged.ordinal > cursor.ordinal {
                return Err(ProjectionAdvancedError {
                    required: acknowledged,
                }
                .into());
            }
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
        if operational.acknowledged_through != cursor.ordinal
            || operational.acknowledged_digest != cursor.digest
        {
            operational.acknowledged_through = cursor.ordinal;
            operational.acknowledged_digest = cursor.digest;
        }
        if self.operational != operational {
            operational.share_unchanged_fields(&self.operational);
            self.operational = operational;
        }
        Ok(())
    }

    pub async fn repair_projection(&mut self) -> Result<()> {
        let session_id = self.materialized.session_id.clone();
        let record = mj_core::runtime::spawn_blocking(move || {
            crate::database::load_session_record(&session_id)
        })
        .await
        .context("projection repair record reader failed")??
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
            self.install_repaired_projection(replacement).await?;
            return Ok(());
        };
        let checkpoint_path = checkpoint.archive_path.clone();
        let archive = mj_core::runtime::spawn_blocking(move || {
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
        self.install_repaired_projection(replacement).await?;
        Ok(())
    }

    async fn install_repaired_projection(
        &mut self,
        replacement: MaterializedSession,
    ) -> Result<()> {
        let ordinal = self.materialized.applied_event_ordinal;
        let digest = self.materialized.applied_event_digest.clone();
        let installed = mj_core::runtime::spawn_blocking(move || {
            crate::database::repair_materialized_session_if_current(&replacement, ordinal, &digest)
        })
        .await
        .context("projection repair writer failed")??;
        // Reload either the repair or the concurrent winner; never replay from
        // a replacement that lost its atomic writer decision.
        (self.materialized, self.window) = load_projection(&self.materialized.session_id).await?;
        if !installed {
            tracing::debug!(session_id = %self.materialized.session_id,
                "projection advanced during repair; using the durable winner");
        }
        self.window.trim(
            &mut self.materialized,
            crate::database::PROJECTION_TAIL_ITEMS,
        );
        Ok(())
    }

    /// The relay state as of the last sync, without copying the projection.
    pub fn operational(&self) -> &RelayOperationalState {
        &self.operational
    }

    pub fn snapshot(&self) -> ManagedSessionSnapshot {
        ManagedSessionSnapshot {
            window: self.window.clone(),
            materialized: self.materialized.clone(),
            operational: self.operational.clone(),
            latest_credential_sync_signal: self.latest_credential_sync_signal.clone(),
            worker_build: self.client.worker_build().map(str::to_owned),
            subagent_requests: self.subagent_requests.clone(),
            subagent_completions: self.subagent_completions.clone(),
            subagent_queue_error: self.subagent_queue_error.clone(),
        }
    }

    pub async fn complete_subagent_request(
        &mut self,
        result: mj_core::subagent::SubagentToolResult,
    ) -> Result<bool> {
        self.client.complete_subagent_request(result).await
    }

    pub async fn set_subagent_admission(&mut self, open: bool) -> Result<()> {
        self.client.set_subagent_admission(open).await
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

    pub async fn update_command_retention(
        &mut self,
        operation_id: Option<String>,
        command_ids: std::collections::BTreeSet<String>,
        reconcile_legacy: bool,
    ) -> Result<()> {
        self.client
            .update_command_retention(operation_id, command_ids, reconcile_legacy)
            .await
    }

    /// The mail the worker holds at a ready checkpoint barrier's cut.
    pub async fn read_checkpoint_mailbox(
        &mut self,
        barrier_command_id: String,
        cut: RelayCursor,
    ) -> Result<mj_core::archive::CanonicalMailbox> {
        self.client
            .read_checkpoint_mailbox(barrier_command_id, cut)
            .await
    }

    /// The command the worker's relay holds for a queued entry. Not tied to a
    /// checkpoint cut; a caller that needs one proves it with a cut-bound read.
    pub async fn read_queued_command(
        &mut self,
        command_id: String,
    ) -> Result<Option<RelayCommand>> {
        Ok(self.client.read_queued_command(command_id).await?.0)
    }

    /// The worker-owned native-session-use evidence at a ready checkpoint barrier's cut.
    pub async fn read_checkpoint_native_use(
        &mut self,
        barrier_command_id: String,
        cut: RelayCursor,
    ) -> Result<Option<mj_core::relay::NativeSessionUseEvidence>> {
        self.client
            .read_checkpoint_native_use(barrier_command_id, cut)
            .await
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

    /// Persist relay-private context for the next real prompt through the
    /// journaled command stream, then catch the projection up to its receipt.
    pub async fn install_prompt_context(&mut self, text: String) -> Result<()> {
        self.client.install_prompt_context(text).await?;
        self.sync_in_place().await?;
        Ok(())
    }

    /// Apply one relay transport page in bounded durable chunks. A transport
    /// page can contain thousands of events, but SQLite has one global writer;
    /// regularly releasing it lets other session actors keep their views
    /// current. The relay GC watermark advances only after the complete page.
    pub async fn apply_event_page(&mut self, page: RelayEventPage) -> Result<RelayCursor> {
        let now = std::time::SystemTime::now();
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        // Reconnect pages can replay events not yet projected before restart.
        // Only failures completed in the last backoff window create marks.
        let history_floor_ms = now_ms.saturating_sub(15 * 60 * 1000);
        self.cache_event_attachments(&page.events).await?;

        let RelayEventPage {
            events,
            through_ordinal,
            through_digest,
        } = page;
        let event_count = events.len();
        let transaction_count = event_count.div_ceil(PROJECTION_TRANSACTION_EVENT_BUDGET);
        let started = Instant::now();
        for events in events.chunks(PROJECTION_TRANSACTION_EVENT_BUDGET) {
            let prior_ordinal = self.materialized.applied_event_ordinal;
            let mut health_observations = Vec::new();
            for event in events {
                if event.ordinal <= prior_ordinal || event.recorded_at_ms < history_floor_ms {
                    continue;
                }
                let mj_core::relay::RelayObservation::CommandCompleted {
                    command_id,
                    outcome:
                        mj_core::relay::RelayCommandOutcome::Prompt {
                            stop_reason,
                            diagnostic,
                            ..
                        },
                    ..
                } = &event.observation
                else {
                    continue;
                };
                let result = mj_core::state::TurnOutcomeKind::Completed {
                    stop_reason: stop_reason.clone(),
                }
                .result();
                match result.kind {
                    mj_core::event_outcome::TurnResultKind::Failed => {
                        if let Some(diagnostic) = diagnostic
                            && let Some(fault) = crate::profile_health::ProfileFault::of(diagnostic)
                        {
                            health_observations.push((
                                command_id.clone(),
                                crate::profile_health::TurnHealth::Failed(
                                    fault,
                                    diagnostic.clone(),
                                ),
                            ));
                        }
                    }
                    mj_core::event_outcome::TurnResultKind::Completed => {
                        if let Ok(millis) = u64::try_from(event.recorded_at_ms) {
                            health_observations.push((
                                command_id.clone(),
                                crate::profile_health::TurnHealth::Succeeded(
                                    std::time::SystemTime::UNIX_EPOCH
                                        + std::time::Duration::from_millis(millis),
                                ),
                            ));
                        }
                    }
                    mj_core::event_outcome::TurnResultKind::InputRequired
                    | mj_core::event_outcome::TurnResultKind::Cancelled
                    | mj_core::event_outcome::TurnResultKind::Rejected
                    | mj_core::event_outcome::TurnResultKind::Interrupted => {}
                }
            }
            let session_id = self.materialized.session_id.clone();
            let profile_health = crate::profile_health::shared();
            for (command_id, observation) in health_observations {
                // Skip the lookup for a success when no mark could change.
                if matches!(observation, crate::profile_health::TurnHealth::Succeeded(_))
                    && !profile_health
                        .lock()
                        .map_err(|_| anyhow::anyhow!("profile health lock poisoned"))?
                        .has_marks(now)
                {
                    continue;
                }
                let lookup_session = session_id.clone();
                let lookup_command = command_id.clone();
                let identity = mj_core::runtime::spawn_blocking(move || {
                    crate::database::load_session_turn_profile_model(
                        &lookup_session,
                        &lookup_command,
                    )
                })
                .await
                .context("profile health turn identity lookup task failed")
                .and_then(|result| result);
                let identity = match identity {
                    Ok(identity) => identity,
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session_id,
                            command_id,
                            %error,
                            "could not load profile health turn identity before projection commit"
                        );
                        continue;
                    }
                };
                let Some((profile_id, stored_model)) = identity else {
                    tracing::debug!(
                        session_id = %session_id,
                        command_id,
                        "profile health has no stored turn selection"
                    );
                    continue;
                };
                let model = stored_model.or_else(|| {
                    mj_core::acp::surface::AcpSessionSurface::from_configuration(
                        &self.materialized.configuration.values,
                    )
                    .current_model()
                    .map(str::to_owned)
                });
                let Some(model) = model else {
                    tracing::debug!(
                        session_id = %session_id,
                        command_id,
                        "profile health has no model for the completed turn"
                    );
                    continue;
                };
                // The provider failure happened even if the following projection commit fails.
                profile_health
                    .lock()
                    .map_err(|_| anyhow::anyhow!("profile health lock poisoned"))?
                    .observe(&profile_id, &model, observation, now);
            }
            let events = events.to_vec();
            let projection = self.materialized.clone();
            let mut window = self.window.clone();
            // Projection is CPU work and its durable page uses synchronous
            // SQLite. Keep both off the async actor runtime so independent
            // sessions stay responsive during each bounded catch-up chunk.
            let (projection, window, credential_sync_signal) = mj_core::runtime::spawn_blocking(
                move || -> Result<(MaterializedSession, mj_core::state::ProjectionWindow, Option<CredentialSyncSignal>)> {
                    // The in-memory projection advances on a working copy and
                    // is published only once its page is durable.
                    let mut projection = projection;
                    if window.omitted_items > 0 {
                        let mut historical = crate::database::load_projection_references(&projection, &events)?;
                        window.omitted_items = window.omitted_items.checked_sub(historical.len())
                            .context("projection reference count exceeds omitted history")?;
                        historical.append(&mut projection.transcript);
                        historical.sort_by(|a,b| (a.position, &a.stable_id).cmp(&(b.position, &b.stable_id)));
                        projection.transcript = historical;
                    }
                    let hydrated = hydrate_referenced_tool_details(&mut projection, &events)?;
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
                                login_fingerprint: relay_event_login_fingerprint(event),
                            });
                        }
                    }
                    drop(projection_index);
                    if !hydrated.is_empty() {
                        for item in &mut projection.transcript {
                            if let Some((summary, full)) = hydrated.get(&item.stable_id)
                                && Arc::ptr_eq(item, full)
                            {
                                *item = Arc::clone(summary);
                            }
                        }
                    }
                    apply_projection_page(&session_id, move |committed| {
                        for (ordinal, previous_digest, digest, mutation) in prepared {
                            match committed.apply(ordinal, &previous_digest, &digest, &mutation)? {
                                ProjectionApplyOutcome::Applied => {}
                                ProjectionApplyOutcome::AlreadyApplied => {
                                    return Err(ProjectionAdvancedError {
                                        required: RelayCursor { ordinal, digest },
                                    }
                                    .into());
                                }
                            }
                        }
                        window.trim(&mut projection, crate::database::PROJECTION_TAIL_ITEMS);
                        committed.set_resolved_title(window.resolved_title(&projection).as_deref())?;
                        Ok((projection, window, credential_sync_signal))
                    })
                },
            )
            .await
            .context("relay projection page task failed")??;
            self.materialized = projection;
            self.window = window;
            if let Some(signal) = credential_sync_signal {
                self.latest_credential_sync_signal = Some(signal);
            }
        }
        tracing::debug!(target: "mj_controller::latency", session_id = %self.materialized.session_id,
            through_ordinal, event_count, transaction_count, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "projection page committed");
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
        if event_count != 0 && self.materialized.execution == MaterializedExecutionState::Idle {
            let session_id = self.materialized.session_id.clone();
            match mj_core::runtime::spawn_blocking(move || {
                crate::database::spill_and_trim_settled_tools_versioned(
                    &session_id,
                    delivered_through,
                )
            })
            .await
            .context("tool detail spill task failed")
            .and_then(std::convert::identity)
            {
                Ok(spilled) if !spilled.is_empty() => {
                    let mut spilled: HashMap<_, _> = spilled
                        .into_iter()
                        .map(|item| (item.stable_id.clone(), item))
                        .collect();
                    for item in &mut self.materialized.transcript {
                        if let Some(compact) = spilled.remove(&item.stable_id)
                            && item.last_changed_at_ms == compact.last_changed_at_ms
                            && item.latest_content_event_ordinal
                                == compact.latest_content_event_ordinal
                        {
                            *item = Arc::new(compact);
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    session_id = %self.materialized.session_id, error = %format!("{error:#}"),
                    "could not spill settled tool details at committed relay frontier"
                ),
            }
        }
        Ok(RelayCursor {
            ordinal: delivered_through,
            digest: self.materialized.applied_event_digest.clone(),
        })
    }

    /// Reconcile this worker's project-memory replica at an explicit durable
    /// boundary. Normal relay attachment and polling must never perform this
    /// filesystem work: a degraded target could otherwise turn reconnects
    /// into an unbounded queue of timed-out snapshot writes.
    ///
    /// The whole transaction runs on its own relay connection, opened from
    /// `memory_relay` (the bulk lane, see
    /// `targets::project_memory_relay_command`), and closed when the
    /// transaction ends. Paged snapshots are owned by their connection, so
    /// one connection carries all of it, and its frames of up to 8 MiB never
    /// queue ahead of other sessions' relay RPCs on the shared control
    /// connection. The checkpoint barrier stays on this session's own
    /// connection. A failure to open the connection is reported, not retried
    /// over the control connection.
    pub async fn sync_project_memory(
        &mut self,
        memory_relay: &CommandSpec,
        config: &mj_core::config::Config,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let Some(target) = self.project_memory.clone() else {
            return Ok(());
        };
        let mut client = tokio::select! {
            biased;
            () = cancel.cancelled() => anyhow::bail!("project-memory synchronization for checkpoint was cancelled"),
            client = RelayClient::connect(memory_relay, &self.materialized.session_id) => client
                .context("open the project-memory relay connection")?,
        };
        let result = self
            .sync_project_memory_on(&mut client, &target, config, cancel)
            .await;
        if let Err(error) = client.detach().await {
            tracing::warn!(
                session_id = self.materialized.session_id,
                error = format!("{error:#}"),
                "could not close the project-memory relay connection"
            );
        }
        result
    }

    async fn sync_project_memory_on(
        &mut self,
        client: &mut RelayClient,
        target: &ProjectMemorySyncTarget,
        config: &mj_core::config::Config,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        if !client.supports_project_memory_sync() {
            tracing::warn!(
                session_id = self.materialized.session_id,
                "worker protocol predates project-memory synchronization; preserving memory through checkpoints only"
            );
            self.project_memory = None;
            return Ok(());
        }
        let snapshot = tokio::select! {
            biased;
            () = cancel.cancelled() => anyhow::bail!("project-memory synchronization for checkpoint was cancelled"),
            result = client.project_memory_snapshot() => result,
        };
        let (baseline, replica) = match snapshot {
            Ok(snapshot) => snapshot,
            // Upgraded workers receive the current matching memory launch config.
            // Transfer-state errors on their new transport must remain visible.
            Err(error)
                if client.protocol_version()
                    < mj_core::relay::RELAY_PROJECT_MEMORY_TRANSFER_PROTOCOL
                    && error
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
        let resolver = crate::project_memory_merge::UtilityModelConflictResolver::new(config);
        let sync = crate::project_memory_merge::merge_and_swap_canonical(
            &target.canonical_root,
            &baseline,
            &replica,
            &resolver,
            cancel.clone(),
        )
        .await?;
        let sync = match sync {
            crate::project_memory_merge::CanonicalSyncOutcome::Swapped(sync) => sync,
            crate::project_memory_merge::CanonicalSyncOutcome::AttemptsExhausted { attempts } => {
                tracing::warn!(
                    session_id = self.materialized.session_id,
                    attempts,
                    "project memory changed during every canonical compare-and-swap attempt; checkpoint will continue"
                );
                return Ok(());
            }
        };
        for resolution in &sync.resolutions {
            tracing::warn!(
                session_id = self.materialized.session_id,
                path = %resolution.conflict.path,
                resolution = resolution.method.as_str(),
                "project memory conflict resolved"
            );
            tracing::debug!(
                session_id = self.materialized.session_id,
                path = %resolution.conflict.path,
                base = ?resolution.conflict.base,
                canonical = %resolution.conflict.canonical,
                replica = %resolution.conflict.replica,
                replica_wins = %resolution.conflict.replica_wins,
                "project memory conflict source text"
            );
        }
        install_project_memory_and_notify(
            client,
            &self.materialized.session_id,
            &baseline,
            &replica,
            sync,
            &cancel,
        )
        .await
    }
}

pub(super) async fn install_project_memory_and_notify<W: ProjectMemoryWorker>(
    worker: &mut W,
    session_id: &str,
    baseline: &ProjectMemorySnapshot,
    replica: &ProjectMemorySnapshot,
    sync: CanonicalSync,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let install = tokio::select! {
        biased;
        () = cancel.cancelled() => anyhow::bail!("project-memory synchronization for checkpoint was cancelled"),
        result = install_merged_tree(worker, baseline, replica, sync.tree.clone()) => result?,
    };
    if install == Some(WorkerInstallOutcome::ReplicaChanged) {
        tracing::debug!(
            %session_id,
            "worker project-memory replica changed after the snapshot; skipped installing the merged tree"
        );
    }
    let Some(notice) = install.and_then(|install| sync.replica_change_notice(replica, install))
    else {
        return Ok(());
    };

    // Give each successful installation its own occurrence key so recurring
    // A -> B -> A -> B transitions are never suppressed by prior accepted rows;
    // mailbox delivery reuses this persisted key across all worker retries.
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("generate project-memory notice nonce: {error}"))?;
    let key = format!(
        "project-memory:{session_id}:{}:{}:{}",
        notice.before.0,
        notice.after.0,
        mj_core::hex::lower_hex(nonce),
    );
    let event = MailboxEvent {
        key: key.clone(),
        source: "project-memory".into(),
        wake: false,
        created_at_ms: chrono::Utc::now()
            .timestamp_millis()
            .try_into()
            .unwrap_or_default(),
        body: MailboxEventBody::PlainText { text: notice.text },
    };
    let target = session_id.to_owned();
    let queued = mj_core::runtime::spawn_blocking(move || {
        let json = serde_json::to_string(&event)?;
        let outcome =
            crate::database::enqueue_mailbox_event(&event.key, &target, &json, false, false)?;
        ensure!(
            outcome != crate::database::Enqueued::TargetGone,
            "target session no longer exists"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("project-memory notice enqueue task failed")
    .and_then(|result| result);
    if let Err(error) = queued {
        tracing::warn!(%session_id, event_key = %key, error = %format!("{error:#}"),
            "could not enqueue project-memory change notice after installing the replica");
    }
    Ok(())
}

pub fn relay_desynchronized(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<RelayRejected>()
            .is_some_and(RelayRejected::is_desynchronized)
    })
}

pub fn projection_integrity_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ProjectionIntegrityError>().is_some())
}

async fn execute_guardian_review(
    request: GuardianReviewRequest,
    classifier: Option<Arc<dyn GuardianStage1>>,
) -> GuardianReviewResult {
    tracing::info!(
        target: "mj_guardian",
        session_id = %request.session_id,
        tool_call_id = %request.tool_call_id,
        request_id = %request.request_id,
        "daemon guardian started reviewing a permission request"
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let result = tokio::time::timeout(
        GUARDIAN_REVIEW_TIMEOUT,
        run_guardian_review(&request, classifier, cancel),
    )
    .await
    .unwrap_or_else(|_| Err(DaemonFailure::new(DaemonGuardianFailure::TimedOut)));
    let decision = match result {
        Ok(decision) => decision,
        Err(failure) => {
            let reason = failure.kind.as_str();
            tracing::warn!(
                target: "mj_guardian",
                session_id = %request.session_id,
                tool_call_id = %request.tool_call_id,
                request_id = %request.request_id,
                reason,
                config_line = failure.config_location.map(|(line, _)| line),
                config_column = failure.config_location.map(|(_, column)| column),
                "daemon guardian review failed"
            );
            GuardianReviewOutcome::AskPerson {
                reason: reason.into(),
            }
        }
    };
    GuardianReviewResult {
        request_id: request.request_id,
        decision,
    }
}

struct DaemonFailure {
    kind: DaemonGuardianFailure,
    config_location: Option<(usize, usize)>,
}

impl DaemonFailure {
    fn new(kind: DaemonGuardianFailure) -> Self {
        Self {
            kind,
            config_location: None,
        }
    }
}

async fn run_guardian_review(
    request: &GuardianReviewRequest,
    classifier: Option<Arc<dyn GuardianStage1>>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<GuardianReviewOutcome, DaemonFailure> {
    let config = match mj_core::runtime::spawn_blocking(mj_core::config::Config::load).await {
        Ok(Ok(config)) => config,
        Ok(Err(error)) => {
            let config_error = error.downcast_ref::<ConfigLoadError>();
            let kind = match config_error {
                Some(ConfigLoadError::Toml { .. }) => DaemonGuardianFailure::ConfigInvalidToml,
                Some(ConfigLoadError::Schema { .. }) => DaemonGuardianFailure::ConfigInvalidSchema,
                Some(ConfigLoadError::NewerSchema { .. }) => {
                    DaemonGuardianFailure::ConfigNeedsUpgrade
                }
                None if error.is::<std::io::Error>() => DaemonGuardianFailure::ConfigUnreadable,
                None => DaemonGuardianFailure::ConfigLoadFailed,
            };
            return Err(DaemonFailure {
                kind,
                config_location: config_error.and_then(ConfigLoadError::location),
            });
        }
        Err(_) => return Err(DaemonFailure::new(DaemonGuardianFailure::ConfigLoadFailed)),
    };
    if !config.guardian.enabled {
        return Ok(GuardianReviewOutcome::AskPerson {
            reason: GUARDIAN_DISABLED_REASON.into(),
        });
    }
    let mut stage1_score = None;
    if config.guardian.local_classifier
        && let Some(classifier) = classifier
    {
        match classifier.classify(request).await {
            GuardianStage1Verdict::Scored {
                unsafe_prob,
                tokens,
                elapsed_ms,
            } => {
                let local_allow = (0.0..=WOLF_STAGE1_ALLOW_THRESHOLD).contains(&unsafe_prob);
                tracing::info!(
                    target: "mj_guardian",
                    stage = "stage1_wolf",
                    unsafe_prob,
                    threshold = WOLF_STAGE1_ALLOW_THRESHOLD,
                    tokens,
                    elapsed_ms,
                    local_allow,
                    "Wolf Defender Jev scored a permission request"
                );
                stage1_score = Some(unsafe_prob);
                if local_allow {
                    return Ok(GuardianReviewOutcome::Allow {
                        stage: "stage1_wolf".into(),
                        stage1_score,
                    });
                }
            }
            GuardianStage1Verdict::Escalated(reason) => {
                tracing::debug!(target: "mj_guardian", ?reason, "Wolf skipped Stage 1 inference");
            }
            GuardianStage1Verdict::Overflow { tokens } => {
                tracing::debug!(target: "mj_guardian", tokens, "Wolf case token limit exceeded");
            }
            GuardianStage1Verdict::Unavailable { reason } => {
                tracing::debug!(target: "mj_guardian", %reason, "Wolf classifier unavailable");
            }
        }
    }
    let decision = match review_guardian_with_utility(&config, request, cancel).await {
        GuardianUtilityOutcome::Reviewed { assessment, .. } => {
            let stage = "utility_llm".into();
            match assessment.enforced_outcome() {
                Outcome::Allow => GuardianReviewOutcome::Allow {
                    stage,
                    stage1_score,
                },
                Outcome::Deny => GuardianReviewOutcome::Deny {
                    rationale: assessment.rationale,
                    risk_level: assessment.risk_level,
                    user_authorization: assessment.user_authorization,
                    stage,
                    stage1_score,
                },
            }
        }
        GuardianUtilityOutcome::NoUtilityModel { .. } => {
            GuardianReviewOutcome::UseAcpFallback { stage1_score }
        }
        GuardianUtilityOutcome::Failed { error } => {
            return Err(DaemonFailure::new(error));
        }
    };
    Ok(decision)
}

type TemporarilyHydratedItems = HashMap<String, (Arc<TranscriptItem>, Arc<TranscriptItem>)>;

/// Supply full inputs only to mutations that can depend on them. Restore the
/// compact Arc after projection if no observation actually changed the item.
fn hydrate_referenced_tool_details(
    projection: &mut MaterializedSession,
    events: &[mj_core::relay::RelayEvent],
) -> Result<TemporarilyHydratedItems> {
    let mut hydrated = HashMap::new();
    if !projection
        .transcript
        .iter()
        .any(|item| item.body.detail_trimmed())
    {
        return Ok(hydrated);
    }
    let references = mj_transcript::projection::historical_references(events)?;
    let mut ids: HashSet<_> = references.ids.into_iter().collect();
    if !references.terminals.is_empty() {
        let index = ProjectionIndex::new(projection);
        for terminal in references.terminals {
            ids.extend(
                index
                    .terminal_referrers(&terminal)
                    .map(|item| item.stable_id.clone()),
            );
        }
    }
    for item in &mut projection.transcript {
        if !item.body.detail_trimmed() || !ids.contains(&item.stable_id) {
            continue;
        }
        match crate::database::read_stored_tool_detail(&projection.session_id, item) {
            Ok(Some(body)) => {
                let mut full = (**item).clone();
                full.body = body;
                let full = Arc::new(full);
                hydrated.insert(
                    item.stable_id.clone(),
                    (Arc::clone(item), Arc::clone(&full)),
                );
                *item = full;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                session_id = %projection.session_id,
                stable_id = %item.stable_id,
                error = %format!("{error:#}"),
                "could not hydrate trimmed tool detail before relay update"
            ),
        }
    }
    Ok(hydrated)
}

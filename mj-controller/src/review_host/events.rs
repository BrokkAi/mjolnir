use super::*;

/// What the host's task processes, in order.
pub(super) enum HostEvent {
    View {
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
        /// Whether this view had a prompt of ours in flight. Only a turn that
        /// answered a prompt arms an automatic review.
        prompt_driven: bool,
        finished_turn: bool,
    },
    /// Drop the retained last-view (with its full transcript) for every session
    /// no longer in the live set. Without this, `sessions` keeps a
    /// `MaterializedSession` per session ever observed and never releases it.
    Retain {
        live: std::collections::BTreeSet<String>,
    },
    Start {
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    },
    Prepared {
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
        prepared: Result<Prepared, StartRefusal>,
    },
    RetryRecovery {
        session_id: String,
    },
    RecoveryPrepared {
        session_id: String,
        prepared: Result<Option<Prepared>, String>,
    },
    RetryCheckpoint {
        session_id: String,
        epoch: u64,
        revision: u64,
    },
    RetryClose {
        session_id: String,
        epoch: u64,
    },
    StateSaved {
        session_id: String,
        completion: PersistenceCompletion,
        result: Result<(), String>,
    },
    /// One asynchronous step of a review that was open when it started.
    ///
    /// `epoch` is which review asked. mjolnir's orchestrator tags every review
    /// outcome with one and drops the ones that no longer match
    /// (`mj-core/src/orchestrator.rs`, `review_outcome_rx`), because a result
    /// arriving after its review was cancelled would otherwise be applied to
    /// whatever review is open now. Session id alone is not enough: a session
    /// can start its next review immediately.
    Step {
        session_id: String,
        epoch: u64,
        step: ReviewStep,
    },
    Resolve {
        session_id: String,
        resolution: Resolution,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Reviews a daemon restart interrupted, so each session's conversation
    /// says what happened to it.
    Initialized {
        result: Result<Vec<String>, String>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PersistenceCompletion {
    Open { epoch: u64 },
    Close { epoch: u64 },
    Checkpoint { epoch: u64, revision: u64 },
}

pub(super) enum PersistenceRequest {
    DiscoverOwned,
    Save {
        session_id: String,
        state: Box<TurnReviewState>,
        completion: Option<PersistenceCompletion>,
    },
}

/// One asynchronous step's result, belonging to exactly one review.
pub(super) enum ReviewStep {
    ReceiptSettled {
        command_id: String,
        phase: super::durable::ReceiptPhase,
        result: Result<Option<Box<mj_core::relay::HandledRelayCommand>>, String>,
    },
    RetryEffect {
        key: String,
    },
    EffectSettled {
        key: String,
        result: Result<(), String>,
    },
    Delta(Result<Vec<mj_core::relay::RepoDelta>, String>),
    Analysis(Result<String, String>),
    RoleStarted {
        role: String,
        result: Result<(), String>,
    },
    RolePrompted {
        role: String,
        result: Result<(), String>,
    },
    PrimaryPrompted(Result<(), String>),
    RoleEvents {
        role: String,
        result: Result<Vec<RelayEvent>, String>,
    },
    Dispatches(Result<Vec<mj_core::relay::ReviewerLaneDispatch>, String>),
    DispatchesAcked {
        ids: Vec<String>,
        result: Result<(), String>,
    },
}

/// Everything one blocking preparation gathered before a review can start.
pub(super) struct Prepared {
    pub(super) state: TurnReviewState,
    pub(super) reviewer: ReviewerIdentity,
    pub(super) tier: ReviewTier,
    /// Read from the live actor after the admission hold is installed and a
    /// reviewer status command drains every actor command ahead of it.
    pub(super) materialized: Box<MaterializedSession>,
    /// Present only while startup reconciles an interrupted corrective
    /// handoff. Such a review skips reviewer processes and retries the exact
    /// primary command id.
    pub(super) resume_forward: Option<PendingForward>,
    /// What the turn changed, when preparation captured it before choosing a
    /// reviewer. The review starts from this capture instead of asking again.
    pub(super) captured: Option<Vec<mj_core::relay::RepoDelta>>,
}

pub(super) struct PendingOpen {
    pub(super) epoch: u64,
    pub(super) manual: bool,
    pub(super) reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    pub(super) prepared: Prepared,
}

pub(super) type ReviewerIdentity = mj_core::review::settings::ResolvedReviewSettings;

/// One open review and its execution context.
pub(super) struct ReviewSlot {
    /// Which review this is. Asynchronous results name it, and results that
    /// name another are dropped.
    pub(super) epoch: u64,
    pub(super) driver: TurnReviewDriver,
    /// One transcript projection per reviewing role, which is how the host
    /// reads a role's answer out of its own relay journal.
    pub(super) roles: BTreeMap<String, RoleTranscript>,
    pub(super) reviewer: ReviewerIdentity,
    pub(super) state: TurnReviewState,
    /// The sidecar reads a new generation as "this is a different reviewer".
    /// Fresh role launches receive a random nonce, so a later review cannot
    /// reuse the native conversation left by an earlier one.
    pub(super) generation: u64,
    pub(super) role_generations: BTreeMap<String, u64>,
    pub(super) outbox: Vec<ReviewRequest>,
    pub(super) receipts: BTreeMap<String, super::durable::ReviewReceipt>,
    pub(super) running_effects: BTreeSet<String>,
    pub(super) delivery_errors: BTreeMap<String, String>,
    pub(super) polling_roles: BTreeSet<String>,
    pub(super) reading_dispatches: bool,
    pub(super) pending_supervisor: Option<Vec<RelayEvent>>,
    pub(super) dispatch_after_completion: bool,
    pub(super) accepted_dispatches: BTreeSet<String>,
    pub(super) pending_dispatch_acks: BTreeSet<String>,
}

/// One role's journal, folded far enough to read its final answer.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct RoleTranscript {
    pub(super) session: Option<MaterializedSession>,
    pub(super) cursor_ordinal: u64,
    pub(super) cursor_digest: String,
}

impl RoleTranscript {
    pub(super) fn apply(&mut self, session_id: &str, events: &[RelayEvent]) -> Result<(), String> {
        let mut next = self.clone();
        let session = next
            .session
            .get_or_insert_with(|| MaterializedSession::empty(session_id));
        for event in events {
            let projected = mj_transcript::projection::project_relay_event(session, event)
                .map_err(|error| format!("project reviewer event {}: {error}", event.ordinal))?;
            mj_transcript::projection::apply_committed_projection_event(
                session,
                event,
                projected.mutation,
            )
            .map_err(|error| format!("apply reviewer event {}: {error}", event.ordinal))?;
            next.cursor_ordinal = event.ordinal;
            next.cursor_digest.clone_from(&event.digest);
        }
        *self = next;
        Ok(())
    }

    /// The role's latest complete answer, which is what the driver reads. Tool
    /// logs and reasoning are deliberately not part of it.
    pub(super) fn latest_answer(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        session
            .transcript
            .iter()
            .rev()
            .find(|item| item.is_nonempty_agent_message())
            .and_then(|item| {
                let mj_core::state::TranscriptBody::Agent { chunks, .. } = &item.body else {
                    return None;
                };
                Some(mj_core::transcript::materialized_chunks_text(chunks))
            })
            .filter(|text| !text.trim().is_empty())
    }
}

pub(super) struct HostState {
    pub(super) control: SessionManagerControl,
    pub(super) config: ReviewConfigSource,
    pub(super) environment: Arc<dyn ReviewEnvironment>,
    pub(super) shared: Arc<HostShared>,
    pub(super) events: mpsc::Sender<HostEvent>,
    pub(super) persistence: Option<mpsc::UnboundedSender<PersistenceRequest>>,
    pub(super) persistence_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) reviews: BTreeMap<String, ReviewSlot>,
    /// Sessions whose review is being prepared. Preparation is asynchronous,
    /// so without this an automatic trigger and a manual `/review` racing each
    /// other would both create a review and the second would overwrite the
    /// first.
    pub(super) preparing: BTreeSet<String>,
    /// Reviews whose durable active marker is being written. They are not
    /// visible and start no agents until that write succeeds.
    pub(super) pending_open: BTreeMap<String, PendingOpen>,
    /// Reviews whose durable active marker is being cleared. Their resolved
    /// view and prompt hold remain until the ordered write completes.
    pub(super) closing: BTreeSet<String>,
    /// Distinguishes reviews. Every asynchronous step carries the epoch of the
    /// review that asked for it, so a late result cannot land on its
    /// successor.
    pub(super) next_epoch: u64,
    /// The last view seen per session: its execution state, for the
    /// Running→Idle edge, and its materialized transcript, for the seed.
    pub(super) sessions: BTreeMap<String, SessionWatch>,
    /// Sessions already told that no reviewer is configured. One notice per
    /// session, not one per turn.
    pub(super) preparation_cancellation: BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>,
    /// Sessions whose durable handoff survived a restart and still needs the
    /// primary relay's idempotent acknowledgement reconciled.
    pub(super) recovery_candidates: BTreeSet<String>,
    pub(super) recovery_in_flight: BTreeSet<String>,
    pub(super) dirty: BTreeSet<String>,
    pub(super) checkpointing: BTreeMap<String, (u64, u64)>,
    pub(super) next_checkpoint_revision: u64,
    pub(super) persistence_errors: BTreeMap<String, String>,
    pub(super) start_replies: BTreeMap<String, oneshot::Sender<Result<(), StartRefusal>>>,
    pub(super) resolve_replies: BTreeMap<String, oneshot::Sender<Result<(), String>>>,
}

pub(super) struct SessionWatch {
    pub(super) execution: MaterializedExecutionState,
    /// Whether the view had a prompt of ours in flight.
    pub(super) prompt_driven: bool,
    pub(super) materialized: Option<Box<MaterializedSession>>,
}

impl HostEvent {
    fn blocked_session(&self) -> Option<&str> {
        match self {
            Self::View { session_id, .. }
            | Self::Step { session_id, .. }
            | Self::Start { session_id, .. }
            | Self::Resolve { session_id, .. } => Some(session_id),
            _ => None,
        }
    }
}

pub(super) async fn host_loop(mut state: HostState, mut events: mpsc::Receiver<HostEvent>) {
    let mut deferred = std::collections::VecDeque::<HostEvent>::new();
    loop {
        let ready = deferred.iter().position(|event| {
            event
                .blocked_session()
                .is_none_or(|id| !state.checkpointing.contains_key(id))
        });
        let mut event = if let Some(index) = ready {
            deferred.remove(index).expect("deferred event")
        } else {
            let shared = state.shared.clone();
            tokio::select! {
                event = events.recv() => { let Some(event) = event else { break; }; event },
                _ = shared.observation_ready.notified() => {
                    let event = shared.observations.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop();
                    let Some(event) = event else { continue; };
                    // Re-arm once, so each turn handles one session fairly.
                    shared.observation_ready.notify_one();
                    event
                }
            }
        };
        if let HostEvent::Start { session_id, .. } = &event {
            let observation = state
                .shared
                .observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(session_id);
            if let Some(observation) = observation {
                state.handle(observation).await;
                state.checkpoint_dirty();
            }
        }
        if event
            .blocked_session()
            .is_some_and(|id| state.checkpointing.contains_key(id))
        {
            match event {
                HostEvent::Start { reply, .. } => answer(reply, Err(StartRefusal("review state is being committed; retry shortly".into()))),
                HostEvent::Resolve { ref session_id, .. } if !deferred.iter().any(|item| matches!(item, HostEvent::Resolve { session_id: id, .. } if id == session_id)) => deferred.push_back(event),
                HostEvent::Resolve { reply, .. } => { let _ = reply.send(Err("another review resolution is already pending".into())); },
                HostEvent::View { ref session_id, .. } => {
                    // Only the newest observation is needed while a checkpoint commits.
                    if let Some(index) = deferred.iter().position(|item| matches!(item, HostEvent::View { session_id: id, .. } if id == session_id)) {
                        let previous = deferred.remove(index);
                        if matches!(previous, Some(HostEvent::View { finished_turn: true, .. }))
                            && let HostEvent::View { finished_turn, .. } = &mut event { *finished_turn = true; }
                    }
                    deferred.push_back(event);
                }
                // There is at most one response per outstanding bounded role effect.
                _ => deferred.push_back(event),
            }
            continue;
        }
        if state.handle(event).await {
            break;
        }
        state.checkpoint_dirty();
    }
}

pub(super) async fn persistence_loop(
    environment: Arc<dyn ReviewEnvironment>,
    events: mpsc::Sender<HostEvent>,
    mut requests: mpsc::UnboundedReceiver<PersistenceRequest>,
    stop_delivery: tokio_util::sync::CancellationToken,
) {
    while let Some(request) = requests.recv().await {
        match request {
            PersistenceRequest::DiscoverOwned => {
                let environment = environment.clone();
                let result = tokio::task::spawn_blocking(move || environment.recoverable_reviews())
                    .await
                    .map_err(|error| format!("review ownership discovery stopped: {error}"))
                    .and_then(|result| result);
                tokio::select! {
                    _ = stop_delivery.cancelled() => {},
                    _ = events.send(HostEvent::Initialized { result }) => {},
                }
            }
            PersistenceRequest::Save {
                session_id,
                state,
                completion,
            } => {
                let environment = environment.clone();
                let owner = session_id.clone();
                let result =
                    tokio::task::spawn_blocking(move || environment.save_state(&owner, &state))
                        .await
                        .map_err(|error| format!("review state persistence task stopped: {error}"))
                        .and_then(|result| result);
                if let Some(completion) = completion {
                    tokio::select! {
                        _ = stop_delivery.cancelled() => {},
                        _ = events.send(HostEvent::StateSaved { session_id, completion, result }) => {},
                    }
                } else if let Err(error) = result {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "could not record how far this session has been reviewed"
                    );
                }
            }
        }
    }
}

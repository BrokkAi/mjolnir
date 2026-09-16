use super::*;

/// What the host's task processes, in order.
pub(super) enum HostEvent {
    View {
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
        /// Whether this view had a prompt of ours in flight. Only a turn that
        /// answered a prompt arms an automatic review.
        prompt_driven: bool,
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
    RecoveryPrepared {
        session_id: String,
        prepared: Result<Option<Prepared>, String>,
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
    Interrupted { interrupted: Vec<String> },
    Shutdown {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PersistenceCompletion {
    Open,
    Forward,
    Close,
}

pub(super) enum PersistenceRequest {
    SweepInterrupted,
    Save {
        session_id: String,
        state: Box<TurnReviewState>,
        completion: Option<PersistenceCompletion>,
    },
    ClearActive {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// One asynchronous step's result, belonging to exactly one review.
pub(super) enum ReviewStep {
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
    Dispatches(Result<Vec<mj_core::review::lanes::ReviewSubagentRequest>, String>),
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
}

pub(super) struct PendingOpen {
    pub(super) epoch: u64,
    pub(super) manual: bool,
    pub(super) reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    pub(super) prepared: Prepared,
}

/// Which harness reviews, and how it is configured. Read from `[review]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReviewerIdentity {
    pub(super) profile: String,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
}

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
}

/// One role's journal, folded far enough to read its final answer.
#[derive(Default)]
pub(super) struct RoleTranscript {
    pub(super) session: Option<MaterializedSession>,
    pub(super) cursor_ordinal: u64,
    pub(super) cursor_digest: String,
}

impl RoleTranscript {
    pub(super) fn apply(&mut self, session_id: &str, events: &[RelayEvent]) {
        let session = self
            .session
            .get_or_insert_with(|| MaterializedSession::empty(session_id));
        for event in events {
            let Ok(projected) = mj_transcript::projection::project_relay_event(session, event)
            else {
                continue;
            };
            if mj_transcript::projection::apply_committed_projection_event(
                session,
                event,
                projected.mutation,
            )
            .is_err()
            {
                continue;
            }
            self.cursor_ordinal = event.ordinal;
            self.cursor_digest.clone_from(&event.digest);
        }
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
    pub(super) events: mpsc::UnboundedSender<HostEvent>,
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
    /// Primary handoff requests wait here until their durable pending record
    /// has been written. This prevents an accepted relay command from racing
    /// a failed SQLite write.
    pub(super) awaiting_forward_persistence: BTreeMap<String, Vec<ReviewRequest>>,
    /// Distinguishes reviews. Every asynchronous step carries the epoch of the
    /// review that asked for it, so a late result cannot land on its
    /// successor.
    pub(super) next_epoch: u64,
    /// The last view seen per session: its execution state, for the
    /// Running→Idle edge, and its materialized transcript, for the seed.
    pub(super) sessions: BTreeMap<String, SessionWatch>,
    /// Sessions already told that no reviewer is configured. One notice per
    /// session, not one per turn.
    pub(super) missing_reviewer_reported: BTreeSet<String>,
    /// Sessions whose durable handoff survived a restart and still needs the
    /// primary relay's idempotent acknowledgement reconciled.
    pub(super) recovery_candidates: BTreeSet<String>,
    pub(super) recovery_in_flight: BTreeSet<String>,
}

pub(super) struct SessionWatch {
    pub(super) execution: MaterializedExecutionState,
    /// Whether the view had a prompt of ours in flight.
    pub(super) prompt_driven: bool,
    pub(super) materialized: Option<Box<MaterializedSession>>,
}

pub(super) async fn host_loop(
    mut state: HostState,
    mut events: mpsc::UnboundedReceiver<HostEvent>,
) {
    while let Some(event) = events.recv().await {
        if state.handle(event).await {
            break;
        }
    }
}

pub(super) async fn persistence_loop(
    environment: Arc<dyn ReviewEnvironment>,
    events: mpsc::UnboundedSender<HostEvent>,
    mut requests: mpsc::UnboundedReceiver<PersistenceRequest>,
) {
    while let Some(request) = requests.recv().await {
        match request {
            PersistenceRequest::SweepInterrupted => {
                let environment = environment.clone();
                match tokio::task::spawn_blocking(move || environment.clear_interrupted()).await {
                    Ok(Ok(interrupted)) if !interrupted.is_empty() => {
                        let _ = events.send(HostEvent::Interrupted { interrupted });
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "could not clear interrupted reviews");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the interrupted-review sweep did not run");
                    }
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
                    let _ = events.send(HostEvent::StateSaved {
                        session_id,
                        completion,
                        result,
                    });
                } else if let Err(error) = result {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "could not record how far this session has been reviewed"
                    );
                }
            }
            PersistenceRequest::ClearActive { reply } => {
                let environment = environment.clone();
                let result = tokio::task::spawn_blocking(move || {
                    environment.clear_interrupted().map(|_| ())
                })
                .await
                .map_err(|error| format!("review shutdown persistence task stopped: {error}"))
                .and_then(|result| result);
                let _ = reply.send(result);
            }
        }
    }
}

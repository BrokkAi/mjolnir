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
    ReviewerFinished {
        session_id: String,
        child_session_id: String,
        answer: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ChildRegistered {
        session_id: String,
        epoch: u64,
        result: Result<String, String>,
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
    LaunchChild,
}

pub(super) enum PersistenceRequest {
    SweepInterrupted,
    CloseFailedChild {
        session_id: String,
        child: String,
    },
    AcknowledgeReviewer {
        session_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Save {
        session_id: String,
        state: Box<TurnReviewState>,
        completion: Option<PersistenceCompletion>,
    },
    /// Records that a review still preparing at shutdown was cut off, so the
    /// next daemon's startup sweep reports it like any other interrupted
    /// review. A preparing review has no state row of its own yet.
    MarkInterrupted {
        session_id: String,
    },
}

/// One asynchronous step's result, belonging to exactly one review.
pub(super) enum ReviewStep {
    Failed(String),
    Delta(Result<Vec<mj_core::relay::RepoDelta>, String>),
    PrimaryPrompted(Result<(), String>),
}

/// Everything one blocking preparation gathered before a review can start.
pub(super) struct Prepared {
    pub(super) state: TurnReviewState,
    pub(super) reviewer: ReviewerIdentity,
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
    /// Keeps recovery copies and worker upgrades off the session from
    /// preparation until the review closes; moved into the review's slot.
    pub(super) background: Option<BackgroundHold>,
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
    pub(super) reviewer: ReviewerIdentity,
    pub(super) state: TurnReviewState,
    /// Released when the slot is removed, which is how every review ends.
    pub(super) _background: Option<BackgroundHold>,
    pub(super) command_id: Option<String>,
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
    pub(super) preparation_cancellation: BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>,
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
    let mut failed_writes = BTreeMap::<String, String>::new();
    while let Some(request) = requests.recv().await {
        match request {
            PersistenceRequest::CloseFailedChild { session_id, child } => {
                if let Some(error) = failed_writes.get(&session_id) {
                    tracing::warn!(%session_id, %child, %error, "the reviewer failure could not be persisted before cleanup");
                }
                let environment = environment.clone();
                tokio::spawn(async move {
                    if let Err(error) = environment.close_child(&child).await {
                        tracing::warn!(%child, %error, "could not close failed reviewer child");
                    }
                });
            }
            PersistenceRequest::AcknowledgeReviewer { session_id, reply } => {
                let result = failed_writes.get(&session_id).cloned().map_or(Ok(()), Err);
                let _ = reply.send(result);
            }
            PersistenceRequest::SweepInterrupted => {
                let sweep = environment.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let children = sweep.orphaned_children()?;
                    sweep
                        .clear_interrupted()
                        .map(|interrupted| (interrupted, children))
                })
                .await;
                match result {
                    Ok(Ok((interrupted, children))) => {
                        let mut closes = tokio::task::JoinSet::new();
                        for child in children {
                            let environment = environment.clone();
                            closes.spawn(async move {
                                if let Err(error) = environment.close_child(&child).await { tracing::warn!(child, %error, "could not close orphaned reviewer child"); }
                            });
                        }
                        while let Some(result) = closes.join_next().await {
                            if let Err(error) = result {
                                tracing::warn!(%error, "orphaned reviewer cleanup stopped");
                            }
                        }
                        if !interrupted.is_empty() {
                            let _ = events.send(HostEvent::Interrupted { interrupted });
                        }
                    }
                    Ok(Err(error)) => tracing::warn!(%error, "could not clear interrupted reviews"),
                    Err(error) => {
                        tracing::warn!(%error, "the interrupted-review sweep did not run")
                    }
                }
            }
            PersistenceRequest::MarkInterrupted { session_id } => {
                let environment = environment.clone();
                let owner = session_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let mut state = environment.load_state(&owner)?;
                    state
                        .active
                        .get_or_insert_with(|| "review-preparing".to_owned());
                    environment.save_state(&owner, &state)
                })
                .await
                .map_err(|error| format!("review shutdown persistence task stopped: {error}"))
                .and_then(|result| result);
                if let Err(error) = result {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "could not record that a review was cut off by shutdown"
                    );
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
                match &result {
                    Ok(()) => {
                        failed_writes.remove(&session_id);
                    }
                    Err(error) => {
                        failed_writes.insert(session_id.clone(), error.clone());
                    }
                }
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
        }
    }
}

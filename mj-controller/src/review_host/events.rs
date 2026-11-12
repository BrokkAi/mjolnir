use super::*;

/// What the host's task processes, in order.
pub(super) enum HostEvent {
    View {
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
    },
    /// Drop the retained last-view (with its full transcript) for every session
    /// no longer in the live set. Without this, `sessions` keeps a
    /// `MaterializedSession` per session ever observed and never releases it.
    Retain {
        live: std::collections::BTreeSet<String>,
    },
    StartPush {
        session_id: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
    OverridePush {
        session_id: String,
        overrides: Vec<ReviewOverride>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
    SetPushBase {
        session_id: String,
        commit: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
    OverrideFinished {
        session_id: String,
    },
    Prepared {
        session_id: String,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
        prepared: Box<Result<Prepared, StartRefusal>>,
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
    /// Whether the session has a review open: being prepared, being opened,
    /// or running. The verdict is queued for delivery before the review
    /// closes, so a review that is gone has either queued its verdict or never
    /// produced one.
    HasOpenReview {
        session_id: String,
        reply: oneshot::Sender<Result<bool, crate::review_host::OpenReviewUnknown>>,
    },
    /// A finished reviewer child whose review no longer tracks it. The host
    /// owns every reviewer child close, so the delegation loop hands it here
    /// instead of closing it again itself.
    CloseChild {
        child_session_id: String,
    },
    /// A reviewer child's session reached a terminal state. The role it
    /// served fails unless it already has an outcome.
    ChildEnded {
        child_session_id: String,
        reason: String,
    },
    ReviewerFinished {
        session_id: String,
        child_session_id: String,
        report: ReviewerReport,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// An ended reviewer's failure could not be saved.
    ReviewerEndNotSaved {
        child_session_id: String,
        reason: String,
        error: String,
    },
    ReviewerOutcomeSaved {
        session_id: String,
        epoch: u64,
        requests: Vec<ReviewRequest>,
        result: Result<(), String>,
    },
    ChildRegistered {
        session_id: String,
        epoch: u64,
        role: String,
        result: Result<String, String>,
    },
    RoleFailed {
        session_id: String,
        epoch: u64,
        role: String,
        child_session_id: Option<String>,
        reason: String,
    },
    RetryPushDelivery {
        session_id: String,
        epoch: u64,
    },
    /// The store would not say whether a reviewer's session had ended, or
    /// what it handed back. The role stays pending until a later read answers.
    ReviewerStandingUnread {
        session_id: String,
        epoch: u64,
        role: String,
        origin: StandingRead,
        error: String,
    },
    RetryReviewerStanding {
        session_id: String,
        epoch: u64,
        role: String,
        origin: StandingRead,
    },
    QuestionTarget {
        session_id: String,
        child_session_id: String,
        question_id: String,
        reply: oneshot::Sender<Option<(String, String)>>,
    },
    Resolve {
        session_id: String,
        resolution: Resolution,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Reviews a daemon restart interrupted, so each session's conversation
    /// says what happened to it.
    Interrupted {
        interrupted: Vec<String>,
    },
    RestoreGroups {
        groups: Vec<(String, TurnReviewState)>,
        abandoned_children: Vec<String>,
    },
    StartupRecoveryFailed(String),
    Shutdown {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// Why a reviewer's standing is being read, which decides what happens once
/// a read answers after failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StandingRead {
    /// Launching a reviewer restored after a restart.
    Restored,
    /// The reviewer's session ended; a handback it saved is its outcome.
    Ended {
        child_session_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PersistenceCompletion {
    Open,
    Captured,
    Close,
    LaunchChild { role: String },
}

pub(super) enum PersistenceRequest {
    SweepInterrupted,
    Load {
        session_id: String,
        reply: oneshot::Sender<Result<TurnReviewState, String>>,
    },
    SaveAcknowledged {
        operation: &'static str,
        session_id: String,
        state: Box<TurnReviewState>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SaveReviewerOutcome {
        session_id: String,
        epoch: u64,
        state: Box<TurnReviewState>,
        requests: Vec<ReviewRequest>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    FinishPush(Box<PushCompletion>),
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
    Delta(Result<Vec<mj_core::relay::RepoDelta>, String>),
    PushDelivered(Result<(Box<TurnReviewState>, ReviewVerdict), String>),
    /// Delivery failed and the requesting session still exists, or could not
    /// be looked up: the delivery is tried again.
    PushRetry(String),
    /// A save failed because the requesting session's record is gone, so no
    /// verdict or review state can be saved for it.
    OwnerGone,
}

/// Everything one blocking preparation gathered before a review can start.
pub(super) struct Prepared {
    pub seed: TurnReviewSeed,
    pub(super) state: TurnReviewState,
    pub(super) reviewers: Vec<ReviewerIdentity>,
    /// Committed work captured before choosing reviewers.
    pub(super) captured: Vec<mj_core::relay::RepoDelta>,
    /// Keeps recovery copies and worker upgrades off the session from
    /// preparation until the review closes; moved into the review's slot.
    pub(super) background: Option<BackgroundHold>,
}

pub(super) struct PendingOpen {
    pub(super) epoch: u64,
    pub(super) reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    pub(super) prepared: Prepared,
    pub(super) driver: TurnReviewDriver,
    pub(super) requests: Vec<ReviewRequest>,
}

pub(super) type ReviewerIdentity = mj_core::review::settings::ResolvedReviewSettings;

/// What a reviewer child gave the host. Handbacks and ended sessions both
/// arrive as reports, so [`HostState::reviewer_finished`] decides every
/// reviewer outcome and failure one at a time, behind a saving outcome.
pub(super) enum ReviewerReport {
    /// The text of its handback.
    Answer(String),
    /// It ended without one; the reason says why.
    Failed(String),
}

pub(super) struct ReviewerHandback {
    pub(super) child_session_id: String,
    pub(super) report: ReviewerReport,
    pub(super) reply: oneshot::Sender<Result<(), String>>,
}

/// One open review and its execution context.
pub(super) struct ReviewSlot {
    /// Which review this is. Asynchronous results name it, and results that
    /// name another are dropped.
    pub(super) epoch: u64,
    pub(super) driver: TurnReviewDriver,
    pub(super) reviewers: Vec<ReviewerIdentity>,
    pub(super) state: TurnReviewState,
    /// Released when the slot is removed, which is how every review ends.
    pub(super) _background: Option<BackgroundHold>,
    pub(super) finishing: bool,
    pub(super) delivery: Arc<super::resolve::Delivery>,
    pub(super) retry_delay: std::time::Duration,
    pub(super) start_reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    pub(super) pending_outcome: Option<PendingOutcome>,
    pub(super) queued_outcomes: Vec<ReviewerHandback>,
    /// Reviewers that ended while their handback waited in
    /// `queued_outcomes`, with the reason. That handback is their outcome, so
    /// no standing read is made yet; the entry moves to the save of that
    /// handback, which reports the end again if the save fails.
    pub(super) ended_while_queued: BTreeMap<String, String>,
}

/// The outcome being saved: the state to return to if the save fails, and the
/// reviewer end that save is the only record of.
pub(super) struct PendingOutcome {
    pub(super) prior: TurnReviewState,
    /// The child and reason of an end noted while this handback was queued.
    pub(super) ended: Option<(String, String)>,
}

impl ReviewSlot {
    /// The wait before the next retry of anything this review retries
    /// (push delivery, a restored reviewer's standing read); each call
    /// doubles the next wait up to 30 s.
    pub(super) fn take_retry_delay(&mut self) -> std::time::Duration {
        let delay = self.retry_delay;
        self.retry_delay = (delay * 2).min(std::time::Duration::from_secs(30));
        delay
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
    pub(super) child_closures: tokio::task::JoinSet<Result<(), String>>,
    pub(super) child_closure_owners:
        BTreeMap<tokio::task::Id, (String, super::reviewer_child::CloseAttempts)>,
    pub(super) reviews: BTreeMap<String, ReviewSlot>,
    /// Sessions whose review is being prepared. Preparation is asynchronous,
    /// so without this an automatic trigger and a manual `/review` racing each
    /// other would both create a review and the second would overwrite the
    /// first.
    pub(super) preparing: BTreeMap<String, PreparingOperation>,
    /// Reviews whose durable active marker is being written. They are not
    /// visible and start no agents until that write succeeds.
    pub(super) pending_open: BTreeMap<String, PendingOpen>,
    /// Reviews whose durable active marker is being cleared. Their resolved
    /// view remains until the ordered write completes.
    pub(super) closing: BTreeSet<String>,
    pub(super) awaiting_capture_persistence: BTreeMap<String, Vec<ReviewRequest>>,
    /// Distinguishes reviews. Every asynchronous step carries the epoch of the
    /// review that asked for it, so a late result cannot land on its
    /// successor.
    pub(super) next_epoch: u64,
    /// The last view seen per session: its execution state, for the
    /// Running→Idle edge, and its materialized transcript, for the seed.
    pub(super) sessions: BTreeMap<String, SessionWatch>,
    pub(super) startup_ready: bool,
    pub(super) startup_failure: Option<String>,
    pub(super) deferred_handbacks: Vec<(String, ReviewerHandback)>,
    pub(super) deferred_admissions: Vec<HostEvent>,
}

pub(super) enum PreparingOperation {
    Review {
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    },
    Override {
        task: tokio::task::JoinHandle<()>,
        result: oneshot::Receiver<Result<serde_json::Value, StartRefusal>>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
}

pub(super) struct SessionWatch {
    pub(super) materialized: Option<Box<MaterializedSession>>,
}

pub(super) async fn host_loop(
    mut state: HostState,
    mut events: mpsc::UnboundedReceiver<HostEvent>,
) {
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                if state.handle(event).await { break; }
            }
            closed = state.child_closures.join_next_with_id(), if !state.child_closures.is_empty() => {
                state.child_closed(closed.expect("a child close was in flight"));
            }
        }
    }
}

pub(super) async fn persistence_loop(
    environment: Arc<dyn ReviewEnvironment>,
    events: mpsc::UnboundedSender<HostEvent>,
    mut requests: mpsc::UnboundedReceiver<PersistenceRequest>,
) {
    type Delivery = (
        String,
        u64,
        Result<(Box<TurnReviewState>, ReviewVerdict), String>,
    );
    let mut deliveries = tokio::task::JoinSet::<Delivery>::new();
    let mut delivery_owners = std::collections::HashMap::<tokio::task::Id, (String, u64)>::new();
    let mut accepting = true;
    while accepting || !deliveries.is_empty() {
        let request = tokio::select! {
            request = requests.recv(), if accepting => match request {
                Some(request) => request,
                None => { accepting = false; continue; }
            },
            completed = deliveries.join_next_with_id(), if !deliveries.is_empty() => {
                match completed.expect("a delivery was in flight") {
                    Ok((task, (session_id, epoch, result))) => {
                        delivery_owners.remove(&task);
                        let _ = events.send(HostEvent::Step { session_id, epoch, step: ReviewStep::PushDelivered(result) });
                    }
                    Err(error) => {
                        tracing::error!(%error, "push review verdict task stopped");
                        if let Some((session_id, epoch)) = delivery_owners.remove(&error.id()) {
                            let reason = format!("push review verdict task stopped: {error}");
                            let _ = events.send(HostEvent::Step { session_id, epoch, step: ReviewStep::PushDelivered(Err(reason)) });
                        }
                    }
                }
                continue;
            }
        };
        match request {
            PersistenceRequest::Load { session_id, reply } => {
                let load = environment.clone();
                let result = tokio::task::spawn_blocking(move || load.load_state(&session_id))
                    .await
                    .map_err(|error| format!("review state load stopped: {error}"))
                    .and_then(|result| result);
                let _ = reply.send(result);
            }
            PersistenceRequest::SaveAcknowledged {
                operation,
                session_id,
                state,
                reply,
            } => {
                // Only local persistence holds admission, never worker queues.
                let admission = match crate::upgrade::activity_unless_draining(operation) {
                    Ok(admission) => admission,
                    Err(error) => {
                        let _ = reply.send(Err(format!("review state save deferred for daemon handoff: {error}; repeat the request after restart")));
                        continue;
                    }
                };
                let save = environment.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _admission = admission;
                    let result = save.save_state(&session_id, &state);
                    if let Err(error) = &result {
                        tracing::warn!(session_id, %error, "review state save failed");
                    }
                    result
                })
                .await
                .map_err(|error| format!("review state save stopped: {error}"))
                .and_then(|result| result);
                let _ = reply.send(result);
            }
            PersistenceRequest::SaveReviewerOutcome {
                session_id,
                epoch,
                state,
                requests,
                reply,
            } => {
                let save = environment.clone();
                let owner = session_id.clone();
                let result = tokio::task::spawn_blocking(move || save.save_state(&owner, &state))
                    .await
                    .map_err(|error| format!("review outcome save stopped: {error}"))
                    .and_then(|result| result);
                let _ = reply.send(result.clone());
                if let Err(error) = events.send(HostEvent::ReviewerOutcomeSaved {
                    session_id,
                    epoch,
                    requests,
                    result,
                }) {
                    tracing::warn!(%error, "review outcome saved after host stopped; next daemon replays it");
                }
            }
            PersistenceRequest::FinishPush(completion) => {
                let environment = environment.clone();
                let owner = (completion.session_id.clone(), completion.epoch);
                let task = deliveries.spawn(async move {
                    let session_id = completion.session_id.clone();
                    let epoch = completion.epoch;
                    let result =
                        super::resolve::finish_push_delivery(environment, *completion).await;
                    (session_id, epoch, result)
                });
                delivery_owners.insert(task.id(), owner);
            }
            PersistenceRequest::SweepInterrupted => {
                let sweep = environment.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let groups = sweep.active_groups()?;
                    let mut retained = BTreeSet::new();
                    for (parent, state) in &groups {
                        let group = state
                            .active_group
                            .as_ref()
                            .ok_or_else(|| format!("{parent}: missing active review group"))?;
                        let driver = TurnReviewDriver::restore(group.driver.clone())?;
                        let awaited = driver.awaited_commands();
                        for item in &group.registrations {
                            if awaited.iter().any(|(role, _)| role == &item.role) {
                                retained.insert((
                                    parent.clone(),
                                    mj_core::subagent::SubagentKind::Reviewer
                                        .request_key(&item.request_key),
                                ));
                            }
                        }
                    }
                    let children: Vec<String> = sweep
                        .reviewer_children()?
                        .into_iter()
                        .filter(|child| {
                            !retained.contains(&(
                                child.parent_session_id.clone(),
                                child.request_key.clone(),
                            ))
                        })
                        .map(|child| child.child_session_id)
                        .collect();
                    sweep
                        .clear_interrupted()
                        .map(|interrupted| (interrupted, children, groups))
                })
                .await;
                match result {
                    Ok(Ok((interrupted, children, groups))) => {
                        if !interrupted.is_empty() {
                            let _ = events.send(HostEvent::Interrupted { interrupted });
                        }
                        let _ = events.send(HostEvent::RestoreGroups {
                            groups,
                            abandoned_children: children,
                        });
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "could not clear interrupted reviews");
                        let _ = events.send(HostEvent::StartupRecoveryFailed(error));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the interrupted-review sweep did not run");
                        let _ = events.send(HostEvent::StartupRecoveryFailed(error.to_string()));
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

pub(super) struct PushCompletion {
    pub(super) session_id: String,
    pub(super) epoch: u64,
    pub(super) review_id: String,
    pub(super) control: SessionManagerControl,
    pub(super) state: TurnReviewState,
    pub(super) deltas: Vec<mj_core::relay::RepoDelta>,
    pub(super) verdict: ReviewVerdict,
    pub(super) delivery: Arc<super::resolve::Delivery>,
}

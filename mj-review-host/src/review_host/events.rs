use super::*;

/// What the host's task processes, in order.
pub enum HostEvent {
    View {
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
        /// The relay protocol of the session's worker, as its view reports.
        protocol: Option<u32>,
    },
    /// Drop the retained last-view (with its full transcript) for every session
    /// no longer in the live set. Without this, `sessions` keeps a
    /// `MaterializedSession` per session ever observed and never releases it.
    Retain {
        live: std::collections::BTreeSet<String>,
    },
    StartPush {
        session_id: String,
        scope: super::reviewer::ReviewScope,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
    OverridePush {
        session_id: String,
        path: Option<String>,
        overrides: Vec<ReviewOverride>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
    SetPushBase {
        session_id: String,
        commit: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    },
    /// A task running a push-review operation (override, baseline, review
    /// off) asks the host to decide something about review state. The host is
    /// the one owner of that state; the task only does the worker's part.
    OperationRequest {
        operation: u64,
        request: OperationRequest,
        reply: oneshot::Sender<Result<OperationAnswer, StartRefusal>>,
    },
    /// A push-review operation ended, on any path. This releases whatever the
    /// operation reserved and answers its caller.
    OperationFinished {
        operation: u64,
    },
    /// One attempt at preparing the review that request `request` asked for.
    Prepared {
        request: u64,
        prepared: Box<Result<Prepared, StartRefusal>>,
    },
    /// A session's stored review state, read so that events waiting for it can
    /// run.
    SessionLoaded {
        session_id: String,
        result: Result<TurnReviewState, String>,
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
pub enum StandingRead {
    /// Launching a reviewer restored after a restart.
    Restored,
    /// The reviewer's session ended; a handback it saved is its outcome.
    Ended {
        child_session_id: String,
        reason: String,
    },
}

/// What the host does when a save finishes. Each names the review it is
/// about, because a session can have several open at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceCompletion {
    Open {
        review_id: String,
    },
    Captured {
        review_id: String,
    },
    Close {
        review_id: String,
        resolution: mj_store::database::FinishedReviewResolution,
    },
    LaunchChild {
        review_id: String,
        role: String,
    },
}

impl PersistenceCompletion {
    pub fn review_id(&self) -> &str {
        match self {
            Self::Open { review_id }
            | Self::Captured { review_id }
            | Self::Close { review_id, .. }
            | Self::LaunchChild { review_id, .. } => review_id,
        }
    }
}

/// What an operation's task asks the host to decide.
pub enum OperationRequest {
    /// Select the findings verdict an override answers and reserve its roots.
    /// `roots` is what the task resolved the caller's `path` to, when it
    /// named one and the worker can resolve it.
    ReserveOverride {
        protocol: u32,
        roots: Option<Vec<std::path::PathBuf>>,
    },
    /// Reserve the one repository a baseline move changes. `root` is `None`
    /// when the worker cannot resolve it without a capture.
    ReserveBase {
        protocol: u32,
        root: Option<std::path::PathBuf>,
    },
    /// Reserve the repositories a review-off request captured.
    ReserveCaptured {
        protocol: u32,
        roots: Vec<std::path::PathBuf>,
    },
    /// Record the declined findings of the selected verdict, and answer with
    /// the save's acknowledgement.
    DeclineFindings { overrides: Vec<ReviewOverride> },
    /// The override's ref is recorded: retire the selected verdict.
    FinishOverride,
    /// A baseline moved in `root`: stop covering it with declined findings.
    FinishBase {
        root: std::path::PathBuf,
        changed: bool,
    },
}

pub enum OperationAnswer {
    /// The roots are reserved. For an override, the verdict it answers.
    Reserved {
        findings: Option<PushFindingsRecord>,
    },
    /// The change is queued; the receiver answers once it is durable.
    Saved(oneshot::Receiver<Result<(), String>>),
    Done,
}

pub enum PersistenceRequest {
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
        /// The text the host settled this save's roles from, which the store
        /// writes verbatim instead of reading the child's mutable handback.
        reports: Vec<mj_store::database::AcceptedReport>,
        requests: Vec<ReviewRequest>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The worker's part of a verdict: record an approved tree.
    FinishPush(Box<PushCompletion>),
    /// The durable end of a verdict, in one admitted step: the verdict
    /// messages and the row without the review they end.
    Deliver(Box<DeliveryCommit>),
    Save {
        session_id: String,
        state: Box<TurnReviewState>,
        completion: Option<PersistenceCompletion>,
    },
}

/// Everything the lane needs to end a review's verdict delivery. The host
/// composed `state`; the lane writes it.
pub struct DeliveryCommit {
    pub session_id: String,
    pub epoch: u64,
    pub review_id: String,
    pub state: TurnReviewState,
    pub events: Vec<mj_core::mailbox::MailboxEvent>,
    pub verdict: ReviewVerdict,
    pub resolution: mj_store::database::FinishedReviewResolution,
    pub delivery: Arc<super::resolve::Delivery>,
    /// Held across the commit so a daemon handoff waits for it.
    pub admission: mj_store::upgrade::Work,
}

/// One asynchronous step's result, belonging to exactly one review.
pub enum ReviewStep {
    Delta(Result<Vec<mj_core::relay::RepoDelta>, String>),
    /// The worker recorded (or refused) the approved trees of a verdict.
    PushRecorded(Result<Approval, String>),
    PushDelivered(Result<ReviewVerdict, String>),
    /// Delivery failed and the requesting session still exists, or could not
    /// be looked up: the delivery is tried again.
    PushRetry(String),
    /// A save failed because the requesting session's record is gone, so no
    /// verdict or review state can be saved for it.
    OwnerGone,
}

/// What recording a verdict's approval decided.
pub struct Approval {
    /// The verdict to deliver. A clean one the worker refused to record is a
    /// failure.
    pub verdict: ReviewVerdict,
    /// Whether the trees were recorded as reviewed, so the review bases
    /// moved.
    pub recorded: bool,
}

/// Everything one blocking preparation gathered before a review can start.
pub struct Prepared {
    pub seed: TurnReviewSeed,
    /// The relay protocol of the session's worker when it captured.
    pub protocol: u32,
    pub reviewers: Vec<ReviewerIdentity>,
    /// The brief writer, when the settings enable one and it resolved.
    pub brief_writer: Option<ReviewerIdentity>,
    /// Why a configured brief writer will not run, for the requester.
    pub brief_skipped: Option<String>,
    /// Committed work captured before choosing reviewers.
    pub captured: Vec<mj_core::relay::RepoDelta>,
    /// Keeps recovery copies and worker upgrades off the session from
    /// preparation until the review closes; moved into the review's slot.
    pub background: Option<BackgroundHold>,
}

/// A review whose durable marker is being written. It owns its roots from
/// here on, so a request that overlaps it is refused.
pub struct PendingOpen {
    pub epoch: u64,
    pub reply: Option<OpenReply>,
    pub prepared: Prepared,
    pub open: OpenReview,
    pub cancel: tokio_util::sync::CancellationToken,
    pub driver: TurnReviewDriver,
    pub requests: Vec<ReviewRequest>,
}

/// The caller of `request_review`, who is told once the review is accepted.
pub struct OpenReply {
    pub reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    /// What the caller is told when the review is accepted.
    pub accepted: serde_json::Value,
}

/// A review's key: which session asked and which review it is. Reviews of one
/// session are adjacent in every map keyed by it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReviewKey {
    pub session_id: String,
    pub review_id: String,
}

pub type ReviewerIdentity = mj_core::review::settings::ResolvedReviewSettings;

/// What a reviewer child gave the host. Handbacks and ended sessions both
/// arrive as reports, so [`HostState::reviewer_finished`] decides every
/// reviewer outcome and failure one at a time, behind a saving outcome.
pub enum ReviewerReport {
    /// The text of its handback.
    Answer(String),
    /// It ended without one; the reason says why.
    Failed(String),
}

pub struct ReviewerHandback {
    pub child_session_id: String,
    pub report: ReviewerReport,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// One open review and its execution context.
pub struct ReviewSlot {
    /// Which review this is. Asynchronous results name it, and results that
    /// name another are dropped.
    pub epoch: u64,
    pub driver: TurnReviewDriver,
    pub reviewers: Vec<ReviewerIdentity>,
    /// This review's part of the session's stored row.
    pub open: OpenReview,
    /// The relay protocol of the session's worker when the review opened.
    pub protocol: u32,
    /// Released when the slot is removed, which is how every review ends.
    pub _background: Option<BackgroundHold>,
    pub finishing: bool,
    /// The host queued a save that takes this review out of the stored row,
    /// so later saves leave it out too.
    pub leaving_row: bool,
    /// The outstanding findings verdicts a queued delivery replaced, for a
    /// delivery that does not finish: its verdict was never delivered, so it
    /// must not stay recorded.
    pub delivery_undo: Option<Vec<PushFindingsRecord>>,
    pub delivery: Arc<super::resolve::Delivery>,
    pub retry_delay: std::time::Duration,
    pub start_reply: Option<OpenReply>,
    pub pending_outcome: Option<PendingOutcome>,
    pub queued_outcomes: Vec<ReviewerHandback>,
    /// Reviewers that ended while their handback waited in
    /// `queued_outcomes`, with the reason. That handback is their outcome, so
    /// no standing read is made yet; the entry moves to the save of that
    /// handback, which reports the end again if the save fails.
    pub ended_while_queued: BTreeMap<String, String>,
}

/// The outcome being saved: the state to return to if the save fails, and the
/// reviewer end that save is the only record of.
pub struct PendingOutcome {
    pub prior: OpenReview,
    /// The child and reason of an end noted while this handback was queued.
    pub ended: Option<(String, String)>,
}

impl ReviewSlot {
    /// The wait before the next retry of anything this review retries
    /// (push delivery, a restored reviewer's standing read); each call
    /// doubles the next wait up to 30 s.
    pub fn take_retry_delay(&mut self) -> std::time::Duration {
        let delay = self.retry_delay;
        self.retry_delay = (delay * 2).min(std::time::Duration::from_secs(30));
        delay
    }
}

pub struct HostState {
    pub control: SessionManagerControl,
    pub config: ReviewConfigSource,
    pub environment: Arc<dyn ReviewEnvironment>,
    pub shared: Arc<HostShared>,
    pub events: mpsc::UnboundedSender<HostEvent>,
    pub persistence: Option<mpsc::UnboundedSender<PersistenceRequest>>,
    pub persistence_task: Option<tokio::task::JoinHandle<()>>,
    pub child_closures: tokio::task::JoinSet<Result<(), String>>,
    pub child_closure_owners:
        BTreeMap<tokio::task::Id, (String, super::reviewer_child::CloseAttempts)>,
    /// Reviewer children a restart found finished, closed [`SWEEP_CLOSE_WIDTH`]
    /// at a time. An old store holds tens of thousands (19,196 in the live
    /// one). A close that fails retries on its own, outside this queue.
    pub sweep_queue: std::collections::VecDeque<String>,
    /// The open reviews. A session's reviews cover disjoint repository roots.
    pub reviews: BTreeMap<ReviewKey, ReviewSlot>,
    /// Reviews being prepared, by request number. Preparation is
    /// asynchronous and has no roots yet, so it cannot conflict; that is
    /// decided when a preparation is ready to open (`prepared`).
    pub preparations: BTreeMap<u64, Preparation>,
    /// Push-review operations (override, baseline, review off) in flight.
    pub operations: BTreeMap<u64, Operation>,
    /// Reviews whose durable active marker is being written. They are not
    /// visible and start no agents until that write succeeds.
    pub pending_open: BTreeMap<ReviewKey, PendingOpen>,
    /// Reviews whose durable active marker is being cleared. Their resolved
    /// view remains until the ordered write completes.
    pub closing: BTreeSet<ReviewKey>,
    pub awaiting_capture_persistence: BTreeMap<ReviewKey, Vec<ReviewRequest>>,
    /// The one owner of each session's review state: the part of its stored
    /// row that is not an open review, with what decides who may change it.
    /// Tasks never read or write the row; they ask the host.
    pub review_state: BTreeMap<String, SessionReviews>,
    /// Events that wait for a session's stored state to be read.
    pub loading: BTreeMap<String, Vec<HostEvent>>,
    /// Distinguishes reviews. Every asynchronous step carries the epoch of the
    /// review that asked for it, so a late result cannot land on its
    /// successor.
    pub next_epoch: u64,
    /// The last view seen per session: its execution state, for the
    /// Running→Idle edge, and its materialized transcript, for the seed.
    pub sessions: BTreeMap<String, SessionWatch>,
    pub startup_ready: bool,
    pub startup_failure: Option<String>,
    pub deferred_handbacks: Vec<(String, ReviewerHandback)>,
    pub deferred_admissions: Vec<HostEvent>,
}

/// A review being prepared: its capture and its reviewer choice.
pub struct Preparation {
    pub session_id: String,
    pub scope: super::reviewer::ReviewScope,
    /// The one record of whether the review was cancelled before it opened.
    /// Preparation races it, so cancelling stops whichever step is running.
    pub cancel: tokio_util::sync::CancellationToken,
    /// The session's base generation when this attempt started. A review that
    /// closed since moved a base, and this capture may be older than it.
    pub base_generation: u64,
    pub reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
}

/// A push-review operation whose task runs on the worker's side.
pub struct Operation {
    pub session_id: String,
    pub name: &'static str,
    pub task: tokio::task::JoinHandle<()>,
    pub result: oneshot::Receiver<Result<serde_json::Value, StartRefusal>>,
    pub reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
}

/// The part of a session's stored review row that is not an open review, and
/// what decides who may change it.
#[derive(Default)]
pub struct SessionReviews {
    /// The declined findings and the outstanding findings records.
    /// `open_reviews` is always empty here; the open reviews are the host's
    /// slots.
    pub part: TurnReviewState,
    /// Counts every close of one of this session's reviews and every
    /// override or baseline move: anything that can move a review base. A
    /// preparation that captured before the count changed captured before
    /// that move.
    pub base_generation: u64,
    /// Repository roots held by an operation that is about to change refs.
    pub reservations: Vec<Reservation>,
}

pub struct Reservation {
    pub operation: u64,
    pub label: &'static str,
    pub roots: ReservedRoots,
    /// The findings verdict an override selected, by review ID.
    pub findings: Option<String>,
    /// The verdict the override retired once its ref was recorded. If the
    /// operation then fails, the save that would have made that durable did
    /// not, and the verdict comes back so a retry still finds it.
    pub retired: Option<PushFindingsRecord>,
}

pub enum ReservedRoots {
    /// These repositories only.
    Roots(Vec<std::path::PathBuf>),
    /// Every repository of the session: the worker cannot say which roots the
    /// operation touches.
    Everything,
}

pub struct SessionWatch {
    pub materialized: Option<Box<MaterializedSession>>,
    /// The worker's protocol as of the latest view.
    pub protocol: Option<u32>,
}

pub async fn host_loop(mut state: HostState, mut events: mpsc::UnboundedReceiver<HostEvent>) {
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

pub async fn persistence_loop(
    environment: Arc<dyn ReviewEnvironment>,
    events: mpsc::UnboundedSender<HostEvent>,
    mut requests: mpsc::UnboundedReceiver<PersistenceRequest>,
) {
    type Delivery = (String, u64, ReviewStep);
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
                        let _ = events.send(HostEvent::Step { session_id, epoch, step: result });
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
                let result = mj_core::runtime::spawn_blocking(move || load.load_state(&session_id))
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
                let admission = match mj_store::upgrade::activity_unless_draining(operation) {
                    Ok(admission) => admission,
                    Err(error) => {
                        let _ = reply.send(Err(format!("review state save deferred for daemon handoff: {error}; repeat the request after restart")));
                        continue;
                    }
                };
                let save = environment.clone();
                let result = mj_core::runtime::spawn_blocking(move || {
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
                reports,
                requests,
                reply,
            } => {
                let save = environment.clone();
                let owner = session_id.clone();
                let result = mj_core::runtime::spawn_blocking(move || {
                    save.save_reviewer_outcome(&owner, &state, &reports)
                })
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
                let owner = (completion.session_id.clone(), completion.epoch);
                let task = deliveries.spawn(async move {
                    let session_id = completion.session_id.clone();
                    let epoch = completion.epoch;
                    let step = ReviewStep::PushRecorded(
                        super::resolve::record_approval(*completion).await,
                    );
                    (session_id, epoch, step)
                });
                delivery_owners.insert(task.id(), owner);
            }
            PersistenceRequest::Deliver(commit) => {
                // Local persistence, like every save: it finishes before the
                // lane takes another request, so the rows the host composed
                // reach the store in the order it composed them.
                let session_id = commit.session_id.clone();
                let epoch = commit.epoch;
                let result = super::resolve::commit_delivery(environment.clone(), *commit).await;
                let _ = events.send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::PushDelivered(result),
                });
            }
            PersistenceRequest::SweepInterrupted => {
                let sweep = environment.clone();
                let result = mj_core::runtime::spawn_blocking(move || {
                    let groups = sweep.active_groups()?;
                    let mut retained = BTreeSet::new();
                    for (parent, state) in &groups {
                        // A review without a group was cut off before it
                        // opened; the clearing below reports it.
                        for group in state
                            .open_reviews
                            .iter()
                            .filter_map(|review| review.active_group.as_ref())
                        {
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
            PersistenceRequest::Save {
                session_id,
                state,
                completion,
            } => {
                let environment = environment.clone();
                let owner = session_id.clone();
                let close = match &completion {
                    Some(PersistenceCompletion::Close {
                        review_id,
                        resolution,
                    }) => Some((review_id.clone(), *resolution)),
                    _ => None,
                };
                let result = mj_core::runtime::spawn_blocking(move || match close {
                    Some((review_id, resolution)) => {
                        environment.close_review(&owner, &review_id, &state, resolution)
                    }
                    None => environment.save_state(&owner, &state),
                })
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

/// A verdict's worker step: the approval to record.
pub struct PushCompletion {
    pub session_id: String,
    pub epoch: u64,
    pub control: SessionManagerControl,
    pub deltas: Vec<mj_core::relay::RepoDelta>,
    pub verdict: ReviewVerdict,
    pub delivery: Arc<super::resolve::Delivery>,
}

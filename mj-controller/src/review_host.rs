//! Where turn review actually runs.
//!
//! The review driver is a pure state machine (`mj_core::review::driver`); this is the
//! process that feeds it. It lives in the controller daemon, which is the one
//! process that owns every session whether or not anyone is watching: it pumps
//! each session's relay every 150 ms, it is the only SQLite writer, and it
//! hosts the phone server. That is why review lives here and not in a UI. A
//! review started from the terminal survives the terminal closing; a session
//! driven only from a phone is reviewed on the same terms; a session nobody is
//! attached to is reviewed too.
//!
//! Every surface is a projection: the terminal and the phone both render
//! [`RuntimeReviewView`] and both resolve a review by asking the host. Neither
//! owns any part of the review.
//!
//! Shape: one task owns all review state and processes [`HostEvent`]s in
//! order. Everything slow -- capturing a delta, staging a reviewer profile,
//! reading a role's journal -- happens in a spawned task that sends its result
//! back as another event. Nothing here holds a lock across an await, and no
//! two reviews can interleave their state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::database::TurnReviewState;
use crate::session_manager::{
    ManagedSessionHandle, ManagedSessionView, ReviewDeliveryAdmission, ReviewerAction,
    ReviewerOutcome, SessionManagerControl, new_command_id,
};
use mj_core::config::ReviewConfig;
use mj_core::state::{MaterializedExecutionState, MaterializedSession};

use mj_core::relay::{RelayCommand, RelayEvent, RelayObservation};

use mj_core::review::lanes::{ReviewTier, UserMessage};
use mj_core::review::verdict::ReviewVerdict;
use mj_review::driver::{
    INTENT_ROLE, PendingForward, Resolution, ReviewRequest, SUPERVISOR_ROLE, TurnReviewDriver,
    TurnReviewPhase, TurnReviewSeed,
};

pub use mj_client::review::{RuntimeReviewView, VerdictKind, VerdictView, role_session_id};

/// How long an idle reviewing role waits before reading its journal again. An
/// attach answers immediately even when nothing has been journaled, so without
/// this a review with several roles would spin on empty pages.
const ROLE_POLL_IDLE_INTERVAL: Duration = Duration::from_millis(200);

/// Why a review could not start. Every variant is something a person can act
/// on, which is why they carry their own sentences rather than a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRefusal(pub String);

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The message every surface gives for a prompt held by an open review.
pub const PROMPT_HELD_MESSAGE: &str =
    "a review of the last turn is open; forward, dismiss or cancel it first";

/// Sessions whose prompts an unresolved review is holding. A hold may admit
/// exactly one command for the matching review's corrective handoff; all
/// ordinary prompts, including controller-authored notices, remain refused.
///
/// This is the authoritative lock, and it is in memory on purpose: the process
/// that owns the review owns the lock, so a lock can never outlive the review
/// that set it. The shipped design kept it in a database row written by the
/// terminal, which is how a killed terminal could hold a session's prompts for
/// ever.
static PROMPT_LOCK: LazyLock<Mutex<BTreeMap<String, PromptHold>>> = LazyLock::new(Mutex::default);

#[derive(Debug, Default)]
struct PromptHold {
    delivery_epoch: Option<u64>,
    delivery_command_id: Option<String>,
}

/// Fresh reviewer conversations need an identity that is unique across all
/// roles, reviews, and controller restarts. A slot-local counter makes an
/// extended review's next supervisor collide with a previous supervisor, so
/// use a random nonce.
pub(crate) fn next_review_generation() -> Result<u64, String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| format!("generate reviewer generation: {error}"))?;
    let generation = u64::from_le_bytes(random);
    if generation == 0 {
        return Err("generate reviewer generation: random nonce was zero".to_owned());
    }
    Ok(generation)
}

/// Whether a prompt for `session_id` must be refused, and why.
#[must_use]
pub fn prompt_refusal(session_id: &str) -> Option<&'static str> {
    PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(session_id)
        .then_some(PROMPT_HELD_MESSAGE)
}

fn hold_prompts(session_id: &str) {
    PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id.to_owned(), PromptHold::default());
}

fn release_prompts(session_id: &str) {
    PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id);
}

/// Grants the actor one narrowly scoped exception to the prompt hold. The
/// grant is tied to both the review epoch and command identity so a delayed
/// request from an older review cannot enter a later one.
fn admit_review_delivery(
    session_id: &str,
    epoch: u64,
    command_id: &str,
) -> Option<ReviewDeliveryAdmission> {
    let mut locks = PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hold = locks.get_mut(session_id)?;
    match (hold.delivery_epoch, hold.delivery_command_id.as_deref()) {
        (Some(existing_epoch), Some(existing_command))
            if existing_epoch != epoch || existing_command != command_id =>
        {
            None
        }
        _ => {
            hold.delivery_epoch = Some(epoch);
            hold.delivery_command_id = Some(command_id.to_owned());
            Some(ReviewDeliveryAdmission::new(
                session_id.to_owned(),
                epoch,
                command_id.to_owned(),
            ))
        }
    }
}

/// Called by the session actor before it bypasses the normal prompt refusal.
/// This check is deliberately kept in the host-owned hold registry so an
/// arbitrary caller cannot turn a generic prompt into an internal delivery.
pub(crate) fn review_delivery_admitted(
    session_id: &str,
    admission: &ReviewDeliveryAdmission,
) -> bool {
    let locks = PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.get(session_id).is_some_and(|hold| {
        admission.session_id() == session_id
            && hold.delivery_epoch == Some(admission.epoch())
            && hold.delivery_command_id.as_deref() == Some(admission.command_id())
    })
}

/// Where the host reads the arming configuration. The daemon reloads
/// `config.toml` every 500 ms already, so this closure just reads whatever it
/// last installed.
pub type ReviewConfigSource = Arc<dyn Fn() -> ReviewConfig + Send + Sync>;

/// Everything a review needs from the controller: whether it can review this
/// session at all, and a staged reviewer profile to launch a role from.
///
/// It is a trait so the host's own tests can drive a whole review without a
/// container, a harness, or the developer's own `config.toml`. The daemon
/// installs [`ControllerEnvironment`], which loads the real controller.
pub trait ReviewEnvironment: Send + Sync {
    /// Refuses, with a sentence for a person, when this session cannot be
    /// reviewed under `profile`.
    fn check(&self, session_id: &str, profile: &str) -> Result<(), String>;

    /// Stages the reviewer profile for one role and describes how to launch
    /// it. Blocking: it copies a profile onto the session's target.
    fn stage(
        &self,
        session_id: &str,
        profile: &str,
        generation: u64,
        mcp_servers: &[mj_core::worker_launch::ReviewMcpServer],
        dispatch_tool: bool,
    ) -> Result<mj_core::worker_launch::ReviewerLaunchConfig, String>;

    /// How far this session has been reviewed. Blocking: it reads the
    /// controller's database.
    fn load_state(&self, session_id: &str) -> Result<TurnReviewState, String>;

    /// Records how far this session has been reviewed. Blocking: the host
    /// routes it through its ordered persistence lane rather than calling it
    /// on the Tokio task that owns review state.
    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String>;

    /// Clears the in-flight flag of every review a restart interrupted, and
    /// reports whose they were. Baselines are deliberately left alone: the
    /// interrupted review never advanced one, so the next review covers the
    /// same change and nothing is lost.
    fn clear_interrupted(&self) -> Result<Vec<String>, String>;
}

/// The production environment: the controller as it is on disk right now.
///
/// It is reloaded per call rather than held, because a review is rare and the
/// answer must reflect the config as it stands when the review starts -- the
/// daemon reloads config.toml every 500 ms for the same reason.
#[derive(Debug, Default)]
pub struct ControllerEnvironment;

impl ReviewEnvironment for ControllerEnvironment {
    fn check(&self, session_id: &str, profile: &str) -> Result<(), String> {
        let controller =
            crate::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        let Some(reviewer) = controller.config.profiles.get(profile) else {
            return Err(format!(
                "turn review needs a reviewer: [review] profile {profile:?} is not a profile in config.toml"
            ));
        };
        if !reviewer.enabled {
            return Err(format!(
                "turn review needs an enabled reviewer: [review] profile {profile:?} is disabled"
            ));
        }
        validate_reviewer_assignment(
            session_id,
            controller.state.sessions.get(session_id),
            profile,
        )
    }

    fn stage(
        &self,
        session_id: &str,
        profile: &str,
        generation: u64,
        mcp_servers: &[mj_core::worker_launch::ReviewMcpServer],
        dispatch_tool: bool,
    ) -> Result<mj_core::worker_launch::ReviewerLaunchConfig, String> {
        let controller =
            crate::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        controller
            .stage_reviewer_profile_with_mcp(
                session_id,
                profile,
                generation,
                mcp_servers,
                dispatch_tool,
            )
            .map_err(|error| format!("{error:#}"))
    }

    fn load_state(&self, session_id: &str) -> Result<TurnReviewState, String> {
        crate::database::turn_review_state(session_id).map_err(|error| format!("{error:#}"))
    }

    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String> {
        crate::database::save_turn_review_state(session_id, state)
            .map_err(|error| format!("{error:#}"))
    }

    fn clear_interrupted(&self) -> Result<Vec<String>, String> {
        crate::database::clear_interrupted_turn_reviews().map_err(|error| format!("{error:#}"))
    }
}

pub(crate) fn validate_reviewer_assignment(
    session_id: &str,
    session: Option<&mj_core::state::SessionRecord>,
    profile: &str,
) -> Result<(), String> {
    let Some(session) = session else {
        return Err(format!(
            "session {session_id:?} is not in the controller store"
        ));
    };
    if session.archived {
        return Err("this session is archived".to_owned());
    }
    if session.last_profile == profile {
        return Err(format!(
            "turn review profile {profile:?} is also this session's primary profile; choose a different [review] profile"
        ));
    }
    Ok(())
}

/// A handle on the review host. Cheap to clone; every method is a message.
#[derive(Clone)]
pub struct TurnReviewHost {
    events: mpsc::UnboundedSender<HostEvent>,
    shared: Arc<HostShared>,
}

/// What surfaces read without waiting for the host's task.
struct HostShared {
    views: Mutex<BTreeMap<String, RuntimeReviewView>>,
    changed: Arc<dyn Fn() + Send + Sync>,
    shutdown: tokio::sync::OnceCell<Result<(), String>>,
}

impl std::fmt::Debug for TurnReviewHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TurnReviewHost")
    }
}

impl TurnReviewHost {
    /// Starts the host's task, reviewing through the real controller.
    #[must_use]
    pub fn spawn(control: SessionManagerControl, config: ReviewConfigSource) -> Self {
        Self::spawn_notifying(control, config, Arc::new(|| {}))
    }

    /// Starts the production host and calls `changed` whenever a surface view
    /// is added, changed, or removed.
    #[must_use]
    pub fn spawn_notifying(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::spawn_in_notifying(control, config, Arc::new(ControllerEnvironment), changed)
    }

    /// The same, against a caller-supplied environment. `config` is read at
    /// each trigger decision.
    #[must_use]
    pub fn spawn_in(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        environment: Arc<dyn ReviewEnvironment>,
    ) -> Self {
        Self::spawn_in_notifying(control, config, environment, Arc::new(|| {}))
    }

    #[must_use]
    fn spawn_in_notifying(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        environment: Arc<dyn ReviewEnvironment>,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        let (persistence, persistence_receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(HostShared {
            views: Mutex::default(),
            changed,
            shutdown: tokio::sync::OnceCell::new(),
        });
        let host = Self {
            events: events.clone(),
            shared: shared.clone(),
        };
        let persistence_task = tokio::spawn(persistence_loop(
            environment.clone(),
            events.clone(),
            persistence_receiver,
        ));
        // The restart sweep is the first operation in the same FIFO lane that
        // records new active reviews, so it cannot clear a review that opened
        // while the sweep was still running.
        persistence
            .send(PersistenceRequest::SweepInterrupted)
            .expect("new review persistence lane accepts its initial sweep");
        tokio::spawn(host_loop(
            HostState {
                control,
                config,
                environment,
                shared,
                events,
                persistence: Some(persistence),
                persistence_task: Some(persistence_task),
                reviews: BTreeMap::new(),
                preparing: BTreeSet::new(),
                pending_open: BTreeMap::new(),
                closing: BTreeSet::new(),
                awaiting_forward_persistence: BTreeMap::new(),
                next_epoch: 0,
                sessions: BTreeMap::new(),
                missing_reviewer_reported: BTreeSet::new(),
                recovery_candidates: BTreeSet::new(),
                recovery_in_flight: BTreeSet::new(),
            },
            receiver,
        ));
        host
    }

    /// Reports one session's latest view. This is the trigger's only input.
    /// Prune the retained last-views to the live session set. The daemon calls
    /// this from its reconcile so a stopped or destroyed session's transcript is
    /// released rather than retained in `sessions` forever.
    pub fn retain_sessions(&self, live: std::collections::BTreeSet<String>) {
        let _ = self.events.send(HostEvent::Retain { live });
    }

    pub fn observe(&self, session_id: &str, view: &ManagedSessionView) {
        // Running -> Idle is an edge, not a level: the session manager
        // suppresses unchanged views, so dropping one here can lose an
        // automatic review permanently. An unbounded hand-off keeps the
        // daemon's update loop nonblocking without dropping that edge.
        let _ = self.events.send(HostEvent::View {
            session_id: session_id.to_owned(),
            snapshot: view
                .snapshot
                .as_ref()
                .map(|snapshot| Box::new(snapshot.materialized.clone())),
            prompt_driven: view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.active_prompt.is_some()),
        });
    }

    /// Reviews the turn that just finished, on request.
    pub async fn start(&self, session_id: &str, manual: bool) -> Result<(), StartRefusal> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::Start {
                session_id: session_id.to_owned(),
                manual,
                reply: Some(reply),
            })
            .map_err(|_| StartRefusal("the review host stopped".to_owned()))?;
        answer
            .await
            .map_err(|_| StartRefusal("the review host stopped".to_owned()))?
    }

    /// Forwards, dismisses, or cancels the open review.
    pub async fn resolve(&self, session_id: &str, resolution: Resolution) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::Resolve {
                session_id: session_id.to_owned(),
                resolution,
                reply,
            })
            .map_err(|_| "the review host stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "the review host stopped".to_owned())?
    }

    /// Stops accepting review work, releases every prompt hold, and drains the
    /// ordered persistence lane before the daemon shuts its database writer
    /// down.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.shared
            .shutdown
            .get_or_init(|| async {
                let (reply, answer) = oneshot::channel();
                self.events
                    .send(HostEvent::Shutdown { reply })
                    .map_err(|_| "the review host stopped".to_owned())?;
                answer
                    .await
                    .map_err(|_| "the review host stopped during shutdown".to_owned())?
            })
            .await
            .clone()
    }

    /// Every open review, for a snapshot a surface renders.
    #[must_use]
    pub fn views(&self) -> Vec<RuntimeReviewView> {
        self.shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// One session's review, if it has one.
    #[must_use]
    pub fn view(&self, session_id: &str) -> Option<RuntimeReviewView> {
        self.shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    /// Whether an unresolved review is holding this session's prompts.
    #[must_use]
    pub fn refuses_prompt(&self, session_id: &str) -> bool {
        prompt_refusal(session_id).is_some()
    }
}

/// What the host's task processes, in order.
enum HostEvent {
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
enum PersistenceCompletion {
    Open,
    Forward,
    Close,
}

enum PersistenceRequest {
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
enum ReviewStep {
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
struct Prepared {
    state: TurnReviewState,
    reviewer: ReviewerIdentity,
    tier: ReviewTier,
    /// Read from the live actor after the admission hold is installed and a
    /// reviewer status command drains every actor command ahead of it.
    materialized: Box<MaterializedSession>,
    /// Present only while startup reconciles an interrupted corrective
    /// handoff. Such a review skips reviewer processes and retries the exact
    /// primary command id.
    resume_forward: Option<PendingForward>,
}

struct PendingOpen {
    epoch: u64,
    manual: bool,
    reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    prepared: Prepared,
}

/// Which harness reviews, and how it is configured. Read from `[review]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewerIdentity {
    profile: String,
    model: Option<String>,
    effort: Option<String>,
}

/// One open review and its execution context.
struct ReviewSlot {
    /// Which review this is. Asynchronous results name it, and results that
    /// name another are dropped.
    epoch: u64,
    driver: TurnReviewDriver,
    /// One transcript projection per reviewing role, which is how the host
    /// reads a role's answer out of its own relay journal.
    roles: BTreeMap<String, RoleTranscript>,
    reviewer: ReviewerIdentity,
    state: TurnReviewState,
    /// The sidecar reads a new generation as "this is a different reviewer".
    /// Fresh role launches receive a random nonce, so a later review cannot
    /// reuse the native conversation left by an earlier one.
    generation: u64,
}

/// One role's journal, folded far enough to read its final answer.
#[derive(Default)]
struct RoleTranscript {
    session: Option<MaterializedSession>,
    cursor_ordinal: u64,
    cursor_digest: String,
}

impl RoleTranscript {
    fn apply(&mut self, session_id: &str, events: &[RelayEvent]) {
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
    fn latest_answer(&self) -> Option<String> {
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

struct HostState {
    control: SessionManagerControl,
    config: ReviewConfigSource,
    environment: Arc<dyn ReviewEnvironment>,
    shared: Arc<HostShared>,
    events: mpsc::UnboundedSender<HostEvent>,
    persistence: Option<mpsc::UnboundedSender<PersistenceRequest>>,
    persistence_task: Option<tokio::task::JoinHandle<()>>,
    reviews: BTreeMap<String, ReviewSlot>,
    /// Sessions whose review is being prepared. Preparation is asynchronous,
    /// so without this an automatic trigger and a manual `/review` racing each
    /// other would both create a review and the second would overwrite the
    /// first.
    preparing: BTreeSet<String>,
    /// Reviews whose durable active marker is being written. They are not
    /// visible and start no agents until that write succeeds.
    pending_open: BTreeMap<String, PendingOpen>,
    /// Reviews whose durable active marker is being cleared. Their resolved
    /// view and prompt hold remain until the ordered write completes.
    closing: BTreeSet<String>,
    /// Primary handoff requests wait here until their durable pending record
    /// has been written. This prevents an accepted relay command from racing
    /// a failed SQLite write.
    awaiting_forward_persistence: BTreeMap<String, Vec<ReviewRequest>>,
    /// Distinguishes reviews. Every asynchronous step carries the epoch of the
    /// review that asked for it, so a late result cannot land on its
    /// successor.
    next_epoch: u64,
    /// The last view seen per session: its execution state, for the
    /// Running→Idle edge, and its materialized transcript, for the seed.
    sessions: BTreeMap<String, SessionWatch>,
    /// Sessions already told that no reviewer is configured. One notice per
    /// session, not one per turn.
    missing_reviewer_reported: BTreeSet<String>,
    /// Sessions whose durable handoff survived a restart and still needs the
    /// primary relay's idempotent acknowledgement reconciled.
    recovery_candidates: BTreeSet<String>,
    recovery_in_flight: BTreeSet<String>,
}

struct SessionWatch {
    execution: MaterializedExecutionState,
    /// Whether the view had a prompt of ours in flight.
    prompt_driven: bool,
    materialized: Option<Box<MaterializedSession>>,
}

async fn host_loop(mut state: HostState, mut events: mpsc::UnboundedReceiver<HostEvent>) {
    while let Some(event) = events.recv().await {
        if state.handle(event).await {
            break;
        }
    }
}

async fn persistence_loop(
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

impl HostState {
    /// Returns true once shutdown has drained persistence and the host loop may
    /// stop.
    async fn handle(&mut self, event: HostEvent) -> bool {
        match event {
            HostEvent::View {
                session_id,
                snapshot,
                prompt_driven,
            } => self.observe(session_id, snapshot, prompt_driven).await,
            HostEvent::Retain { live } => self.retain_sessions(&live),
            HostEvent::Start {
                session_id,
                manual,
                reply,
            } => self.begin(session_id, manual, reply),
            HostEvent::Prepared {
                session_id,
                manual,
                reply,
                prepared,
            } => self.prepared(session_id, manual, reply, prepared),
            HostEvent::RecoveryPrepared {
                session_id,
                prepared,
            } => self.recovery_prepared(session_id, prepared),
            HostEvent::StateSaved {
                session_id,
                completion,
                result,
            } => self.state_saved(session_id, completion, result),
            HostEvent::Resolve {
                session_id,
                resolution,
                reply,
            } => {
                let answer = self.resolve(&session_id, resolution);
                let _ = reply.send(answer);
            }
            HostEvent::Step {
                session_id,
                epoch,
                step,
            } => self.step(session_id, epoch, step),
            HostEvent::Interrupted { interrupted } => {
                for session_id in interrupted {
                    self.recovery_candidates.insert(session_id.clone());
                    if self.sessions.get(&session_id).is_some_and(|watch| {
                        matches!(watch.execution, MaterializedExecutionState::Idle)
                    }) {
                        self.begin_recovery(&session_id);
                    }
                }
            }
            HostEvent::Shutdown { reply } => {
                let result = self.shutdown().await;
                let _ = reply.send(result);
                return true;
            }
        }
        false
    }

    /// Applies one asynchronous result to the review that asked for it.
    fn step(&mut self, session_id: String, epoch: u64, step: ReviewStep) {
        // A result from a review that has since been cancelled, resolved, or
        // replaced is not this review's business.
        if self.reviews.get(&session_id).map(|slot| slot.epoch) != Some(epoch) {
            return;
        }
        match step {
            ReviewStep::Delta(result) => {
                let requests = match result {
                    Ok(deltas) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.delta_captured(deltas))
                        .unwrap_or_default(),
                    Err(error) => {
                        self.fail(
                            &session_id,
                            format!("the change could not be captured: {error}"),
                        );
                        return;
                    }
                };
                self.run(&session_id, requests);
            }
            ReviewStep::Analysis(result) => {
                let requests = self
                    .reviews
                    .get_mut(&session_id)
                    .map(|slot| slot.driver.analysis_completed(result))
                    .unwrap_or_default();
                self.run(&session_id, requests);
            }
            ReviewStep::RoleStarted { role, result } => {
                let requests = match self.reviews.get_mut(&session_id) {
                    Some(slot) => match result {
                        Ok(()) => slot.driver.role_started(&role),
                        // A lane that cannot start is a coverage gap the
                        // supervisor is told about; any other role failing to
                        // start fails the review.
                        Err(error) if mj_review::lanes::lane_by_id(&role).is_some() => {
                            slot.driver.lane_failed(&role, error)
                        }
                        Err(error)
                            if matches!(
                                slot.driver.phase(),
                                TurnReviewPhase::Forwarding { .. }
                            ) =>
                        {
                            tracing::debug!(
                                session_id = %session_id,
                                role = %role,
                                %error,
                                "ignoring a late reviewer start result during primary handoff"
                            );
                            return;
                        }
                        Err(error) => {
                            self.fail(&session_id, error);
                            return;
                        }
                    },
                    None => return,
                };
                self.run(&session_id, requests);
            }
            ReviewStep::RolePrompted { role, result } => {
                if let Err(error) = result {
                    if self.reviews.get(&session_id).is_some_and(|slot| {
                        matches!(slot.driver.phase(), TurnReviewPhase::Forwarding { .. })
                    }) {
                        // Role cleanup can report after the primary handoff
                        // has begun. It is unrelated to that handoff and must
                        // not release its prompt hold or replace its findings.
                        tracing::debug!(
                            session_id = %session_id,
                            role = %role,
                            %error,
                            "ignoring a late reviewer-role result during primary handoff"
                        );
                        return;
                    }
                    self.fail(
                        &session_id,
                        format!("reviewing role {role:?} could not be prompted: {error}"),
                    );
                }
            }
            ReviewStep::PrimaryPrompted(result) => {
                let requests = match result {
                    Ok(()) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.forward_succeeded())
                        .unwrap_or_default(),
                    Err(error) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.forward_failed(error))
                        .unwrap_or_default(),
                };
                self.run(&session_id, requests);
            }
            ReviewStep::RoleEvents { role, result } => self.role_events(session_id, role, result),
            ReviewStep::Dispatches(result) => {
                let requests = match result {
                    Ok(requests) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.lanes_dispatched(requests))
                        .unwrap_or_default(),
                    // A dropped dispatch would leave the supervisor waiting for
                    // lanes that never run, so it fails the review rather than
                    // stalling it.
                    Err(error) => {
                        if self.reviews.get(&session_id).is_some_and(|slot| {
                            matches!(slot.driver.phase(), TurnReviewPhase::Forwarding { .. })
                        }) {
                            tracing::debug!(
                                session_id = %session_id,
                                %error,
                                "ignoring a late lane dispatch result during primary handoff"
                            );
                            return;
                        }
                        self.fail(
                            &session_id,
                            format!(
                                "the review could not collect the supervisor's specialists: {error}"
                            ),
                        );
                        return;
                    }
                };
                self.run(&session_id, requests);
            }
        }
    }

    /// Release the retained last-view for every session no longer in the live
    /// set, so a stopped or destroyed session's `MaterializedSession` (its full
    /// transcript) does not linger. The in-flight review state in `reviews`/
    /// `closing` is separate and keeps its own lifetime.
    fn retain_sessions(&mut self, live: &std::collections::BTreeSet<String>) {
        self.sessions
            .retain(|session_id, _| live.contains(session_id));
    }

    /// Watches one session for the edge that arms an automatic review.
    async fn observe(
        &mut self,
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
        prompt_driven: bool,
    ) {
        let execution = snapshot
            .as_ref()
            .map_or(MaterializedExecutionState::Idle, |snapshot| {
                snapshot.execution
            });
        let previous = self.sessions.insert(
            session_id.clone(),
            SessionWatch {
                execution,
                prompt_driven,
                materialized: snapshot,
            },
        );
        if self.recovery_candidates.contains(&session_id)
            && matches!(execution, MaterializedExecutionState::Idle)
        {
            self.begin_recovery(&session_id);
            return;
        }
        // A turn the harness starts on its own also runs and then goes idle.
        // Reviewing that is a separate decision, so the edge that arms a
        // review is the end of a turn that answered a prompt.
        let finished_turn = previous.as_ref().is_some_and(|watch| {
            watch.prompt_driven
                && matches!(watch.execution, MaterializedExecutionState::Running { .. })
        }) && matches!(execution, MaterializedExecutionState::Idle);
        if !finished_turn || !(self.config)().enabled {
            return;
        }
        self.begin(session_id, false, None);
    }

    /// Decides whether a review can start, and prepares one if it can.
    ///
    /// The cheap gates are answered here; the ones that need the database or
    /// the worker are answered in the preparation task, so this never blocks
    /// the host's loop.
    fn begin(
        &mut self,
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    ) {
        if crate::controller::move_session::move_owns_session(&session_id) {
            answer(reply, Err(StartRefusal("session is moving".to_owned())));
            return;
        }
        if let Some(refusal) = self.refuse_start(&session_id) {
            answer(reply, Err(refusal));
            return;
        }
        if self.preparing.contains(&session_id) {
            answer(
                reply,
                Err(StartRefusal("a review is already starting".to_owned())),
            );
            return;
        }
        let config = (self.config)();
        let Some(profile) = config.reviewer_profile().map(str::to_owned) else {
            // Configuration is the only place this can be fixed, so the
            // message names the key. A session hears it once, not once a turn.
            let refusal = StartRefusal(
                "turn review needs a reviewer: set [review] profile in config.toml".to_owned(),
            );
            if self.missing_reviewer_reported.insert(session_id.clone()) {
                self.record_notice(&session_id, refusal.0.clone());
            }
            answer(reply, Err(refusal));
            return;
        };
        let reviewer = ReviewerIdentity {
            profile,
            model: config.model.clone(),
            effort: config.effort.clone(),
        };
        let tier = config.tier;
        let control = self.control.clone();
        let events = self.events.clone();
        let prepare_session = session_id.clone();
        let environment = self.environment.clone();
        // Admission and prompt refusal are one transition. Any prompt already
        // ahead of the preparation's reviewer-status command is drained before
        // `prepare` reads the actor view; every later prompt sees this hold.
        hold_prompts(&session_id);
        self.preparing.insert(session_id);
        tokio::spawn(async move {
            let prepared = prepare(&control, &environment, &prepare_session, &reviewer, tier).await;
            let _ = events.send(HostEvent::Prepared {
                session_id: prepare_session,
                manual,
                reply,
                prepared,
            });
        });
    }

    /// Reconciles a durable corrective handoff left by a previous daemon.
    /// This path does not require a reviewer profile: the review already has
    /// findings, and only the primary relay's idempotent command needs to be
    /// observed again.
    fn begin_recovery(&mut self, session_id: &str) {
        if !self.recovery_candidates.contains(session_id)
            || self.recovery_in_flight.contains(session_id)
            || self.reviews.contains_key(session_id)
            || self.preparing.contains(session_id)
        {
            return;
        }
        let Some(watch) = self.sessions.get(session_id) else {
            return;
        };
        if watch
            .materialized
            .as_ref()
            .is_none_or(|snapshot| !snapshot.queued_prompts.is_empty())
        {
            return;
        }
        hold_prompts(session_id);
        self.recovery_in_flight.insert(session_id.to_owned());
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let prepared = prepare_recovery(&control, &environment, &session_id).await;
            let _ = events.send(HostEvent::RecoveryPrepared {
                session_id,
                prepared,
            });
        });
    }

    fn recovery_prepared(
        &mut self,
        session_id: String,
        prepared: Result<Option<Prepared>, String>,
    ) {
        self.recovery_in_flight.remove(&session_id);
        match prepared {
            Ok(Some(prepared)) => {
                self.recovery_candidates.remove(&session_id);
                self.preparing.insert(session_id.clone());
                self.prepared(session_id, false, None, Ok(prepared));
            }
            Ok(None) => {
                self.recovery_candidates.remove(&session_id);
                release_prompts(&session_id);
                self.record_notice(
                    &session_id,
                    "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes".to_owned(),
                );
            }
            Err(error) => {
                // Keep the candidate so a later connected/idle observation can
                // retry. No success notice is emitted for an unknown outcome.
                tracing::warn!(session_id = %session_id, %error, "could not reconcile an interrupted review handoff");
                release_prompts(&session_id);
            }
        }
    }

    /// The gates that need nothing but the host's own state.
    fn refuse_start(&self, session_id: &str) -> Option<StartRefusal> {
        if self.reviews.contains_key(session_id) {
            return Some(StartRefusal("a review is already open".to_owned()));
        }
        if self.recovery_candidates.contains(session_id)
            || self.recovery_in_flight.contains(session_id)
        {
            return Some(StartRefusal(
                "an interrupted review handoff is being reconciled".to_owned(),
            ));
        }
        let Some(watch) = self.sessions.get(session_id) else {
            return Some(StartRefusal("this session is not connected".to_owned()));
        };
        if !matches!(watch.execution, MaterializedExecutionState::Idle) {
            return Some(StartRefusal(
                "a review runs between turns; this one is still working".to_owned(),
            ));
        }
        let queued = watch
            .materialized
            .as_ref()
            .is_some_and(|materialized| !materialized.queued_prompts.is_empty());
        if queued {
            // Reviewing now would hold prompts the user has already sent. The
            // review after the queue drains covers the whole batch instead.
            return Some(StartRefusal(
                "prompts are queued; the review waits for them".to_owned(),
            ));
        }
        None
    }

    fn prepared(
        &mut self,
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
        prepared: Result<Prepared, StartRefusal>,
    ) {
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.preparing.remove(&session_id);
                release_prompts(&session_id);
                answer(reply, Err(refusal));
                return;
            }
        };
        if self.reviews.contains_key(&session_id) {
            self.preparing.remove(&session_id);
            release_prompts(&session_id);
            let refusal = StartRefusal("a review is already open".to_owned());
            answer(reply, Err(refusal));
            return;
        }
        self.next_epoch = self.next_epoch.saturating_add(1);
        let epoch = self.next_epoch;
        let mut prepared = prepared;
        prepared.state.active = Some(format!("review-{epoch}"));
        let state = prepared.state.clone();
        self.pending_open.insert(
            session_id.clone(),
            PendingOpen {
                epoch,
                manual,
                reply,
                prepared,
            },
        );
        if let Err(error) =
            self.persist(session_id.clone(), state, Some(PersistenceCompletion::Open))
        {
            let pending = self
                .pending_open
                .remove(&session_id)
                .expect("pending review was just inserted");
            let retry_recovery = pending.prepared.resume_forward.is_some();
            self.preparing.remove(&session_id);
            if retry_recovery {
                self.recovery_candidates.insert(session_id.clone());
            }
            release_prompts(&session_id);
            answer(
                pending.reply,
                Err(StartRefusal(format!(
                    "could not record the active review: {error}"
                ))),
            );
        }
    }

    fn state_saved(
        &mut self,
        session_id: String,
        completion: PersistenceCompletion,
        result: Result<(), String>,
    ) {
        match completion {
            PersistenceCompletion::Open => {
                let Some(pending) = self.pending_open.remove(&session_id) else {
                    return;
                };
                self.preparing.remove(&session_id);
                if let Err(error) = result {
                    if pending.prepared.resume_forward.is_some() {
                        self.recovery_candidates.insert(session_id.clone());
                    }
                    release_prompts(&session_id);
                    answer(
                        pending.reply,
                        Err(StartRefusal(format!(
                            "could not record the active review: {error}"
                        ))),
                    );
                    return;
                }
                let seed = seed_from_session(
                    &pending.prepared.materialized,
                    pending.prepared.tier,
                    &pending.prepared.state,
                    if pending.manual {
                        "manual"
                    } else {
                        "automatic"
                    },
                );
                let (driver, requests) =
                    if let Some(pending) = pending.prepared.resume_forward.clone() {
                        let command_id = pending.command_id.clone();
                        let (mut driver, _) = TurnReviewDriver::resume_forward(seed, pending);
                        let requests = driver.forward(command_id);
                        (driver, requests)
                    } else {
                        TurnReviewDriver::start(seed)
                    };
                self.reviews.insert(
                    session_id.clone(),
                    ReviewSlot {
                        epoch: pending.epoch,
                        driver,
                        roles: BTreeMap::new(),
                        reviewer: pending.prepared.reviewer,
                        state: pending.prepared.state,
                        // `start_role` assigns a process-wide generation before
                        // every fresh role. Zero remains the explicit
                        // generation for a role that resumes in place.
                        generation: 0,
                    },
                );
                answer(pending.reply, Ok(()));
                self.run(&session_id, requests);
            }
            PersistenceCompletion::Forward => {
                let Some(requests) = self.awaiting_forward_persistence.remove(&session_id) else {
                    return;
                };
                if let Err(error) = result {
                    if let Some(slot) = self.reviews.get_mut(&session_id) {
                        slot.driver.forward_failed(format!(
                            "the handoff could not be recorded durably: {error}"
                        ));
                    }
                    self.publish(&session_id);
                    return;
                }
                self.run(&session_id, requests);
            }
            PersistenceCompletion::Close => {
                self.closing.remove(&session_id);
                if let Err(error) = result {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "could not clear the active review marker"
                    );
                }
                let notice = self.reviews.get(&session_id).and_then(|slot| {
                    resolution_notice(slot.driver.phase(), slot.driver.last_verdict())
                });
                self.reviews.remove(&session_id);
                release_prompts(&session_id);
                if let Some(notice) = notice {
                    self.record_notice(&session_id, notice);
                }
                self.publish(&session_id);
            }
        }
    }

    fn persist(
        &self,
        session_id: String,
        state: TurnReviewState,
        completion: Option<PersistenceCompletion>,
    ) -> Result<(), String> {
        self.persistence
            .as_ref()
            .ok_or_else(|| "the review persistence lane stopped".to_owned())?
            .send(PersistenceRequest::Save {
                session_id,
                state: Box::new(state),
                completion,
            })
            .map_err(|_| "the review persistence lane stopped".to_owned())
    }

    /// Forwards, dismisses, or cancels an open review on a surface's request.
    fn resolve(&mut self, session_id: &str, resolution: Resolution) -> Result<(), String> {
        let (requests, pending_state) = {
            let Some(slot) = self.reviews.get_mut(session_id) else {
                return Err("no review is open for that session".to_owned());
            };
            let requests = match resolution {
                Resolution::Forwarded => {
                    if !slot.driver.can_forward() {
                        return Err("there are no findings to forward".to_owned());
                    }
                    slot.driver.forward(
                        new_command_id("review-forward").map_err(|error| format!("{error:#}"))?,
                    )
                }
                Resolution::Dismissed => {
                    if slot.driver.verdict().is_none() {
                        return Err("the review has not reached a verdict yet".to_owned());
                    }
                    slot.driver.dismiss()
                }
                Resolution::Cancelled => slot.driver.cancel(),
                Resolution::NothingToReview | Resolution::CoverageStarted => {
                    return Err("that is not a resolution a surface can ask for".to_owned());
                }
            };
            if requests.is_empty() {
                return Err("the review could not be resolved that way".to_owned());
            }
            let pending_state = if resolution == Resolution::Forwarded {
                let Some(pending) = slot.driver.pending_forward() else {
                    return Err("the review handoff has no durable findings".to_owned());
                };
                slot.state.pending_forward = Some(pending);
                Some(slot.state.clone())
            } else {
                None
            };
            (requests, pending_state)
        };
        if let Some(state) = pending_state {
            self.awaiting_forward_persistence
                .insert(session_id.to_owned(), requests);
            if let Err(error) = self.persist(
                session_id.to_owned(),
                state,
                Some(PersistenceCompletion::Forward),
            ) {
                self.awaiting_forward_persistence.remove(session_id);
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.driver.forward_failed(format!(
                        "the handoff could not be recorded durably: {error}"
                    ));
                }
                self.publish(session_id);
                return Err(error);
            }
            self.publish(session_id);
            return Ok(());
        }
        self.run(session_id, requests);
        Ok(())
    }

    /// Ends a review that cannot continue. Every failure path is the same: a
    /// verdict the user dismisses, and a baseline that stays where it was, so
    /// the change is reviewed again rather than silently skipped.
    fn fail(&mut self, session_id: &str, message: impl Into<String>) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        slot.state.active = None;
        let state = slot.state.clone();
        let requests = slot.driver.request_failed(message);
        // A failed review remains visible so the person can dismiss it, but it
        // no longer owns the turn or has work capable of progressing.
        release_prompts(session_id);
        if let Err(error) = self.persist(session_id.to_owned(), state, None) {
            tracing::warn!(session_id, %error, "could not queue failed review persistence");
        }
        self.run(session_id, requests);
    }

    fn run(&mut self, session_id: &str, requests: Vec<ReviewRequest>) {
        for request in requests {
            self.run_one(session_id, request);
        }
        self.publish(session_id);
    }

    fn run_one(&mut self, session_id: &str, request: ReviewRequest) {
        match request {
            ReviewRequest::CaptureDelta { baselines } => {
                self.review_step(
                    session_id,
                    ReviewerAction::CaptureDelta { baselines },
                    |outcome| {
                        ReviewStep::Delta(match outcome {
                            Ok(ReviewerOutcome::Delta { repositories }) => Ok(repositories),
                            other => Err(unexpected(other)),
                        })
                    },
                );
            }
            ReviewRequest::AnalyzeDelta { repositories } => {
                self.review_step(
                    session_id,
                    ReviewerAction::AnalyzeDelta { repositories },
                    |outcome| {
                        ReviewStep::Analysis(match outcome {
                            Ok(ReviewerOutcome::ChangedFunctions { packet }) => Ok(packet),
                            other => Err(unexpected(other)),
                        })
                    },
                );
            }
            ReviewRequest::StartRole { role, fresh } => self.start_role(session_id, role, fresh),
            ReviewRequest::PromptRole {
                role,
                command_id,
                prompt,
            } => {
                self.prompt_role(session_id, &role, command_id, prompt);
                self.poll_role(session_id, &role, Duration::ZERO);
            }
            ReviewRequest::PromptPrimary { command_id, prompt } => {
                // The review's own corrective prompt must not be held by the
                // review's own lock, and by this point the review has
                // resolved, so the lock is already released below.
                self.prompt_primary(session_id, command_id, prompt);
            }
            ReviewRequest::PauseRole { role } => {
                let session_id = session_id.to_owned();
                self.spawn_reviewer(
                    session_id.clone(),
                    Some(role),
                    ReviewerAction::Pause,
                    move |outcome| {
                        if let Err(error) = outcome {
                            tracing::debug!(
                                session_id = %session_id,
                                %error,
                                "pausing a review role failed"
                            );
                        }
                        None
                    },
                );
            }
            ReviewRequest::AdvanceBaseline {
                trees,
                reviewed_through_ordinal,
            } => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.baselines = trees.clone();
                    slot.state.reviewed_through_ordinal = reviewed_through_ordinal;
                    // Clear an accepted handoff in the same durable state
                    // write as its prior-review record and baseline. Until
                    // this point shutdown/restart must retain it for command
                    // id reconciliation.
                    slot.state.pending_forward = None;
                    let state = slot.state.clone();
                    if let Err(error) = self.persist(session_id.to_owned(), state, None) {
                        tracing::warn!(session_id, %error, "could not queue review baseline persistence");
                    }
                }
                let session_id = session_id.to_owned();
                self.spawn_reviewer(
                    session_id.clone(),
                    None,
                    ReviewerAction::AdvanceBaseline { trees },
                    move |outcome| {
                        if let Err(error) = outcome {
                            // The controller's copy is what the next capture is
                            // taken against; the worker-side ref is only a gc
                            // pin, so a failure here costs nothing but the pin.
                            tracing::debug!(
                                session_id = %session_id,
                                %error,
                                "the review baseline ref could not be pinned"
                            );
                        }
                        None
                    },
                );
            }
            ReviewRequest::RecordPriorReview { prior } => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.prior_review = Some(prior);
                }
            }
            ReviewRequest::ClearPriorReview => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.prior_review = None;
                    let state = slot.state.clone();
                    if let Err(error) = self.persist(session_id.to_owned(), state, None) {
                        tracing::warn!(session_id, %error, "could not queue prior review cleanup");
                    }
                }
            }
            ReviewRequest::Close => {
                if self.closing.contains(session_id) {
                    return;
                }
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.active = None;
                    if matches!(
                        slot.driver.phase(),
                        TurnReviewPhase::Resolved(Resolution::Cancelled)
                    ) {
                        // A user explicitly cancelled a rejected handoff, so
                        // discard its retry record along with the held pane.
                        // An in-flight or accepted handoff never reaches this
                        // branch before its durable reconciliation sequence.
                        slot.state.pending_forward = None;
                    }
                    let state = slot.state.clone();
                    self.closing.insert(session_id.to_owned());
                    // The primary handoff has already returned a durable
                    // acceptance before Close can be requested. Releasing now
                    // lets later user prompts queue behind that accepted
                    // corrective turn; no held prompt can overtake it.
                    release_prompts(session_id);
                    if let Err(error) = self.persist(
                        session_id.to_owned(),
                        state,
                        Some(PersistenceCompletion::Close),
                    ) {
                        tracing::warn!(session_id, %error, "could not queue review close persistence");
                        self.closing.remove(session_id);
                        self.reviews.remove(session_id);
                        release_prompts(session_id);
                    }
                }
            }
        }
    }

    /// Stages the configured reviewer profile and starts one role under it.
    fn start_role(&mut self, session_id: &str, role: String, fresh: bool) {
        let fresh_generation = if fresh {
            match next_review_generation() {
                Ok(generation) => Some(generation),
                Err(error) => {
                    self.fail(
                        session_id,
                        format!("the reviewer could not allocate a fresh conversation: {error}"),
                    );
                    return;
                }
            }
        } else {
            None
        };
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        // A fresh role must not reuse the running harness session: the
        // validator judges the reviewer's claims against source, so it must
        // not inherit them. Bumping the generation is what the sidecar reads
        // as "this is a different reviewer".
        if let Some(generation) = fresh_generation {
            slot.generation = generation;
        }
        let generation = slot.generation;
        let epoch = slot.epoch;
        let reviewer = slot.reviewer.clone();
        let repositories = slot.driver.repository_roots();
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let result = launch_role(
                &control,
                &environment,
                &session_id,
                &role,
                &reviewer,
                generation,
                &repositories,
            )
            .await;
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::RoleStarted { role, result },
            });
        });
    }

    fn prompt_role(&mut self, session_id: &str, role: &str, command_id: String, prompt: String) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let owner = session_id.to_owned();
        let result_role = role.to_owned();
        self.spawn_reviewer(
            session_id.to_owned(),
            Some(role.to_owned()),
            ReviewerAction::Submit {
                command_id,
                command: prompt_command(prompt),
            },
            move |outcome| {
                let result = match outcome {
                    Ok(ReviewerOutcome::Accepted { .. }) => Ok(()),
                    other => Err(unexpected(other)),
                };
                Some(HostEvent::Step {
                    session_id: owner,
                    epoch,
                    step: ReviewStep::RolePrompted {
                        role: result_role,
                        result,
                    },
                })
            },
        );
    }

    /// Sends the review's corrective prompt to the primary agent. The actor
    /// receives a scoped admission that is valid only for this review and
    /// command id; the hold remains until the resulting event acknowledges a
    /// durable relay acceptance.
    fn prompt_primary(&mut self, session_id: &str, command_id: String, prompt: String) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let Some(admission) = admit_review_delivery(session_id, epoch, &command_id) else {
            self.step(
                session_id.to_owned(),
                epoch,
                ReviewStep::PrimaryPrompted(Err(
                    "the review handoff admission is no longer valid".to_owned()
                )),
            );
            return;
        };
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let submitted = async {
                let handle = control
                    .session(session_id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                handle
                    .submit_review_delivery(admission, prompt_command(prompt))
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"))
            }
            .await;
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::PrimaryPrompted(submitted),
            });
        });
    }

    /// Reads one role's journal from where the host left off.
    fn poll_role(&mut self, session_id: &str, role: &str, delay: Duration) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        let transcript = slot.roles.entry(role.to_owned()).or_default();
        let after_ordinal = transcript.cursor_ordinal;
        let after_digest = if transcript.cursor_digest.is_empty() {
            mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            transcript.cursor_digest.clone()
        };
        let epoch = slot.epoch;
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        let role = role.to_owned();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = reviewer_action(
                &control,
                &session_id,
                Some(role.clone()),
                ReviewerAction::Attach {
                    after_ordinal,
                    after_digest,
                },
            )
            .await;
            let result = match result {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                other => Err(unexpected(other)),
            };
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::RoleEvents { role, result },
            });
        });
    }

    fn role_events(
        &mut self,
        session_id: String,
        role: String,
        result: Result<Vec<RelayEvent>, String>,
    ) {
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                if self.reviews.get(&session_id).is_some_and(|slot| {
                    matches!(slot.driver.phase(), TurnReviewPhase::Forwarding { .. })
                }) {
                    tracing::debug!(
                        session_id = %session_id,
                        role = %role,
                        %error,
                        "ignoring a late reviewer-role poll during primary handoff"
                    );
                    return;
                }
                self.fail(&session_id, error);
                return;
            }
        };
        let Some(slot) = self.reviews.get_mut(&session_id) else {
            return;
        };
        let idle = events.is_empty();
        let relay_session = role_session_id(&session_id, &role);
        let transcript = slot.roles.entry(role.clone()).or_default();
        transcript.apply(&relay_session, &events);
        // The newest agent message is not enough on its own: after the
        // validator starts, the reviewer's own findings are still the newest
        // message in that role's journal. The relay's completion record for
        // the exact command the driver submitted is what settles it.
        let awaited = slot
            .driver
            .awaited_commands()
            .into_iter()
            .find(|(awaited_role, _)| *awaited_role == role)
            .map(|(_, command_id)| command_id);
        let completed = awaited.as_ref().is_some_and(|awaited| {
            events.iter().any(|event| {
                matches!(
                    &event.observation,
                    RelayObservation::CommandCompleted { command_id, outcome }
                        if command_id == awaited
                            && matches!(
                                outcome,
                                mj_core::relay::RelayCommandOutcome::Prompt { .. }
                            )
                )
            })
        });
        let requests = match (completed, awaited) {
            (true, Some(awaited)) => {
                let answer = slot
                    .roles
                    .get(&role)
                    .and_then(RoleTranscript::latest_answer)
                    .unwrap_or_default();
                let slot = self.reviews.get_mut(&session_id).expect("the slot is open");
                slot.driver.role_turn_completed(&awaited, &answer)
            }
            _ => Vec::new(),
        };
        self.run(&session_id, requests);
        let Some(slot) = self.reviews.get(&session_id) else {
            return;
        };
        if slot.driver.active_roles().contains(&role) {
            self.poll_role(
                &session_id,
                &role,
                if idle {
                    ROLE_POLL_IDLE_INTERVAL
                } else {
                    Duration::ZERO
                },
            );
        }
        if role == SUPERVISOR_ROLE
            && self
                .reviews
                .get(&session_id)
                .is_some_and(|slot| slot.driver.supervisor_running())
        {
            self.poll_dispatches(&session_id);
        }
    }

    /// Collects the specialist lanes the supervisor asked for through its MCP
    /// tool. The tool answers the supervisor at once and leaves the request in
    /// the worker; this is where the host picks it up and launches them.
    fn poll_dispatches(&mut self, session_id: &str) {
        self.review_step(session_id, ReviewerAction::TakeLaneDispatches, |outcome| {
            ReviewStep::Dispatches(match outcome {
                Ok(ReviewerOutcome::LaneDispatches { requests }) => Ok(requests),
                other => Err(unexpected(other)),
            })
        });
    }

    /// Puts one controller-authored line into the session's conversation, so a
    /// resolution is visible on every surface rather than in one UI's notice
    /// bar.
    fn record_notice(&self, session_id: &str, text: String) {
        let control = self.control.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let recorded = async {
                let handle = control
                    .session(session_id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                let command_id =
                    new_command_id("turn-review-notice").map_err(|error| format!("{error:#}"))?;
                handle
                    .submit(command_id, RelayCommand::RecordNotice { text })
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"))
            }
            .await;
            // The conversation line is a courtesy; a relay that refuses it has
            // not damaged the review.
            if let Err(error) = recorded {
                tracing::debug!(
                    session_id = %session_id,
                    %error,
                    "could not record a review notice in the conversation"
                );
            }
        });
    }

    /// Runs one reviewer action for the default role and feeds its outcome
    /// back to the review that asked for it.
    fn review_step(
        &mut self,
        session_id: &str,
        action: ReviewerAction,
        into_step: impl FnOnce(Result<ReviewerOutcome, String>) -> ReviewStep + Send + 'static,
    ) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let owner = session_id.to_owned();
        self.spawn_reviewer(session_id.to_owned(), None, action, move |outcome| {
            Some(HostEvent::Step {
                session_id: owner,
                epoch,
                step: into_step(outcome),
            })
        });
    }

    fn spawn_reviewer(
        &self,
        session_id: String,
        role: Option<String>,
        action: ReviewerAction,
        into_event: impl FnOnce(Result<ReviewerOutcome, String>) -> Option<HostEvent> + Send + 'static,
    ) {
        let control = self.control.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome = reviewer_action(&control, &session_id, role, action).await;
            if let Some(event) = into_event(outcome) {
                let _ = events.send(event);
            }
        });
    }

    /// Republishes what surfaces read. Called after every state change, so a
    /// snapshot poll and a phone request see the same review.
    fn publish(&self, session_id: &str) {
        let mut views = self
            .shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = match self.reviews.get(session_id) {
            Some(slot) => {
                let next = slot.view(session_id);
                if views.get(session_id) == Some(&next) {
                    false
                } else {
                    views.insert(session_id.to_owned(), next);
                    true
                }
            }
            None => views.remove(session_id).is_some(),
        };
        drop(views);
        if changed {
            (self.shared.changed)();
        }
    }

    async fn shutdown(&mut self) -> Result<(), String> {
        let session_ids = self
            .preparing
            .iter()
            .chain(self.reviews.keys())
            .chain(self.recovery_in_flight.iter())
            .chain(self.recovery_candidates.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        for pending in std::mem::take(&mut self.pending_open).into_values() {
            answer(
                pending.reply,
                Err(StartRefusal("the daemon is shutting down".to_owned())),
            );
        }

        // Take the lane once and use that single sender for every final write.
        // No sender may survive the join below or the receiver can never
        // observe EOF.
        let lane = self.persistence.take();
        for (session_id, slot) in &mut self.reviews {
            slot.state.active = None;
            let queued = lane
                .as_ref()
                .ok_or_else(|| "the review persistence lane stopped".to_owned())
                .and_then(|persistence| {
                    persistence
                        .send(PersistenceRequest::Save {
                            session_id: session_id.clone(),
                            state: Box::new(slot.state.clone()),
                            completion: None,
                        })
                        .map_err(|_| "the review persistence lane stopped".to_owned())
                });
            if let Err(error) = queued {
                tracing::warn!(session_id, %error, "could not queue review shutdown persistence");
            }
        }
        self.preparing.clear();
        self.closing.clear();
        self.reviews.clear();
        for session_id in &session_ids {
            release_prompts(session_id);
            self.publish(session_id);
        }

        let clear_result = match lane {
            Some(lane) => {
                let (reply, cleared) = oneshot::channel();
                let sent = lane
                    .send(PersistenceRequest::ClearActive { reply })
                    .map_err(|_| "the review persistence lane stopped during shutdown".to_owned());
                drop(lane);
                match sent {
                    Ok(()) => cleared.await.map_err(|_| {
                        "the review persistence lane stopped before cleanup".to_owned()
                    })?,
                    Err(error) => Err(error),
                }
            }
            None => Err("the review persistence lane already stopped".to_owned()),
        };
        let task_result = match self.persistence_task.take() {
            Some(task) => task
                .await
                .map_err(|error| format!("review persistence lane panicked: {error}")),
            None => Ok(()),
        };
        clear_result.and(task_result)
    }
}

impl ReviewSlot {
    fn view(&self, session_id: &str) -> RuntimeReviewView {
        let verdict = match self.driver.phase() {
            TurnReviewPhase::Forwarding { synthesis, .. } => Some(VerdictView {
                kind: VerdictKind::Findings,
                text: synthesis.clone(),
                allowed: if matches!(
                    self.driver.phase(),
                    TurnReviewPhase::Forwarding { error: Some(_), .. }
                ) {
                    vec![Resolution::Forwarded, Resolution::Cancelled]
                } else {
                    Vec::new()
                },
            }),
            _ => self.driver.verdict().map(|verdict| match verdict {
                ReviewVerdict::Clean => VerdictView {
                    kind: VerdictKind::Clean,
                    text: String::new(),
                    allowed: Vec::new(),
                },
                ReviewVerdict::Findings { synthesis, .. } => VerdictView {
                    kind: VerdictKind::Findings,
                    text: synthesis.clone(),
                    allowed: vec![
                        Resolution::Forwarded,
                        Resolution::Dismissed,
                        Resolution::Cancelled,
                    ],
                },
                ReviewVerdict::Failed { reason } => VerdictView {
                    kind: VerdictKind::Failed,
                    text: reason.clone(),
                    // A failed review has nothing to forward, and dismissing
                    // it does not advance the baseline: the change stays
                    // unreviewed either way.
                    allowed: vec![Resolution::Dismissed, Resolution::Cancelled],
                },
            }),
        };
        RuntimeReviewView {
            session_id: session_id.to_owned(),
            tier: self.driver.tier(),
            phase: self.driver.phase().clone(),
            roles: self.driver.roles(),
            status: self.driver.status().to_owned(),
            verdict,
        }
    }
}

fn answer(
    reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    result: Result<(), StartRefusal>,
) {
    if let Some(reply) = reply {
        let _ = reply.send(result);
    }
}

fn unexpected(outcome: Result<ReviewerOutcome, String>) -> String {
    match outcome {
        Ok(other) => format!("unexpected reviewer response {other:?}"),
        Err(error) => error,
    }
}

fn prompt_command(prompt: String) -> RelayCommand {
    RelayCommand::Prompt {
        prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new(prompt),
        )],
    }
}

async fn reviewer_action(
    control: &SessionManagerControl,
    session_id: &str,
    role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    let handle: ManagedSessionHandle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| format!("{error:#}"))?;
    handle
        .reviewer_as(role, action)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Stages the configured reviewer profile and starts one role under it.
async fn launch_role(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
    role: &str,
    reviewer: &ReviewerIdentity,
    generation: u64,
    repositories: &[PathBuf],
) -> Result<(), String> {
    // A specialist lane's analyzers are its identity, so it gets the `slopcop`
    // set as well as navigation; every other role navigates and reads rather
    // than running analyzers. The intent analyst gets no tools at all: it
    // reads the user's messages, not the code.
    let lane = mj_review::lanes::lane_by_id(role).is_some();
    let mcp_servers = if role == INTENT_ROLE {
        Vec::new()
    } else {
        mj_review::bifrost::review_mcp_servers(
            repositories,
            if lane {
                mj_review::lanes::LANE_BIFROST_TOOLSET
            } else {
                mj_review::lanes::SUPERVISOR_BIFROST_TOOLSET
            },
        )
    };
    // Only the supervisor may launch specialists.
    let dispatch_tool = role == SUPERVISOR_ROLE;
    let staged = {
        let session_id = session_id.to_owned();
        let profile = reviewer.profile.clone();
        let environment = environment.clone();
        tokio::task::spawn_blocking(move || {
            environment.stage(
                &session_id,
                &profile,
                generation,
                &mcp_servers,
                dispatch_tool,
            )
        })
        .await
        .map_err(|error| format!("staging the reviewer stopped: {error}"))??
    };
    let mut config = staged;
    config.model = reviewer.model.clone();
    config.effort = reviewer.effort.clone();
    match reviewer_action(
        control,
        session_id,
        Some(role.to_owned()),
        ReviewerAction::Start {
            config: Box::new(config),
        },
    )
    .await
    {
        Ok(ReviewerOutcome::Started(_)) => Ok(()),
        other => Err(unexpected(other)),
    }
}

/// Everything a review needs that only the database and the worker can answer.
async fn prepare(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
    reviewer: &ReviewerIdentity,
    tier: ReviewTier,
) -> Result<Prepared, StartRefusal> {
    let profile = reviewer.profile.clone();
    let session = session_id.to_owned();
    let environment = environment.clone();
    // Both answers come from the controller and its database, so they are
    // asked together, once, off the host's loop.
    let checked = tokio::task::spawn_blocking(move || -> Result<TurnReviewState, String> {
        environment.check(&session, &profile)?;
        environment.load_state(&session)
    })
    .await
    .map_err(|error| StartRefusal(format!("preparing the review stopped: {error}")))?;
    let state = checked.map_err(StartRefusal)?;
    // Mutual exclusion with a plan-review second opinion: they share the
    // default reviewer role, and the running one keeps the slot. Checked
    // against the worker rather than against any UI's state, because the
    // worker is the only place that knows.
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| StartRefusal(format!("{error:#}")))?;
    match handle.reviewer(ReviewerAction::Status).await {
        Ok(ReviewerOutcome::Status(state)) if state.active_prompt.is_some() => {
            return Err(StartRefusal(
                "the reviewer is busy with a second opinion".to_owned(),
            ));
        }
        Ok(_) => {}
        Err(error) => return Err(StartRefusal(format!("{error:#}"))),
    }
    // Reviewer actions and primary prompt submissions are serialized by the
    // same session actor. Anything accepted before the admission hold is
    // reflected here; anything after it was refused by the actor.
    let view = handle.view();
    if !view.connected {
        return Err(StartRefusal("this session is not connected".to_owned()));
    }
    let Some(snapshot) = view.snapshot else {
        return Err(StartRefusal(
            "this session has no transcript yet".to_owned(),
        ));
    };
    if !matches!(
        snapshot.materialized.execution,
        MaterializedExecutionState::Idle
    ) {
        return Err(StartRefusal(
            "a review runs between turns; this one is still working".to_owned(),
        ));
    }
    if !snapshot.materialized.queued_prompts.is_empty() {
        return Err(StartRefusal(
            "prompts are queued; the review waits for them".to_owned(),
        ));
    }
    Ok(Prepared {
        state,
        reviewer: reviewer.clone(),
        tier,
        materialized: Box::new(snapshot.materialized),
        resume_forward: None,
    })
}

/// Loads the durable handoff before touching the live actor. A missing
/// pending record means the accepted result had already been reconciled; the
/// interrupted review can then stay cancelled without fabricating a notice.
async fn prepare_recovery(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
) -> Result<Option<Prepared>, String> {
    let session = session_id.to_owned();
    let environment = environment.clone();
    let state = tokio::task::spawn_blocking(move || environment.load_state(&session))
        .await
        .map_err(|error| format!("loading the pending review handoff stopped: {error}"))??;
    let Some(pending) = state.pending_forward.clone() else {
        return Ok(None);
    };
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| format!("{error:#}"))?;
    let view = handle.view();
    if !view.connected {
        return Err("the primary session is not connected".to_owned());
    }
    let Some(snapshot) = view.snapshot else {
        return Err("the primary session has no transcript yet".to_owned());
    };
    if !matches!(
        snapshot.materialized.execution,
        MaterializedExecutionState::Idle
    ) {
        return Err("the primary session is still working".to_owned());
    }
    if !snapshot.materialized.queued_prompts.is_empty() {
        return Err("prompts are queued; the pending handoff waits for them".to_owned());
    }
    Ok(Some(Prepared {
        state,
        // No reviewer process is started for a handoff-only recovery.
        reviewer: ReviewerIdentity {
            profile: String::new(),
            model: None,
            effort: None,
        },
        tier: ReviewTier::Quick,
        materialized: Box::new(snapshot.materialized),
        resume_forward: Some(pending),
    }))
}

/// The transcript line a resolution leaves behind, on every surface.
#[must_use]
pub fn resolution_notice(
    phase: &TurnReviewPhase,
    last_verdict: Option<&ReviewVerdict>,
) -> Option<String> {
    let TurnReviewPhase::Resolved(resolution) = phase else {
        return None;
    };
    Some(match resolution {
        Resolution::Forwarded => "Review findings sent to the agent".to_owned(),
        Resolution::Dismissed => match last_verdict {
            Some(ReviewVerdict::Clean) => "Review complete: no material findings".to_owned(),
            Some(ReviewVerdict::Failed { .. }) => {
                "Review failed; the change stays unreviewed".to_owned()
            }
            _ => "Review dismissed".to_owned(),
        },
        Resolution::Cancelled => match last_verdict {
            Some(ReviewVerdict::Failed { .. }) => {
                "Review failed; the change stays unreviewed".to_owned()
            }
            _ => "Review cancelled".to_owned(),
        },
        Resolution::NothingToReview => "Nothing to review: the turn changed no files".to_owned(),
        Resolution::CoverageStarted => {
            "Review coverage starts here; the next completed turn is reviewed".to_owned()
        }
    })
}

/// Builds the review's seed from the session's own projection.
///
/// This is the daemon-side twin of what the chat used to read out of its view
/// state: the latest user prompt is the task, all chronological user messages
/// are the intent context, the agent's closing message is the result,
/// and a compact trajectory says what it did.
fn seed_from_session(
    session: &MaterializedSession,
    tier: ReviewTier,
    state: &TurnReviewState,
    _trigger: &str,
) -> TurnReviewSeed {
    let reviewed_through = state.reviewed_through_ordinal;
    let mut task = String::new();
    let mut user_messages = Vec::new();
    let mut initial_result = String::new();
    let mut trajectory = Vec::new();
    for item in &session.transcript {
        match &item.body {
            mj_core::state::TranscriptBody::User { content } => {
                let text = mj_core::transcript::materialized_content_text(content);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if mj_core::second_opinion::is_control_origin_prompt(text) {
                    continue;
                }
                task = text.to_owned();
                // The intent analyst needs the complete chronological user
                // history to distinguish a current steering prompt from an
                // earlier requirement. `task` separately identifies the
                // latest outer prompt.
                user_messages.push(UserMessage::prompt(text));
                if item.position > reviewed_through {
                    trajectory.push(format!("user: {text}"));
                }
            }
            mj_core::state::TranscriptBody::Agent { chunks, .. } => {
                if !item.is_nonempty_agent_message() {
                    continue;
                }
                let text = mj_core::transcript::materialized_chunks_text(chunks);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                initial_result = text.to_owned();
                if item.position > reviewed_through {
                    trajectory.push(format!("agent: {text}"));
                }
            }
            mj_core::state::TranscriptBody::Tool { call, .. } => {
                // The tool's own title, straight out of the stored ACP call:
                // the trajectory says what the agent did, and the captured
                // patch already carries what it changed.
                let title = call
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if item.position > reviewed_through && !title.is_empty() {
                    trajectory.push(format!("tool: {title}"));
                }
            }
            _ => {}
        }
    }
    TurnReviewSeed {
        tier,
        task,
        user_messages,
        initial_result,
        trajectory: trajectory.join("\n"),
        baselines: state.baselines.clone(),
        through_ordinal: session.applied_event_ordinal,
        prior_review: state.prior_review.clone(),
    }
}

#[cfg(test)]
mod tests;

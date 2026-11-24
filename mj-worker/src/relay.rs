//! Persistent, transport-neutral protocol core for a Hel target worker.
//!
//! The worker never listens on a network port. Controllers speak newline-
//! delimited JSON through `mj worker proxy`, which can itself be carried over
//! SSH or a container exec stream.
//!
//! This module is the root of the relay implementation. It keeps the
//! `DurableRelay` type itself: opening and recovering a session, the
//! operational-state and activity clocks, and recording observations and
//! session updates. The rest of the implementation sits in sibling modules:
//! [`requests`] answers relay envelopes, [`commands`] owns the command queue
//! and the claim scheduler, [`background`] tracks agent-owned background work
//! and terminals, [`replay`] serves replay pages and cursor validation, and
//! [`journal`] does durable journal I/O. The wire protocol and the
//! deterministic event/snapshot state machine are defined in mj-core and used
//! by both runtime owners.

pub use mj_core::relay::*;
mod background;
#[cfg(test)]
pub(crate) use background::AirTaskFixture;
mod checkpoint_hold;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InPlaceHarnessRestartDecision {
    Proceed,
    AlreadyRestarted,
}

#[cfg(test)]
mod checkpoint_hold_tests;
mod commands;
#[cfg(test)]
mod in_place_move_tests;
mod journal;
mod native_history;
mod replay;
mod requests;
#[cfg(unix)]
mod reviewer_admission;
#[cfg(unix)]
pub(crate) use reviewer_admission::ReviewerAdmission;
mod serving;
mod verdict;
use background::{AirTaskEntry, KimiProvisionalTask, KimiTaskEntry, is_agent_output};
use commands::validate_identifier;
pub use replay::{DeferredRelayAttach, RelayReplayPlan};
pub use serving::serve_relay_json_lines;
#[cfg(test)]
mod authentication_tests;
#[cfg(test)]
mod capacity_tests;
#[cfg(test)]
mod snapshot_tests;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use agent_client_protocol::v1::schema::{ContentBlock, SessionUpdate};
use anyhow::{Context, Result, anyhow, bail};

use journal::{
    JournalReadMode, RelayJournalSpan, open_relay_journal, read_restored_relay_seed,
    visit_relay_journal_file,
};
use mj_checkpoint::archive::CanonicalQueuedCommandKind;
use mj_core::clock::epoch_millis;
/// Whether this relay models turns the harness starts on its own.
///
/// The harness-specific policy chooses which signals open and settle a turn. See
/// `.agents/docs/claude-autonomous-turns.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HarnessTurnPolicy {
    #[default]
    Disabled,
    ClaudeAdapter,
    CodexAdapter,
    Antigravity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SteeringDeliveryMode {
    #[default]
    Acp,
    /// The harness has no mailbox hook; carry waking events through its
    /// ordinary durable prompt queue and safe-boundary steering instead.
    AcpWithMailbox,
    HookPostToolBatch,
    HookTurnEnd,
}

impl SteeringDeliveryMode {
    fn for_harness(harness: mj_core::config::HarnessKind, boundary_hook: bool) -> Self {
        match (harness, boundary_hook) {
            (mj_core::config::HarnessKind::Dsh, _) => Self::AcpWithMailbox,
            (
                mj_core::config::HarnessKind::Claude | mj_core::config::HarnessKind::Antigravity,
                true,
            ) => Self::HookPostToolBatch,
            (
                mj_core::config::HarnessKind::Claude | mj_core::config::HarnessKind::Antigravity,
                false,
            ) => Self::HookTurnEnd,
            _ => Self::Acp,
        }
    }

    fn uses_acp(self) -> bool {
        matches!(self, Self::Acp | Self::AcpWithMailbox)
    }
}

/// How long a stop sent during a turn Claude Code started on its own waits
/// for the result that ends an interrupted cycle before the relay ends the
/// turn itself.
pub const HARNESS_TURN_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Where this relay learns about commands the agent left running.
///
/// The authoritative source of agent-owned background commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BackgroundWorkPolicy {
    /// Live terminals Hel spawned for the agent.
    #[default]
    HostedTerminals,
    /// AIR tasks announced by Claude or Codex, together with hosted terminals.
    AirTasks,
    /// Kimi's native detached-agent set, together with hosted terminals.
    KimiTasks,
    AntigravityTasks,
}

pub(crate) struct PendingLiveBaseline {
    reply: Option<
        tokio::sync::oneshot::Sender<
            std::result::Result<mj_core::relay::LiveSessionBaseline, String>,
        >,
    >,
}

struct PendingNativeResume {
    #[cfg(any(unix, test))]
    resume_id: u64,
    goal: Option<mj_core::goal::GoalSnapshot>,
}

/// The relay behind its lock. Every change to the relay takes this lock
/// through [`Self::lock`], so each such release is counted as a possible
/// change: a connection waiting for one (see
/// [`mj_core::relay::RelayRequest::WaitForChange`]) wakes and compares what it
/// is waiting on, without the relay listing its mutations. A waiting
/// connection reads through [`Self::observe`], which cannot change the relay
/// and is not counted, so waits never wake one another.
pub struct RelayCell {
    relay: std::sync::Mutex<DurableRelay>,
    released: tokio::sync::watch::Sender<u64>,
    #[cfg(unix)]
    pub(crate) preparation_writer: std::sync::Arc<crate::worker_runtime::PreparationGate>,
    #[cfg(unix)]
    pub(crate) guardian: std::sync::Arc<crate::worker_runtime::guardian_endpoint::GuardianEndpoint>,
}

impl RelayCell {
    pub fn new(relay: DurableRelay) -> Self {
        Self {
            relay: std::sync::Mutex::new(relay),
            released: tokio::sync::watch::Sender::new(0),
            #[cfg(unix)]
            preparation_writer: Default::default(),
            #[cfg(unix)]
            guardian: Default::default(),
        }
    }

    pub fn lock(&self) -> std::sync::LockResult<RelayGuard<'_>> {
        let released = &self.released;
        match self.relay.lock() {
            Ok(guard) => Ok(RelayGuard { guard, released }),
            Err(poisoned) => Err(std::sync::PoisonError::new(RelayGuard {
                guard: poisoned.into_inner(),
                released,
            })),
        }
    }

    /// Read the relay without counting as a possible change. The guard only
    /// reads, so nothing a change wait must see can happen under it.
    pub fn observe(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, DurableRelay>> {
        self.relay.lock()
    }

    /// Changes on every release of a [`Self::lock`] guard.
    pub fn releases(&self) -> tokio::sync::watch::Receiver<u64> {
        self.released.subscribe()
    }
}

pub struct RelayGuard<'a> {
    guard: std::sync::MutexGuard<'a, DurableRelay>,
    released: &'a tokio::sync::watch::Sender<u64>,
}

impl std::ops::Deref for RelayGuard<'_> {
    type Target = DurableRelay;

    fn deref(&self) -> &DurableRelay {
        &self.guard
    }
}

impl std::ops::DerefMut for RelayGuard<'_> {
    fn deref_mut(&mut self) -> &mut DurableRelay {
        &mut self.guard
    }
}

impl Drop for RelayGuard<'_> {
    fn drop(&mut self) {
        // A woken waiter takes the lock to look, so it sees this holder's
        // writes once the mutex guard below is released.
        self.released
            .send_modify(|count| *count = count.wrapping_add(1));
    }
}

/// Relay evidence about the native thread named at launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSessionHistory {
    /// The thread may contain history and must not be replaced.
    MayHaveHistory,
    /// The thread may be replaced only if the harness reports it missing.
    LoadBeforeReplace,
    /// Durable evidence proves this relay created the thread and never used it.
    CreatedHereUnused,
}

/// Durable, session-side ACP store-and-forward relay.
///
/// `RelaySnapshot` is the canonical operational state. Journal spans retain
/// observations newer than the durable controller acknowledgement. Operation-owned
/// command receipts survive separately until their consumers finish.
pub struct DurableRelay {
    /// Immutable input supplied when this worker was created; execution and
    /// completion live exclusively in the ordinary durable command queue.
    initial_commands: Vec<mj_core::worker_launch::InitialWorkerCommand>,
    // Only the Unix worker runtime updates this from SetOperatorAway.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) operator_away: crate::acp::OperatorAway,
    root: PathBuf,
    relay_version: String,
    /// Content address of the executable serving this relay, reported in
    /// hello. The crate version cannot tell two builds of the same release
    /// apart, and a controller has to know whether the worker it is talking to
    /// is the binary it would install today.
    worker_build: Option<String>,
    /// Epoch milliseconds at which the harness session was last configured, or
    /// 0 while it is not: the one owner of ACP process readiness, which is
    /// never recovered from the journal. [`Self::is_acp_ready`] derives the
    /// readiness flag from it, and the work-facts scan reads the instant to
    /// tell the harness's own processes, which exist by then, from work the
    /// agent started, so the two cannot disagree.
    harness_ready_ms: Arc<AtomicI64>,
    /// A bridge has connected and its session is not open yet: the load or
    /// new-session request and the model and mode applied after it. Output
    /// in this window answers no prompt (R8-4). Never recovered from the
    /// journal: every bridge start records its own `AgentInitialized`.
    session_setup: bool,
    checkpoint_only: bool,
    /// Active in-place harness restart operation ID serialized with the hold.
    in_place_harness_restart_in_flight: Option<String>,
    /// Live reviewer leases share this owner's atomic checkpoint decision.
    reviewer_admissions: BTreeMap<u64, String>,
    /// Reviewer leases exist only around the Unix reviewer runtime, so this
    /// stays compiled on Windows with the field it feeds.
    #[cfg_attr(not(unix), allow(dead_code))]
    next_reviewer_admission: u64,
    /// Optional extension advertised by the current ACP process.
    steering_supported: Option<bool>,
    /// The connected bridge advertises steering, so the relay may steer
    /// queued prompts on its own; the bridge queues them to its next safe
    /// boundary. Belongs to the bridge, like readiness.
    automatic_steering: bool,
    /// How this harness receives an accepted steer. Claude never uses ACP
    /// steering because its bridge can cancel an in-flight tool hook.
    steering_delivery_mode: SteeringDeliveryMode,
    /// When this worker delivers mailbox messages to a working Codex session,
    /// resolved from its staged provider. `None` until preparation resolves it,
    /// and for harnesses whose delivery is not classified here; `None` never
    /// permits a `PostToolUse` delivery.
    mailbox_delivery_mode: Option<mj_core::mailbox::MailboxDeliveryMode>,
    snapshot: RelaySnapshot,
    /// Canonical, non-overlapping slices of the durable journal. Event bodies
    /// stay on disk; only enough metadata to locate a requested ordinal is
    /// retained in memory.
    journal_spans: Vec<RelayJournalSpan>,
    /// A small optimization for recent cursor validation. This is never the
    /// source of truth and is deliberately fixed-size.
    hot_events: VecDeque<RelayEvent>,
    /// Digests proven while serving older replay pages. Controllers normally
    /// return the previous page's frontier, so retaining those sparse cursors
    /// avoids decompressing that segment again merely to validate the next
    /// request. Event bodies remain on disk and authoritative.
    replay_cursors: VecDeque<(u64, String)>,
    /// Journal bytes appended since `relay-state.json` last matched this
    /// snapshot. Zero means the durable file is exactly this state.
    unpersisted_journal_bytes: usize,
    /// Bumped whenever a captured [`RelayReplayPlan`] stops describing the
    /// journal on disk: sealing moves the active segment's events into a new
    /// file, and garbage collection rewrites and prunes segments. Plain
    /// appends never bump it — they only extend the active segment, which a
    /// lock-free reader already tolerates.
    journal_generation: u64,
    /// Retry physical prefix cleanup once after reopen, or after a failed GC.
    journal_cleanup_pending: bool,
    /// A failed rollback left the on-disk append boundary unknown.
    journal_append_failure: Option<String>,
    #[cfg(test)]
    journal_append_fault: Option<journal::JournalAppendFault>,
    pending_live_baselines: Vec<PendingLiveBaseline>,
    pending_native_resumes: Vec<PendingNativeResume>,
    acp_activity: AcpActivityClock,
    step_clock: crate::acp::StepClock,
    /// Whether harness announcements or output with no prompt open a turn.
    /// The runtime sets this from the configured harness right after `open`.
    harness_turns: HarnessTurnPolicy,
    /// Where commands the agent left running are read from.
    background_work: BackgroundWorkPolicy,
    /// Tool calls that still report pending or in-progress, with the start of
    /// their current status. This is stronger foreground evidence than a
    /// harness-neutral step clock, whose prose steps have no portable ending.
    ///
    /// A shared handle rather than a private map: the turn stall watchdog
    /// reads the same tool calls, and when it could not, a turn blocked in a
    /// long build was failed as if the harness had died (#1020).
    foreground_tools: mj_core::activity::ToolsInFlight,
    turn_context: mj_transcript::turn_context::TurnContext,
    verdict_harness: Option<mj_core::config::HarnessKind>,
    pub(crate) jev_disabled: bool,
    #[cfg_attr(not(unix), allow(dead_code))] // The worker control runtime is Unix-only.
    pub(crate) authentication_login_path: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))] // Read only by the Unix worker control runtime.
    pub(crate) login_identity: crate::acp::ClaudeLoginIdentity,
    replied_verdict: verdict::RepliedVerdictState,
    pub(crate) recovers_child_quota: bool,
    /// Adapter-owned tasks, keyed by their owning session and task identity.
    air_tasks: BTreeMap<String, AirTaskEntry>,
    /// Work facts the coordinator observed for the runtime event it is about to
    /// apply (see [`Self::event_settles_autonomous_turn`]). Only a settlement
    /// of a turn the harness started on its own takes them; every event
    /// application replaces them, so a stale observation never reaches a later
    /// turn.
    pending_settle_facts: Option<mj_core::work_facts::WorkFacts>,
    /// Claude tasks whose stop this process requested and the adapter has not
    /// yet acknowledged. The adapter answers a stop with a plain agent chunk
    /// and no model cycle, so no result would settle a harness turn opened
    /// for that chunk.
    claude_pending_stops: BTreeMap<String, BTreeSet<String>>,
    antigravity_work: background::AntigravityWorkState,
    /// Kimi detached agents and processes confirmed by its native journal.
    kimi_background_tasks: BTreeMap<String, KimiTaskEntry>,
    /// Positive ACP launch evidence retained until the native journal
    /// correlates it through native task identity or `parentToolCallId`.
    kimi_provisional_tasks: BTreeMap<String, KimiProvisionalTask>,
    kimi_observed_task_ids: BTreeSet<String>,
    kimi_observed_tool_ids: BTreeSet<String>,
    /// Whether provider-owned background work is synchronized. Other
    /// policies leave this absent.
    background_work_known: Option<bool>,
    /// The classifier the sub-agent handback check asks. Only the Unix worker
    /// sets and reads it; the field stays on Windows so the relay type matches.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) handback_verdict_client: Option<crate::acp::verdict_client::VerdictClient>,
    /// When a Claude background task last completed or failed, until the
    /// harness turn that answers it opens. Claude Code always follows such a
    /// settle with a task-notification turn; publishing the settle time lets
    /// every quiet check treat the gap as a turn in flight.
    task_settled_at_ms: Option<i64>,
    /// ACP terminals are connection-owned and disappear when that connection
    /// is torn down, so they belong in memory rather than the durable relay
    /// snapshot or transcript journal.
    active_agent_terminals: BTreeMap<String, ActiveAgentTerminal>,
    /// A short-lived guard against a fast child reporting close before the
    /// create handler publishes its started event on the shared channel.
    closed_agent_terminals: BTreeSet<String>,
    /// Hosted terminal each agent tool call embedded, keyed by the native tool
    /// call id. Kimi runs a detached shell in one of our terminals, so this is
    /// what ties its native task record to the terminal doing the work.
    agent_terminal_tool_calls: BTreeMap<String, String>,
    /// The worker's own writes waiting for the ready checkpoint barrier to
    /// end, in arrival order. See `checkpoint_hold`.
    held_writes: VecDeque<checkpoint_hold::HeldWrite>,
    /// The barrier whose hold something that could not wait already ended.
    hold_ended_for: Option<String>,
    /// The barrier whose cut background work started after. See
    /// `checkpoint_hold`.
    cut_spoiled_for: Option<String>,
    /// The background work the relay last saw, to tell work that starts from
    /// work already running. See `checkpoint_hold`.
    known_background_work: BTreeSet<String>,
    /// Benchmark aid: stage and persist a snapshot on every append, the way
    /// the relay did before transcript appends were amortized, so both
    /// policies can be timed in one process.
    #[cfg(test)]
    stage_snapshot_every_append: bool,
}

impl DurableRelay {
    /// The connection owns this queue hold until its cut or disconnection.
    pub fn reserve_live_baseline(
        &mut self,
    ) -> tokio::sync::oneshot::Receiver<
        std::result::Result<mj_core::relay::LiveSessionBaseline, String>,
    > {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        self.pending_live_baselines
            .push(PendingLiveBaseline { reply: Some(reply) });
        self.service_live_baselines();
        receiver
    }

    #[cfg(any(unix, test))]
    pub(crate) fn resume_after_adoption_wait(&mut self) -> Result<()> {
        self.promote_next_queued_command()?;
        Ok(())
    }

    fn live_baseline_pending(&mut self) -> bool {
        self.service_live_baselines();
        self.pending_live_baselines
            .iter()
            .any(|request| request.reply.is_some())
    }

    fn service_live_baselines(&mut self) {
        self.pending_live_baselines.retain(|request| {
            request
                .reply
                .as_ref()
                .is_some_and(|reply| !reply.is_closed())
        });
        if !self
            .pending_live_baselines
            .iter()
            .any(|request| request.reply.is_some())
            || !self.quiescent_for_adoption()
        {
            return;
        }
        let cut = self
            .live_session_baseline()
            .map_err(|error| format!("{error:#}"));
        for request in &mut self.pending_live_baselines {
            if let Some(reply) = request.reply.take() {
                let _ = reply.send(cut.clone());
            }
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn native_goal_resume_sent(
        &mut self,
        resume_id: u64,
        goal: Option<mj_core::goal::GoalSnapshot>,
    ) {
        let goal = goal.or_else(|| self.snapshot.goal.snapshot.clone());
        self.pending_native_resumes
            .push(PendingNativeResume { resume_id, goal });
    }

    #[cfg(any(unix, test))]
    pub(crate) fn native_goal_resume_rejected(&mut self, resume_id: u64) {
        self.pending_native_resumes
            .retain(|pending| pending.resume_id != resume_id);
        self.service_live_baselines();
    }

    pub(crate) fn quiescent_for_adoption(&self) -> bool {
        self.adoption_blockers().is_empty()
    }

    pub(crate) fn adoption_blockers(&self) -> Vec<String> {
        let mut blockers = Vec::new();
        if self.turn_in_progress() || self.has_held_claude_turn() {
            blockers.push(if self.snapshot.assessment_questions.is_empty() {
                "the worker's current turn to finish".to_owned()
            } else {
                "the worker's current turn, blocked on a pending question or approval, to finish"
                    .to_owned()
            });
        }
        if self.turn_cancellation_pending() {
            blockers.push("the pending turn cancellation to finish".into());
        }
        if self.snapshot.clear_context_in_progress() {
            blockers.push("the worker's context reset (/clear) to finish".into());
        }
        if !self.pending_native_resumes.is_empty()
            || self.snapshot.dispatches.values().any(|dispatch| {
                matches!(
                    dispatch.command,
                    RelayCommand::GoalControl {
                        action: mj_core::goal::GoalControlAction::Resume
                    }
                ) && matches!(
                    dispatch.state,
                    RelayDispatchState::Pending | RelayDispatchState::InFlight
                )
            })
        {
            blockers.push("the accepted native goal resume to finish".into());
        }
        if self.configuration_dispatch_in_progress() {
            blockers.push("the in-flight configuration change to finish".into());
        }
        blockers.extend(self.foreground_tools.snapshot().iter().map(|tool| {
            format!(
                "foreground tool {} to finish",
                tool.title.as_deref().unwrap_or(&tool.tool_call_id)
            )
        }));
        blockers.extend(
            self.snapshot
                .active_user_shells
                .values()
                .filter(|shell| shell.started_at_ms.is_some())
                .map(|shell| format!("running shell {:?} to finish", shell.command)),
        );
        blockers.extend(
            self.active_agent_terminals
                .values()
                .map(|terminal| format!("live harness terminal {:?} to exit", terminal.command)),
        );
        blockers.sort();
        blockers
    }

    #[cfg(test)]
    pub(crate) fn live_baseline_wait_message(&self) -> Option<String> {
        let blockers = self.adoption_blockers();
        (!blockers.is_empty()).then(|| format!("waiting for {}", blockers.join("; ")))
    }

    pub fn live_session_baseline(&self) -> Result<mj_core::relay::LiveSessionBaseline> {
        use mj_core::relay::{LiveQueuedCommand, LiveSessionBaseline};
        let mut state = self.operational_state();
        anyhow::ensure!(
            self.quiescent_for_adoption(),
            "live recovery cut requires a quiescent worker"
        );
        state.execution = RelayExecutionState::Idle;
        state.activity_turn_started_at_ms = None;
        let queue = self
            .snapshot
            .queued_prompts
            .iter()
            .map(|queued| {
                let command = self
                    .snapshot
                    .dispatches
                    .get(&queued.command_id)
                    .with_context(|| {
                        format!("queued command {} has no dispatch", queued.command_id)
                    })?
                    .command
                    .clone();
                Ok(LiveQueuedCommand {
                    command_id: queued.command_id.clone(),
                    command,
                    created_at_ms: queued.created_at_ms,
                    accepted_ordinal: self
                        .snapshot
                        .handled_commands
                        .get(&queued.command_id)
                        .map(|receipt| receipt.accepted_ordinal),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let delivered_initial_prompts = self
            .initial_commands
            .iter()
            .filter_map(|initial| {
                if !matches!(initial.command, RelayCommand::Prompt { .. })
                    || !self
                        .snapshot
                        .initial_command_ids
                        .contains(&initial.command_id)
                    || self
                        .snapshot
                        .queued_prompts
                        .iter()
                        .any(|queued| queued.command_id == initial.command_id)
                {
                    return None;
                }
                let receipt = self.snapshot.handled_commands.get(&initial.command_id);
                let delivered = match receipt {
                    // A legacy pin prevents reseeding, but acceptance alone cannot
                    // distinguish delivery from removal before dispatch.
                    None => false,
                    Some(receipt)
                        if receipt.failure.as_deref() == Some("queued command was removed") =>
                    {
                        false
                    }
                    Some(receipt) => {
                        matches!(receipt.outcome, Some(RelayCommandOutcome::Prompt { .. }))
                    // Receipts do not distinguish failure before/after start. Preserve
                    // the delivered interpretation for every failure except queue removal.
                    || receipt.failure.is_some()
                    }
                };
                delivered.then(|| LiveQueuedCommand {
                    command_id: initial.command_id.clone(),
                    // Startup owns the original brief even after terminal receipt
                    // compaction. Receipts decide delivery, never display content.
                    command: initial.command.clone(),
                    created_at_ms: state
                        .active_prompt
                        .as_ref()
                        .filter(|active| active.command_id == initial.command_id)
                        .map_or(0, |active| active.created_at_ms),
                    accepted_ordinal: receipt.map(|receipt| receipt.accepted_ordinal),
                })
            })
            .collect();
        let live_questions = self.operator_away.live_questions();
        Ok(LiveSessionBaseline {
            delivered_initial_prompts,
            state,
            queue,
            questions: self
                .snapshot
                .pending_elicitations
                .iter()
                .filter_map(|(id, body)| {
                    body.as_ref()
                        .filter(|question| live_questions.get(id) == Some(*question))
                        .cloned()
                })
                .collect(),
            unavailable_questions: self
                .snapshot
                .pending_elicitations
                .iter()
                .filter(|(id, body)| live_questions.contains_key(*id) && body.is_none())
                .count(),
            native_agent_replay: self
                .snapshot
                .native_agent_replay
                .as_ref()
                .map(|agents| agents.values().cloned().collect()),
            native_use_evidence: self.snapshot.native_use_evidence.clone(),
        })
    }

    /// The Unix worker is the only production caller; the helper stays
    /// compiled on Windows so the relay tests still build there.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn open(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
    ) -> Result<Self> {
        Self::open_with_harness(root, session_id, relay_version, false, None, &[])
    }

    /// Open a live relay with its harness delivery mode established before
    /// restart recovery can settle interrupted turns.
    pub fn open_for_harness(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
        harness: mj_core::config::HarnessKind,
        initial_commands: &[mj_core::worker_launch::InitialWorkerCommand],
    ) -> Result<Self> {
        Self::open_with_harness(
            root,
            session_id,
            relay_version,
            false,
            Some(harness),
            initial_commands,
        )
    }

    /// Open existing durable state without promoting work onto a harness.
    pub fn open_for_checkpoint(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
    ) -> Result<Self> {
        Self::open_for_checkpoint_with_initial_commands(root, session_id, relay_version, &[])
    }

    /// Preserve launch receipt owners during checkpoint-only upgrades without seeding work.
    pub fn open_for_checkpoint_with_initial_commands(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
        initial_commands: &[mj_core::worker_launch::InitialWorkerCommand],
    ) -> Result<Self> {
        let root = root.into();
        anyhow::ensure!(
            root.join(RELAY_STATE_FILE).is_file(),
            "checkpoint recovery requires existing relay state"
        );
        Self::open_with_harness(
            root,
            session_id,
            relay_version,
            true,
            None,
            initial_commands,
        )
    }

    pub(crate) fn discard_queued_prompts_before_start(
        root: &Path,
        session_id: &str,
        command_id: &str,
    ) -> Result<()> {
        let mut relay = Self::open_with_harness(
            root.to_path_buf(),
            session_id.to_owned(),
            env!("CARGO_PKG_VERSION"),
            true,
            None,
            &[],
        )?;
        relay
            .submit_command(command_id, RelayCommand::ClearQueuedPrompts)?
            .map_err(|error| anyhow!("discard queued prompts before startup: {}", error.message))?;
        Ok(())
    }

    fn open_with_harness(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
        checkpoint_only: bool,
        harness: Option<mj_core::config::HarnessKind>,
        initial_commands: &[mj_core::worker_launch::InitialWorkerCommand],
    ) -> Result<Self> {
        let root = root.into();
        let session_id = session_id.into();
        validate_identifier(&session_id, "session ID")?;
        mj_core::config::create_owner_only_directory(&root.join(RELAY_JOURNAL_DIR))
            .with_context(|| format!("create relay state directory {}", root.display()))?;

        let state_path = root.join(RELAY_STATE_FILE);
        let mut upgraded_state = false;
        let mut snapshot = if state_path.exists() {
            let bytes =
                fs::read(&state_path).with_context(|| format!("read {}", state_path.display()))?;
            let mut snapshot: RelaySnapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", state_path.display()))?;
            if snapshot.format_version > RELAY_STATE_VERSION {
                bail!(
                    "relay state schema {} is newer than supported schema {RELAY_STATE_VERSION}",
                    snapshot.format_version
                );
            }
            // Upgrade an older snapshot in place. The stored frontier digests
            // stay valid — each is recomputed with the formula matching the
            // record's format. Seed newly owned state before advancing the
            // schema marker; the next persist writes the current version.
            if snapshot.format_version < 23 {
                seed_legacy_open_request(&mut snapshot);
            }
            if snapshot.format_version < 24
                && let Some(turn) = snapshot.harness_turn.as_mut()
            {
                turn.autonomous =
                    snapshot.continuation.autonomous_turn_started == Some(turn.first_ordinal);
            }
            // An older worker recorded a withdrawn continuation only as the
            // quota flag, so a retry stays blocked for a session already in
            // that state at upgrade.
            if snapshot.format_version < 25
                && snapshot.continuation.quota_suppressed
                && snapshot.continuation.withdrawn_ordinal.is_none()
            {
                snapshot.continuation.withdrawn_ordinal = Some(snapshot.latest_ordinal);
            }
            if snapshot.format_version < 27 {
                seed_legacy_notification_stack(&mut snapshot);
            }
            if snapshot.format_version < 28 {
                // Old Moves had no explicit pins. Keep their acceptance receipts
                // until the daemon has reconciled its durable operation owner.
                snapshot.legacy_command_retention = true;
            }
            if snapshot.format_version < 32 {
                mj_core::relay::seed_legacy_delegated_parent_instructions(&mut snapshot);
            }
            seed_missing_notification_time(&mut snapshot);
            upgraded_state = snapshot.format_version < RELAY_STATE_VERSION;
            snapshot.format_version = RELAY_STATE_VERSION;
            // Older snapshots can still identify a live turn, even though
            // they did not retain its start through subsequent background work.
            snapshot.activity_turn_started_at_ms = snapshot
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or_else(|| {
                    snapshot
                        .harness_turn
                        .as_ref()
                        .map(|turn| turn.started_at_ms)
                })
                .or(snapshot.activity_turn_started_at_ms);
            if snapshot.session_id != session_id {
                bail!(
                    "relay state belongs to session {}, not {session_id}",
                    snapshot.session_id
                );
            }
            snapshot
        } else {
            let mut snapshot = RelaySnapshot::new(session_id);
            if let Some(restored) = read_restored_relay_seed(&root)? {
                snapshot.assessment_context = None;
                if let Some(value) = &restored.assessment_state {
                    let state = mj_core::assessment::Checkpoint::decode(value)?;
                    snapshot.assessment_context = state.context.map(Arc::new);
                    snapshot.assessment = state.assessment.map(std::sync::Arc::new);
                    snapshot.continuation = state.continuation;
                    snapshot.capacity_retry = state.capacity_retry;
                    snapshot.turn_completion = state.turn_completion;
                    if let Some(open) = state.open_request {
                        snapshot.request = open.request;
                        snapshot.person_input_through = open.person_input_through;
                    } else {
                        seed_legacy_open_request(&mut snapshot);
                    }
                }
                snapshot.latest_ordinal = restored.event_frontier;
                if let Some(stack) = restored.notification_stack {
                    snapshot.notification_stack = Arc::new(stack);
                    seed_missing_notification_time(&mut snapshot);
                } else {
                    seed_legacy_notification_stack(&mut snapshot);
                }
                snapshot.latest_digest = restored.event_frontier_digest.clone();
                snapshot.acknowledged_through = restored.event_frontier;
                snapshot.acknowledged_digest = restored.event_frontier_digest.clone();
                snapshot.recovery_floor_ordinal = restored.event_frontier;
                snapshot.recovery_floor_digest = restored.event_frontier_digest;
                // The accepted model and effort the worker pins on its first
                // bridge start, exactly as it does after a restart.
                snapshot.config.extend(restored.accepted_config);
                snapshot.restored_native_session_unused = restored.native_session_unused;
                if let Some(evidence) = restored.native_use_evidence {
                    snapshot.native_session_used = evidence.usage == NativeSessionUse::Used;
                    snapshot.native_use_evidence = Some(evidence);
                } else {
                    snapshot.record_native_use(if restored.native_session_unused {
                        NativeSessionUse::Unused
                    } else {
                        NativeSessionUse::Unknown
                    });
                }
                for queued in restored.queued_prompts {
                    anyhow::ensure!(
                        mj_core::continuation::valid_command_id(&queued.command_id),
                        "invalid restored queued command ID"
                    );
                    if queued.content.is_empty() {
                        bail!("restored queued command {} is empty", queued.command_id);
                    }
                    if snapshot.handled_commands.contains_key(&queued.command_id) {
                        bail!(
                            "restored canonical session contains duplicate queued command {}",
                            queued.command_id
                        );
                    }
                    let command = match queued.kind {
                        CanonicalQueuedCommandKind::Prompt => RelayCommand::Prompt {
                            prompt: queued
                                .content
                                .into_iter()
                                .map(serde_json::from_value)
                                .collect::<serde_json::Result<Vec<ContentBlock>>>()
                                .with_context(|| {
                                    format!(
                                        "decode ACP content for restored queued command {}",
                                        queued.command_id
                                    )
                                })?,
                        },
                        CanonicalQueuedCommandKind::SetConfig { key, value } => {
                            RelayCommand::SetConfig { key, value }
                        }
                    };
                    snapshot.handled_commands.insert(
                        queued.command_id.clone(),
                        HandledRelayCommand {
                            terminal_identity: None,
                            accepted_ordinal: restored.event_frontier,
                            terminal_ordinal: None,
                            outcome: None,
                            failure: None,
                        },
                    );
                    snapshot.dispatches.insert(
                        queued.command_id.clone(),
                        RelayDispatchRecord {
                            command,
                            state: RelayDispatchState::Queued,
                        },
                    );
                    snapshot.queued_prompts.push(QueuedRelayPrompt {
                        command_id: queued.command_id,
                        created_at_ms: queued.queued_at_ms,
                    });
                }
                // The archived worker's mail, as the restore policy selected
                // it. A claimed context may name a prompt the controller
                // admits later under the same command ID; dispatch finds it
                // there before that prompt starts.
                if let Some(mailbox) = restored.mailbox {
                    snapshot.pending_mailbox_events = mailbox.pending_events;
                    snapshot.delivered_mailbox_event_keys = mailbox.delivered_event_keys;
                    snapshot.mailbox_prompt_contexts = mailbox.prompt_contexts;
                }
            }
            snapshot
        };

        // Startup pins live until destroy. Upgrade from the lifetime prompt
        // watermark, which /clear never resets: startup's final brief is the
        // first accepted prompt, so that watermark proves the whole prefix.
        // Pre-watermark states use their legacy native-use proof instead.
        // Without it, retain only accepted receipt IDs and complete a crashed
        // selector prefix normally. Missing selectors are idempotent settings.
        // This also finishes migration first opened without launch arguments.
        let startup_owner_count = snapshot.initial_command_ids.len();
        for initial in initial_commands {
            if snapshot.latest_prompt_accepted_ordinal.is_some()
                || snapshot.native_session_used
                || snapshot.handled_commands.contains_key(&initial.command_id)
            {
                snapshot
                    .initial_command_ids
                    .insert(initial.command_id.clone());
            }
        }
        let registered_startup_owners = snapshot.initial_command_ids.len() != startup_owner_count;
        let compacted_terminal_commands = snapshot.compact_terminal_commands()?;
        let assigned_store_id = snapshot.store_id.is_none();
        if assigned_store_id {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random)
                .map_err(|error| anyhow::anyhow!("generate worker store identity: {error}"))?;
            snapshot.store_id = Some(format!("{:032x}", u128::from_le_bytes(random)));
        }
        validate_relay_snapshot_frontiers(&snapshot)?;
        // Upgrade native-use evidence before discarding the old retained prefix.
        let upgrading_native =
            snapshot.native_session_id.is_some() && snapshot.native_use_evidence.is_none();
        let upgrading_questions = snapshot
            .assessment_questions
            .iter()
            .any(|id| !snapshot.pending_elicitations.contains_key(id));
        let upgrading_history = upgrading_native || upgrading_questions;
        let retained_through = if upgrading_history {
            snapshot
                .acknowledged_through
                .min(snapshot.recovery_floor_ordinal)
        } else {
            snapshot.retained_through()
        };
        let retained_digest = if upgrading_history
            && snapshot.recovery_floor_ordinal < snapshot.acknowledged_through
        {
            snapshot.recovery_floor_digest.clone()
        } else {
            snapshot.retained_digest().to_owned()
        };
        let snapshot_ordinal = snapshot.latest_ordinal;
        let missing_native_history = upgrading_native;
        let (journal_spans, hot_events) = open_relay_journal(
            &root.join(RELAY_JOURNAL_DIR),
            retained_through,
            &retained_digest,
            snapshot_ordinal,
            &mut snapshot,
        )?;
        // Shipped retry records were command-bound. Read them forward once;
        // all new transitions use the turn-owned assessment state machine.
        let migrated_assessment =
            snapshot.assessment.is_none() && snapshot.retry_assessment.is_some();
        if migrated_assessment && let Some(old) = snapshot.retry_assessment.take() {
            let completion = old.evidence.completion.clone().unwrap_or_else(|| {
                mj_core::activity::verdict::CompletionEvidence::bounded(
                    "legacy_completed_turn",
                    None,
                )
            });
            let mut a = mj_core::assessment::TurnAssessment::pending(
                old.command_id,
                old.ordinal,
                epoch_millis(),
                completion,
            );
            a.evidence = Some(old.evidence);
            snapshot.assessment = Some(std::sync::Arc::new(a));
        }
        ensure_serialized_budget(
            &snapshot.operational_state(),
            RELAY_STATE_BYTE_BUDGET,
            "relay operational state",
        )?;

        let mut relay = Self {
            in_place_harness_restart_in_flight: None,
            operator_away: Default::default(),
            root,
            relay_version: relay_version.into(),
            worker_build: Some(mj_core::worker_build::worker_build_id().to_owned()),
            harness_ready_ms: Arc::new(AtomicI64::new(0)),
            session_setup: false,
            checkpoint_only,
            reviewer_admissions: BTreeMap::new(),
            next_reviewer_admission: 0,
            steering_supported: None,
            initial_commands: if checkpoint_only {
                Vec::new()
            } else {
                initial_commands.to_vec()
            },
            automatic_steering: false,
            steering_delivery_mode: harness
                .map(|harness| SteeringDeliveryMode::for_harness(harness, false))
                .unwrap_or_default(),
            mailbox_delivery_mode: None,
            snapshot,
            journal_spans,
            hot_events,
            replay_cursors: VecDeque::new(),
            unpersisted_journal_bytes: 0,
            journal_generation: 0,
            journal_cleanup_pending: true,
            journal_append_failure: None,
            #[cfg(test)]
            journal_append_fault: None,
            pending_live_baselines: Vec::new(),
            pending_native_resumes: Vec::new(),
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            harness_turns: HarnessTurnPolicy::default(),
            background_work: BackgroundWorkPolicy::default(),
            foreground_tools: mj_core::activity::ToolsInFlight::default(),
            turn_context: Default::default(),
            verdict_harness: None,
            jev_disabled: mj_core::jev::disabled_by_environment(),
            authentication_login_path: None,
            login_identity: Default::default(),
            replied_verdict: Default::default(),
            recovers_child_quota: false,
            air_tasks: BTreeMap::new(),
            pending_settle_facts: None,
            claude_pending_stops: BTreeMap::new(),
            antigravity_work: background::AntigravityWorkState::default(),
            kimi_background_tasks: BTreeMap::new(),
            kimi_provisional_tasks: BTreeMap::new(),
            kimi_observed_task_ids: BTreeSet::new(),
            kimi_observed_tool_ids: BTreeSet::new(),
            background_work_known: None,
            handback_verdict_client: None,
            task_settled_at_ms: None,
            active_agent_terminals: BTreeMap::new(),
            closed_agent_terminals: BTreeSet::new(),
            agent_terminal_tool_calls: BTreeMap::new(),
            held_writes: VecDeque::new(),
            hold_ended_for: None,
            cut_spoiled_for: None,
            known_background_work: BTreeSet::new(),
            #[cfg(test)]
            stage_snapshot_every_append: false,
        };
        let recovered_native_history =
            missing_native_history && relay.recover_native_history_evidence();
        let recovered_questions = upgrading_questions && relay.recover_pending_elicitations();
        // Startup already reads the active journal into the bounded hot window.
        // Reuse it: old sealed history must not make worker startup slow or fail.
        if !checkpoint_only {
            let prompt_text = |command: &RelayCommand| {
                command.prompt_blocks().map(|prompt| {
                    prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            };
            // The queue observation may be collected before delivery. The
            // durable dispatch still owns the active prompt and its boundary.
            let mut prompts: BTreeMap<String, Option<String>> = relay
                .snapshot
                .dispatches
                .iter()
                .filter_map(|(id, dispatch)| {
                    prompt_text(&dispatch.command).map(|text| (id.clone(), Some(text)))
                })
                .collect();
            // The relay owner persists autonomous identity in the open turn
            // and its settled record; native starts inside prompts are not
            // assistant-message boundaries.
            let mut autonomous_starts = BTreeSet::new();
            if let Some(turn) = relay.snapshot.autonomous_harness_turn() {
                autonomous_starts.insert(turn.first_ordinal);
            }
            // Terminal records retain command kinds after dispatch bodies are
            // gone. Their starts still establish boundaries; an unavailable
            // body stays distinct from an empty prompt.
            for event in &relay.hot_events {
                if let RelayObservation::HarnessTurnSettled {
                    autonomous: Some(turn),
                    ..
                } = &event.observation
                {
                    autonomous_starts.insert(turn.start_ordinal);
                }
                let command_id = match &event.observation {
                    RelayObservation::CommandCompleted {
                        command_id,
                        command: Some(RelayCommandKind::Prompt),
                        ..
                    }
                    | RelayObservation::CommandCompleted {
                        command_id,
                        outcome: RelayCommandOutcome::Prompt { .. },
                        ..
                    }
                    | RelayObservation::CommandRejected {
                        command_id,
                        command: RelayCommandKind::Prompt,
                        ..
                    }
                    | RelayObservation::CommandInterrupted {
                        command_id,
                        command: RelayCommandKind::Prompt,
                        ..
                    } => Some(command_id),
                    _ => None,
                };
                if let Some(command_id) = command_id {
                    prompts.entry(command_id.clone()).or_default();
                }
                if let RelayObservation::CommandInterrupted {
                    continuation_id: Some(continuation_id),
                    ..
                } = &event.observation
                {
                    prompts.entry(continuation_id.clone()).or_default();
                }
            }
            let earlier_history_omitted = relay
                .hot_events
                .front()
                .is_none_or(|event| event.ordinal > 1)
                && relay.snapshot.latest_ordinal > 0;
            if earlier_history_omitted {
                relay.turn_context.mark_earlier_history_omitted();
            }
            for event in &relay.hot_events {
                if autonomous_starts.contains(&event.ordinal) {
                    relay.turn_context.observe_autonomous_turn_start();
                }
                if let RelayObservation::CommandQueued {
                    command_id,
                    command,
                    ..
                } = &event.observation
                    && let Some(text) = prompt_text(command)
                {
                    prompts.insert(command_id.clone(), Some(text));
                }
                let prompt = if let Some(command_id) =
                    mj_transcript::turn_context::delivered_prompt_command_id(&event.observation)
                {
                    prompts.remove(command_id)
                } else {
                    if let RelayObservation::CommandCompleted { command_id, .. } =
                        &event.observation
                    {
                        prompts.remove(command_id);
                    }
                    None
                };
                if let Some(prompt) = prompt {
                    relay.turn_context.observe_prompt_delivery(
                        mj_transcript::turn_context::delivered_prompt_command_id(
                            &event.observation,
                        ),
                        prompt.as_deref(),
                    );
                } else {
                    relay.turn_context.observe_relay(&event.observation, None);
                }
            }
            // The latest persisted human instructions govern every turn,
            // including mailbox wakes and restart continuations. Use what
            // remains in that history, without matching an executing request ID.
            if let Some(history) = relay.snapshot.authorization_context() {
                relay.turn_context.restore_user_prompt_tail(&history);
            }
        }
        relay
            .turn_context
            .set_authorization_context(relay.snapshot.authorization_context());
        relay
            .turn_context
            .set_trusted_user_inputs(relay.snapshot.trusted_user_inputs());
        relay
            .turn_context
            .set_delegated_parent_instructions(&relay.snapshot);
        relay
            .turn_context
            .set_prior_guardian_reviews(relay.snapshot.prior_guardian_reviews());
        // Live-only work cannot be reconstructed on reopen. Nor can replay
        // prove an idle transition that was not yet saved with the snapshot.
        let replayed = relay.snapshot.latest_ordinal > snapshot_ordinal;
        let idle = relay.activity_is_idle();
        if relay.snapshot.activity_was_idle != Some(idle) {
            relay.snapshot.idle_since_ms = None;
            relay.snapshot.activity_was_idle = Some(idle);
        }
        if idle {
            relay.snapshot.activity_turn_started_at_ms = None;
        }
        if !state_path.exists()
            || upgraded_state
            || replayed
            || assigned_store_id
            || recovered_native_history
            || recovered_questions
            || migrated_assessment
            || compacted_terminal_commands
            || registered_startup_owners
        {
            relay.persist_snapshot().with_context(|| {
                if recovered_questions {
                    format!(
                        "save upgraded worker pending questions {:?}",
                        relay
                            .snapshot
                            .pending_elicitations
                            .keys()
                            .collect::<Vec<_>>()
                    )
                } else {
                    "save recovered worker snapshot".to_owned()
                }
            })?;
        }
        // Complete an ACK's interrupted file collection before any startup
        // append can try sealing an acknowledged active file with no live span.
        // Evidence upgrades above are durable before their old files disappear.
        relay.garbage_collect_relay_history()?;
        if !checkpoint_only {
            relay.return_mailbox_hook_lease(
                mj_core::relay::MailboxHookLeaseReturnReason::WorkerRestart,
            )?;
        }
        relay.adopt_unqueued_queue_commands()?;
        relay.ensure_initial_commands_succeeded()?;
        relay.seed_initial_commands()?;
        relay.recover_nonterminal_commands()?;
        relay.ensure_initial_commands_succeeded()?;
        relay.promote_next_queued_command()?;
        if !checkpoint_only {
            // No locator task survives a worker process. Record the final
            // no-result observation for any request interrupted by the restart.
            let pending: Vec<_> = relay
                .snapshot
                .notification_stack
                .entries
                .iter()
                .filter(|entry| entry.message == mj_core::attention::EntryMessage::Locating)
                .map(|entry| entry.completed_ordinal)
                .collect();
            for completed in pending {
                relay.finish_question_locator(completed, None)?;
            }
        }
        Ok(relay)
    }

    /// Complete the creation prefix before publishing any control endpoint.
    /// The final prompt's receipt proves the entire prefix was journaled. Once
    /// a turn has run, receipt compaction must never cause initialization again.
    fn seed_initial_commands(&mut self) -> Result<()> {
        let Some(last) = self.initial_commands.last() else {
            return Ok(());
        };
        if self.snapshot.initial_command_ids.contains(&last.command_id) {
            return Ok(());
        }
        anyhow::ensure!(
            matches!(last.command, RelayCommand::Prompt { .. }) && !last.optional,
            "initial input must end with the original prompt"
        );
        let commands = self.initial_commands.clone();
        for (position, initial) in commands.iter().enumerate() {
            anyhow::ensure!(
                position + 1 == commands.len()
                    || matches!(initial.command, RelayCommand::SetConfig { .. }),
                "initial input before the brief must be configuration"
            );
            if self
                .snapshot
                .initial_command_ids
                .contains(&initial.command_id)
            {
                continue;
            }
            self.submit_command(&initial.command_id, initial.command.clone())?
                .map_err(|error| {
                    anyhow!(
                        "initialize worker command {}: {}",
                        initial.command_id,
                        error.message
                    )
                })?;
            self.snapshot
                .initial_command_ids
                .insert(initial.command_id.clone());
        }
        // No endpoint is published during seeding. A crash before this write
        // recovers the accepted IDs from their ordinary journal receipts.
        self.persist_snapshot()?;
        Ok(())
    }

    /// A failed required creation command cannot release the original brief
    /// or any follow-up. Its journaled failure also stops a restarted worker.
    fn ensure_initial_commands_succeeded(&self) -> Result<()> {
        for initial in self.initial_commands.iter().filter(|initial| {
            !initial.optional && matches!(initial.command, RelayCommand::SetConfig { .. })
        }) {
            if let Some(failure) = self
                .snapshot
                .handled_commands
                .get(&initial.command_id)
                .and_then(|receipt| receipt.failure.as_deref())
            {
                return Err(anyhow::Error::new(
                    mj_core::relay::WorkerHarnessStartupFailure {
                        message: format!(
                            "initial worker command {} ({:?}) failed: {failure}",
                            initial.command_id,
                            initial.command.kind()
                        ),
                        kind: None,
                    },
                ));
            }
        }
        Ok(())
    }

    /// Adopt queueable commands that an earlier relay format accepted outside
    /// the durable queue. Configuration changes used to dispatch through the
    /// control path, so a command accepted just before an upgrade would
    /// otherwise stay accepted forever without ever being promoted.
    fn adopt_unqueued_queue_commands(&mut self) -> Result<()> {
        let mut adopted: Vec<(u64, QueuedRelayPrompt)> = Vec::new();
        for (command_id, dispatch) in &self.snapshot.dispatches {
            if dispatch.state != RelayDispatchState::Queued
                || !dispatch.command.is_queue_entry()
                || self
                    .snapshot
                    .queued_prompts
                    .iter()
                    .any(|queued| queued.command_id == *command_id)
            {
                continue;
            }
            let Some(handled) = self.snapshot.handled_commands.get(command_id) else {
                continue;
            };
            adopted.push((
                handled.accepted_ordinal,
                QueuedRelayPrompt {
                    command_id: command_id.clone(),
                    created_at_ms: epoch_millis(),
                },
            ));
        }
        if adopted.is_empty() {
            return Ok(());
        }
        let existing = std::mem::take(&mut self.snapshot.queued_prompts);
        let mut ordered: Vec<(u64, QueuedRelayPrompt)> = existing
            .into_iter()
            .map(|queued| {
                let accepted = self
                    .snapshot
                    .handled_commands
                    .get(&queued.command_id)
                    .map_or(0, |handled| handled.accepted_ordinal);
                (accepted, queued)
            })
            .collect();
        ordered.extend(adopted);
        ordered.sort_by_key(|(accepted, _)| *accepted);
        self.snapshot.queued_prompts = ordered.into_iter().map(|(_, queued)| queued).collect();
        self.persist_snapshot()
    }

    /// Called while the worker owner is held through durable tool admission.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn originating_command_id(&self) -> Option<String> {
        self.snapshot
            .active_prompt
            .as_ref()
            .map(|prompt| prompt.command_id.clone())
            .or_else(|| {
                self.snapshot
                    .autonomous_harness_turn()
                    .as_ref()
                    .map(|turn| mj_core::continuation::harness_turn_id(turn.first_ordinal))
            })
    }

    pub fn operational_state(&self) -> RelayOperationalState {
        let mut state = self.snapshot.operational_state();
        state.jev_rate_limit = mj_core::jev::current_rate_limit();
        state.acp_ready = Some(self.is_acp_ready());
        state.checkpoint_only = self.checkpoint_only;
        state.steering_supported = self.effective_steering_supported();
        state.mailbox_delivery = self.mailbox_delivery_mode;
        state.turn_controls = Some(self.turn_controls());
        state.last_acp_activity_at_ms = self.acp_activity.last_at_ms();
        state.current_step_started_at_ms = self.step_clock.started_at_ms();
        state.active_agent_terminals = self.active_agent_terminals.values().cloned().collect();
        state.native_agent_count = self.native_agent_count();
        state.clear_context = self.supports_clear_context();
        state.background_commands = self.background_commands();
        state.background_work_known = self.background_work_known;
        // Answer the activity question once, here, where every fact is in
        // hand. Consumers read this instead of each deriving their own.
        let facts = self.activity_facts();
        state.expected_continuation = facts.expected_continuation;
        state.inferred_idle_since_ms = facts.inferred_idle_since_ms;
        state.task_settled_at_ms = facts.task_settled_at_ms;
        state.background_needed = facts.background_needed;
        state.activity = Some(mj_core::activity::classify(&facts));
        state.foreground_tool_started_at_ms =
            facts.tools_in_flight.last().map(|tool| tool.started_at_ms);
        state.tools_in_flight = facts.tools_in_flight;
        // The worker alone decides whether it still owes a retry of a failed
        // turn (the durable intent, apart from its submit gate); the daemon
        // only reads the answer. `authentication_retry_waiting` is retired.
        state.held_login_fingerprint =
            if self.verdict_harness == Some(mj_core::config::HarnessKind::Claude) {
                self.login_identity.snapshot().1
            } else {
                self.login_identity.file_fingerprint()
            };
        state.authentication_retry_intent = self
            .authentication_retry_intent()
            .map(|(_, rejected)| rejected.to_owned());
        state
    }

    /// Whether the ACP process is configured: derived from the readiness
    /// instant, never stored beside it.
    pub(crate) fn is_acp_ready(&self) -> bool {
        self.harness_ready_ms.load(Ordering::Acquire) != 0
    }

    pub(crate) fn set_acp_ready(&mut self, ready: bool) {
        // A second configuration of an open session is not a new harness: the
        // instant stays that of the first, or work started in between would
        // read as part of the harness.
        if ready && !self.is_acp_ready() {
            self.harness_ready_ms
                .store(epoch_millis().max(1), Ordering::Release);
        } else if !ready {
            self.harness_ready_ms.store(0, Ordering::Release);
        }
    }

    /// The instant the harness session was last configured, shared with
    /// connections that observe work.
    pub fn harness_ready_clock(&self) -> Arc<AtomicI64> {
        self.harness_ready_ms.clone()
    }

    /// A stopped bridge cannot become ready again until its replacement configures.
    pub fn clear_acp_readiness(&mut self) {
        self.set_acp_ready(false);
        self.steering_supported = None;
        self.automatic_steering = false;
    }

    /// Publish the current harness's steering support without changing the journal.
    pub fn set_steering_supported(&mut self, supported: Option<bool>) {
        self.steering_supported = supported;
    }

    /// Effective steering capability combining ACP support with hook-based delivery.
    pub fn effective_steering_supported(&self) -> Option<bool> {
        match self.steering_delivery_mode {
            SteeringDeliveryMode::HookPostToolBatch => self.steering_supported.map(|_| true),
            SteeringDeliveryMode::HookTurnEnd => self.steering_supported.map(|_| false),
            SteeringDeliveryMode::Acp | SteeringDeliveryMode::AcpWithMailbox => {
                self.steering_supported
            }
        }
    }

    fn supports_clear_context(&self) -> bool {
        matches!(
            self.verdict_harness,
            Some(
                mj_core::config::HarnessKind::Codex
                    | mj_core::config::HarnessKind::Claude
                    | mj_core::config::HarnessKind::Kimi
                    | mj_core::config::HarnessKind::Antigravity
            )
        )
    }

    /// Allow the relay to steer queued prompts into the running turn itself.
    pub fn set_automatic_steering(&mut self, enabled: bool) {
        self.automatic_steering = enabled;
    }

    /// Select steering delivery for the configured harness and staged profile.
    /// The safe turn-end mode is selected before worker admission opens;
    /// preparation upgrades it only after verifying a usable hook marker.
    pub fn set_steering_delivery_mode(
        &mut self,
        harness: mj_core::config::HarnessKind,
        boundary_hook: bool,
    ) {
        self.steering_delivery_mode = SteeringDeliveryMode::for_harness(harness, boundary_hook);
    }

    /// Set when a Codex session's mailbox messages may reach it, from the
    /// provider in its staged configuration. The `PostToolUse` drain and the
    /// published operational state both read this one field.
    pub fn set_mailbox_delivery_mode(&mut self, mode: mj_core::mailbox::MailboxDeliveryMode) {
        self.mailbox_delivery_mode = Some(mode);
    }

    #[cfg(unix)]
    pub(crate) fn update_mailbox_delivery_mode(
        &mut self,
        mode: Option<mj_core::mailbox::MailboxDeliveryMode>,
    ) {
        self.mailbox_delivery_mode = mode;
    }

    #[cfg(unix)]
    pub(crate) fn in_place_move_hold_active(&self) -> bool {
        self.snapshot.in_place_move_hold.is_some()
    }

    /// The relay-owned turn boundary shared by command promotion, checkpoint
    /// admission, and background-work accounting. Activity classification
    /// also presents an in-progress `/clear` as a turn, but that command must
    /// be allowed to promote itself when the session is otherwise idle.
    pub(super) fn turn_in_progress(&self) -> bool {
        self.snapshot.active_prompt.is_some() || self.snapshot.harness_turn.is_some()
    }

    /// Every fact that bears on whether this session is working.
    ///
    /// The idle clock runs on every journal append, so this assembles the
    /// facts directly rather than through a whole operational state, which
    /// would clone the session's configuration once per streamed chunk. It
    /// must stay identical to `RelayOperationalState::facts`, which is what
    /// the daemon reads; `worker_facts_match_the_published_state` pins them
    /// together.
    pub fn activity_facts(&self) -> mj_core::activity::ActivityFacts {
        let background = self.background_commands();
        let background_commands = background.len() + self.native_agent_count();
        self.turn_context
            .set_counts(background_commands, self.snapshot.queued_prompts.len());
        let mut facts = mj_core::activity::ActivityFacts {
            execution: self.snapshot.execution,
            prompt_started_at_ms: self
                .snapshot
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or_else(|| self.snapshot.clear_context_started_at_ms()),
            harness_turn_started_at_ms: self.snapshot.harness_turn.map(|turn| turn.started_at_ms),
            turn_started_at_ms: self.snapshot.activity_turn_started_at_ms,
            expected_continuation: None,
            inferred_idle_since_ms: None,
            queued_commands: self.snapshot.queued_prompts.len(),
            tools_in_flight: self.foreground_tools.snapshot(),
            background_started_at_ms: background
                .iter()
                .map(|command| command.started_at_ms)
                .chain(
                    self.snapshot
                        .active_user_shells
                        .values()
                        .filter_map(|shell| shell.started_at_ms),
                )
                .min(),
            background_commands,
            antigravity_task_started_at_ms: BackgroundCommand::oldest_antigravity_task_start(
                &background,
            ),
            active_user_shells: self.snapshot.active_user_shells.len(),
            active_agent_terminals: self.active_agent_terminals.len(),
            goal_active: self.snapshot.goal.active(),
            goal_running: self.snapshot.goal.running(),
            goal_pending_resume: self.snapshot.goal.pending_resume.is_some(),
            goal_decision: self.snapshot.goal.decision.is_some(),
            goal_synchronized: self.snapshot.goal.synchronized(),
            background_work_known: self.background_work_known,
            acp_ready: Some(self.is_acp_ready()),
            checkpoint_only: self.checkpoint_only,
            checkpoint_barrier: self.snapshot.checkpoint_barrier.is_some(),
            capacity_retry_armed: self
                .snapshot
                .capacity_retry
                .as_ref()
                .is_some_and(|retry| !retry.submitted),
            last_acp_activity_at_ms: self.acp_activity.last_at_ms(),
            current_step_started_at_ms: self.step_clock.started_at_ms(),
            idle_since_ms: self.snapshot.idle_since_ms,
            // A settle only counts until the turn it promised has started:
            // no turn open now, and none started since the settle.
            task_settled_at_ms: self.task_settled_at_ms.filter(|settled| {
                self.snapshot.active_prompt.is_none()
                    && self.snapshot.harness_turn.is_none()
                    && self
                        .snapshot
                        .activity_turn_started_at_ms
                        .is_none_or(|started| *settled >= started)
            }),
            background_needed: None,
        };
        self.turn_context.set_background_inventory(
            background,
            self.snapshot
                .native_agents
                .iter()
                .filter(|(_, agent)| {
                    agent.state == mj_core::native_agent::NativeAgentState::Running
                })
                .map(|(id, _)| id.clone())
                .collect(),
        );
        (
            facts.expected_continuation,
            facts.inferred_idle_since_ms,
            facts.background_needed,
        ) = self
            .replied_verdict
            .inference(self.turn_context.generation(), &facts);
        facts
    }

    /// Whether the session's idle clock should be running.
    ///
    /// The same classification every other part of Mjolnir uses. This was a
    /// private duplicate of it, and it disagreed: it counted neither queued
    /// commands nor open terminals, so the worker could publish "idle since"
    /// about a session the controller called busy.
    fn activity_is_idle(&self) -> bool {
        mj_core::activity::classify(&self.activity_facts()).is_idle()
    }

    /// Update only at activity mutations, never when a viewer reads status.
    fn refresh_idle_clock(&mut self, now_ms: i64) -> bool {
        let idle = self.activity_is_idle();
        let previous = self.snapshot.activity_was_idle;
        if previous == Some(idle) {
            return false;
        }
        self.snapshot.idle_since_ms = (idle && previous == Some(false)).then_some(now_ms);
        if idle {
            self.snapshot.activity_turn_started_at_ms = None;
        }
        self.snapshot.activity_was_idle = Some(idle);
        true
    }

    pub(crate) fn persist_activity_transition(&mut self) -> Result<()> {
        // The one place background work gains members: every change to it
        // ends here, journaled or not.
        self.note_background_work()?;
        if self.refresh_idle_clock(epoch_millis()) {
            self.persist_snapshot()
                .context("persist session idle transition")?;
        }
        self.service_live_baselines();
        Ok(())
    }

    /// Set the worker-input compatibility identity served in hello.
    pub fn set_worker_build(&mut self, build: Option<String>) {
        self.worker_build = build;
    }

    /// Choose whether agent output with no prompt in flight opens a turn.
    pub fn set_harness_turn_policy(&mut self, policy: HarnessTurnPolicy) {
        self.harness_turns = policy;
    }

    #[cfg(unix)]
    pub(crate) fn uses_claude_harness_turns(&self) -> bool {
        self.harness_turns == HarnessTurnPolicy::ClaudeAdapter
    }

    /// Launch metadata owns this fact, including older environment-only launches.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn set_managed_subagent(&mut self, managed: bool) -> Result<()> {
        if self.snapshot.managed_subagent != managed {
            self.record_observation(RelayObservation::ManagedSubagentConfigured { managed })?;
        }
        self.settle_local_quota()?;
        self.suppress_child_assessment()
    }

    /// The latest finished turn's work processes while some may still be
    /// alive, with the identities the worker's watcher recounts.
    pub(crate) fn watched_work(
        &self,
    ) -> Option<(
        mj_core::work_facts::WorkLiveness,
        Vec<mj_core::work_facts::ProcessIdentity>,
    )> {
        use mj_core::work_facts::Observed;
        let liveness = self
            .snapshot
            .work_liveness
            .as_ref()
            .filter(|l| l.alive > 0)?;
        let completion = self
            .snapshot
            .turn_completion
            .as_ref()
            .filter(|turn| turn.completed_ordinal == liveness.completed_ordinal)?;
        // A prompted turn's facts are in its handled command; an autonomous
        // harness turn's are on its settlement.
        let facts = match self
            .snapshot
            .handled_commands
            .get(&completion.command_id)
            .and_then(|handled| handled.outcome.as_ref())
        {
            Some(RelayCommandOutcome::Prompt { work_facts, .. }) => work_facts.as_deref()?,
            _ => {
                &self
                    .snapshot
                    .harness_turn_work
                    .as_ref()
                    .filter(|work| work.completed_ordinal == liveness.completed_ordinal)?
                    .facts
            }
        };
        match &facts.processes {
            Observed::Known(processes) => Some((liveness.clone(), processes.identities.clone())),
            Observed::Unavailable { .. } => None,
        }
    }

    /// Publish a recount, unless the turn it counts is no longer the current
    /// one or the count has not changed. Returns whether it was recorded.
    pub(crate) fn record_work_liveness(
        &mut self,
        liveness: mj_core::work_facts::WorkLiveness,
    ) -> Result<bool> {
        let changed = self.snapshot.work_liveness.as_ref().is_some_and(|current| {
            current.completed_ordinal == liveness.completed_ordinal
                && current.alive != liveness.alive
        });
        if changed {
            self.record_observation(RelayObservation::WorkLivenessChanged { liveness })?;
        }
        Ok(changed)
    }

    pub fn set_turn_verdict_harness(&mut self, harness: mj_core::config::HarnessKind) {
        self.verdict_harness = Some(harness);
        if self.turn_context.decision_log().is_none() {
            match mj_core::jev::DecisionLog::open(self.root.join("jev-decisions")) {
                Ok(log) => self.turn_context.set_decision_log(log),
                Err(error) => tracing::warn!(%error, "Jev diagnostic log unavailable"),
            }
        }
        self.turn_context.set_session_id(&self.snapshot.session_id);
        self.publish_assessment_diagnostic();
    }

    pub fn turn_context(&self) -> mj_transcript::turn_context::TurnContext {
        self.turn_context.clone()
    }

    pub fn acp_activity_clock(&self) -> AcpActivityClock {
        self.acp_activity.clone()
    }

    pub fn step_clock(&self) -> crate::acp::StepClock {
        self.step_clock.clone()
    }

    /// The tool calls the agent has open. Shared, not copied: the ACP driver's
    /// stall watchdog reads exactly what this relay records.
    pub fn tools_in_flight(&self) -> mj_core::activity::ToolsInFlight {
        self.foreground_tools.clone()
    }

    /// The directory holding this relay's durable state.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The journal ordinal of the newest work this worker accepted: a prompt, or
    /// a turn the harness started on its own. Zero before any. The worker's own
    /// answer to whether work arrived after a finish.
    pub fn newest_accepted_work_ordinal(&self) -> u64 {
        self.snapshot
            .latest_prompt_accepted_ordinal
            .into_iter()
            .chain(self.snapshot.last_harness_turn_started_ordinal)
            .max()
            .unwrap_or(0)
    }

    pub fn latest_ordinal(&self) -> u64 {
        self.snapshot.latest_ordinal
    }

    pub fn latest_digest(&self) -> &str {
        &self.snapshot.latest_digest
    }

    pub fn acknowledged_through(&self) -> u64 {
        self.snapshot.acknowledged_through
    }

    pub fn acknowledged_digest(&self) -> &str {
        &self.snapshot.acknowledged_digest
    }

    pub fn events_after(&self, after_ordinal: u64, after_digest: &str) -> Result<Vec<RelayEvent>> {
        let plan = self.replay_plan();
        plan.validate_cursor(after_ordinal, after_digest)?;
        plan.read_events_after(after_ordinal, after_digest, usize::MAX)
            .map(|page| page.events)
    }

    pub fn native_session_may_have_history(&self) -> bool {
        self.native_session_history() == NativeSessionHistory::MayHaveHistory
    }

    /// Classify native history using the durable evidence, including after ACK pruning.
    pub fn native_session_history(&self) -> NativeSessionHistory {
        let dispatched = self
            .snapshot
            .dispatches
            .values()
            .any(RelayDispatchRecord::prompt_may_have_reached_agent);
        if let Some(evidence) = &self.snapshot.native_use_evidence {
            // Before the restored worker opens its first harness session,
            // `native_session_id` is still `None` while the seed's archived
            // evidence is already present. Honor `restored_native_session_unused`
            // so an unused restored session can still take `LoadBeforeReplace`.
            if self.snapshot.native_session_id.is_none() {
                return if evidence.version == 1
                    && evidence.usage == NativeSessionUse::Unused
                    && self.snapshot.restored_native_session_unused
                    && !dispatched
                {
                    NativeSessionHistory::LoadBeforeReplace
                } else {
                    NativeSessionHistory::MayHaveHistory
                };
            }
            if evidence.version != 1
                || Some(&evidence.native_session_id) != self.snapshot.native_session_id.as_ref()
                || evidence.usage != NativeSessionUse::Unused
                || dispatched
            {
                return NativeSessionHistory::MayHaveHistory;
            }
            return match evidence.opened_ordinal {
                Some(_) => NativeSessionHistory::CreatedHereUnused,
                None if self.snapshot.restored_native_session_unused => {
                    NativeSessionHistory::LoadBeforeReplace
                }
                None => NativeSessionHistory::MayHaveHistory,
            };
        }
        if self.snapshot.native_session_used || dispatched {
            return NativeSessionHistory::MayHaveHistory;
        }
        match self.snapshot.native_session_opened_ordinal {
            // History below the recovery floor was archived. A thread opened
            // above it could never have contributed to that history.
            Some(opened) if opened > self.snapshot.recovery_floor_ordinal => {
                NativeSessionHistory::CreatedHereUnused
            }
            // The checkpoint hint only reflects visible user prompts. The
            // harness can still have history from a mailbox wake.
            None if self.snapshot.restored_native_session_unused => {
                NativeSessionHistory::LoadBeforeReplace
            }
            _ => NativeSessionHistory::MayHaveHistory,
        }
    }

    /// Whether the checkpoint this relay was restored from shows that the
    /// native session it continues never received a prompt, while no session
    /// has opened here yet.
    pub fn restored_native_session_unused(&self) -> bool {
        self.snapshot.restored_native_session_unused
            && self.snapshot.native_session_opened_ordinal.is_none()
    }

    /// Record that the native thread has been used and can never be replaced.
    /// Persisted immediately: the evidence is live ACP traffic, and a worker
    /// that dies right after it must not come back believing the thread empty.
    pub fn mark_native_session_used(&mut self) -> Result<()> {
        if self
            .snapshot
            .native_use_evidence
            .as_ref()
            .is_some_and(|evidence| {
                evidence.version == 1
                    && Some(&evidence.native_session_id) == self.snapshot.native_session_id.as_ref()
                    && evidence.usage == NativeSessionUse::Used
            })
        {
            return Ok(());
        }
        self.snapshot.native_session_used = true;
        self.snapshot.record_native_use(NativeSessionUse::Used);
        self.persist_snapshot()
    }

    /// How many times the journal's files stopped matching a captured
    /// [`RelayReplayPlan`]. A lock-free reader compares this before and after
    /// its read to tell a stale plan from a real desynchronization.
    pub fn journal_generation(&self) -> u64 {
        self.journal_generation
    }

    /// Journal an observation. While a checkpoint barrier holds its cut, one
    /// that can wait is held until the barrier ends and the current frontier
    /// is returned (see `checkpoint_hold`).
    pub fn record_observation(&mut self, observation: RelayObservation) -> Result<u64> {
        let Some(observation) = self.hold_observation(observation)? else {
            return Ok(self.snapshot.latest_ordinal);
        };
        if let RelayObservation::NativeAgent {
            event: mj_core::native_agent::NativeAgentEvent::Update { session_id, update },
        } = &observation
        {
            self.consume_claude_stop_acknowledgement(Some(session_id), update);
        }
        let acp_ready = match &observation {
            RelayObservation::SessionConfigured { .. } => Some(true),
            RelayObservation::AgentInitialized { .. }
            | RelayObservation::SessionRestarted
            | RelayObservation::Closing
            | RelayObservation::Closed => Some(false),
            _ => None,
        };
        let session_setup = match &observation {
            RelayObservation::AgentInitialized { .. } => Some(true),
            RelayObservation::SessionOpened { .. }
            | RelayObservation::SessionConfigured { .. }
            | RelayObservation::SessionRestarted
            | RelayObservation::Closing
            | RelayObservation::Closed => Some(false),
            _ => None,
        };
        let ordinal = self.append_relay_event(None, observation)?;
        if let Some(ready) = acp_ready {
            self.set_acp_ready(ready);
        }
        if let Some(setup) = session_setup {
            self.session_setup = setup;
        }
        Ok(ordinal)
    }

    pub fn record_session_update(&mut self, mut update: SessionUpdate) -> Result<u64> {
        // An ACP file edit arrives as the whole file before and the whole file
        // after. Store the patch between them instead: nothing downstream
        // reconstructs a file from those copies, and the patch is proportional
        // to the edit rather than to the file. See `mj_core::diff`.
        match &mut update {
            SessionUpdate::ToolCall(call) => {
                mj_core::diff::compact_tool_call_content(&mut call.content);
            }
            SessionUpdate::ToolCallUpdate(call) => {
                if let Some(content) = call.fields.content.as_mut() {
                    mj_core::diff::compact_tool_call_content(content);
                }
            }
            _ => {}
        }
        // A notification that changes no agent work waits for a ready
        // checkpoint barrier to end; see `checkpoint_hold`.
        let Some(update) = self.hold_session_update(update)? else {
            return Ok(self.snapshot.latest_ordinal);
        };
        let native_before = self.snapshot.goal.running();
        let mut native_after = self.snapshot.goal.clone();
        native_after.apply(&update)?;
        let resume_running = if let SessionUpdate::SessionInfoUpdate(info) = &update
            && let Some(execution) = info.meta.as_ref().and_then(|meta| meta.get("execution"))
        {
            let execution: mj_core::goal::GoalExecution =
                serde_json::from_value(execution.clone())?;
            execution.status == "running" && native_after.execution.as_ref() == Some(&execution)
        } else {
            false
        };
        let resumes_goal = |pending: &PendingNativeResume| {
            pending.goal.as_ref().is_none_or(|expected| {
                native_after
                    .snapshot
                    .as_ref()
                    .is_some_and(|goal| goal.same_goal(expected))
            })
        };
        let resumed_native = resume_running && self.pending_native_resumes.iter().any(resumes_goal);
        let codex = self.harness_turns == HarnessTurnPolicy::CodexAdapter;
        if (codex && !native_before && native_after.running())
            || (resumed_native && self.snapshot.harness_turn.is_none())
        {
            self.append_relay_event(
                None,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        // Arrival after the marker does not associate an idle update with
        // this resume. Only running proof or an explicit goal change settles it.
        let goal_report = if let SessionUpdate::SessionInfoUpdate(info) = &update {
            info.meta.as_ref().and_then(mj_core::goal::goal_meta)
        } else {
            None
        };
        let explicitly_cleared = goal_report.is_some_and(serde_json::Value::is_null);
        let goal_reported = goal_report.is_some();
        self.pending_native_resumes.retain(|pending| {
            let settled = explicitly_cleared
                || (goal_reported
                    && pending.goal.as_ref().is_some_and(|expected| {
                        native_after.snapshot.as_ref().is_none_or(|goal| {
                            !goal.same_goal(expected)
                                || (expected.status != goal.status && !goal.active())
                        })
                    }));
            !(settled
                || (resumed_native
                    && self.snapshot.harness_turn.is_some()
                    && resumes_goal(pending)))
        });
        let claude = self.harness_turns == HarnessTurnPolicy::ClaudeAdapter;
        // A stop we requested is acknowledged with a plain chunk and nothing
        // else: no model cycle runs, so no result would ever settle a turn
        // opened for it. Consume the expectation and keep the line.
        let owner = self.snapshot.native_session_id.clone();
        let ack = claude && self.consume_claude_stop_acknowledgement(owner.as_deref(), &update);
        // The adapter publishes notices as agent text at times no model cycle
        // runs. Such text is a notice row of its own, so it neither opens a
        // turn that nothing would end nor joins the previous reply.
        if claude
            && !ack
            && let Some(message) = self.notice_outside_any_turn(&update)
        {
            return self.record_observation(RelayObservation::Notice { message });
        }
        if claude && !ack && self.opens_harness_turn(&update) {
            self.open_harness_turn_if_allowed()?;
        }
        let ordinal = self.record_observation(RelayObservation::SessionUpdate {
            update: Box::new(update),
        })?;
        // A Claude turn settles on its SDK result instead, in
        // `claude_turn_result`: the adapter's origin marker is left out when a
        // cycle produced no assistant usage.
        if codex && native_before && !native_after.running() && self.snapshot.harness_turn.is_some()
        {
            self.settle_harness_turn(Some("codex".into()), "end_turn")?;
        }
        Ok(ordinal)
    }

    /// Whether a Claude cycle result ends the open harness turn. Since
    /// claude-agent-acp 0.101.1 (#1455), an announced task owns its boundary:
    /// raw results can precede its final output or belong to a previous cycle,
    /// and only that task's terminal edge may release its turn.
    fn claude_result_settles_turn(&self) -> bool {
        self.harness_turns == HarnessTurnPolicy::ClaudeAdapter
            && self.snapshot.active_prompt.is_none()
            && self.snapshot.harness_turn.is_some()
            && !self.announced_harness_turn()
    }

    /// Whether applying `event` would end a turn the harness started on its
    /// own. The coordinator asks before it applies the event, so it can observe
    /// the worker's processes first and hand the facts over with
    /// [`Self::offer_settle_facts`]: the same observation, taken at the same
    /// point, as for a prompt that is finishing. Each arm names the condition
    /// its settling site tests.
    pub fn event_settles_autonomous_turn<R>(
        &self,
        event: &mj_core::acp::RuntimeEventWithReload<R>,
    ) -> bool {
        use mj_core::acp::RuntimeEventWithReload as RuntimeEvent;
        if self.snapshot.active_prompt.is_some()
            || self.snapshot.autonomous_harness_turn().is_none()
        {
            return false;
        }
        match event {
            RuntimeEvent::ClaudeTurnResult(_) => self.claude_result_settles_turn(),
            RuntimeEvent::AirTaskChanged { session_id, update } => {
                self.air_update_settles_turn(session_id, update)
            }
            RuntimeEvent::SessionUpdate { update } => {
                self.harness_turns == HarnessTurnPolicy::CodexAdapter
                    && self.snapshot.goal.running()
                    && serde_json::from_value::<SessionUpdate>(update.clone())
                        .ok()
                        .is_some_and(|update| {
                            let mut after = self.snapshot.goal.clone();
                            after.apply(&update).is_ok() && !after.running()
                        })
            }
            _ => false,
        }
    }

    /// Hand the facts observed for the event about to be applied to the relay.
    /// Replaces whatever an earlier event was given.
    pub fn offer_settle_facts(&mut self, facts: Option<mj_core::work_facts::WorkFacts>) {
        self.pending_settle_facts = facts;
    }

    /// Claude Code ended a model cycle that did not end one of Mjolnir's
    /// prompts. If a turn Claude Code started on its own is open, this result
    /// is its boundary, whatever started the cycle.
    pub fn claude_turn_result(&mut self, result: &mj_core::acp::ClaudeTurnResult) -> Result<()> {
        if !self.claude_result_settles_turn() {
            return Ok(());
        }
        self.settle_harness_turn(
            Some(
                result
                    .origin_kind
                    .clone()
                    .unwrap_or_else(|| "human".to_owned()),
            ),
            if result.is_error {
                crate::acp::drive::PROMPT_ERROR_STOP_REASON
            } else {
                "end_turn"
            },
        )
    }

    /// The announced task owns completion unless an unanswered user stop overrides it.
    fn announced_harness_turn(&self) -> bool {
        self.snapshot.harness_turn.is_some_and(|turn| {
            self.air_tasks
                .values()
                .any(|entry| entry.turn_ordinal() == Some(turn.first_ordinal))
        })
    }

    /// The first ordinal of the open Claude harness turn a stop was applied
    /// to, while nothing has answered that stop yet. Claude Code answers a
    /// stop of a running cycle with the cycle's result; a turn it never ran
    /// gets no answer, so the coordinator ends it after
    /// [`HARNESS_TURN_STOP_GRACE`]. The snapshot owns the stop: a journal
    /// event records it, so it is the same on every path and after a restart.
    pub fn unanswered_harness_turn_stop(&self) -> Option<u64> {
        if !matches!(
            self.harness_turns,
            HarnessTurnPolicy::ClaudeAdapter | HarnessTurnPolicy::Antigravity
        ) {
            return None;
        }
        let turn = self.snapshot.autonomous_harness_turn()?;
        self.snapshot
            .cancelling_harness_turn
            .is_some_and(|stop| stop.applied && stop.first_ordinal == turn.first_ordinal)
            .then_some(turn.first_ordinal)
    }

    /// End the harness turn a stop was applied to, if it is still open. A
    /// user stop overrides even an announced task that never reports its end.
    /// Returns whether a turn was ended.
    pub fn end_unanswered_harness_turn_stop(&mut self, first_ordinal: u64) -> Result<bool> {
        if self.unanswered_harness_turn_stop() != Some(first_ordinal) {
            return Ok(false);
        }
        tracing::warn!(
            session_id = %self.snapshot.session_id,
            first_ordinal,
            "The harness did not answer a stop of its autonomous turn; ending the turn"
        );
        self.settle_harness_turn(Some("stop_unanswered".to_owned()), "cancelled")?;
        Ok(true)
    }

    /// Whether a request that configures the session, rather than a prompt,
    /// is waiting for the harness's answer. The Claude adapter publishes some
    /// notices as agent text while it answers such a request: a switch to a
    /// model without Auto mode sends "Auto mode unavailable". No model cycle
    /// runs for it, so no result would ever end a turn opened for that text.
    fn configuration_request_in_flight(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            dispatch.state == RelayDispatchState::InFlight
                && matches!(
                    dispatch.command,
                    RelayCommand::SetConfig { .. }
                        | RelayCommand::SetSessionMode { .. }
                        | RelayCommand::RestoreExecutionMode
                )
        })
    }

    fn settle_harness_turn(&mut self, origin: Option<String>, stop_reason: &str) -> Result<()> {
        let settled_ordinal = self.snapshot.harness_turn.map(|turn| turn.first_ordinal);
        let work_facts = self.pending_settle_facts.take().map(Box::new);
        self.append_relay_event(
            None,
            RelayObservation::HarnessTurnSettled {
                autonomous: self
                    .snapshot
                    .autonomous_harness_turn()
                    .filter(|_| self.snapshot.active_prompt.is_none())
                    .map(|turn| mj_core::relay::SettledHarnessTurn {
                        start_ordinal: turn.first_ordinal,
                        // claude-agent-acp 0.99.0 / Claude Code 2.1.308 (2026-11-06)
                        // forwards SDK results, including error-shaped interrupted cycles.
                        // Its interrupt has no yield deadline: a result can arrive after
                        // stop_unanswered. The snapshot's recorded stop owns the
                        // cancellation reason.
                        stop_reason: if self
                            .snapshot
                            .cancelling_harness_turn
                            .is_some_and(|stop| stop.first_ordinal == turn.first_ordinal)
                        {
                            "cancelled".into()
                        } else {
                            stop_reason.into()
                        },
                        work_facts,
                    }),
                origin,
                // The projection cannot see `active_prompt`, so the event has
                // to carry whether a prompt of ours is still running.
                prompt_in_flight: self.snapshot.active_prompt.is_some(),
            },
        )?;
        // Settlement and detachment share the relay owner lock. Keep task IDs
        // as tombstones so late terminal updates cannot claim a successor.
        // Late output may open an unannounced turn after its raw result; a
        // fresh Stop still ends that turn through the same grace fallback.
        for entry in self.air_tasks.values_mut() {
            if let background::AirTaskState::Turn(ordinal) = &mut entry.state
                && *ordinal == settled_ordinal
            {
                *ordinal = None;
            }
        }
        self.persist_activity_transition()
    }

    /// Whether agent output arriving now belongs to no turn: no prompt runs,
    /// no harness turn is open, and the session is either still being set up
    /// or answering a configuration request. The Claude adapter reports its
    /// Auto mode fallback as agent text at both times: while a resumed
    /// session loads and its model is applied (R8-4), and while it answers a
    /// live model change (R4-2, R8-5).
    fn output_outside_any_turn(&self) -> bool {
        (self.session_setup || self.configuration_request_in_flight())
            && self.snapshot.active_prompt.is_none()
            && self.snapshot.harness_turn.is_none()
    }

    /// The notice row for agent text that belongs to no turn, without the
    /// adapter's Markdown emphasis, which a notice row would show literally.
    fn notice_outside_any_turn(&self, update: &SessionUpdate) -> Option<String> {
        if !self.output_outside_any_turn() {
            return None;
        }
        let message = background::agent_chunk_text(update)?
            .replace("**", "")
            .trim()
            .to_owned();
        (!message.is_empty()).then_some(message)
    }

    /// Whether this update reveals the harness working with nothing of Hel's
    /// in flight, which is what opens a harness-initiated turn. Output that
    /// arrives while a configuration request waits for its answer belongs to
    /// that request, and output while the session is set up belongs to no
    /// turn.
    fn opens_harness_turn(&self, update: &SessionUpdate) -> bool {
        is_agent_output(update)
            // A tool update refines a call the adapter already announced. When
            // a cycle ends, the adapter fails its unfinished calls ("Claude
            // stopped this tool call before it ran.") after the result, and no
            // cycle would end a turn opened for that status. Checked against
            // claude-agent-acp 0.94.0 `dist/tool-calls/renderer.js`
            // (`unfinished`) on 2026-10-23; the file is unchanged in 0.97.0.
            && !matches!(update, SessionUpdate::ToolCallUpdate(_))
            // Our terminal inventory/card is not evidence of a parent model
            // cycle: a child may request it while the parent remains idle.
            // Reannouncing a retired call also cannot establish a new cycle.
            && !matches!(update, SessionUpdate::ToolCall(call)
                if crate::acp::is_fallback_terminal_tool_call(call)
                    || self.foreground_tools.is_retired(call.tool_call_id.0.as_ref()))
            && self.can_open_harness_turn()
    }

    /// The shared admission decision for Claude output and autonomous spawns.
    fn can_open_harness_turn(&self) -> bool {
        self.snapshot.active_prompt.is_none()
            && self.snapshot.harness_turn.is_none()
            && !self.session_setup
            && !self.configuration_request_in_flight()
            && !matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
    }

    /// Open the one durable boundary shared by output and turn announcements.
    pub(crate) fn open_harness_turn_if_allowed(&mut self) -> Result<()> {
        if !self.can_open_harness_turn() {
            return Ok(());
        }
        // New work invalidates a ready checkpoint cut. Replaying earlier held
        // writes may itself open a turn, so ask the owner again afterwards.
        self.end_checkpoint_hold()?;
        if self.can_open_harness_turn() {
            self.task_settled_at_ms = None;
            self.append_relay_event(
                None,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        Ok(())
    }

    fn digest_at(&self, ordinal: u64) -> Result<Option<String>> {
        self.replay_plan().digest_at(ordinal)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

    /// A recorded GLiDE answer whose frozen rule gives `expected` (`await_input`,
    /// `finished`, `wait`, `retry_provider`, `recover_quota` or `uncertain`).
    /// `background` replaces the leftover-process distribution `[needed, unneeded,
    /// unclear]` when a test needs a particular quiet judgment.
    pub(crate) fn glide_answer(
        expected: &str,
        background: Option<[f64; 3]>,
    ) -> mj_core::activity::eot_glide::types::Probabilities {
        let samples = crate::acp::verdict_client::glide_turn::test_samples();
        let mut answer = samples
            .as_array()
            .unwrap()
            .iter()
            .find(|sample| sample["expected"] == expected)
            .unwrap_or_else(|| panic!("no recorded GLiDE answer for {expected}"))
            .clone();
        if let Some([needed, unneeded, unclear]) = background {
            answer["answers"]["background"]["probabilities"] =
                serde_json::json!({"needed":needed,"unneeded":unneeded,"unclear":unclear});
        }
        crate::acp::verdict_client::glide_turn::Probabilities::parse(&answer).unwrap()
    }

    pub(crate) fn relay_request(request_id: &str, request: RelayRequest) -> RelayRequestEnvelope {
        RelayRequestEnvelope {
            request_id: request_id.to_owned(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request,
        }
    }

    pub(crate) fn submit_relay(
        relay: &mut DurableRelay,
        command_id: &str,
        command: RelayCommand,
    ) -> u64 {
        let response = relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command,
            },
        ));
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Accepted {
                    command_id: accepted,
                    ordinal,
                },
        } = response.body
        else {
            panic!("expected accepted relay command, got {:?}", response.body);
        };
        assert_eq!(accepted, command_id);
        ordinal
    }

    pub(crate) fn attach_relay(
        relay: &mut DurableRelay,
        request_id: &str,
        after_ordinal: u64,
    ) -> RelayResponseEnvelope {
        let after_digest = relay.digest_at(after_ordinal).unwrap().unwrap();
        relay.handle(relay_request(
            request_id,
            RelayRequest::Attach {
                after_ordinal,
                after_digest,
                known_state_digest: None,
            },
        ))
    }

    pub(crate) fn acknowledge_relay(
        relay: &mut DurableRelay,
        request_id: &str,
        through_ordinal: u64,
    ) -> RelayResponseEnvelope {
        let through_digest = relay.digest_at(through_ordinal).unwrap().unwrap();
        relay.handle(relay_request(
            request_id,
            RelayRequest::Acknowledge {
                through_ordinal,
                through_digest,
            },
        ))
    }

    pub(crate) fn prompt(text: &str) -> RelayCommand {
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from(text)],
        }
    }

    pub(crate) fn set_config(key: &str, value: &str) -> RelayCommand {
        RelayCommand::SetConfig {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }

    pub(crate) fn queued_command_ids(relay: &DurableRelay) -> Vec<String> {
        relay
            .operational_state()
            .queued_prompts
            .into_iter()
            .map(|queued| queued.command_id)
            .collect()
    }

    pub(crate) fn finish_prompt(relay: &mut DurableRelay, command_id: &str) {
        relay
            .record_command_completed(
                command_id,
                RelayCommandOutcome::Prompt {
                    work_facts: None,
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
    }

    pub(crate) fn retained_events(relay: &DurableRelay) -> Vec<RelayEvent> {
        relay
            .events_after(
                relay.snapshot.retained_through(),
                relay.snapshot.retained_digest(),
            )
            .unwrap()
    }

    pub(crate) fn submit_release(
        relay: &mut DurableRelay,
        command_id: &str,
        barrier_command_id: &str,
    ) -> RelayResponseEnvelope {
        relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command: RelayCommand::ReleaseCheckpoint {
                    barrier_command_id: barrier_command_id.to_owned(),
                },
            },
        ))
    }

    pub(crate) fn submit_floor(
        relay: &mut DurableRelay,
        command_id: &str,
        through: RelayCursor,
    ) -> RelayResponseEnvelope {
        relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command: RelayCommand::AdvanceRecoveryFloor { through },
            },
        ))
    }

    pub(crate) fn ready_checkpoint(relay: &mut DurableRelay, command_id: &str) -> RelayCursor {
        submit_relay(
            relay,
            command_id,
            RelayCommand::BeginCheckpoint { reason: None },
        );
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, command_id);
        relay.record_checkpoint_ready(command_id).unwrap();
        relay.operational_state().checkpoint_ready.unwrap()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod attachment_tests;

#[cfg(test)]
mod continuation_tests;

#[cfg(test)]
mod handback_tests;

/// Seed once at the legacy persisted-format boundary, never during publication.
fn seed_legacy_open_request(snapshot: &mut RelaySnapshot) {
    let Some(assessment) = snapshot.assessment.as_deref() else {
        return;
    };
    if assessment.question_dismissed() {
        return;
    }
    let question = assessment.inferred_question().or_else(|| {
        (mj_core::state::classify_prompt_completion(&assessment.completion.stop_reason)
            == mj_core::state::PromptCompletion::InputRequired)
            .then(|| mj_core::assessment::InferredQuestion {
                turn_id: assessment.turn_id.clone(),
                completed_ordinal: assessment.completed_ordinal,
            })
    });
    if let Some(question) = question {
        snapshot.person_input_through = snapshot
            .latest_prompt_accepted_ordinal
            .filter(|ordinal| *ordinal > question.completed_ordinal)
            .unwrap_or(0);
        snapshot.request = Some(mj_core::assessment::OpenRequest {
            question,
            question_message: assessment
                .question_message
                .clone()
                .or_else(|| snapshot.completed_reply_message.clone()),
        });
    }
}

fn seed_legacy_notification_stack(snapshot: &mut RelaySnapshot) {
    use mj_core::attention::{
        EntryMessage, NotificationKind, NotificationStack, NotificationStackEntry,
    };
    let mut entries = Vec::new();
    if let Some(request) = snapshot.open_request() {
        entries.push(NotificationStackEntry {
            completed_at_ms: snapshot
                .assessment
                .as_deref()
                .filter(|assessment| {
                    assessment.completed_ordinal == request.question.completed_ordinal
                })
                .map_or(0, |assessment| assessment.completed_at_ms),
            completed_ordinal: request.question.completed_ordinal,
            kind: NotificationKind::NeedsInput,
            message: request
                .question_message
                .clone()
                .map_or(EntryMessage::None, EntryMessage::Found),
        });
    }
    if let Some(assessment) = snapshot.assessment.as_deref()
        && assessment.current()
        && assessment.attention == Some(mj_core::attention::AssessmentAttention::Important)
        && assessment.completed_ordinal > snapshot.person_input_through
        && snapshot.turn_completion.as_ref().is_none_or(|turn| {
            turn.completed_ordinal == assessment.completed_ordinal
                && turn.command_id == assessment.turn_id
        })
        && !entries
            .iter()
            .any(|entry| entry.completed_ordinal == assessment.completed_ordinal)
    {
        entries.push(NotificationStackEntry {
            completed_at_ms: assessment.completed_at_ms,
            completed_ordinal: assessment.completed_ordinal,
            kind: NotificationKind::Important,
            message: snapshot
                .completed_reply_message
                .clone()
                .map_or(EntryMessage::None, EntryMessage::Found),
        });
        entries.sort_by_key(|entry| entry.completed_ordinal);
    }
    if !entries.is_empty() {
        snapshot.notification_stack = Arc::new(NotificationStack {
            revision: snapshot.latest_ordinal,
            entries,
        });
    }
}

/// Older stack snapshots omitted the time. The matching assessment retains
/// the original completion event's time even when its journal was collected.
fn seed_missing_notification_time(snapshot: &mut RelaySnapshot) {
    if let Some(assessment) = snapshot.assessment.as_deref()
        && let Some(index) = snapshot
            .notification_stack
            .entries
            .iter()
            .position(|entry| {
                entry.completed_at_ms == 0
                    && entry.completed_ordinal == assessment.completed_ordinal
            })
    {
        Arc::make_mut(&mut snapshot.notification_stack).entries[index].completed_at_ms =
            assessment.completed_at_ms;
    }
}

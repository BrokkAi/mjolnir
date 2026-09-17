//! The one answer to "is this session working, idle, or stalled".
//!
//! Every part of Mjolnir used to decide this for itself: the worker's idle
//! clock, the dashboard's activity column, the checkpoint gate, the
//! restart-readiness wait, the viewer's `chat_phase`, and the turn stall
//! watchdog. They read overlapping subsets of the same facts and disagreed,
//! and every bug in this area was one of them contradicting another.
//!
//! So there is one set of facts ([`ActivityFacts`]), one classifier
//! ([`classify`]), and one place each separate question is answered:
//!
//! * What is the session doing? [`classify`] returns an [`ActivityState`].
//! * Does it still own work that killing the worker would destroy?
//!   [`has_work_in_flight`].
//! * May a controller replace this worker? [`safe_to_replace`].
//! * May a routine checkpoint open a barrier? [`checkpoint_blocker`].
//! * Has the turn stopped responding? [`stall_verdict`].
//!
//! The worker produces the facts and publishes both them and the classified
//! state. Nothing else re-derives an answer; a consumer that only has the
//! state asks the state, and a consumer that has the facts asks the function.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::HarnessKind;
use crate::relay::RelayExecutionState;
use crate::state::MaterializedExecutionState;

mod tools;
pub use tools::{InFlightToolCall, ToolsInFlight};

/// Every fact that bears on whether a session is working.
///
/// Built by the worker from its relay, and by the daemon from the operational
/// state a worker sent. Counts and timestamps rather than whole collections,
/// so building it on a render path is cheap and so this module does not have
/// to know the shape of anything it does not decide with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityFacts {
    /// The durable execution flag folded from the event journal.
    pub execution: RelayExecutionState,
    /// When the prompt in flight started, while one is in flight.
    pub prompt_started_at_ms: Option<i64>,
    /// When the turn the harness started on its own began, while it is open.
    pub harness_turn_started_at_ms: Option<i64>,
    /// Start of the latest turn, retained until its background work settles.
    pub turn_started_at_ms: Option<i64>,
    /// Commands accepted and waiting behind the one in flight.
    pub queued_commands: usize,
    /// Tool calls the agent has open, oldest first.
    pub tools_in_flight: Vec<InFlightToolCall>,
    /// Oldest start among background commands and user shells.
    pub background_started_at_ms: Option<i64>,
    pub background_commands: usize,
    pub active_user_shells: usize,
    pub active_agent_terminals: usize,
    pub goal_active: bool,
    pub goal_running: bool,
    pub goal_pending_resume: bool,
    pub goal_decision: bool,
    pub goal_synchronized: bool,
    /// Whether provider-owned background work is known. Only Kimi reports it.
    pub background_work_known: Option<bool>,
    /// Whether the worker has finished opening its ACP session. A worker too
    /// old to report it leaves this empty and is treated as ready.
    pub acp_ready: Option<bool>,
    /// This process serves recovered state without running a harness.
    pub checkpoint_only: bool,
    /// A checkpoint barrier is waiting to capture.
    pub checkpoint_barrier: bool,
    /// When anything at all last arrived over ACP.
    pub last_acp_activity_at_ms: Option<i64>,
    /// When the step the agent is on began, while a step is in flight.
    pub current_step_started_at_ms: Option<i64>,
    /// Start of the current observed idle period, when the relay knows it.
    pub idle_since_ms: Option<i64>,
}

impl Default for ActivityFacts {
    /// A session with nothing happening. `RelayExecutionState` deliberately
    /// has no `Default` of its own — defaulting a wire enum to `Idle` is the
    /// mistake that made a running turn report itself as finished — so the
    /// only place that choice is made is here, for tests and for building
    /// facts field by field.
    fn default() -> Self {
        Self {
            execution: RelayExecutionState::Idle,
            prompt_started_at_ms: None,
            harness_turn_started_at_ms: None,
            turn_started_at_ms: None,
            queued_commands: 0,
            tools_in_flight: Vec::new(),
            background_started_at_ms: None,
            background_commands: 0,
            active_user_shells: 0,
            active_agent_terminals: 0,
            goal_active: false,
            goal_running: false,
            goal_pending_resume: false,
            goal_decision: false,
            goal_synchronized: false,
            background_work_known: None,
            acp_ready: None,
            checkpoint_only: false,
            checkpoint_barrier: false,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms: None,
            idle_since_ms: None,
        }
    }
}

/// What a session is doing, at the granularity every part of Mjolnir agrees on.
///
/// Serialized internally tagged so it can be published inside the operational
/// state a worker sends. [`Self::Unrecognized`] is the forward-compatibility
/// landing place: a reader too old to know a variant a newer worker sends
/// deserializes it here rather than failing, which would drop the whole
/// snapshot. It is deliberately the cautious answer — never idle, always work
/// in flight — because the alternative is presenting a working session as
/// finished, which is the failure this module exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ActivityState {
    /// Nobody can currently see the worker, so this is the last thing that was
    /// known about it. Never idle: a turn that outlives a daemon restart must
    /// not be reported as finished.
    Unknown {
        last_known: Box<ActivityState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_ms: Option<i64>,
    },
    Closed,
    Closing,
    /// A turn is in flight: a prompt Mjolnir sent, or one the harness started.
    Turn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_ms: Option<i64>,
    },
    /// No turn boundary is open, but a tool call is running. Harnesses that
    /// mark no turn of their own are visible only this way.
    Tool {
        tool_call_id: String,
        started_at_ms: i64,
    },
    /// Only work the agent left running behind it.
    Background {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_ms: Option<i64>,
    },
    /// A native goal owns the session.
    Goal,
    Idle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_ms: Option<i64>,
    },
    /// A state this build does not know. See the type documentation.
    #[serde(other)]
    Unrecognized,
}

impl Default for ActivityState {
    fn default() -> Self {
        Self::Idle { since_ms: None }
    }
}

impl ActivityState {
    /// Nothing is running: no turn, no tool, no background work, no goal.
    ///
    /// An unknown or unrecognized state is never idle. Missing knowledge must
    /// not be presented as confirmed idleness.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle { .. })
    }

    /// The agent is computing right now, as opposed to holding work that is
    /// merely not finished.
    #[must_use]
    pub fn is_working(&self) -> bool {
        match self {
            Self::Turn { .. } | Self::Tool { .. } | Self::Background { .. } | Self::Closing => true,
            Self::Unknown { last_known, .. } => last_known.is_working(),
            Self::Idle { .. } | Self::Goal | Self::Closed | Self::Unrecognized => false,
        }
    }

    /// Whether this state alone means the session still owns work.
    ///
    /// Callers that hold the facts should ask [`has_work_in_flight`] instead,
    /// which also covers queued commands, open terminals and unsynchronized
    /// provider state. This is for a caller that has only a state.
    ///
    /// `Unknown` always counts, whatever was last known. A session nobody can
    /// see may have started a turn since, so replacing its worker or taking a
    /// checkpoint cut against it could destroy work. This does not block
    /// recovering a worker that is truly gone: that session reports `Closed`,
    /// not `Unknown`, because [`while_disconnected`] keeps the two apart.
    #[must_use]
    pub fn has_work_in_flight(&self) -> bool {
        !matches!(self, Self::Idle { .. } | Self::Closed)
    }

    /// The four-valued phase the viewer and the API have always reported.
    ///
    /// An unknown state answers with what was last known, which is the whole
    /// point: a turn that spans a daemon restart keeps reporting `Running`
    /// instead of falling back to the default value of an enum.
    #[must_use]
    pub fn chat_phase(&self) -> RelayExecutionState {
        match self {
            Self::Closed => RelayExecutionState::Closed,
            Self::Closing => RelayExecutionState::Closing,
            Self::Turn { .. } | Self::Tool { .. } | Self::Unrecognized => {
                RelayExecutionState::Running
            }
            Self::Background { .. } | Self::Goal | Self::Idle { .. } => RelayExecutionState::Idle,
            Self::Unknown { last_known, .. } => last_known.chat_phase(),
        }
    }

    /// The last state actually observed, looking through `Unknown`.
    #[must_use]
    pub fn last_known(&self) -> &Self {
        match self {
            Self::Unknown { last_known, .. } => last_known.last_known(),
            other => other,
        }
    }
}

/// What a session is doing, from everything known about it.
///
/// Pure: the same facts always give the same state, with no clock and no I/O,
/// so the transitions can be driven directly from a test.
#[must_use]
pub fn classify(facts: &ActivityFacts) -> ActivityState {
    match facts.execution {
        RelayExecutionState::Closed => return ActivityState::Closed,
        RelayExecutionState::Closing => return ActivityState::Closing,
        RelayExecutionState::Idle | RelayExecutionState::Running => {}
    }
    let turn_started_at_ms = facts
        .prompt_started_at_ms
        .or(facts.harness_turn_started_at_ms)
        .or(facts.turn_started_at_ms);
    // A bare `Running` flag is not on its own evidence of a turn: the durable
    // projection can lag behind a turn that has already ended, and presenting
    // a stale flag as live work is how a finished session came to look busy.
    // Something live has to corroborate it — a prompt, a turn the harness
    // opened, a step in flight, or a native goal actually running.
    let running_is_corroborated = facts.execution == RelayExecutionState::Running
        && (facts.current_step_started_at_ms.is_some() || facts.goal_running);
    if facts.prompt_started_at_ms.is_some() || facts.harness_turn_started_at_ms.is_some() {
        return ActivityState::Turn {
            started_at_ms: turn_started_at_ms,
        };
    }
    // A harness that marks no turn of its own is visible only through the
    // tool calls it has open, so those are a state in their own right, and a
    // named tool is a better answer than an unnamed turn.
    if let Some(tool) = facts.tools_in_flight.first() {
        return ActivityState::Tool {
            tool_call_id: tool.tool_call_id.clone(),
            started_at_ms: tool.started_at_ms,
        };
    }
    if running_is_corroborated {
        return ActivityState::Turn {
            started_at_ms: turn_started_at_ms,
        };
    }
    if facts.background_commands > 0 || facts.active_user_shells > 0 {
        return ActivityState::Background {
            started_at_ms: facts.background_started_at_ms,
        };
    }
    if facts.goal_active || facts.goal_running {
        return ActivityState::Goal;
    }
    ActivityState::Idle {
        since_ms: facts.idle_since_ms,
    }
}

/// Whether anything the worker owns would be destroyed by killing it now.
///
/// This is the superset: on top of whatever the session is doing, it counts
/// the work that is accepted but not started (queued commands), the terminals
/// and shells the agent opened, a goal decision waiting to be applied, and
/// provider state that is not known to be settled. The durable execution flag
/// alone is not enough — a stale projection can report `Idle` while a turn the
/// harness started, or a tool it is still running, is live — which is why this
/// reads the classified state rather than that flag.
#[must_use]
pub fn has_work_in_flight(facts: &ActivityFacts) -> bool {
    if facts.execution == RelayExecutionState::Closed {
        return false;
    }
    classify(facts).has_work_in_flight()
        // The durable flag on its own is too weak to claim the agent is
        // working, but far too strong to ignore when the question is whether
        // killing the worker would destroy something.
        || facts.execution != RelayExecutionState::Idle
        || facts.queued_commands > 0
        || facts.active_agent_terminals > 0
        || facts.goal_pending_resume
        || facts.goal_decision
        || facts.acp_ready == Some(false)
        || facts.background_work_known == Some(false)
}

/// Whether nothing at all is happening, including no checkpoint barrier.
///
/// A held barrier is the one reason this refuses that is not itself work: a
/// caller already holding a barrier asks [`has_work_in_flight`] to learn
/// whether anything *else* is running.
#[must_use]
pub fn is_quiet(facts: &ActivityFacts) -> bool {
    !facts.checkpoint_barrier && !has_work_in_flight(facts)
}

/// Whether a controller may replace this worker without losing work.
///
/// Older Codex and Kimi workers cannot prove that native goals or background
/// agents are absent, even when their snapshots look quiet.
#[must_use]
pub fn safe_to_replace(facts: &ActivityFacts, harness: HarnessKind) -> bool {
    is_quiet(facts)
        && (harness != HarnessKind::Codex || facts.goal_synchronized)
        && (harness != HarnessKind::Kimi || facts.background_work_known == Some(true))
}

/// Why a routine checkpoint may not admit a barrier, or `None` when it may.
///
/// This asks only about provider-owned work, which is a different question
/// from whether a turn is running: a checkpoint that finds a turn running can
/// try again in a minute, but a checkpoint taken while a native goal or a
/// background agent owns the session captures a state that does not exist.
/// Unknown native state fails closed.
#[must_use]
pub fn checkpoint_blocker(facts: &ActivityFacts, harness: HarnessKind) -> Option<&'static str> {
    if facts.checkpoint_only || facts.execution == RelayExecutionState::Closed {
        None
    } else if harness == HarnessKind::Codex && !facts.goal_synchronized {
        Some("Codex goal and execution state is not synchronized; checkpoint deferred")
    } else if facts.goal_active || facts.goal_running || facts.goal_decision {
        Some("an active goal owns this session; pause the goal before checkpointing")
    } else if harness != HarnessKind::Kimi {
        None
    } else if facts.background_work_known.is_none() {
        Some(
            "Kimi worker has not reported background-agent synchronization support; checkpoint requires a worker reporting synchronized task state",
        )
    } else if facts.background_work_known == Some(false) {
        Some(
            "Kimi background-agent state is not synchronized; checkpoint requires a synchronized empty task list",
        )
    } else if facts.background_commands > 0 {
        Some(
            "Kimi background agents are still active; checkpoint requires their completion and a synchronized empty task list",
        )
    } else {
        None
    }
}

/// The four-valued phase to report for a session the daemon can see.
///
/// The execution flag is the phase, with one correction: a live turn the flag
/// has not caught up with is still running. `ActivityState::chat_phase` is the
/// same question for a session nobody can see, where there is no flag to read.
#[must_use]
pub fn chat_phase(facts: &ActivityFacts) -> RelayExecutionState {
    match facts.execution {
        RelayExecutionState::Closing | RelayExecutionState::Closed => facts.execution,
        _ if matches!(
            classify(facts),
            ActivityState::Turn { .. } | ActivityState::Tool { .. }
        ) =>
        {
            RelayExecutionState::Running
        }
        execution => execution,
    }
}

/// What to report about a session nobody can currently see.
///
/// The daemon calls this while it has no live connection to a worker: after
/// its own restart, during a reattach, or while the target is unreachable.
/// The worker itself is unaffected by any of that, so the honest answer is the
/// last state the durable projection recorded, marked unknown — never `Idle`,
/// which is what a default value used to produce and what made automation
/// treat a running turn as finished.
#[must_use]
pub fn while_disconnected(
    durable: MaterializedExecutionState,
    since_ms: Option<i64>,
) -> ActivityState {
    let last_known = match durable {
        MaterializedExecutionState::Idle => ActivityState::Idle { since_ms: None },
        MaterializedExecutionState::Running { started_at_ms } => ActivityState::Turn {
            started_at_ms: Some(started_at_ms),
        },
        MaterializedExecutionState::Closing => ActivityState::Closing,
        MaterializedExecutionState::Closed => ActivityState::Closed,
    };
    // A worker that is gone is gone: saying "unknown" about a closed session
    // would block the recovery that is supposed to clean it up.
    if matches!(last_known, ActivityState::Closed) {
        return ActivityState::Closed;
    }
    ActivityState::Unknown {
        last_known: Box::new(last_known),
        since_ms,
    }
}

/// How long a turn may go without a sign of life before the worker fails it.
///
/// Two bounds, because silence means two different things. With nothing in
/// flight, silence is a bridge that stopped relaying and the turn is lost
/// after `silence`. With a tool call open, silence is normal — a twenty-minute
/// build produces no protocol traffic at all — so only the much longer
/// `tool_call` bound applies, and it exists solely to catch a bridge that died
/// leaving a tool card open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallPolicy {
    /// Silence with nothing in flight. `None` disables the watchdog.
    pub silence: Option<Duration>,
    /// How long one tool call may run. `None` means no bound.
    pub tool_call: Option<Duration>,
}

impl StallPolicy {
    /// The shortest and longest a watchdog waits between two checks.
    pub const CHECK_FLOOR: Duration = Duration::from_millis(50);
    pub const CHECK_CEILING: Duration = Duration::from_secs(1);

    /// Whether either bound can ever trip.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.silence.is_some() || self.tool_call.is_some()
    }

    /// How long to wait before asking again, given what is in flight now.
    ///
    /// Never zero, because a caller sleeps on this in a loop and a zero wait
    /// would spin. Never longer than [`Self::CHECK_CEILING`] either: what is
    /// in flight changes while the watchdog waits — a tool call opens or ends
    /// — and a watchdog that had slept until the bound it computed from the
    /// old facts would miss the change entirely. Waking once a second and
    /// asking again costs nothing and is the only way the answer stays true
    /// to what is happening.
    #[must_use]
    pub fn next_check(&self, facts: &ActivityFacts, now_ms: i64) -> Duration {
        let remaining = match facts.tools_in_flight.first() {
            Some(oldest) => self
                .tool_call
                .map(|bound| bound.saturating_sub(elapsed(oldest.started_at_ms, now_ms))),
            None => match (self.silence, facts.last_acp_activity_at_ms) {
                (Some(bound), Some(last)) => Some(bound.saturating_sub(elapsed(last, now_ms))),
                (Some(bound), None) => Some(bound),
                (None, _) => None,
            },
        };
        remaining
            .unwrap_or(Self::CHECK_CEILING)
            .clamp(Self::CHECK_FLOOR, Self::CHECK_CEILING)
    }
}

/// Whether a turn has stopped responding, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallVerdict {
    /// Still alive: something arrived recently, or a tool call is running
    /// within its bound.
    Live,
    /// Nothing arrived and nothing is in flight.
    Silent { silent_ms: u64 },
    /// One tool call outlived the bound on a single tool call.
    ToolCall {
        tool_call_id: String,
        running_ms: u64,
        silent_ms: u64,
    },
}

/// Whether a running turn has stopped responding.
///
/// The rule that fixes the long-tool-call failure is one line of it: when a
/// tool call is open, the silence bound does not apply at all. A harness
/// blocked in a twenty-minute `cargo nextest` run sends no protocol traffic
/// and used to be failed for it, even though Mjolnir could see the tool call
/// it was blocked in the whole time.
#[must_use]
pub fn stall_verdict(facts: &ActivityFacts, policy: StallPolicy, now_ms: i64) -> StallVerdict {
    let silence = facts
        .last_acp_activity_at_ms
        .map(|last| elapsed(last, now_ms));
    let silent_ms = silence.unwrap_or_default().as_millis() as u64;
    if let Some(oldest) = facts.tools_in_flight.first() {
        let running = elapsed(oldest.started_at_ms, now_ms);
        return match policy.tool_call {
            Some(bound) if running >= bound => StallVerdict::ToolCall {
                tool_call_id: oldest.tool_call_id.clone(),
                running_ms: running.as_millis() as u64,
                silent_ms,
            },
            _ => StallVerdict::Live,
        };
    }
    // No evidence of activity at all is not evidence of silence: a turn is
    // marked active when its prompt is sent, so an empty clock means this
    // session has not started one.
    match (policy.silence, silence) {
        (Some(bound), Some(silence)) if silence >= bound => StallVerdict::Silent { silent_ms },
        _ => StallVerdict::Live,
    }
}

fn elapsed(since_ms: i64, now_ms: i64) -> Duration {
    Duration::from_millis(now_ms.saturating_sub(since_ms).max(0) as u64)
}

#[cfg(test)]
mod tests;

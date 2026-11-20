// Port of mj-core/src/activity.rs.

// The one answer to "is this session working, idle, or stalled".
//
// Every part of Mjolnir used to decide this for itself: the worker's idle
// clock, the dashboard's activity column, the checkpoint gate, the
// restart-readiness wait, the viewer's `chat_phase`, and the turn stall
// watchdog. They read overlapping subsets of the same facts and disagreed,
// and every bug in this area was one of them contradicting another.
//
// So there is one set of facts (ActivityFacts), one classifier (Classify),
// and one place each separate question is answered:
//
//   - What is the session doing? Classify returns an ActivityState.
//   - Does it still own work that killing the worker would destroy?
//     HasWorkInFlight.
//   - May a controller replace this worker? SafeToReplace.
//   - May a routine checkpoint start now? RoutineCheckpointWait.
//   - Would provider-owned work make a checkpoint cut wrong?
//     CheckpointBlocker, which is part of the answer above and is also
//     asked alone while a barrier is held.
//   - Has the turn stopped responding? StallVerdictOf.
//
// The worker produces the facts and publishes both them and the classified
// state. Nothing else re-derives an answer; a consumer that only has the
// state asks the state, and a consumer that has the facts asks the function.

package core

import (
	"encoding/json"
	"fmt"
	"time"
)

// ActivityFacts is every fact that bears on whether a session is working.
//
// Built by the worker from its relay, and by the daemon from the operational
// state a worker sent. Counts and timestamps rather than whole collections,
// so building it on a render path is cheap and so this module does not have
// to know the shape of anything it does not decide with.
//
// The zero value is a session with nothing happening, except that Execution
// must be set: RelayExecutionState deliberately has no default of its own —
// defaulting a wire enum to `Idle` is the mistake that made a running turn
// report itself as finished — so use DefaultActivityFacts.
type ActivityFacts struct {
	// Execution is the durable execution flag folded from the event journal.
	Execution RelayExecutionState `json:"execution"`
	// PromptStartedAtMs is when the prompt in flight started, while one is in flight.
	PromptStartedAtMs *int64 `json:"prompt_started_at_ms"`
	// HarnessTurnStartedAtMs is when the turn the harness started on its own
	// began, while it is open.
	HarnessTurnStartedAtMs *int64 `json:"harness_turn_started_at_ms"`
	// TurnStartedAtMs is the start of the latest turn, retained until its
	// background work settles.
	TurnStartedAtMs *int64 `json:"turn_started_at_ms"`
	// QueuedCommands is the commands accepted and waiting behind the one in flight.
	QueuedCommands int `json:"queued_commands"`
	// ToolsInFlight is the tool calls the agent has open, oldest first.
	ToolsInFlight []InFlightToolCall `json:"tools_in_flight"`
	// BackgroundStartedAtMs is the oldest start among background commands
	// and user shells.
	BackgroundStartedAtMs *int64 `json:"background_started_at_ms"`
	BackgroundCommands    int    `json:"background_commands"`
	ExpectedContinuation  *int64 `json:"expected_continuation"`
	// InferredIdleSinceMs is a generation-checked Jev inference; does not
	// discharge owned work.
	InferredIdleSinceMs  *int64 `json:"inferred_idle_since_ms"`
	ActiveUserShells     int    `json:"active_user_shells"`
	ActiveAgentTerminals int    `json:"active_agent_terminals"`
	GoalActive           bool   `json:"goal_active"`
	GoalRunning          bool   `json:"goal_running"`
	GoalPendingResume    bool   `json:"goal_pending_resume"`
	GoalDecision         bool   `json:"goal_decision"`
	GoalSynchronized     bool   `json:"goal_synchronized"`
	// BackgroundWorkKnown says whether provider-owned background work is
	// known. Only Kimi reports it.
	BackgroundWorkKnown *bool `json:"background_work_known"`
	// AcpReady says whether the worker has finished opening its ACP session.
	// A worker too old to report it leaves this empty and is treated as ready.
	AcpReady *bool `json:"acp_ready"`
	// CheckpointOnly: this process serves recovered state without running a harness.
	CheckpointOnly bool `json:"checkpoint_only"`
	// CheckpointBarrier: a checkpoint barrier is waiting to capture.
	CheckpointBarrier bool `json:"checkpoint_barrier"`
	// CapacityRetryArmed: the worker has armed a capacity retry: it will
	// submit a prompt of its own when the wait is up, so the session is not
	// finished with its work.
	CapacityRetryArmed bool `json:"capacity_retry_armed"`
	// LastAcpActivityAtMs is when anything at all last arrived over ACP.
	LastAcpActivityAtMs *int64 `json:"last_acp_activity_at_ms"`
	// CurrentStepStartedAtMs is when the step the agent is on began, while a
	// step is in flight.
	CurrentStepStartedAtMs *int64 `json:"current_step_started_at_ms"`
	// IdleSinceMs is the start of the current observed idle period, when the
	// relay knows it.
	IdleSinceMs *int64 `json:"idle_since_ms"`
	// TaskSettledAtMs is when a background task the harness will follow up on
	// last settled, while no turn has opened since. Claude Code answers every
	// settled background command with a task-notification turn of its own;
	// for ImminentTurnWindowMs after the settle the session is treated as
	// having a turn in flight, because the process facts lag the queue by a
	// second or so and that second is when a worker upgrade killed a turn.
	TaskSettledAtMs *int64 `json:"task_settled_at_ms,omitempty"`
	// BackgroundNeeded is Jev's confident judgment of whether anyone still
	// depends on the background commands and agent terminals the session
	// holds. false lets a `sleep infinity` stop counting as work; nil and
	// true leave today's rule in force. Process-local, never restored.
	BackgroundNeeded *bool `json:"background_needed,omitempty"`
}

// DefaultActivityFacts is a session with nothing happening. The only place
// the Idle execution default is chosen, for tests and for building facts
// field by field.
func DefaultActivityFacts() ActivityFacts {
	return ActivityFacts{Execution: RelayExecutionStateIdle, ToolsInFlight: []InFlightToolCall{}}
}

// ImminentTurnWindowMs is how long after a background task settles the
// session counts as having a turn about to start. The observed gap between
// the settle and the harness turn is one to three seconds; the window is
// generous because a missed turn costs a person a restart and a bounded wait
// costs nothing.
const ImminentTurnWindowMs int64 = 60_000

// TurnImminent reports whether a settled background task means a harness
// turn is about to open.
func TurnImminent(facts *ActivityFacts, nowMs int64) bool {
	if facts.PromptStartedAtMs != nil || facts.HarnessTurnStartedAtMs != nil || facts.TaskSettledAtMs == nil {
		return false
	}
	age := activitySaturatingSub(nowMs, *facts.TaskSettledAtMs)
	return age >= 0 && age <= ImminentTurnWindowMs
}

// leftoverProcessesCount reports whether the background commands and agent
// terminals still count as work. A user's own shell always counts; Jev is
// never asked about it.
func leftoverProcessesCount(facts *ActivityFacts) bool {
	notUnneeded := facts.BackgroundNeeded == nil || *facts.BackgroundNeeded
	return notUnneeded && (facts.BackgroundCommands > 0 || facts.ActiveAgentTerminals > 0)
}

// ActivityStateKind names an ActivityState variant; the values are the wire
// tags.
type ActivityStateKind string

const (
	// ActivityStateUnknown: nobody can currently see the worker, so this is
	// the last thing that was known about it. Never idle: a turn that
	// outlives a daemon restart must not be reported as finished.
	ActivityStateUnknown ActivityStateKind = "unknown"
	ActivityStateClosed  ActivityStateKind = "closed"
	ActivityStateClosing ActivityStateKind = "closing"
	// ActivityStateTurn: a turn is in flight: a prompt Mjolnir sent, or one
	// the harness started.
	ActivityStateTurn ActivityStateKind = "turn"
	// ActivityStateTool: no turn boundary is open, but a tool call is
	// running. Harnesses that mark no turn of their own are visible only
	// this way.
	ActivityStateTool ActivityStateKind = "tool"
	// ActivityStateBackground: only work the agent left running behind it.
	ActivityStateBackground ActivityStateKind = "background"
	// ActivityStateExpecting: the completed reply says unseen background
	// work will resume the agent. This inference owns no work and never
	// blocks worker replacement.
	ActivityStateExpecting ActivityStateKind = "expecting"
	// ActivityStateGoal: a native goal is active; durable intent alone is not
	// work in flight.
	ActivityStateGoal ActivityStateKind = "goal"
	// ActivityStateCheckingQuota: the daemon is checking or waiting on a
	// provider quota recovery. The wire value predates the rename and stays
	// for older readers and writers.
	ActivityStateCheckingQuota ActivityStateKind = "checking-continuation"
	// ActivityStateRetry: the worker is waiting to resubmit a prompt the
	// provider refused for capacity. Nothing is computing, but the session
	// is not finished.
	ActivityStateRetry ActivityStateKind = "retry"
	ActivityStateIdle  ActivityStateKind = "idle"
	// ActivityStateUnrecognized is a state this build does not know. See the
	// type documentation.
	ActivityStateUnrecognized ActivityStateKind = "unrecognized"
)

// ActivityState is what a session is doing, at the granularity every part of
// Mjolnir agrees on.
//
// Serialized internally tagged by "state" so it can be published inside the
// operational state a worker sends. ActivityStateUnrecognized is the
// forward-compatibility landing place: a reader too old to know a variant a
// newer worker sends deserializes it here rather than failing, which would
// drop the whole snapshot. It is deliberately the cautious answer — never
// idle, always work in flight — because the alternative is presenting a
// working session as finished, which is the failure this module exists to
// prevent.
//
// One struct with a kind: each field belongs to the variants named on it.
// The zero value is not valid; the Rust default is Idle with no since.
type ActivityState struct {
	Kind ActivityStateKind
	// LastKnown: Unknown (required).
	LastKnown *ActivityState
	// SinceMs: Unknown, Idle (optional); Expecting (required).
	SinceMs *int64
	// StartedAtMs: Turn, Background (optional); Tool (required).
	StartedAtMs *int64
	// LastActivityAtMs: Turn, Tool. When anything last arrived from the
	// harness over ACP. A reader subtracts it from the current time to get
	// the silence age, which is the only evidence there is that a running
	// turn may have stopped making progress. Silence during a tool call is
	// ordinary: a long build sends no protocol traffic at all. Older workers
	// do not report it.
	LastActivityAtMs *int64
	// ToolCallID: Tool (required).
	ToolCallID string
}

// DefaultActivityState is Idle with no known start.
func DefaultActivityState() ActivityState {
	return ActivityState{Kind: ActivityStateIdle}
}

func (s ActivityState) MarshalJSON() ([]byte, error) {
	switch s.Kind {
	case ActivityStateUnknown:
		if s.LastKnown == nil {
			return nil, fmt.Errorf("unknown activity state has no last known state")
		}
		return agentTaggedJSON("state", string(s.Kind), struct {
			LastKnown *ActivityState `json:"last_known"`
			SinceMs   *int64         `json:"since_ms,omitempty"`
		}{s.LastKnown, s.SinceMs})
	case ActivityStateTurn:
		return agentTaggedJSON("state", string(s.Kind), struct {
			StartedAtMs      *int64 `json:"started_at_ms,omitempty"`
			LastActivityAtMs *int64 `json:"last_activity_at_ms,omitempty"`
		}{s.StartedAtMs, s.LastActivityAtMs})
	case ActivityStateTool:
		if s.StartedAtMs == nil {
			return nil, fmt.Errorf("tool activity state has no start")
		}
		return agentTaggedJSON("state", string(s.Kind), struct {
			ToolCallID       string `json:"tool_call_id"`
			StartedAtMs      int64  `json:"started_at_ms"`
			LastActivityAtMs *int64 `json:"last_activity_at_ms,omitempty"`
		}{s.ToolCallID, *s.StartedAtMs, s.LastActivityAtMs})
	case ActivityStateBackground:
		return agentTaggedJSON("state", string(s.Kind), struct {
			StartedAtMs *int64 `json:"started_at_ms,omitempty"`
		}{s.StartedAtMs})
	case ActivityStateExpecting:
		if s.SinceMs == nil {
			return nil, fmt.Errorf("expecting activity state has no since")
		}
		return agentTaggedJSON("state", string(s.Kind), struct {
			SinceMs int64 `json:"since_ms"`
		}{*s.SinceMs})
	case ActivityStateIdle:
		return agentTaggedJSON("state", string(s.Kind), struct {
			SinceMs *int64 `json:"since_ms,omitempty"`
		}{s.SinceMs})
	case ActivityStateClosed, ActivityStateClosing, ActivityStateGoal,
		ActivityStateCheckingQuota, ActivityStateRetry, ActivityStateUnrecognized:
		return agentTaggedJSON("state", string(s.Kind), nil)
	}
	return nil, fmt.Errorf("unknown activity state kind %q", s.Kind)
}

func (s *ActivityState) UnmarshalJSON(data []byte) error {
	tag, err := agentTagOf(data, "state")
	if err != nil {
		return err
	}
	var fields struct {
		LastKnown        *ActivityState `json:"last_known"`
		SinceMs          *int64         `json:"since_ms"`
		StartedAtMs      *int64         `json:"started_at_ms"`
		LastActivityAtMs *int64         `json:"last_activity_at_ms"`
		ToolCallID       *string        `json:"tool_call_id"`
	}
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	kind := ActivityStateKind(tag)
	missing := func(name string) error { return fmt.Errorf("missing field `%s`", name) }
	switch kind {
	case ActivityStateUnknown:
		if fields.LastKnown == nil {
			return missing("last_known")
		}
		*s = ActivityState{Kind: kind, LastKnown: fields.LastKnown, SinceMs: fields.SinceMs}
	case ActivityStateTurn:
		*s = ActivityState{Kind: kind, StartedAtMs: fields.StartedAtMs, LastActivityAtMs: fields.LastActivityAtMs}
	case ActivityStateTool:
		if fields.ToolCallID == nil {
			return missing("tool_call_id")
		}
		if fields.StartedAtMs == nil {
			return missing("started_at_ms")
		}
		*s = ActivityState{Kind: kind, ToolCallID: *fields.ToolCallID, StartedAtMs: fields.StartedAtMs, LastActivityAtMs: fields.LastActivityAtMs}
	case ActivityStateBackground:
		*s = ActivityState{Kind: kind, StartedAtMs: fields.StartedAtMs}
	case ActivityStateExpecting:
		if fields.SinceMs == nil {
			return missing("since_ms")
		}
		*s = ActivityState{Kind: kind, SinceMs: fields.SinceMs}
	case ActivityStateIdle:
		*s = ActivityState{Kind: kind, SinceMs: fields.SinceMs}
	case ActivityStateClosed, ActivityStateClosing, ActivityStateGoal,
		ActivityStateCheckingQuota, ActivityStateRetry:
		*s = ActivityState{Kind: kind}
	default:
		*s = ActivityState{Kind: ActivityStateUnrecognized}
	}
	return nil
}

// IsIdle: the agent is idle, either observed or inferred after a completed
// reply. Recorded background tasks may remain; lifecycle callers must use
// the facts.
//
// An unknown or unrecognized state is never idle. Missing knowledge must not
// be presented as confirmed idleness.
func (s *ActivityState) IsIdle() bool {
	return s.Kind == ActivityStateIdle
}

// IsWorking: the agent is computing right now, as opposed to holding work
// that is merely not finished.
func (s *ActivityState) IsWorking() bool {
	switch s.Kind {
	case ActivityStateTurn, ActivityStateTool, ActivityStateBackground, ActivityStateClosing:
		return true
	case ActivityStateUnknown:
		return s.LastKnown.IsWorking()
	}
	return false
}

// HasWorkInFlight reports whether this state alone means the session still
// owns work.
//
// Callers that hold the facts should ask the HasWorkInFlight function
// instead, which also covers queued commands, open terminals and
// unsynchronized provider state. This is for a caller that has only a state.
//
// Unknown always counts, whatever was last known. A session nobody can see
// may have started a turn since, so replacing its worker or taking a
// checkpoint cut against it could destroy work. This does not block
// recovering a worker that is truly gone: that session reports Closed, not
// Unknown, because WhileDisconnected keeps the two apart.
func (s *ActivityState) HasWorkInFlight() bool {
	switch s.Kind {
	case ActivityStateExpecting, ActivityStateIdle, ActivityStateGoal, ActivityStateClosed:
		return false
	}
	return true
}

// ChatPhase is the four-valued phase the viewer and the API have always
// reported.
//
// An unknown state answers with what was last known, which is the whole
// point: a turn that spans a daemon restart keeps reporting Running instead
// of falling back to the default value of an enum.
func (s *ActivityState) ChatPhase() RelayExecutionState {
	switch s.Kind {
	case ActivityStateClosed:
		return RelayExecutionStateClosed
	case ActivityStateClosing:
		return RelayExecutionStateClosing
	case ActivityStateTurn, ActivityStateTool, ActivityStateUnrecognized:
		return RelayExecutionStateRunning
	case ActivityStateUnknown:
		return s.LastKnown.ChatPhase()
	}
	return RelayExecutionStateIdle
}

// LastKnownState is the last state actually observed, looking through
// Unknown. (Rust: `last_known`; the name is taken by the field.)
func (s *ActivityState) LastKnownState() *ActivityState {
	if s.Kind == ActivityStateUnknown {
		return s.LastKnown.LastKnownState()
	}
	return s
}

// LastActivityAt is when the harness last sent anything, while work is in
// flight. (Rust: `last_activity_at_ms`; the name is taken by the field.)
//
// Only a Turn or a Tool carries this: silence means nothing about a session
// that is not running anything. An Unknown state answers with what was last
// known, so a session the daemon cannot see still reports the last activity
// it saw rather than pretending there was none.
func (s *ActivityState) LastActivityAt() *int64 {
	switch s.Kind {
	case ActivityStateTurn, ActivityStateTool:
		return s.LastActivityAtMs
	case ActivityStateUnknown:
		return s.LastKnown.LastActivityAt()
	}
	return nil
}

// SilentForMs is how long the harness has been silent while work is in
// flight.
//
// This is a fact, not a verdict. Mjolnir does not decide from it that a turn
// is dead — silence during a long build is ordinary, and guessing wrong
// destroys real work (#1020). It is published so a person or an orchestrator
// can see "running, nothing for eleven minutes" and choose what to do, and so
// the opt-in watchdog has one place to read it from.
//
// false when nothing is running, when the worker is too old to report the
// timestamp, or when the timestamp is in the future.
func (s *ActivityState) SilentForMs(nowMs int64) (uint64, bool) {
	last := s.LastActivityAt()
	if last == nil || nowMs < *last {
		return 0, false
	}
	return uint64(nowMs - *last), true
}

// SilentForMs is how long the harness has been silent, from the facts rather
// than a state.
//
// The same answer as ActivityState.SilentForMs, for a caller that holds
// ActivityFacts. There is one rule and it lives here: silence is only
// meaningful while something is in flight.
func SilentForMs(facts *ActivityFacts, nowMs int64) (uint64, bool) {
	state := Classify(facts)
	return state.SilentForMs(nowMs)
}

// DescribeDuration says how long something took, in the coarsest unit that
// still tells the truth.
//
// A bound shortened for a test trips in seconds, and reporting that as
// "about 1 minute" makes the message read like a bug in itself.
func DescribeDuration(millis uint64) string {
	seconds := millis / 1_000
	if seconds < 90 {
		return Counted(seconds, "second", "seconds")
	}
	return "about " + Counted(seconds/60, "minute", "minutes")
}

// SilenceWorthReporting: below this, silence is ordinary and saying so is
// noise rather than news.
//
// Every surface uses the same threshold so a turn does not read as quiet in
// one place and ordinary in another.
const SilenceWorthReporting = 60 * time.Second

// SilenceNote is the one phrase every surface uses for a running turn that
// has gone quiet.
//
// false when nothing is running, when the worker is too old to report the
// ACP clock, or when the session has been quiet for less than
// SilenceWorthReporting. This is a report, not a verdict: a turn waiting on
// a long build is silent and perfectly healthy.
func SilenceNote(state *ActivityState, nowMs int64) (string, bool) {
	silentMs, ok := state.SilentForMs(nowMs)
	if !ok || silentMs < uint64(SilenceWorthReporting.Milliseconds()) {
		return "", false
	}
	return "no harness activity for " + DescribeDuration(silentMs), true
}

// Classify says what a session is doing, from everything known about it.
//
// Pure: the same facts always give the same state, with no clock and no I/O,
// so the transitions can be driven directly from a test.
func Classify(facts *ActivityFacts) ActivityState {
	panic("unported: mj-core/src/activity.rs classify")
}

// HasWorkInFlight reports whether anything the worker owns would be
// destroyed by killing it now.
//
// This is the superset: on top of whatever the session is doing, it counts
// the work that is accepted but not started (queued commands), the terminals
// and shells the agent opened, a goal decision waiting to be applied, and
// provider state that is not known to be settled. The durable execution flag
// alone is not enough — a stale projection can report Idle while a turn the
// harness started, or a tool it is still running, is live — which is why
// this reads the classified state rather than that flag.
func HasWorkInFlight(facts *ActivityFacts) bool {
	return QuietAt(facts, EpochMillis()).WorkInFlight()
}

// HasWorkInFlightAt is HasWorkInFlight at a given time, for callers and
// tests that hold a clock.
func HasWorkInFlightAt(facts *ActivityFacts, nowMs int64) bool {
	return QuietAt(facts, nowMs).WorkInFlight()
}

// QuietKind names a Quiet variant.
type QuietKind int

const (
	QuietYes QuietKind = iota
	// QuietBarrier: nothing is running, but a checkpoint barrier is held;
	// stopping the worker would lose nothing, replacing it must still wait.
	QuietBarrier
	QuietNo
)

// Quiet says whether the worker may be stopped without losing anything, or
// why not.
//
// The three layers, in order: facts about turns that no judgment may
// override; Jev's judgment about leftover processes, which can only make a
// session quieter; and the process-fact rule that applies when Jev has said
// nothing. See `.agents/docs/jev.md`, "Quiet".
type Quiet struct {
	Kind QuietKind
	// NoReason is the reason for QuietNo.
	NoReason string
}

func (q Quiet) IsYes() bool {
	return q.Kind == QuietYes
}

func (q Quiet) WorkInFlight() bool {
	return q.Kind == QuietNo
}

// Reason is the reason, for a log line.
func (q Quiet) Reason() string {
	switch q.Kind {
	case QuietYes:
		return "quiet"
	case QuietBarrier:
		return "checkpoint barrier held"
	}
	return q.NoReason
}

func QuietAt(facts *ActivityFacts, nowMs int64) Quiet {
	panic("unported: mj-core/src/activity.rs quiet_at")
}

// CanSubmit reports whether a prompt could start now: no turn, tool, queued
// command, user shell, checkpoint barrier or goal question in the way.
//
// Background commands and agents do not block it; whether they will move the
// session on is DriverPresent's question.
func CanSubmit(facts *ActivityFacts) bool {
	return len(SubmitBlockers(facts)) == 0
}

// SubmitBlockers says what keeps CanSubmit false, one name per fact that is
// set, so a caller that declines to submit can say why. Empty exactly when a
// prompt could start.
func SubmitBlockers(facts *ActivityFacts) []string {
	panic("unported: mj-core/src/activity.rs submit_blockers")
}

// DriverPresent reports whether something other than Mjolnir will move the
// session on: a turn, an active goal, an armed retry, or background work
// that Jev has not judged idle. A `sleep infinity` counts only until that
// judgment, unlike in HasWorkInFlight, which asks whether killing the worker
// loses work.
func DriverPresent(facts *ActivityFacts) bool {
	state := Classify(facts)
	return state.Kind == ActivityStateGoal ||
		state.HasWorkInFlight() ||
		TurnImminent(facts, EpochMillis())
}

// IsQuiet reports whether nothing at all is happening, including no
// checkpoint barrier.
//
// A held barrier is the one reason this refuses that is not itself work: a
// caller already holding a barrier asks HasWorkInFlight to learn whether
// anything *else* is running.
func IsQuiet(facts *ActivityFacts) bool {
	return QuietAt(facts, EpochMillis()).IsYes()
}

// SafeToReplace reports whether a controller may replace this worker without
// losing work.
//
// Older Codex and Kimi workers cannot prove that native goals or background
// agents are absent, even when their snapshots look quiet.
func SafeToReplace(facts *ActivityFacts, harness HarnessKind) bool {
	return IsQuiet(facts) &&
		(harness != HarnessKindCodex || facts.GoalSynchronized) &&
		(harness != HarnessKindKimi || (facts.BackgroundWorkKnown != nil && *facts.BackgroundWorkKnown))
}

// MovePurpose is why a Move is being admitted. Checkpoint callers use this to
// select noninterrupting quota recovery.
type MovePurpose int

const (
	MovePurposeManual MovePurpose = iota
	MovePurposeQuotaRetry
)

// SafeToMove is the shared interruption decision for Move preparation and
// source-lease validation.
func SafeToMove(facts *ActivityFacts, harness HarnessKind, _ MovePurpose) bool {
	return SafeToReplace(facts, harness)
}

// ReplacementBlockers says what keeps SafeToReplace false, one name per
// cause, so a caller that declines to replace a worker can say why. Empty
// exactly when SafeToReplace is true.
func ReplacementBlockers(facts *ActivityFacts, harness HarnessKind) []string {
	panic("unported: mj-core/src/activity.rs replacement_blockers")
}

// CheckpointBlocker says why provider-owned work rules out a routine
// checkpoint cut, or false.
//
// This asks only about provider-owned work, which is a different question
// from whether a turn is running: a checkpoint that finds a turn running can
// try again in a minute, but a checkpoint taken while a native goal or a
// background agent owns the session captures a state that does not exist.
// Unknown native state fails closed.
//
// Whether a routine checkpoint may start is RoutineCheckpointWait, which
// includes this. This alone is for a checkpoint that already holds its
// barrier: the barrier accepts and queues new work, so work in flight no
// longer says anything about the cut, but provider-owned work still does.
func CheckpointBlocker(facts *ActivityFacts, harness HarnessKind) (string, bool) {
	panic("unported: mj-core/src/activity.rs checkpoint_blocker")
}

// CheckpointWait is why a routine checkpoint has to wait.
type CheckpointWait interface {
	isCheckpointWait()
}

// CheckpointWaitProviderWork is provider-owned work that a checkpoint cut
// would capture half done. The text is CheckpointBlocker's reason.
type CheckpointWaitProviderWork struct {
	Reason string
}

// CheckpointWaitWorkInFlight: the session is working: a turn, a tool, a
// queued command, a background command, a shell or a terminal. See
// HasWorkInFlight.
type CheckpointWaitWorkInFlight struct{}

func (CheckpointWaitProviderWork) isCheckpointWait() {}
func (CheckpointWaitWorkInFlight) isCheckpointWait() {}

// RoutineCheckpointWait reports whether a routine checkpoint may start now
// (nil), or why it has to wait.
//
// A routine checkpoint is one that can run again later: a recovery copy, or
// a checkpoint someone asked for while the session runs. Close is not one;
// it may interrupt the work instead.
//
// This is the one answer. The recovery coordinator asks it before it starts
// a copy, and the checkpoint asks it again before and while it opens its
// barrier, because the session can start working in between. When the two
// asked different questions, the coordinator started copies that the barrier
// then deferred, once a second for as long as the disagreement lasted.
func RoutineCheckpointWait(facts *ActivityFacts, harness HarnessKind) CheckpointWait {
	if reason, blocked := CheckpointBlocker(facts, harness); blocked {
		return CheckpointWaitProviderWork{Reason: reason}
	}
	if HasWorkInFlight(facts) {
		return CheckpointWaitWorkInFlight{}
	}
	return nil
}

// ChatPhase is the four-valued phase to report for a session the daemon can
// see.
//
// The execution flag is the phase, with one correction: a live turn the flag
// has not caught up with is still running. ActivityState.ChatPhase is the
// same question for a session nobody can see, where there is no flag to
// read.
func ChatPhase(facts *ActivityFacts) RelayExecutionState {
	switch facts.Execution {
	case RelayExecutionStateClosing, RelayExecutionStateClosed:
		return facts.Execution
	}
	if kind := Classify(facts).Kind; kind == ActivityStateTurn || kind == ActivityStateTool {
		return RelayExecutionStateRunning
	}
	return facts.Execution
}

// WhileDisconnected says what to report about a session nobody can currently
// see.
//
// The daemon calls this while it has no live connection to a worker: after
// its own restart, during a reattach, or while the target is unreachable.
// The worker itself is unaffected by any of that, so the honest answer is the
// last state the durable projection recorded, marked unknown — never Idle,
// which is what a default value used to produce and what made automation
// treat a running turn as finished.
func WhileDisconnected(durable MaterializedExecutionState, sinceMs *int64) ActivityState {
	panic("unported: mj-core/src/activity.rs while_disconnected")
}

// StallPolicy is how long a turn may go without a sign of life before the
// worker fails it.
//
// Two bounds, because silence means two different things. With nothing in
// flight, silence is a bridge that stopped relaying and the turn is lost
// after Silence. With a tool call open, silence is normal — a twenty-minute
// build produces no protocol traffic at all — so only the much longer
// ToolCall bound applies, and it exists solely to catch a bridge that died
// leaving a tool card open.
type StallPolicy struct {
	// Silence with nothing in flight. nil disables the watchdog.
	Silence *time.Duration
	// ToolCall is how long one tool call may run. nil means no bound.
	ToolCall *time.Duration
}

// StallPolicyCheckFloor and StallPolicyCheckCeiling are the shortest and
// longest a watchdog waits between two checks.
const StallPolicyCheckFloor = 50 * time.Millisecond
const StallPolicyCheckCeiling = time.Second

// Enabled reports whether either bound can ever trip.
func (p StallPolicy) Enabled() bool {
	return p.Silence != nil || p.ToolCall != nil
}

// NextCheck is how long to wait before asking again, given what is in
// flight now.
//
// Never zero, because a caller sleeps on this in a loop and a zero wait
// would spin. Never longer than StallPolicyCheckCeiling either: what is in
// flight changes while the watchdog waits — a tool call opens or ends — and
// a watchdog that had slept until the bound it computed from the old facts
// would miss the change entirely. Waking once a second and asking again
// costs nothing and is the only way the answer stays true to what is
// happening.
func (p StallPolicy) NextCheck(facts *ActivityFacts, nowMs int64) time.Duration {
	panic("unported: mj-core/src/activity.rs StallPolicy::next_check")
}

// StallVerdict is whether a turn has stopped responding, and why.
type StallVerdict interface {
	isStallVerdict()
}

// StallVerdictLive: still alive: something arrived recently, or a tool call
// is running within its bound.
type StallVerdictLive struct{}

// StallVerdictSilent: nothing arrived and nothing is in flight.
type StallVerdictSilent struct {
	SilentMs uint64
}

// StallVerdictToolCall: one tool call outlived the bound on a single tool call.
type StallVerdictToolCall struct {
	ToolCallID string
	RunningMs  uint64
	SilentMs   uint64
}

func (StallVerdictLive) isStallVerdict()     {}
func (StallVerdictSilent) isStallVerdict()   {}
func (StallVerdictToolCall) isStallVerdict() {}

// StallVerdictOf reports whether a running turn has stopped responding.
// (Rust: `stall_verdict`.)
//
// The rule that fixes the long-tool-call failure is one line of it: when a
// tool call is open, the silence bound does not apply at all. A harness
// blocked in a twenty-minute `cargo nextest` run sends no protocol traffic
// and used to be failed for it, even though Mjolnir could see the tool call
// it was blocked in the whole time.
func StallVerdictOf(facts *ActivityFacts, policy StallPolicy, nowMs int64) StallVerdict {
	panic("unported: mj-core/src/activity.rs stall_verdict")
}

func elapsed(sinceMs, nowMs int64) time.Duration {
	return time.Duration(max(activitySaturatingSub(nowMs, sinceMs), 0)) * time.Millisecond
}

// activitySaturatingSub is Rust's i64::saturating_sub.
func activitySaturatingSub(a, b int64) int64 {
	difference := a - b
	if (b > 0 && difference > a) || (b < 0 && difference < a) {
		if b > 0 {
			return -1 << 63
		}
		return 1<<63 - 1
	}
	return difference
}

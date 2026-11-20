// Port of mj-client/src/usage_format.rs.

// Shared formatting for live-session, provider quota, and rate-limit displays.

package client

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// FormatCPUPermille is a machine CPU share: tenths are kept below ten
// percent.
func FormatCPUPermille(permille uint16) string {
	if permille < 100 {
		return fmt.Sprintf("%d.%01d%%", permille/10, permille%10)
	}
	return fmt.Sprintf("%d%%", (uint32(permille)+5)/10)
}

// FormatClock renders a running clock: the two largest units that still fit,
// so a glance reads the magnitude rather than counting colons.
//
// `36s`, `43m36s`, `1h43m`, `2d03h`. Every live clock in Mjolnir uses this,
// which is why it lives here rather than beside any one of them.
func FormatClock(seconds uint64) string {
	const minute = 60
	const hour = 60 * minute
	const day = 24 * hour
	switch {
	case seconds < minute:
		return fmt.Sprintf("%ds", seconds)
	case seconds < hour:
		return fmt.Sprintf("%dm%02ds", seconds/minute, seconds%minute)
	case seconds < day:
		return fmt.Sprintf("%dh%02dm", seconds/hour, (seconds%hour)/minute)
	default:
		return fmt.Sprintf("%dd%02dh", seconds/day, (seconds%day)/hour)
	}
}

// FormatTurnClock renders a session's current-turn clock. A session with no
// turn in flight reads `[idle]` rather than showing an empty cell.
func FormatTurnClock(nowEpochSeconds uint64, currentTurnStartedAt *uint64) string {
	if currentTurnStartedAt == nil {
		return "[idle]"
	}
	return FormatClock(usageSaturatingSub(nowEpochSeconds, *currentTurnStartedAt))
}

// usageSaturatingSub is Rust's u64::saturating_sub.
func usageSaturatingSub(a, b uint64) uint64 {
	if a < b {
		return 0
	}
	return a - b
}

// SessionActivity is what a session is doing right now, beyond whether a
// turn is running.
//
// The dashboard rows, the chat pane title and the phone all render the same
// activity facts from this, so they agree on what "idle" means.
type SessionActivity struct {
	// DismissQuestionSupported is whether the worker can durably dismiss an
	// inferred question.
	DismissQuestionSupported bool
	PursuingGoal             bool
	CheckingResponse         bool
	CapacityRetry            *core.CapacityRetry
	QuotaRecovery            *core.QuotaRecovery
	// ActivityTurnStartedAtMs is the durable turn start retained through the
	// background work it launched.
	ActivityTurnStartedAtMs *int64
	PromptInFlight          bool
	// Execution is the relay execution state, when this activity came from
	// an operational snapshot. Materialized-only callers leave it unset.
	Execution *core.RelayExecutionState
	// IdleSinceMs is the start of the current observed idle period, when the
	// relay knows it. Older workers leave this unset even when they report
	// an idle state.
	IdleSinceMs *int64
	// HarnessTurnStartedAtMs is when the turn the harness started on its own
	// began, in epoch milliseconds, while that turn is open. A session with
	// one open is working even if the projection has not caught up yet.
	HarnessTurnStartedAtMs *int64
	// ForegroundToolStartedAtMs is the start of foreground tool work, or a
	// current SDK step while execution is Running. This also covers
	// harnesses without autonomous-turn markers.
	ForegroundToolStartedAtMs *int64
	// ReportedState is what the session is doing, as the worker itself
	// reported it (Rust's `state` field; State() is the answer). This type
	// renders activity; it no longer decides it. When the answer did not
	// come with the snapshot — an older worker, or a row assembled by hand —
	// the same core activity classifier answers from the facts below, so
	// there is still only one decision in the system.
	ReportedState *core.ActivityState
	// BackgroundCommands are commands the agent left running with nothing
	// waiting on them.
	BackgroundCommands []core.BackgroundCommand
	// ActiveUserShells are user shells still owned by the relay. These are
	// separate from agent background commands, but still make a session
	// non-idle.
	ActiveUserShells []core.ActiveUserShell
}

// SessionActivityOf reads the activity out of what a session's relay last
// reported.
func SessionActivityOf(operational *core.RelayOperationalState) SessionActivity {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::of")
}

// SessionActivityOfState is the activity a bare state describes, with only
// the clocks the state itself carries. For a caller that holds the worker's
// state and nothing else, such as a wait's answer.
func SessionActivityOfState(state core.ActivityState) SessionActivity {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::of_state")
}

// IsIdle is whether the session has no foreground or background work in
// flight.
//
// Timestamps are deliberately tested for presence rather than validity: an
// invalid timestamp still proves that work exists, and must not make a row
// claim that the session is idle. The optional projected turn start is kept
// separate because it is supplied by the materialized session rather than
// the relay's operational snapshot.
func (a *SessionActivity) IsIdle(currentTurnStartedAt *uint64) bool {
	if currentTurnStartedAt != nil {
		return false
	}
	state := a.State()
	return state.IsIdle()
}

// IsWorking: waiting for an answer and queued work are not computation.
// Background commands can still run independently while the foreground asks
// a question.
func (a *SessionActivity) IsWorking(currentTurnStartedAt *uint64, waitingForInput bool) bool {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::is_working")
}

// DisplayClock: compact and detailed clocks use the same evidence as
// activity indicators.
func (a *SessionActivity) DisplayClock(nowEpochSeconds uint64, currentTurnStartedAt *uint64, currentStepStartedAtMs *uint64, detailed bool) string {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::display_clock")
}

// Details classifies the current activity and retains the timestamps that
// support that classification. This is the structured counterpart to the
// text clocks below, so web and terminal surfaces use the same precedence
// and never have to parse a rendered activity string.
func (a *SessionActivity) Details(currentTurnStartedAtMs, currentStepStartedAtMs *int64) SessionActivityDetails {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::details")
}

// quietSince is when the current quiet period began, in epoch seconds, once
// it has lasted long enough to be worth reporting.
//
// Absent when nothing is running, when the worker is too old to report the
// ACP clock, or when the session has been quiet for less than
// core.SilenceWorthReporting. The threshold lives in core's activity module
// so every surface agrees on when a turn reads as quiet.
func (a *SessionActivity) quietSince(nowEpochSeconds uint64) (uint64, bool) {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::quiet_since")
}

// State is what this session is doing: the worker's own answer when the
// snapshot carried one, and otherwise the same classifier applied to the
// same facts.
func (a *SessionActivity) State() core.ActivityState {
	if a.ReportedState != nil {
		return *a.ReportedState
	}
	facts := a.facts()
	return core.Classify(&facts)
}

// facts is the facts this row holds, in the shape the shared classifier
// decides with.
func (a *SessionActivity) facts() core.ActivityFacts {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::facts")
}

// kind is the rendering vocabulary for what the session is doing.
//
// A mapping of the shared activity state. The one fact added here is the
// caller's own turn start, which comes from the materialized projection
// rather than from the relay: a turn the durable record knows about is a
// turn whatever the live state says.
func (a *SessionActivity) kind(currentTurnStartedAt *uint64) SessionActivityKind {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::kind")
}

func (a *SessionActivity) lifecycleLabel() string {
	if a.Execution != nil {
		switch *a.Execution {
		case core.RelayExecutionStateClosing:
			return "Closing"
		case core.RelayExecutionStateClosed:
			return "Closed"
		}
	}
	return "Lifecycle"
}

func (a *SessionActivity) harnessTurnSince() (uint64, bool) {
	if a.HarnessTurnStartedAtMs == nil {
		return 0, false
	}
	return epochSeconds(*a.HarnessTurnStartedAtMs)
}

func (a *SessionActivity) foregroundToolSince() (uint64, bool) {
	if a.ForegroundToolStartedAtMs == nil {
		return 0, false
	}
	return epochSeconds(*a.ForegroundToolStartedAtMs)
}

// backgroundSince is the epoch second the oldest background command started.
// Invalid command timestamps are ignored only for the clock; their presence
// still makes IsIdle false.
func (a *SessionActivity) backgroundSince() (uint64, bool) {
	panic("unported: mj-client/src/usage_format.rs SessionActivity::background_since")
}

// SessionActivityKind is how a row renders what a session is doing.
type SessionActivityKind int

const (
	SessionActivityKindExpecting SessionActivityKind = iota
	SessionActivityKindCheckingQuota
	SessionActivityKindGoal
	SessionActivityKindTurn
	SessionActivityKindStep
	SessionActivityKindBackground
	SessionActivityKindLifecycle
	SessionActivityKindIdle
)

// SessionActivityDetails is the structured activity facts shared by the
// terminal and web projections.
type SessionActivityDetails struct {
	Kind                  SessionActivityKind
	TurnStartedAtMs       *int64
	StepStartedAtMs       *int64
	BackgroundStartedAtMs *int64
	IdleSinceMs           *int64
	// LastActivityAtMs is when the harness last sent anything, while work is
	// in flight. See core.ActivityState.SilentForMs.
	LastActivityAtMs *int64
	Label            *string
}

func epochSeconds(timestampMs int64) (uint64, bool) {
	if timestampMs < 0 {
		return 0, false
	}
	return uint64(timestampMs) / 1000, true
}

func elapsedLabel(label string, nowEpochSeconds uint64, startedAt *uint64) string {
	if startedAt == nil {
		return label
	}
	return fmt.Sprintf("%s %s", label, FormatClock(usageSaturatingSub(nowEpochSeconds, *startedAt)))
}

// FormatActivityColumns is the clock columns a wide session row shows: the
// running turn and its current step, the background work the session left
// running, or `[idle]`.
//
// The step is the tool call, message or thought the agent is on now, which
// the worker times with core.StepClock. A worker too old to report one leaves
// the step reading as the whole turn rather than pretending to a precision
// it does not have.
func FormatActivityColumns(nowEpochSeconds uint64, currentTurnStartedAt *uint64, currentStepStartedAtMs *uint64, activity *SessionActivity) []string {
	panic("unported: mj-client/src/usage_format.rs format_activity_columns")
}

// FormatActivityClock is the single-cell form of FormatActivityColumns, for
// a narrow row.
func FormatActivityClock(nowEpochSeconds uint64, currentTurnStartedAt *uint64, activity *SessionActivity) string {
	panic("unported: mj-client/src/usage_format.rs format_activity_clock")
}

func quotaStatus(recovery *core.QuotaRecovery, nowSeconds uint64) string {
	if recovery.RetryAtMs == nil {
		return "Quota limit · reset time unknown"
	}
	deadline := max(*recovery.RetryAtMs, 0)
	return "Quota limit · resumes in " + FormatClock(usageSaturatingSub(uint64(deadline)/1000, nowSeconds))
}

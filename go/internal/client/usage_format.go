// Port of mj-client/src/usage_format.rs.

// Shared formatting for live-session, provider quota, and rate-limit displays.

package client

import (
	"fmt"
	"math"
	"slices"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"

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
	activity := SessionActivity{
		DismissQuestionSupported: operational.RelayProtocolVersion != nil &&
			*operational.RelayProtocolVersion >= core.RelayDismissQuestionProtocol,
		PursuingGoal: operational.Goal.Active(),
		// The control flag also holds waits open after a failed request.
		// Only pending assessments belong in the activity display.
		CheckingResponse: operational.RetryAssessmentPending &&
			(operational.Assessment == nil || operational.Assessment.Status == core.StatusPending),
		PromptInFlight:     operational.ActivePrompt != nil,
		IdleSinceMs:        cloneInt64(operational.IdleSinceMs),
		BackgroundCommands: slices.Clone(operational.BackgroundCommands),
		ActiveUserShells:   slices.Clone(operational.ActiveUserShells),
	}
	if operational.CapacityRetry != nil {
		retry := *operational.CapacityRetry
		activity.CapacityRetry = &retry
	}
	if recovery := operational.Continuation.QuotaRecovery; recovery != nil && !recovery.Submitted {
		copied := *recovery
		activity.QuotaRecovery = &copied
	}
	if operational.ActivePrompt != nil {
		started := operational.ActivePrompt.StartedAtMs
		activity.ActivityTurnStartedAtMs = &started
	} else {
		activity.ActivityTurnStartedAtMs = cloneInt64(operational.ActivityTurnStartedAtMs)
	}
	execution := operational.Execution
	activity.Execution = &execution
	if operational.HarnessTurn != nil {
		started := operational.HarnessTurn.StartedAtMs
		activity.HarnessTurnStartedAtMs = &started
	}
	activity.ForegroundToolStartedAtMs = cloneInt64(operational.ForegroundToolStartedAtMs)
	if activity.ForegroundToolStartedAtMs == nil && operational.Execution == core.RelayExecutionStateRunning &&
		operational.CurrentStepStartedAtMs != nil && *operational.CurrentStepStartedAtMs >= 0 {
		activity.ForegroundToolStartedAtMs = cloneInt64(operational.CurrentStepStartedAtMs)
	}
	state := operational.ActivityState()
	activity.ReportedState = &state
	return activity
}

func cloneInt64(value *int64) *int64 {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// SessionActivityOfState is the activity a bare state describes, with only
// the clocks the state itself carries. For a caller that holds the worker's
// state and nothing else, such as a wait's answer.
func SessionActivityOfState(state core.ActivityState) SessionActivity {
	lastKnown := state.LastKnownState()
	activity := SessionActivity{ReportedState: &state}
	switch lastKnown.Kind {
	case core.ActivityStateTurn:
		activity.HarnessTurnStartedAtMs = cloneInt64(lastKnown.StartedAtMs)
	case core.ActivityStateTool:
		activity.ForegroundToolStartedAtMs = cloneInt64(lastKnown.StartedAtMs)
	case core.ActivityStateClosing:
		execution := core.RelayExecutionStateClosing
		activity.Execution = &execution
	case core.ActivityStateClosed:
		execution := core.RelayExecutionStateClosed
		activity.Execution = &execution
	}
	return activity
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
	switch a.kind(currentTurnStartedAt) {
	case SessionActivityKindIdle, SessionActivityKindGoal, SessionActivityKindExpecting, SessionActivityKindCheckingQuota:
		return false
	case SessionActivityKindLifecycle:
		return a.Execution != nil && *a.Execution == core.RelayExecutionStateClosing
	case SessionActivityKindBackground:
		return true
	default:
		return !waitingForInput || len(a.BackgroundCommands) > 0 || len(a.ActiveUserShells) > 0
	}
}

// nowMsOf is Rust's `now.saturating_mul(1000).min(i64::MAX as u64) as i64`.
func nowMsOf(nowEpochSeconds uint64) int64 {
	if nowEpochSeconds > math.MaxInt64/1000 {
		return math.MaxInt64
	}
	return int64(nowEpochSeconds * 1000)
}

// optionalSeconds turns a (seconds, ok) pair into Rust's Option<u64>.
func optionalSeconds(seconds uint64, ok bool) *uint64 {
	if !ok {
		return nil
	}
	return &seconds
}

// DisplayClock: compact and detailed clocks use the same evidence as
// activity indicators.
func (a *SessionActivity) DisplayClock(nowEpochSeconds uint64, currentTurnStartedAt *uint64, currentStepStartedAtMs *uint64, detailed bool) string {
	if a.QuotaRecovery != nil {
		return quotaStatus(a.QuotaRecovery, nowEpochSeconds)
	}
	if a.CapacityRetry != nil {
		return a.CapacityRetry.Status(nowMsOf(nowEpochSeconds))
	}
	if a.CheckingResponse {
		return "Checking response"
	}
	kind := a.kind(currentTurnStartedAt)
	switch kind {
	case SessionActivityKindCheckingQuota:
		return "Checking quota"
	case SessionActivityKindExpecting:
		return "expecting the agent to continue"
	case SessionActivityKindGoal:
		return "Pursuing goal"
	case SessionActivityKindIdle:
		return "Idle"
	case SessionActivityKindLifecycle:
		return a.lifecycleLabel()
	}
	turn := currentTurnStartedAt
	if turn == nil {
		turn = optionalSeconds(a.harnessTurnSince())
	}
	if turn == nil && a.ActivityTurnStartedAtMs != nil {
		turn = optionalSeconds(epochSeconds(*a.ActivityTurnStartedAtMs))
	}
	if detailed {
		// How long the harness has been quiet, shown only once it is worth
		// saying. It is a fact next to the other clocks, not a warning:
		// Mjolnir does not end a turn for silence, so a reader seeing "Q
		// 11m" decides for themselves whether to keep waiting.
		quiet := ""
		if since, ok := a.quietSince(nowEpochSeconds); ok {
			quiet = " " + elapsedLabel("Q", nowEpochSeconds, &since)
		}
		switch kind {
		case SessionActivityKindTurn:
			var step *uint64
			if currentStepStartedAtMs != nil {
				seconds := *currentStepStartedAtMs / 1000
				step = &seconds
			} else if turn != nil {
				seconds := *turn
				step = &seconds
			}
			if step != nil && turn != nil {
				seconds := max(*step, *turn)
				step = &seconds
			}
			return fmt.Sprintf("%s %s%s", elapsedLabel("T", nowEpochSeconds, turn),
				elapsedLabel("S", nowEpochSeconds, step), quiet)
		case SessionActivityKindStep:
			return elapsedLabel("S", nowEpochSeconds, optionalSeconds(a.foregroundToolSince())) + quiet
		case SessionActivityKindBackground:
			return elapsedLabel("BG", nowEpochSeconds, optionalSeconds(a.backgroundSince()))
		}
		panic("unreachable: every other activity kind returned above")
	}
	var started *uint64
	switch kind {
	case SessionActivityKindTurn:
		started = turn
	case SessionActivityKindStep:
		started = turn
		if started == nil {
			started = optionalSeconds(a.foregroundToolSince())
		}
	case SessionActivityKindBackground:
		started = optionalSeconds(a.backgroundSince())
	}
	label := "Working"
	if kind == SessionActivityKindBackground {
		var counts []string
		if tasks := len(a.BackgroundCommands); tasks > 0 {
			counts = append(counts, fmt.Sprintf("%d task%s", tasks, pluralS(tasks)))
		}
		if shells := len(a.ActiveUserShells); shells > 0 {
			counts = append(counts, fmt.Sprintf("%d shell%s", shells, pluralS(shells)))
		}
		label = strings.Join(counts, ", ")
	}
	return elapsedLabel(label, nowEpochSeconds, started)
}

func pluralS(count int) string {
	if count == 1 {
		return ""
	}
	return "s"
}

// nonNegative is Rust's `.filter(|timestamp| *timestamp >= 0)` on an
// Option<i64>.
func nonNegative(value *int64) *int64 {
	if value == nil || *value < 0 {
		return nil
	}
	return value
}

// Details classifies the current activity and retains the timestamps that
// support that classification. This is the structured counterpart to the
// text clocks below, so web and terminal surfaces use the same precedence
// and never have to parse a rendered activity string.
func (a *SessionActivity) Details(currentTurnStartedAtMs, currentStepStartedAtMs *int64) SessionActivityDetails {
	var projectedTurn *uint64
	if currentTurnStartedAtMs != nil {
		zero := uint64(0)
		projectedTurn = &zero
	}
	kind := a.kind(projectedTurn)
	details := SessionActivityDetails{Kind: kind}
	switch kind {
	case SessionActivityKindTurn:
		turn := currentTurnStartedAtMs
		if turn == nil {
			turn = a.HarnessTurnStartedAtMs
		}
		details.TurnStartedAtMs = cloneInt64(nonNegative(turn))
		step := nonNegative(currentStepStartedAtMs)
		if step == nil {
			step = details.TurnStartedAtMs
		}
		if step != nil && details.TurnStartedAtMs != nil {
			latest := max(*step, *details.TurnStartedAtMs)
			details.StepStartedAtMs = &latest
		}
	case SessionActivityKindStep:
		details.StepStartedAtMs = cloneInt64(nonNegative(a.ForegroundToolStartedAtMs))
	case SessionActivityKindBackground:
		var oldest *int64
		consider := func(timestamp int64) {
			if timestamp >= 0 && (oldest == nil || timestamp < *oldest) {
				oldest = &timestamp
			}
		}
		for _, command := range a.BackgroundCommands {
			consider(command.StartedAtMs)
		}
		for _, shell := range a.ActiveUserShells {
			if shell.StartedAtMs != nil {
				consider(*shell.StartedAtMs)
			}
		}
		details.BackgroundStartedAtMs = oldest
	}
	var label string
	switch kind {
	case SessionActivityKindLifecycle:
		label = a.lifecycleLabel()
	case SessionActivityKindGoal:
		label = "Pursuing goal"
	case SessionActivityKindCheckingQuota:
		label = "Checking quota"
	case SessionActivityKindExpecting:
		label = "expecting the agent to continue"
	}
	if label != "" {
		details.Label = &label
	}
	if kind == SessionActivityKindIdle {
		details.IdleSinceMs = cloneInt64(nonNegative(a.IdleSinceMs))
	}
	state := a.State()
	details.LastActivityAtMs = cloneInt64(nonNegative(state.LastActivityAt()))
	return details
}

// quietSince is when the current quiet period began, in epoch seconds, once
// it has lasted long enough to be worth reporting.
//
// Absent when nothing is running, when the worker is too old to report the
// ACP clock, or when the session has been quiet for less than
// core.SilenceWorthReporting. The threshold lives in core's activity module
// so every surface agrees on when a turn reads as quiet.
func (a *SessionActivity) quietSince(nowEpochSeconds uint64) (uint64, bool) {
	if nowEpochSeconds > math.MaxInt64/1000 {
		return 0, false
	}
	nowMs := int64(nowEpochSeconds * 1000)
	state := a.State()
	last := state.LastActivityAt()
	if last == nil {
		return 0, false
	}
	if *last < 0 && nowMs > math.MaxInt64+*last {
		return 0, false
	}
	silentMs := nowMs - *last
	if silentMs < 0 || silentMs < core.SilenceWorthReporting.Milliseconds() {
		return 0, false
	}
	return epochSeconds(*last)
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
	facts := core.ActivityFacts{
		Execution:              core.RelayExecutionStateIdle,
		HarnessTurnStartedAtMs: cloneInt64(a.HarnessTurnStartedAtMs),
		TurnStartedAtMs:        cloneInt64(a.ActivityTurnStartedAtMs),
		ToolsInFlight:          []core.InFlightToolCall{},
		BackgroundCommands:     len(a.BackgroundCommands),
		ActiveUserShells:       len(a.ActiveUserShells),
		GoalActive:             a.PursuingGoal,
		CapacityRetryArmed:     a.CapacityRetry != nil && !a.CapacityRetry.Submitted,
		IdleSinceMs:            cloneInt64(a.IdleSinceMs),
	}
	if a.Execution != nil {
		facts.Execution = *a.Execution
	}
	if a.PromptInFlight {
		zero := int64(0)
		facts.PromptStartedAtMs = &zero
	}
	if a.ForegroundToolStartedAtMs != nil {
		facts.ToolsInFlight = append(facts.ToolsInFlight, core.InFlightToolCall{
			Status:      acpschema.ToolCallStatusInProgress,
			StartedAtMs: *a.ForegroundToolStartedAtMs,
		})
	}
	if seconds, ok := a.backgroundSince(); ok && seconds <= math.MaxInt64 {
		// Rust multiplies in i64 and would overflow only past year 292
		// million; the seconds come from millisecond timestamps.
		started := int64(seconds) * 1000
		facts.BackgroundStartedAtMs = &started
	}
	return facts
}

// kind is the rendering vocabulary for what the session is doing.
//
// A mapping of the shared activity state. The one fact added here is the
// caller's own turn start, which comes from the materialized projection
// rather than from the relay: a turn the durable record knows about is a
// turn whatever the live state says.
func (a *SessionActivity) kind(currentTurnStartedAt *uint64) SessionActivityKind {
	state := a.State()
	lastKnown := state.LastKnownState()
	switch lastKnown.Kind {
	case core.ActivityStateClosing, core.ActivityStateClosed:
		return SessionActivityKindLifecycle
	}
	if currentTurnStartedAt != nil {
		return SessionActivityKindTurn
	}
	switch lastKnown.Kind {
	case core.ActivityStateTurn:
		return SessionActivityKindTurn
	case core.ActivityStateTool:
		return SessionActivityKindStep
	case core.ActivityStateBackground:
		return SessionActivityKindBackground
	case core.ActivityStateGoal, core.ActivityStateRetry:
		return SessionActivityKindGoal
	case core.ActivityStateIdle:
		return SessionActivityKindIdle
	case core.ActivityStateExpecting:
		return SessionActivityKindExpecting
	case core.ActivityStateCheckingQuota:
		return SessionActivityKindCheckingQuota
	}
	// A state this build does not know is something happening, and "Turn"
	// is the honest way to render an unnamed something.
	return SessionActivityKindTurn
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
	var oldest uint64
	found := false
	consider := func(seconds uint64, ok bool) {
		if ok && (!found || seconds < oldest) {
			oldest, found = seconds, true
		}
	}
	for _, command := range a.BackgroundCommands {
		consider(epochSeconds(command.StartedAtMs))
	}
	for _, shell := range a.ActiveUserShells {
		if shell.StartedAtMs != nil {
			consider(epochSeconds(*shell.StartedAtMs))
		}
	}
	return oldest, found
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

// String is the variant's name, as Rust's derived Debug writes it.
func (k SessionActivityKind) String() string {
	switch k {
	case SessionActivityKindExpecting:
		return "Expecting"
	case SessionActivityKindCheckingQuota:
		return "CheckingQuota"
	case SessionActivityKindGoal:
		return "Goal"
	case SessionActivityKindTurn:
		return "Turn"
	case SessionActivityKindStep:
		return "Step"
	case SessionActivityKindBackground:
		return "Background"
	case SessionActivityKindLifecycle:
		return "Lifecycle"
	case SessionActivityKindIdle:
		return "Idle"
	}
	return fmt.Sprintf("SessionActivityKind(%d)", int(k))
}

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
	if activity.QuotaRecovery != nil {
		return []string{quotaStatus(activity.QuotaRecovery, nowEpochSeconds)}
	}
	if activity.CapacityRetry != nil {
		return []string{activity.CapacityRetry.Status(nowMsOf(nowEpochSeconds))}
	}
	if activity.CheckingResponse {
		return []string{"Checking response"}
	}
	// A third column only when the harness has gone quiet for long enough
	// to be worth saying. It is a fact beside the other clocks, not a
	// warning: Mjolnir does not end a turn for silence, so the reader
	// decides whether a quiet turn is worth cancelling.
	var quiet []string
	if since, ok := activity.quietSince(nowEpochSeconds); ok {
		quiet = []string{elapsedLabel("Quiet", nowEpochSeconds, &since)}
	}
	switch activity.kind(currentTurnStartedAt) {
	case SessionActivityKindTurn:
		turnStarted := currentTurnStartedAt
		if turnStarted == nil {
			turnStarted = optionalSeconds(activity.harnessTurnSince())
		}
		var stepStarted *uint64
		if currentStepStartedAtMs != nil {
			seconds := *currentStepStartedAtMs / 1000
			stepStarted = &seconds
		} else {
			stepStarted = turnStarted
		}
		if stepStarted != nil && turnStarted != nil {
			seconds := max(*stepStarted, *turnStarted)
			stepStarted = &seconds
		} else {
			stepStarted = nil
		}
		return append([]string{
			elapsedLabel("Turn", nowEpochSeconds, turnStarted),
			elapsedLabel("Step", nowEpochSeconds, stepStarted),
		}, quiet...)
	case SessionActivityKindStep:
		return append([]string{
			elapsedLabel("Step", nowEpochSeconds, optionalSeconds(activity.foregroundToolSince())),
		}, quiet...)
	case SessionActivityKindBackground:
		// The two leading spaces hold the width `Turn` takes, so the clocks
		// stay in one column whichever state a row is in.
		return []string{elapsedLabel("  BG", nowEpochSeconds, optionalSeconds(activity.backgroundSince()))}
	case SessionActivityKindLifecycle:
		return []string{activity.lifecycleLabel()}
	case SessionActivityKindIdle:
		return []string{"[idle]"}
	case SessionActivityKindGoal:
		return []string{"Pursuing goal"}
	case SessionActivityKindCheckingQuota:
		return []string{"Checking quota"}
	default:
		return []string{"expecting the agent to continue"}
	}
}

// FormatActivityClock is the single-cell form of FormatActivityColumns, for
// a narrow row.
func FormatActivityClock(nowEpochSeconds uint64, currentTurnStartedAt *uint64, activity *SessionActivity) string {
	if activity.QuotaRecovery != nil {
		return quotaStatus(activity.QuotaRecovery, nowEpochSeconds)
	}
	if activity.CapacityRetry != nil {
		return activity.CapacityRetry.Status(nowMsOf(nowEpochSeconds))
	}
	if activity.CheckingResponse {
		return "Checking response"
	}
	switch activity.kind(currentTurnStartedAt) {
	case SessionActivityKindTurn:
		if currentTurnStartedAt != nil {
			return FormatTurnClock(nowEpochSeconds, currentTurnStartedAt)
		}
		return "[" + elapsedLabel("Turn", nowEpochSeconds, optionalSeconds(activity.harnessTurnSince())) + "]"
	case SessionActivityKindStep:
		return "[" + elapsedLabel("Step", nowEpochSeconds, optionalSeconds(activity.foregroundToolSince())) + "]"
	case SessionActivityKindBackground:
		return "[" + elapsedLabel("BG", nowEpochSeconds, optionalSeconds(activity.backgroundSince())) + "]"
	case SessionActivityKindLifecycle:
		return "[" + activity.lifecycleLabel() + "]"
	case SessionActivityKindIdle:
		return "[idle]"
	case SessionActivityKindGoal:
		return "Pursuing goal"
	case SessionActivityKindCheckingQuota:
		return "Checking quota"
	default:
		return "expecting the agent to continue"
	}
}

func quotaStatus(recovery *core.QuotaRecovery, nowSeconds uint64) string {
	if recovery.RetryAtMs == nil {
		return "Quota limit · reset time unknown"
	}
	deadline := max(*recovery.RetryAtMs, 0)
	return "Quota limit · resumes in " + FormatClock(usageSaturatingSub(uint64(deadline)/1000, nowSeconds))
}

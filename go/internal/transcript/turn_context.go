// Port of mj-transcript/src/turn_context.rs.
//
// Process-local classification evidence using the shared transcript summary.

package transcript

import (
	"math"
	"reflect"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type turnContextState struct {
	generation     uint64
	parentActivity *time.Time
	decisionLog    *core.DecisionLog
	authorization  *core.ContextHistory
	userPromptTail *string
	messageID      *string
	// assistantTextTail and lastCompletedMessage are Rust Strings.
	assistantTextTail    string
	lastCompletedMessage string
	summary              TranscriptSummary
	backgroundCommands   int
	queuedCommands       int
	backgroundInventory  []core.BackgroundCommand
	nativeAgentIDs       []string
	sessionID            string
	// guardianReviews are Mjolnir guardian reviews in this request. A new
	// request starts with none; a recovery continues the same request and
	// keeps them.
	guardianReviews guardianReviews
}

// guardianReviews is Codex's per-turn guardian denial circuit breaker
// (`ext/guardian-reviewer/src/circuit_breaker.rs`): it trips once per
// request on 3 consecutive denials or 10 denials among the last 50 reviews.
type guardianReviews struct {
	consecutiveDenials uint32
	recent             []bool
	tripped            bool
}

const maxConsecutiveGuardianDenials uint32 = 3
const maxRecentGuardianDenials = 10
const guardianDenialWindow = 50

// DeliveredPromptCommandID is the prompt that became visible to the harness
// at this durable observation. A queued or unconfirmed steering prompt has
// not been delivered yet.
func DeliveredPromptCommandID(observation core.RelayObservation) (string, bool) {
	switch o := observation.(type) {
	case core.RelayObservationCommandStarted:
		return o.CommandID, true
	case core.RelayObservationCommandCompleted:
		if steered, ok := o.Outcome.(core.RelayCommandOutcomeSteered); ok {
			return steered.QueuedCommandID, true
		}
	case core.RelayObservationMailboxEventsDelivered:
		if o.Steering != nil {
			return o.Steering.QueuedPromptID, true
		}
	}
	return "", false
}

// TurnContext is the small process-local evidence accumulator the relay and
// runtime share. Hold it as *TurnContext: the zero value is ready (Rust's
// Default), and copies of the pointer share state as Rust's clones share
// their Arc<Mutex<..>>.
type TurnContext struct {
	mu    sync.Mutex
	state turnContextState
}

// MarkParentActivity is the process-local parent clock; child traffic and
// inventory updates never mark it.
func (c *TurnContext) MarkParentActivity() {
	c.MarkParentActivityAt(time.Now())
}

// MarkParentActivityAt records the monotonic time at which parent activity
// was observed.
func (c *TurnContext) MarkParentActivityAt(at time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.parentActivity = &at
}

func (c *TurnContext) ParentActivity() (time.Time, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.state.parentActivity == nil {
		return time.Time{}, false
	}
	return *c.state.parentActivity, true
}

func (c *TurnContext) SetDecisionLog(log core.DecisionLog) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.decisionLog = &log
}

func (c *TurnContext) DecisionLog() (core.DecisionLog, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.state.decisionLog == nil {
		return core.DecisionLog{}, false
	}
	return *c.state.decisionLog, true
}

// Reset starts a turn. The user prompt tail carries the prompt only when the
// person wrote it: a prompt Mjolnir submitted (a mailbox wake, a child
// report, a continuation, a parent's message to a child) is not the person's
// latest request, and Jev scored those turns better without it
// (.agents/docs/jev-still-open-20261106.md).
func (c *TurnContext) Reset(commandID *string, prompt string) {
	c.resetRequest(commandID, &prompt)
}

// resetRequest returns whether this delivery establishes a request in the
// context.
func (c *TurnContext) resetRequest(commandID *string, prompt *string) bool {
	recovery := commandID != nil && core.IsRestartContinuation(*commandID)
	var userPromptTail *string
	if prompt != nil {
		tail := ""
		if recovery || core.IsUserAuthoredMessage(commandID, *prompt) {
			tail = Tail(*prompt, core.UserPromptBytes)
		}
		userPromptTail = &tail
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	state := &c.state
	generation := state.generation + 1
	now := time.Now()
	if recovery {
		if state.userPromptTail == nil && userPromptTail != nil && prompt != nil {
			state.userPromptTail = userPromptTail
			// The carried request precedes the retained replay suffix.
			// Preserve that suffix and its partially observed answer.
			state.summary.PushUser(*prompt)
			entries := state.summary.Entries
			if len(entries) > 0 {
				last := entries[len(entries)-1]
				copy(entries[1:], entries[:len(entries)-1])
				entries[0] = last
			}
		}
		// Recovery keeps the request, but delivery closes the interrupted
		// message just as the transcript does. An unlabelled chunk from the
		// new harness must not inherit the old message ID.
		completeMessage(state)
		state.generation = generation
		state.parentActivity = &now
		return false
	}
	*state = turnContextState{
		generation:          generation,
		parentActivity:      &now,
		decisionLog:         state.decisionLog,
		authorization:       state.authorization,
		userPromptTail:      userPromptTail,
		backgroundCommands:  state.backgroundCommands,
		queuedCommands:      state.queuedCommands,
		backgroundInventory: state.backgroundInventory,
		nativeAgentIDs:      state.nativeAgentIDs,
		sessionID:           state.sessionID,
		summary:             state.summary,
	}
	return true
}

// ObservePromptDelivery records a known prompt delivery whose body may no
// longer be retained.
func (c *TurnContext) ObservePromptDelivery(commandID *string, prompt *string) {
	if c.resetRequest(commandID, prompt) && prompt != nil {
		c.mu.Lock()
		defer c.mu.Unlock()
		c.state.summary.PushUser(*prompt)
	}
}

// ObserveAutonomousTurnStart begins a turn the relay owner has identified as
// autonomous.
func (c *TurnContext) ObserveAutonomousTurnStart() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.assistantTextTail = ""
	c.state.lastCompletedMessage = ""
	c.state.messageID = nil
	c.state.generation++
}

// ObserveRelay records durable transcript observations through the same path
// during live work and replay.
func (c *TurnContext) ObserveRelay(observation core.RelayObservation, prompt *string) {
	delivery := false
	_, commandStarted := observation.(core.RelayObservationCommandStarted)
	switch o := observation.(type) {
	case core.RelayObservationCommandStarted:
		delivery = true
	case core.RelayObservationCommandCompleted:
		switch o.Outcome.(type) {
		case core.RelayCommandOutcomeSteered:
			delivery = true
		case core.RelayCommandOutcomeContextCleared:
			c.ClearHistory()
			return
		}
	case core.RelayObservationMailboxEventsDelivered:
		delivery = o.Steering != nil
	// ACP completion may precede native execution's actual end.
	// Deliveries, autonomous starts and these durable bridge/native
	// boundaries own the reply stream instead.
	case core.RelayObservationSessionRestarted:
		c.completeMessage()
		return
	case core.RelayObservationHarnessTurnSettled:
		if !o.PromptInFlight {
			c.completeMessage()
		}
		return
	case core.RelayObservationElicitationResolved:
		if o.Reply != nil && o.Action != core.OperatorAwayAction {
			c.completeMessage()
		}
		return
	case core.RelayObservationSessionUpdate:
		c.Observe(&o.Update)
		return
	case core.RelayObservationTerminalOutput:
		c.ObserveTerminal(&core.TerminalOutputRecord{
			TerminalID: o.TerminalID,
			Output:     core.NewPackedText(o.Output),
			Truncated:  o.Truncated,
			ExitCode:   o.ExitCode,
			Signal:     o.Signal,
		})
		return
	}
	// A steering confirmation proves delivery even after its queue body was
	// collected. Keep the reply boundary without inventing the missing
	// prompt text.
	if delivery && (prompt != nil || !commandStarted) {
		var commandID *string
		if id, ok := DeliveredPromptCommandID(observation); ok {
			commandID = &id
		}
		c.ObservePromptDelivery(commandID, prompt)
	}
}

// completeMessage closes the current message under the lock.
func (c *TurnContext) completeMessage() {
	c.mu.Lock()
	defer c.mu.Unlock()
	completeMessage(&c.state)
}

// Invalidate invalidates a pending verdict at a lifecycle boundary without
// discarding evidence.
func (c *TurnContext) Invalidate() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.generation++
}

func (c *TurnContext) Generation() uint64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.state.generation
}

// Counts is the background and queued command counts.
func (c *TurnContext) Counts() (int, int) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.state.backgroundCommands, c.state.queuedCommands
}

func (c *TurnContext) SetCounts(backgroundCommands, queuedCommands int) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.state.backgroundCommands != backgroundCommands || c.state.queuedCommands != queuedCommands {
		c.state.generation++
		c.state.backgroundCommands = backgroundCommands
		c.state.queuedCommands = queuedCommands
	}
}

func (c *TurnContext) SetSessionID(sessionID string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.sessionID = sessionID
}

func (c *TurnContext) SessionID() string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.state.sessionID
}

// SetAuthorizationContext publishes the relay owner's durable
// user-instruction history to the ACP classifier. A change invalidates any
// request built from older context. nil is none.
func (c *TurnContext) SetAuthorizationContext(authorization *core.ContextHistory) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if !contextHistoryEqual(c.state.authorization, authorization) {
		c.state.authorization = cloneContextHistory(authorization)
		c.state.generation++
	}
}

// contextHistoryEqual is Rust's PartialEq on Option<ContextHistory>, where
// an empty message list equals another whether or not Go allocated it.
func contextHistoryEqual(a, b *core.ContextHistory) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	left, right := *a, *b
	if len(left.Messages) == 0 {
		left.Messages = nil
	}
	if len(right.Messages) == 0 {
		right.Messages = nil
	}
	return reflect.DeepEqual(left, right)
}

// cloneContextHistory is a deep copy, so the published history is the
// context's own as Rust's moved value is; nil stays nil.
func cloneContextHistory(history *core.ContextHistory) *core.ContextHistory {
	if history == nil {
		return nil
	}
	copied := *history
	copied.Messages = slices.Clone(history.Messages)
	if history.OpenAssistantID != nil {
		id := *history.OpenAssistantID
		copied.OpenAssistantID = &id
	}
	if history.SetAsideThrough != nil {
		through := *history.SetAsideThrough
		copied.SetAsideThrough = &through
	}
	return &copied
}

// RestoreUserPromptTail restores the latest human instructions without
// changing replayed reply boundaries.
func (c *TurnContext) RestoreUserPromptTail(history *core.ContextHistory) {
	c.mu.Lock()
	defer c.mu.Unlock()
	tail := ""
	for i := len(history.Messages) - 1; i >= 0; i-- {
		if history.Messages[i].Role == "user" {
			tail = Tail(history.Messages[i].Text, core.UserPromptBytes)
			break
		}
	}
	c.state.userPromptTail = &tail
	c.state.generation++
}

// AuthorizationContext is a copy of the published history, nil when none.
func (c *TurnContext) AuthorizationContext() *core.ContextHistory {
	c.mu.Lock()
	defer c.mu.Unlock()
	return cloneContextHistory(c.state.authorization)
}

// SetBackgroundInventory compares identities as well as counts; identical
// level reports are not activity.
func (c *TurnContext) SetBackgroundInventory(commands []core.BackgroundCommand, nativeAgentIDs []string) {
	commands = slices.Clone(commands)
	nativeAgentIDs = slices.Clone(nativeAgentIDs)
	slices.SortStableFunc(commands, func(a, b core.BackgroundCommand) int { return strings.Compare(a.ID, b.ID) })
	slices.Sort(nativeAgentIDs)
	c.mu.Lock()
	defer c.mu.Unlock()
	if !slices.Equal(c.state.backgroundInventory, commands) || !slices.Equal(c.state.nativeAgentIDs, nativeAgentIDs) {
		c.state.backgroundInventory = commands
		c.state.nativeAgentIDs = nativeAgentIDs
		c.state.generation++
	}
}

func (c *TurnContext) Observe(update *schema.SessionUpdate) {
	c.mu.Lock()
	defer c.mu.Unlock()
	state := &c.state
	now := time.Now()
	state.parentActivity = &now
	if update.AgentMessageChunk != nil || update.AgentThoughtChunk != nil ||
		update.ToolCall != nil || update.ToolCallUpdate != nil {
		state.generation++
	}
	state.summary.Observe(update)
	switch {
	case update.AgentMessageChunk != nil:
		chunk := update.AgentMessageChunk
		// Like authorization history, an omitted ID continues the current
		// message; explicit IDs establish a new boundary.
		id := state.messageID
		if chunk.MessageID != nil {
			text := string(*chunk.MessageID)
			id = &text
		}
		if !optionalStringEqual(id, state.messageID) {
			completeMessage(state)
			state.messageID = id
		}
		if chunk.Content.Text != nil {
			// Trim each chunk before appending, so a huge chunk never grows
			// retained state.
			state.assistantTextTail += Tail(chunk.Content.Text.Text, core.AssistantTextBytes)
			state.assistantTextTail = Tail(state.assistantTextTail, core.AssistantTextBytes)
		}
	case update.ToolCall != nil, update.ToolCallUpdate != nil, update.AgentThoughtChunk != nil:
		completeMessage(state)
	}
}

func (c *TurnContext) ObserveTerminal(record *core.TerminalOutputRecord) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.summary.ObserveTerminal(record)
	c.state.generation++
}

// RenderTranscript is the retained transcript as the guardian reviewer reads
// it: recent context first, within limit bytes.
func (c *TurnContext) RenderTranscript(limit int) string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.state.summary.Render(limit)
}

// GuardianTripped reports whether the guardian's denials tripped the breaker
// in this request, so its remaining approvals go to the person.
func (c *TurnContext) GuardianTripped() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.state.guardianReviews.tripped
}

// RecordGuardianReview records one completed guardian review in this
// request. Returns whether this denial tripped the breaker.
func (c *TurnContext) RecordGuardianReview(denied bool) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	reviews := &c.state.guardianReviews
	if denied {
		if reviews.consecutiveDenials < math.MaxUint32 {
			reviews.consecutiveDenials++
		}
	} else {
		reviews.consecutiveDenials = 0
	}
	reviews.recent = append(reviews.recent, denied)
	if len(reviews.recent) > guardianDenialWindow {
		reviews.recent = slices.Delete(reviews.recent, 0, 1)
	}
	recentDenials := 0
	for _, review := range reviews.recent {
		if review {
			recentDenials++
		}
	}
	trips := denied && !reviews.tripped &&
		(reviews.consecutiveDenials >= maxConsecutiveGuardianDenials || recentDenials >= maxRecentGuardianDenials)
	reviews.tripped = reviews.tripped || trips
	return trips
}

func (c *TurnContext) MarkEarlierHistoryOmitted() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.summary.MarkEarlierHistoryOmitted()
}

func (c *TurnContext) ClearHistory() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.state.summary = TranscriptSummary{}
	c.state.userPromptTail = nil
	c.state.assistantTextTail = ""
	c.state.lastCompletedMessage = ""
	c.state.messageID = nil
	c.state.authorization = nil
	c.state.generation++
}

func (c *TurnContext) Evidence(harness core.HarnessKind, phase core.TurnPhase, facts *core.ActivityFacts, nowMs int64) core.TurnEvidence {
	c.mu.Lock()
	defer c.mu.Unlock()
	state := &c.state
	summary := state.summary.LatestUserMessages()
	entries := state.summary.Entries
	lastAssistant := state.summary.latestIndex(SummaryRoleAssistant)
	var finalToolCalls []core.ToolOutcome
	for _, entry := range entries[lastAssistant+1:] {
		if len(finalToolCalls) == core.InFlightTools {
			break
		}
		if entry.Role != SummaryRoleTool {
			continue
		}
		name, ok := transcriptGetString(entry.Tool, "name")
		if !ok {
			name = entry.Text
		}
		status, ok := transcriptGetString(entry.Tool, "status")
		if !ok {
			status = "unknown"
		}
		finalToolCalls = append(finalToolCalls, core.ToolOutcome{Name: core.ToolTitle(name), Status: status})
	}
	var silentForS uint64
	if facts.LastAcpActivityAtMs != nil {
		silentForS = uint64(max(saturatingSubI64(nowMs, *facts.LastAcpActivityAtMs), 0)) / 1000
	}
	toolsInFlight := []core.ToolEvidence{}
	for _, tool := range facts.ToolsInFlight[:min(len(facts.ToolsInFlight), core.InFlightTools)] {
		title := "unknown tool"
		for i := range entries {
			if entries[i].ID == tool.ToolCallID {
				title = entries[i].Text
				break
			}
		}
		toolsInFlight = append(toolsInFlight, core.ToolEvidence{
			Title:    core.ToolTitle(title),
			RunningS: uint64(max(saturatingSubI64(nowMs, tool.StartedAtMs), 0)) / 1000,
		})
	}
	userPromptTail := ""
	if state.userPromptTail != nil {
		userPromptTail = *state.userPromptTail
	}
	assistantTextTail := state.assistantTextTail
	if assistantTextTail == "" {
		assistantTextTail = state.lastCompletedMessage
	}
	return core.TurnEvidence{
		Authorization:      nil,
		FinalToolCalls:     finalToolCalls,
		Background:         nil,
		Harness:            harness,
		Phase:              phase,
		SilentForS:         silentForS,
		ToolsInFlight:      toolsInFlight,
		TranscriptSummary:  summary.Render(48 * 1024),
		BackgroundCommands: facts.BackgroundCommands,
		QueuedCommands:     facts.QueuedCommands,
		UserPromptTail:     userPromptTail,
		AssistantTextTail:  assistantTextTail,
		Completion:         nil,
	}
}

// saturatingSubI64 is Rust's i64::saturating_sub.
func saturatingSubI64(a, b int64) int64 {
	difference := a - b
	if (b < 0 && difference < a) || (b > 0 && difference > a) {
		if b < 0 {
			return math.MaxInt64
		}
		return math.MinInt64
	}
	return difference
}

func completeMessage(state *turnContextState) {
	if state.assistantTextTail != "" {
		state.lastCompletedMessage = state.assistantTextTail
		state.assistantTextTail = ""
	}
	state.messageID = nil
}

// Tail is the last maximumBytes of text, cut at a character boundary: the
// tail every turn classifier receives.
func Tail(text string, maximumBytes int) string {
	start := max(len(text)-maximumBytes, 0)
	for start > 0 && start < len(text) && !utf8.RuneStart(text[start]) {
		start--
	}
	value := text[start:]
	core.TruncateStringStart(&value, maximumBytes)
	return value
}

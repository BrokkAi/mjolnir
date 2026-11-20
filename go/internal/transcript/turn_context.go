// Port of mj-transcript/src/turn_context.rs.
//
// Process-local classification evidence using the shared transcript summary.

package transcript

import (
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
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::reset_request")
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
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::observe_relay")
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
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::set_authorization_context")
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
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::authorization_context")
}

// SetBackgroundInventory compares identities as well as counts; identical
// level reports are not activity.
func (c *TurnContext) SetBackgroundInventory(commands []core.BackgroundCommand, nativeAgentIDs []string) {
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::set_background_inventory")
}

func (c *TurnContext) Observe(update *schema.SessionUpdate) {
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::observe")
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
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::record_guardian_review")
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
	panic("unported: mj-transcript/src/turn_context.rs TurnContext::evidence")
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

// Port of mj-core/src/acp/step_clock.rs.

// When the step the agent is on right now began.
//
// ACP has no notion of a step. An agent streams one message as hundreds of
// `agent_message_chunk` updates and revises one tool call a dozen times, so
// the time since the last ACP message is almost always a fraction of a
// second and says nothing about progress. What a person reads as a step is
// coarser: this thought, this message, this tool call, this run of a
// command.
//
// So this reduces each update to a stepSignature and restarts the clock only
// when the signature changes. Chunks of the same message, appended tool
// output under an unchanged status, and the metadata updates every harness
// sprays between them all leave the clock alone.
//
// The rule comes from recordings of all five bridges under
// `testdata/step_clock`; the tests replay them.

package core

import (
	"sync"

	"github.com/BrokkAi/acp-go/schema"
)

type stepSignatureKind int

const (
	// stepSignatureMessage: streaming prose to the user.
	stepSignatureMessage stepSignatureKind = iota
	// stepSignatureThought: streaming its reasoning.
	stepSignatureThought
	// stepSignaturePlan: publishing or revising its plan.
	stepSignaturePlan
	// stepSignatureTool: working on one tool call. The status is part of the
	// signature because `pending`, `in_progress` and `completed` are separate
	// steps, while content appended under an unchanged status is the same
	// step going on.
	stepSignatureTool
)

// stepSignature is what the agent is doing, at the granularity a person
// reads as one step. id and status are set only for stepSignatureTool.
type stepSignature struct {
	kind   stepSignatureKind
	id     string
	status schema.ToolCallStatus
}

type stepState struct {
	startedAtMs *int64
	signature   *stepSignature
	// toolStatuses is the status each tool call last reported. A
	// `tool_call_update` may omit `status` and only add content or fix a
	// title, and every bridge does; such an update belongs to whatever the
	// call was already doing.
	toolStatuses map[string]schema.ToolCallStatus
}

// StepClock is a process-local clock for the current step, shared between
// the ACP client handlers and the relay that reports operational state. Share
// it by pointer; the zero value is ready to use.
//
// Like AcpActivityClock it stays out of the durable journal: this is render
// timing, not recoverable session history.
type StepClock struct {
	mu    sync.Mutex
	inner stepState
}

// StartedAtMs is the epoch milliseconds the current step began, while one is
// in flight.
func (c *StepClock) StartedAtMs() *int64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.inner.startedAtMs
}

// BeginTurn: a turn is starting: the first step begins now, with no history
// behind it from the turn before.
func (c *StepClock) BeginTurn() {
	c.beginTurnAt(EpochMillis())
}

// EndTurn: the turn is over, so no step is in flight. Whatever the next turn
// does first opens a step of its own.
func (c *StepClock) EndTurn() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.inner.startedAtMs = nil
	c.inner.signature = nil
	clear(c.inner.toolStatuses)
}

// BeginClientWork: the agent asked the client to do something: run a
// terminal, answer a permission prompt, serve an extension request. That is
// work starting even when the update stream has not moved on, so the step
// restarts — but the signature stays, so the updates that follow the same
// tool call under the same status still do not restart it again.
func (c *StepClock) BeginClientWork() {
	c.beginClientWorkAt(EpochMillis())
}

// Observe folds one session update in, restarting the step when it opens one.
func (c *StepClock) Observe(update *schema.SessionUpdate) {
	c.observeAt(update, EpochMillis())
}

func (c *StepClock) beginTurnAt(nowMs int64) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.inner.startedAtMs = &nowMs
	c.inner.signature = nil
	clear(c.inner.toolStatuses)
}

func (c *StepClock) beginClientWorkAt(nowMs int64) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.inner.startedAtMs = &nowMs
}

func (c *StepClock) observeAt(update *schema.SessionUpdate, nowMs int64) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.inner.toolStatuses == nil {
		c.inner.toolStatuses = map[string]schema.ToolCallStatus{}
	}
	signature, ok := signatureOf(update, c.inner.toolStatuses)
	if !ok || (c.inner.signature != nil && *c.inner.signature == signature) {
		return
	}
	c.inner.signature = &signature
	c.inner.startedAtMs = &nowMs
}

// signatureOf returns the step an update belongs to, or false when it says
// nothing about what the agent is doing.
//
// Usage, session-info, mode, config and available-command updates are the
// false cases. Every bridge interleaves them with real work — Claude Code
// sends `usage_update` between two updates of the same running tool call —
// so treating them as steps would restart the clock at random.
func signatureOf(update *schema.SessionUpdate, toolStatuses map[string]schema.ToolCallStatus) (stepSignature, bool) {
	panic("unported: mj-core/src/acp/step_clock.rs signature_of")
}

// Port of mj-core/src/activity/tools.rs.

// The tool calls the agent has open right now.
//
// One tracker, shared by everything that needs the answer. The worker's
// durable relay reports it in the operational state so the daemon, the CLI
// and the browser can see that a session blocked in a long build is working;
// the turn stall watchdog reads the same handle so it does not fail that
// turn. Before this existed the relay kept a private map and the watchdog
// could not reach it, which is exactly how a healthy twenty-minute `cargo
// nextest` run came to be failed as a stalled turn.
//
// Like AcpActivityClock and StepClock this is process-local and deliberately
// outside the durable journal: the tool calls a harness had open die with the
// harness, and a restarted worker must not inherit them.

package core

import (
	"cmp"
	"slices"
	"strings"
	"sync"

	"github.com/BrokkAi/acp-go/schema"
)

// InFlightToolCall is one tool call the agent has open.
type InFlightToolCall struct {
	ToolCallID string  `json:"tool_call_id"`
	Title      *string `json:"title,omitempty"`
	// Status is Pending or InProgress; a call in any other status is not in
	// flight.
	Status      schema.ToolCallStatus `json:"status"`
	StartedAtMs int64                 `json:"started_at_ms"`
}

type toolsInFlightEntry struct {
	title       *string
	status      schema.ToolCallStatus
	startedAtMs int64
}

// ToolsInFlight is a shared handle on the open tool calls. Share it by
// pointer; the zero value is ready to use.
type ToolsInFlight struct {
	mu      sync.Mutex
	entries map[string]toolsInFlightEntry
}

// Observe folds one session update in.
//
// Pending and in-progress have portable ACP meanings across every harness,
// so they are the two statuses that prove foreground work. Prose and plan
// updates carry no corresponding end, so they cannot safely open a call of
// their own.
func (t *ToolsInFlight) Observe(update *schema.SessionUpdate) {
	t.observeAt(update, EpochMillis())
}

// ObserveWithStart folds one session update in, dating a call the update
// opens from the step it belongs to rather than from the moment it was read.
// The step clock is the better start: it is when the agent began this piece
// of work, not when the relay got round to recording it.
func (t *ToolsInFlight) ObserveWithStart(update *schema.SessionUpdate, startedAtMs int64) {
	t.observeAt(update, startedAtMs)
}

// Open opens a call that the harness never reported as a tool call, such as
// a terminal Mjolnir runs on the agent's behalf.
func (t *ToolsInFlight) Open(toolCallID string, startedAtMs int64) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.entries == nil {
		t.entries = map[string]toolsInFlightEntry{}
	}
	t.entries[toolCallID] = toolsInFlightEntry{status: schema.ToolCallStatusInProgress, startedAtMs: startedAtMs}
}

func (t *ToolsInFlight) Close(toolCallID string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	delete(t.entries, toolCallID)
}

// Snapshot returns every open call, oldest first, so the first is the one
// whose age bounds the turn.
func (t *ToolsInFlight) Snapshot() []InFlightToolCall {
	t.mu.Lock()
	defer t.mu.Unlock()
	calls := make([]InFlightToolCall, 0, len(t.entries))
	for toolCallID, entry := range t.entries {
		var title *string
		if entry.title != nil {
			copied := *entry.title
			title = &copied
		}
		calls = append(calls, InFlightToolCall{
			ToolCallID:  toolCallID,
			Title:       title,
			Status:      entry.status,
			StartedAtMs: entry.startedAtMs,
		})
	}
	slices.SortFunc(calls, func(left, right InFlightToolCall) int {
		if order := cmp.Compare(left.StartedAtMs, right.StartedAtMs); order != 0 {
			return order
		}
		return strings.Compare(left.ToolCallID, right.ToolCallID)
	})
	return calls
}

func (t *ToolsInFlight) IsEmpty() bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	return len(t.entries) == 0
}

// NewestStartedAtMs is the newest open call's start, which is what the
// operational state has always published as `foreground_tool_started_at_ms`.
func (t *ToolsInFlight) NewestStartedAtMs() *int64 {
	t.mu.Lock()
	defer t.mu.Unlock()
	var newest *int64
	for _, entry := range t.entries {
		if newest == nil || entry.startedAtMs > *newest {
			started := entry.startedAtMs
			newest = &started
		}
	}
	return newest
}

// Clear: the harness that owned these calls is gone, and so are they.
func (t *ToolsInFlight) Clear() {
	t.mu.Lock()
	defer t.mu.Unlock()
	clear(t.entries)
}

func (t *ToolsInFlight) observeAt(update *schema.SessionUpdate, nowMs int64) {
	var toolCallID string
	var status *schema.ToolCallStatus
	var title *string
	switch {
	case update.ToolCall != nil:
		toolCallID = string(update.ToolCall.ToolCallID)
		// An absent status is `pending`, ACP's default.
		pending := schema.ToolCallStatusPending
		status = &pending
		if update.ToolCall.Status != nil {
			status = update.ToolCall.Status
		}
		copied := update.ToolCall.Title
		title = &copied
	case update.ToolCallUpdate != nil:
		toolCallID = string(update.ToolCallUpdate.ToolCallID)
		status = update.ToolCallUpdate.Status
	default:
		return
	}
	if status == nil {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	if *status != schema.ToolCallStatusPending && *status != schema.ToolCallStatusInProgress {
		delete(t.entries, toolCallID)
		return
	}
	if t.entries == nil {
		t.entries = map[string]toolsInFlightEntry{}
	}
	entry, ok := t.entries[toolCallID]
	if !ok {
		t.entries[toolCallID] = toolsInFlightEntry{title: title, status: *status, startedAtMs: nowMs}
		return
	}
	if title != nil {
		entry.title = title
	}
	// A call that moves from pending to in-progress has begun a new step, so
	// its clock restarts; repeated updates under an unchanged status are the
	// same work going on.
	if entry.status != *status {
		entry.status = *status
		entry.startedAtMs = nowMs
	}
	t.entries[toolCallID] = entry
}

// Port of mj-core/src/state.rs.
//
// Durable controller-side state for Hel-managed sessions.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

// StateVersion is the durable state schema version.
const StateVersion uint32 = 1

// SessionState is a session's durable lifecycle state.
type SessionState string

const (
	SessionStateProvisioning SessionState = "provisioning"
	// SessionStateStartupCleanup: startup failed; teardown owns the target
	// until termination is confirmed.
	SessionStateStartupCleanup SessionState = "startup-cleanup"
	SessionStateRunning        SessionState = "running"
	SessionStateDisconnected   SessionState = "disconnected"
	SessionStateCheckpointing  SessionState = "checkpointing"
	SessionStateClosing        SessionState = "closing"
	SessionStateDestroying     SessionState = "destroying"
	// SessionStateStopped: checkpointed and torn down. Persisted as
	// "archived" before the verb was renamed, so the alias keeps those
	// records loading.
	SessionStateStopped SessionState = "stopped"
	// SessionStateParked: a Mjolnir sub-agent whose turn ended and whose
	// parent was told: its worker process tree is stopped so it holds no
	// processes in the parent's container, while its record, relation,
	// target locator and worker root (relay journal, native session id)
	// stay. Only a parent's send_message starts it again. Nothing that
	// connects to, reconnects, recovers or upgrades live sessions acts on it.
	SessionStateParked                SessionState = "parked"
	SessionStateLost                  SessionState = "lost"
	SessionStateError                 SessionState = "error"
	SessionStateDestroyedWithDataLoss SessionState = "destroyed-with-data-loss"
)

// UnmarshalJSON accepts the serde spellings, including the "archived" alias.
func (s *SessionState) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	state, ok := SessionStateFromStored(text)
	if !ok {
		return fmt.Errorf("unknown variant `%s`", text)
	}
	*s = state
	return nil
}

// SessionTransitionKind: a lifecycle transition temporarily replaces the
// conversation in control surfaces. Operation ownership takes precedence
// over intermediate durable session states.
type SessionTransitionKind string

const (
	SessionTransitionKindStarting   SessionTransitionKind = "starting"
	SessionTransitionKindResuming   SessionTransitionKind = "resuming"
	SessionTransitionKindMoving     SessionTransitionKind = "moving"
	SessionTransitionKindSuspending SessionTransitionKind = "suspending"
	SessionTransitionKindDestroying SessionTransitionKind = "destroying"
	// SessionTransitionKindStopping: a sub-agent stopped because its parent
	// is being suspended.
	SessionTransitionKindStopping SessionTransitionKind = "stopping"
)

func (k SessionTransitionKind) Label() string {
	switch k {
	case SessionTransitionKindStarting:
		return "Starting"
	case SessionTransitionKindResuming:
		return "Resuming"
	case SessionTransitionKindMoving:
		return "Moving"
	case SessionTransitionKindSuspending:
		return "Suspending"
	case SessionTransitionKindDestroying:
		return "Destroying"
	case SessionTransitionKindStopping:
		return "Stopping"
	}
	return string(k)
}

// SessionTransitionKindForSession is SessionTransitionKind::for_session.
func SessionTransitionKindForSession(state SessionState, operation *SessionTransitionKind) (SessionTransitionKind, bool) {
	if operation != nil {
		return *operation, true
	}
	return state.TransitionKind()
}

// MaterializedExecutionState is controller-owned execution state derived
// from the relay event stream. Serialized with tag "state" (snake_case).
// The Rust default is MaterializedExecutionStateIdle.
type MaterializedExecutionState interface {
	isMaterializedExecutionState()
}

type MaterializedExecutionStateIdle struct{}

type MaterializedExecutionStateRunning struct {
	StartedAtMs int64 `json:"started_at_ms"`
}

type MaterializedExecutionStateClosing struct{}

type MaterializedExecutionStateClosed struct{}

func (MaterializedExecutionStateIdle) isMaterializedExecutionState()    {}
func (MaterializedExecutionStateRunning) isMaterializedExecutionState() {}
func (MaterializedExecutionStateClosing) isMaterializedExecutionState() {}
func (MaterializedExecutionStateClosed) isMaterializedExecutionState()  {}

func (MaterializedExecutionStateIdle) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("state", "idle", nil)
}

func (v MaterializedExecutionStateRunning) MarshalJSON() ([]byte, error) {
	type plain MaterializedExecutionStateRunning
	return stateMarshalTagged("state", "running", plain(v))
}

func (MaterializedExecutionStateClosing) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("state", "closing", nil)
}

func (MaterializedExecutionStateClosed) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("state", "closed", nil)
}

func unmarshalMaterializedExecutionState(data []byte) (MaterializedExecutionState, error) {
	tag, err := stateTag(data, "state")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "idle":
		return MaterializedExecutionStateIdle{}, nil
	case "running":
		type plain MaterializedExecutionStateRunning
		value, err := stateVariant[plain, plain](data)
		return MaterializedExecutionStateRunning(value), err
	case "closing":
		return MaterializedExecutionStateClosing{}, nil
	case "closed":
		return MaterializedExecutionStateClosed{}, nil
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// QueuedCommandKind is what a durable queue entry does when its turn comes.
//
// Serialized without a tag for prompts so entries written before
// configuration changes could be queued keep loading unchanged. Externally
// tagged, snake_case: "prompt" or {"set_config":{"key":..,"value":..}}.
type QueuedCommandKind interface {
	isQueuedCommandKind()
	IsPrompt() bool
}

type QueuedCommandKindPrompt struct{}

type QueuedCommandKindSetConfig struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

func (QueuedCommandKindPrompt) isQueuedCommandKind()    {}
func (QueuedCommandKindSetConfig) isQueuedCommandKind() {}

func (QueuedCommandKindPrompt) IsPrompt() bool    { return true }
func (QueuedCommandKindSetConfig) IsPrompt() bool { return false }

func (QueuedCommandKindPrompt) MarshalJSON() ([]byte, error) { return json.Marshal("prompt") }

func (v QueuedCommandKindSetConfig) MarshalJSON() ([]byte, error) {
	type plain QueuedCommandKindSetConfig
	return stateMarshalExternal("set_config", plain(v))
}

func unmarshalQueuedCommandKind(data []byte) (QueuedCommandKind, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	switch {
	case tag == "prompt" && content == nil:
		return QueuedCommandKindPrompt{}, nil
	case tag == "set_config" && content != nil:
		type plain QueuedCommandKindSetConfig
		value, err := stateVariant[plain, plain](content)
		return QueuedCommandKindSetConfig(value), err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// ConfigCommandText is the composer form of a configuration change, used
// both as the queue entry's display text and as the text peeled back into
// the composer for editing.
func ConfigCommandText(key, value string) string {
	if key == "fast-mode" {
		return "/fast"
	}
	return "/" + key + " " + value
}

type MaterializedQueuedPrompt struct {
	CommandID string `json:"command_id"`
	// Kind is QueuedCommandKindPrompt when nil or absent.
	Kind       QueuedCommandKind `json:"kind,omitempty"`
	Content    []any             `json:"content"`
	QueuedAtMs int64             `json:"queued_at_ms"`
	// AcceptedOrdinal is the relay acceptance ordinal of the CommandQueued
	// event that created this entry. It is the turn identity the API hands
	// back to callers, so wait can tell one queued prompt's outcome from
	// another's.
	AcceptedOrdinal *uint64 `json:"accepted_ordinal,omitempty"`
}

// MarshalJSON omits a prompt kind, as skip_serializing_if = is_prompt does.
func (p MaterializedQueuedPrompt) MarshalJSON() ([]byte, error) {
	type mirror struct {
		CommandID       string            `json:"command_id"`
		Kind            QueuedCommandKind `json:"kind,omitempty"`
		Content         []any             `json:"content"`
		QueuedAtMs      int64             `json:"queued_at_ms"`
		AcceptedOrdinal *uint64           `json:"accepted_ordinal,omitempty"`
	}
	out := mirror{p.CommandID, p.Kind, p.Content, p.QueuedAtMs, p.AcceptedOrdinal}
	if out.Kind != nil && out.Kind.IsPrompt() {
		out.Kind = nil
	}
	if out.Content == nil {
		out.Content = []any{}
	}
	return json.Marshal(out)
}

func (p *MaterializedQueuedPrompt) UnmarshalJSON(data []byte) error {
	type plain MaterializedQueuedPrompt
	aux := struct {
		*plain
		Kind stateUnionJSON[QueuedCommandKind] `json:"kind"`
	}{plain: (*plain)(p)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	p.Kind = aux.Kind.V
	if p.Kind == nil {
		p.Kind = QueuedCommandKindPrompt{}
	}
	return nil
}

// MaterializedTurn is the prompt currently executing, recorded when its
// CommandStarted event is projected and cleared when the command completes.
type MaterializedTurn struct {
	CommandID       string  `json:"command_id"`
	AcceptedOrdinal *uint64 `json:"accepted_ordinal,omitempty"`
	// TurnStartPosition is the ordinal of the CommandStarted event, which is
	// also the transcript position of the turn's first item.
	TurnStartPosition uint64 `json:"turn_start_position"`
	StartedAtMs       int64  `json:"started_at_ms"`
	// SteeredInto is the relay prompt still executing this turn after a
	// steer moved the turn to a queued prompt. That prompt's completion ends
	// this turn.
	SteeredInto *string `json:"steered_into,omitempty"`
}

func (t *MaterializedTurn) UnmarshalJSON(data []byte) error {
	type plain MaterializedTurn
	return stateDecodeStrict(data, (*plain)(t))
}

// BelongsTo reports whether the ending of relay prompt commandID ends this turn.
func (t *MaterializedTurn) BelongsTo(commandID string) bool {
	return t.CommandID == commandID || (t.SteeredInto != nil && *t.SteeredInto == commandID)
}

// TurnOutcomeKind is how a prompt ended. Tag "kind", snake_case.
type TurnOutcomeKind interface {
	isTurnOutcomeKind()
	// Result is defined in event_outcome.go.
	Result() TurnResult
	// String is how a turn ended, in words a person reads: "completed, end
	// of turn", "interrupted", or "failed: <reason>".
	String() string
}

// TurnOutcomeKindCompleted: the harness finished the turn and reported this
// stop reason.
type TurnOutcomeKindCompleted struct {
	StopReason string `json:"stop_reason"`
}

// TurnOutcomeKindRejected: the relay refused the command before it ran.
type TurnOutcomeKindRejected struct {
	Message string         `json:"message"`
	Reason  *OutcomeReason `json:"reason,omitempty"`
}

// TurnOutcomeKindInterrupted: the command was interrupted after being accepted.
type TurnOutcomeKindInterrupted struct {
	Message string         `json:"message"`
	Reason  *OutcomeReason `json:"reason,omitempty"`
}

func (TurnOutcomeKindCompleted) isTurnOutcomeKind()   {}
func (TurnOutcomeKindRejected) isTurnOutcomeKind()    {}
func (TurnOutcomeKindInterrupted) isTurnOutcomeKind() {}

func (v TurnOutcomeKindCompleted) Result() TurnResult   { return turnOutcomeKindResult(v) }
func (v TurnOutcomeKindRejected) Result() TurnResult    { return turnOutcomeKindResult(v) }
func (v TurnOutcomeKindInterrupted) Result() TurnResult { return turnOutcomeKindResult(v) }

func (v TurnOutcomeKindCompleted) String() string   { return turnOutcomeKindString(v) }
func (v TurnOutcomeKindRejected) String() string    { return turnOutcomeKindString(v) }
func (v TurnOutcomeKindInterrupted) String() string { return turnOutcomeKindString(v) }

func (v TurnOutcomeKindCompleted) MarshalJSON() ([]byte, error) {
	type plain TurnOutcomeKindCompleted
	return stateMarshalTagged("kind", "completed", plain(v))
}

func (v TurnOutcomeKindRejected) MarshalJSON() ([]byte, error) {
	type plain TurnOutcomeKindRejected
	return stateMarshalTagged("kind", "rejected", plain(v))
}

func (v TurnOutcomeKindInterrupted) MarshalJSON() ([]byte, error) {
	type plain TurnOutcomeKindInterrupted
	return stateMarshalTagged("kind", "interrupted", plain(v))
}

func unmarshalTurnOutcomeKind(data []byte) (TurnOutcomeKind, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "completed":
		type plain TurnOutcomeKindCompleted
		value, err := stateVariant[plain, plain](data)
		return TurnOutcomeKindCompleted(value), err
	case "rejected":
		type plain TurnOutcomeKindRejected
		value, err := stateVariant[plain, plain](data)
		return TurnOutcomeKindRejected(value), err
	case "interrupted":
		type plain TurnOutcomeKindInterrupted
		value, err := stateVariant[plain, plain](data)
		return TurnOutcomeKindInterrupted(value), err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// turnOutcomeKindString is the Display impl of TurnOutcomeKind.
func turnOutcomeKindString(kind TurnOutcomeKind) string {
	panic("unported: mj-core/src/state.rs TurnOutcomeKind::fmt")
}

// stopReasonWords renders a stop reason as the harness spells it
// (MaxTokens, max_turn_requests) as lower-case words.
func stopReasonWords(stopReason string) string {
	panic("unported: mj-core/src/state.rs stop_reason_words")
}

// PromptCompletion is the shared classification of a prompt's stop reason.
type PromptCompletion int

const (
	PromptCompletionInputRequired PromptCompletion = iota
	PromptCompletionFinished
	PromptCompletionCancelled
	PromptCompletionQuotaLimit
	PromptCompletionError
)

// ClassifyPromptCompletion is the shared interpretation for wait responses
// and durable completion events.
func ClassifyPromptCompletion(stopReason string) PromptCompletion {
	var normalized strings.Builder
	for _, character := range stopReason {
		if character != '_' && character != '-' {
			normalized.WriteString(strings.ToLower(string(character)))
		}
	}
	switch normalized.String() {
	case "endturn":
		return PromptCompletionFinished
	case "awaitinginput":
		return PromptCompletionInputRequired
	// The classifier ended a turn the harness left open after a closing
	// reply; the agent is done, not asking.
	case "inferredfinished":
		return PromptCompletionFinished
	case "cancelled", "canceled":
		return PromptCompletionCancelled
	case "quotalimit":
		return PromptCompletionQuotaLimit
	}
	return PromptCompletionError
}

// MaterializedTurnOutcome is the most recent finished prompt on a session.
type MaterializedTurnOutcome struct {
	AnsweredThrough   *uint64         `json:"answered_through,omitempty"`
	Diagnostic        *TurnDiagnostic `json:"diagnostic,omitempty"`
	Usage             *TokenUsage     `json:"usage,omitempty"`
	CommandID         string          `json:"command_id"`
	AcceptedOrdinal   *uint64         `json:"accepted_ordinal,omitempty"`
	TurnStartPosition *uint64         `json:"turn_start_position,omitempty"`
	CompletedOrdinal  uint64          `json:"completed_ordinal"`
	CompletedAtMs     int64           `json:"completed_at_ms"`
	Outcome           TurnOutcomeKind `json:"outcome"`
	// WorkFacts is what the child's worker observed when this turn ended
	// (see RelayCommandOutcome::Prompt).
	WorkFacts *WorkFacts `json:"work_facts,omitempty"`
}

func (o *MaterializedTurnOutcome) UnmarshalJSON(data []byte) error {
	type plain MaterializedTurnOutcome
	aux := struct {
		*plain
		Outcome stateUnionJSON[TurnOutcomeKind] `json:"outcome"`
	}{plain: (*plain)(o)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	if !aux.Outcome.Set || aux.Outcome.V == nil {
		return fmt.Errorf("missing field `outcome`")
	}
	o.Outcome = aux.Outcome.V
	return nil
}

// IsResubmittedBy reports whether this command carries out the request
// again, including provider retries and any generation of worker restart
// continuation.
func (o *MaterializedTurnOutcome) IsResubmittedBy(commandID string) bool {
	panic("unported: mj-core/src/state.rs MaterializedTurnOutcome::is_resubmitted_by")
}

// AnsweredOrdinal is the highest acceptance answered by the projection, or a
// legacy row.
func (o *MaterializedTurnOutcome) AnsweredOrdinal() (uint64, bool) {
	panic("unported: mj-core/src/state.rs MaterializedTurnOutcome::answered_ordinal")
}

func (o *MaterializedTurnOutcome) Answers(ordinal uint64) bool {
	if o.AcceptedOrdinal != nil && *o.AcceptedOrdinal == ordinal {
		return true
	}
	through, ok := o.AnsweredOrdinal()
	return ok && through >= ordinal
}

// InterruptionOrdinal: only work that actually started can have been interrupted.
func (o *MaterializedTurnOutcome) InterruptionOrdinal() (uint64, bool) {
	if _, interrupted := o.Outcome.(TurnOutcomeKindInterrupted); o.TurnStartPosition != nil && interrupted {
		return o.CompletedOrdinal, true
	}
	return 0, false
}

// MaterializedSession is the canonical controller projection for one
// logical ACP session.
type MaterializedSession struct {
	SessionID           string `json:"session_id"`
	AppliedEventOrdinal uint64 `json:"applied_event_ordinal"`
	AppliedEventDigest  string `json:"applied_event_digest"`
	// LastActivityAtMs is a monotonic controller projection watermark
	// derived from relay event receipt times. It is deliberately independent
	// of retained rows.
	LastActivityAtMs *int64                     `json:"last_activity_at_ms"`
	Execution        MaterializedExecutionState `json:"execution"`
	SessionTitle     *string                    `json:"session_title,omitempty"`
	// Configuration is omitted when empty.
	Configuration SessionConfiguration `json:"configuration"`
	// Transcript items are shared by pointer so cloning a snapshot copies
	// handles rather than the whole conversation.
	Transcript    []*TranscriptItem          `json:"transcript,omitempty"`
	QueuedPrompts []MaterializedQueuedPrompt `json:"queued_prompts,omitempty"`
	// PendingElicitations: in-flight form requests are projected durably,
	// but their answers are connection-only and never enter this state.
	PendingElicitations []ElicitationRequest `json:"pending_elicitations,omitempty"`
	// ActiveTurn is the prompt running right now, if any.
	ActiveTurn *MaterializedTurn `json:"active_turn,omitempty"`
	// LastTurnOutcome is the most recently finished prompt, kept after the
	// session stops so a caller can still read how the last turn ended.
	LastTurnOutcome *MaterializedTurnOutcome `json:"last_turn_outcome,omitempty"`
}

// MarshalJSON keeps serde's field order while omitting an empty configuration.
func (s MaterializedSession) MarshalJSON() ([]byte, error) {
	type mirror struct {
		SessionID           string                     `json:"session_id"`
		AppliedEventOrdinal uint64                     `json:"applied_event_ordinal"`
		AppliedEventDigest  string                     `json:"applied_event_digest"`
		LastActivityAtMs    *int64                     `json:"last_activity_at_ms"`
		Execution           MaterializedExecutionState `json:"execution"`
		SessionTitle        *string                    `json:"session_title,omitempty"`
		Configuration       *SessionConfiguration      `json:"configuration,omitempty"`
		Transcript          []*TranscriptItem          `json:"transcript,omitempty"`
		QueuedPrompts       []MaterializedQueuedPrompt `json:"queued_prompts,omitempty"`
		PendingElicitations []ElicitationRequest       `json:"pending_elicitations,omitempty"`
		ActiveTurn          *MaterializedTurn          `json:"active_turn,omitempty"`
		LastTurnOutcome     *MaterializedTurnOutcome   `json:"last_turn_outcome,omitempty"`
	}
	out := mirror{
		s.SessionID, s.AppliedEventOrdinal, s.AppliedEventDigest, s.LastActivityAtMs,
		s.Execution, s.SessionTitle, nil, s.Transcript, s.QueuedPrompts,
		s.PendingElicitations, s.ActiveTurn, s.LastTurnOutcome,
	}
	if !s.Configuration.IsEmpty() {
		out.Configuration = &s.Configuration
	}
	if out.Execution == nil {
		out.Execution = MaterializedExecutionStateIdle{}
	}
	return json.Marshal(out)
}

func (s *MaterializedSession) UnmarshalJSON(data []byte) error {
	type plain MaterializedSession
	aux := struct {
		*plain
		Execution stateUnionJSON[MaterializedExecutionState] `json:"execution"`
	}{plain: (*plain)(s)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	if !aux.Execution.Set || aux.Execution.V == nil {
		return fmt.Errorf("missing field `execution`")
	}
	s.Execution = aux.Execution.V
	return nil
}

// MaterializedSessionSummary is the small portion of a durable projection
// needed to populate dashboard rows before the live session delivers its
// full transcript snapshot.
type MaterializedSessionSummary struct {
	SessionID           string                     `json:"session_id"`
	AppliedEventOrdinal uint64                     `json:"applied_event_ordinal"`
	LastActivityAtMs    *int64                     `json:"last_activity_at_ms"`
	Execution           MaterializedExecutionState `json:"execution"`
	SessionTitle        *string                    `json:"session_title"`
	LastAgentMessage    *string                    `json:"last_agent_message"`
	LastUserMessage     *string                    `json:"last_user_message"`
	// LastAgentMessageFollowsLastUser reports whether the last nonempty agent
	// message appears after the last nonempty user message in transcript order.
	LastAgentMessageFollowsLastUser   bool     `json:"last_agent_message_follows_last_user"`
	AgentMessageLatestContentOrdinals []uint64 `json:"agent_message_latest_content_ordinals"`
	InterruptionEventOrdinals         []uint64 `json:"interruption_event_ordinals"`
}

func (s *MaterializedSessionSummary) UnmarshalJSON(data []byte) error {
	type plain MaterializedSessionSummary
	aux := struct {
		*plain
		Execution stateUnionJSON[MaterializedExecutionState] `json:"execution"`
	}{plain: (*plain)(s)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	s.Execution = aux.Execution.V
	return nil
}

// LatestUserPosition is the latest user item, including empty responses, in
// this projection.
func (s *MaterializedSession) LatestUserPosition() (uint64, bool) {
	panic("unported: mj-core/src/state.rs MaterializedSession::latest_user_position")
}

// LatestPersonPosition is the latest prompt written by the person,
// excluding generated wake messages.
func (s *MaterializedSession) LatestPersonPosition() (uint64, bool) {
	panic("unported: mj-core/src/state.rs MaterializedSession::latest_person_position")
}

// UnansweredInputOutcome is the latest input-requiring outcome, unless work
// or a later user item has already answered it. A transcript-free header
// supplies the user position retained from its original publication; full
// sessions pass nil.
func (s *MaterializedSession) UnansweredInputOutcome(assessment *TurnAssessment, publishedLatestUserPosition *uint64) *MaterializedTurnOutcome {
	panic("unported: mj-core/src/state.rs MaterializedSession::unanswered_input_outcome")
}

// MaterializedSessionEmpty is MaterializedSession::empty.
func MaterializedSessionEmpty(sessionID string) MaterializedSession {
	return MaterializedSession{
		SessionID:          sessionID,
		AppliedEventDigest: RelayEventGenesisDigest,
		Execution:          MaterializedExecutionStateIdle{},
	}
}

// GetLastActivityAtMs is MaterializedSession::last_activity_at_ms (the Go
// field of the same name holds the value).
func (s *MaterializedSession) GetLastActivityAtMs() *int64 {
	return s.LastActivityAtMs
}

// ResolvedTitle resolves the title exposed by a live materialized session.
//
// Sessions created before provisional titles were projected can still have
// an untitled transcript. Derive the same bounded fallback from their first
// visible user prompt when reading them.
func (s *MaterializedSession) ResolvedTitle() (string, bool) {
	panic("unported: mj-core/src/state.rs MaterializedSession::resolved_title")
}

func (s *MaterializedSession) UnreadAgentMessagesAfter(viewedThroughEventOrdinal uint64) uint64 {
	panic("unported: mj-core/src/state.rs MaterializedSession::unread_agent_messages_after")
}

func (s *MaterializedSession) UnreadInterruptionsAfter(viewedThroughEventOrdinal uint64) uint64 {
	var count uint64
	for _, ordinal := range s.InterruptionEventOrdinals() {
		if ordinal > viewedThroughEventOrdinal {
			count++
		}
	}
	return count
}

func (s *MaterializedSession) InterruptionEventOrdinals() []uint64 {
	panic("unported: mj-core/src/state.rs MaterializedSession::interruption_event_ordinals")
}

func (s *MaterializedSession) Validate() error {
	panic("unported: mj-core/src/state.rs MaterializedSession::validate")
}

// ManagedSessionSnapshot is a materialized session paired with the live
// worker's relay state. The session manager hands this to every reader that
// needs both the durable projection and the connection's operational status.
type ManagedSessionSnapshot struct {
	Materialized MaterializedSession
	// Window is what Materialized.Transcript leaves out, and the facts that
	// live there. See ProjectionWindow.
	Window      ProjectionWindow
	Operational SharedSnapshot[RelayOperationalState]
	// LatestCredentialSyncSignal is the newest relay event observed by this
	// live actor that asks for immediate credential reconciliation. This is
	// intentionally ephemeral: it avoids retaining raw replay pages or
	// rescanning projected history.
	LatestCredentialSyncSignal *CredentialSyncSignal
	// WorkerBuild is the worker-input compatibility identity reported in
	// hello. Nil when the connection did not come from a live worker or the
	// worker predates the field; either way the worker is not known to match
	// this controller.
	WorkerBuild *string
	// SubagentRequests is pending parent-tool work fetched from the target worker.
	SubagentRequests SharedSnapshot[[]SubagentToolRequest]
	// SubagentCompletions are compact completion receipts; full tool replies
	// remain worker-private.
	SubagentCompletions SharedSnapshot[[]SubagentCompletion]
	// SubagentQueueError: delegation transport failures do not make the
	// worker's controls unreachable.
	SubagentQueueError *string
}

// ProjectionWindow is what a projection's transcript window leaves out.
//
// A polled projection carries only the end of the transcript, because that
// is all any viewer shows and loading the rest is work proportional to
// history. Two facts a reader needs live outside that window: the
// provisional title comes from the first user message, and the newest turn
// start is outside it whenever a single turn is longer than the window. Both
// are read separately, with one indexed query each, rather than found by
// scanning.
//
// The unread event watermark is also retained here and restored from the
// stored summary ordinals when an actor reloads. A complete projection finds
// these facts in the items it already holds, via ProjectionWindowOf.
type ProjectionWindow struct {
	// LatestUnreadEventOrdinal is retained across actor trimming and restored
	// from stored projection rows. Clients consume served attention rather
	// than this daemon-local fact.
	LatestUnreadEventOrdinal *uint64 `json:"-"`
	// OmittedItems is the number of transcript items before the window. Zero
	// when the projection is whole.
	OmittedItems int `json:"omitted_items"`
	// ProvisionalTitle is the title derived from the first user message.
	ProvisionalTitle *string `json:"provisional_title"`
	// LatestTurnStartPosition is the position of the newest turn start — a
	// user message or the marker for a turn the harness began on its own —
	// whether or not it is in the window. Nil when the session has none.
	LatestTurnStartPosition *uint64 `json:"latest_turn_start_position"`
}

// Trim keeps the tail and complete turns containing unsettled content.
func (w *ProjectionWindow) Trim(session *MaterializedSession, target int) {
	panic("unported: mj-core/src/state.rs ProjectionWindow::trim")
}

// ProjectionWindowOf is the window of a projection that omits nothing.
func ProjectionWindowOf(session *MaterializedSession) ProjectionWindow {
	panic("unported: mj-core/src/state.rs ProjectionWindow::of")
}

// ResolvedTitle is the session's title, using the same precedence as
// MaterializedSession.ResolvedTitle but taking the provisional title from
// the window rather than from a transcript head that a polled projection
// does not carry.
func (s *ManagedSessionSnapshot) ResolvedTitle() (string, bool) {
	panic("unported: mj-core/src/state.rs ManagedSessionSnapshot::resolved_title")
}

// LatestCompletedTurnOrdinal is the position of the turn this session most
// recently finished, or false while it is still working. Same answer as the
// free LatestCompletedTurnOrdinal, from a position the window carries rather
// than a scan back through the transcript.
func (s *ManagedSessionSnapshot) LatestCompletedTurnOrdinal() (uint64, bool) {
	if _, idle := s.Materialized.Execution.(MaterializedExecutionStateIdle); !idle {
		return 0, false
	}
	if s.Window.LatestTurnStartPosition == nil {
		return 0, false
	}
	return *s.Window.LatestTurnStartPosition, true
}

// LatestCompletedTurnOrdinal is the position where the session's most
// recent finished turn began, or false while it is still working. A turn
// starts at a user message or at the marker for a turn the harness began on
// its own, so autonomous work is covered once it settles.
func LatestCompletedTurnOrdinal(session *MaterializedSession) (uint64, bool) {
	panic("unported: mj-core/src/state.rs latest_completed_turn_ordinal")
}

func ValidateRelayEventDigest(digest, name string) error {
	valid := len(digest) == 64
	for i := 0; valid && i < len(digest); i++ {
		c := digest[i]
		valid = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')
	}
	if !valid {
		return fmt.Errorf("%s must be a lowercase SHA-256 digest", name)
	}
	return nil
}

func ValidateRelayEventFrontier(ordinal uint64, digest, name string) error {
	if err := ValidateRelayEventDigest(digest, name); err != nil {
		return err
	}
	if (ordinal == 0) != (digest == RelayEventGenesisDigest) {
		return fmt.Errorf("%s has inconsistent ordinal %d and digest %s", name, ordinal, digest)
	}
	return nil
}

// stateIsFalse is state.rs is_false (the serde skip predicate; Go uses omitempty).
func stateIsFalse(value bool) bool { return !value }

// AsStr is the persisted and wire spelling, matching the serde encoding.
func (s SessionState) AsStr() string { return string(s) }

// SessionStateFromStored reads a stored spelling. Rows written before the
// verb was renamed still say "archived".
func SessionStateFromStored(value string) (SessionState, bool) {
	switch value {
	case "archived":
		return SessionStateStopped, true
	case "provisioning", "startup-cleanup", "running", "disconnected", "checkpointing",
		"closing", "destroying", "stopped", "parked", "lost", "error", "destroyed-with-data-loss":
		return SessionState(value), true
	}
	return "", false
}

// TransitionKind: recovery without a live operation still hides an
// unfinished target transition. Ordinary checkpoints and reconnects
// deliberately keep their conversation visible.
func (s SessionState) TransitionKind() (SessionTransitionKind, bool) {
	switch s {
	case SessionStateProvisioning:
		return SessionTransitionKindStarting, true
	case SessionStateStartupCleanup:
		return SessionTransitionKindStopping, true
	case SessionStateClosing:
		return SessionTransitionKindSuspending, true
	case SessionStateDestroying:
		return SessionTransitionKindDestroying, true
	}
	return "", false
}

// IsActive is true while the session still belongs on the dashboard.
// Closing and Checkpointing stay active on purpose: a stop that has not
// produced a verified checkpoint must not make its row disappear. A Parked
// sub-agent is active too: it is still its parent's child, still listed,
// and its parent's suspend, destroy or workspace close must still end it.
// Code that needs a live worker must ask HasLiveWorker.
func (s SessionState) IsActive() bool {
	switch s {
	case SessionStateProvisioning, SessionStateStartupCleanup, SessionStateRunning,
		SessionStateDisconnected, SessionStateCheckpointing, SessionStateClosing,
		SessionStateDestroying, SessionStateParked, SessionStateError:
		return true
	}
	return false
}

// CanReceiveChildWork reports whether a managed child in this state can
// still be given work: not failed, lost, stopped or on its way out. A
// parked child wakes for it.
func (s SessionState) CanReceiveChildWork() bool {
	switch s {
	case SessionStateProvisioning, SessionStateRunning, SessionStateDisconnected,
		SessionStateCheckpointing, SessionStateParked:
		return true
	}
	return false
}

// HasLiveWorker is true while the session may have a worker process tree on
// its target, including one still being started or torn down: the states in
// which a sub-agent counts against its parent's cap.
func (s SessionState) HasLiveWorker() bool {
	switch s {
	case SessionStateProvisioning, SessionStateStartupCleanup, SessionStateRunning,
		SessionStateDisconnected, SessionStateCheckpointing, SessionStateClosing,
		SessionStateDestroying:
		return true
	}
	return false
}

// PodmanWorkspaceLocator records where a Podman session's workspace lives.
// Tag "kind", kebab-case; the Rust default is ContainerLayer.
type PodmanWorkspaceLocator interface {
	isPodmanWorkspaceLocator()
}

type PodmanWorkspaceLocatorContainerLayer struct{}

type PodmanWorkspaceLocatorVolume struct {
	Name string `json:"name"`
}

type PodmanWorkspaceLocatorHostPath struct {
	Path     string   `json:"path"`
	Helper   []string `json:"helper"`
	Resource string   `json:"resource"`
}

func (PodmanWorkspaceLocatorContainerLayer) isPodmanWorkspaceLocator() {}
func (PodmanWorkspaceLocatorVolume) isPodmanWorkspaceLocator()         {}
func (PodmanWorkspaceLocatorHostPath) isPodmanWorkspaceLocator()       {}

func (PodmanWorkspaceLocatorContainerLayer) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("kind", "container-layer", nil)
}

func (v PodmanWorkspaceLocatorVolume) MarshalJSON() ([]byte, error) {
	type plain PodmanWorkspaceLocatorVolume
	return stateMarshalTagged("kind", "volume", plain(v))
}

func (v PodmanWorkspaceLocatorHostPath) MarshalJSON() ([]byte, error) {
	type plain PodmanWorkspaceLocatorHostPath
	if v.Helper == nil {
		v.Helper = []string{}
	}
	return stateMarshalTagged("kind", "host-path", plain(v))
}

func unmarshalPodmanWorkspaceLocator(data []byte) (PodmanWorkspaceLocator, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "container-layer":
		return PodmanWorkspaceLocatorContainerLayer{}, nil
	case "volume":
		type plain PodmanWorkspaceLocatorVolume
		value, err := stateVariant[plain, plain](data)
		return PodmanWorkspaceLocatorVolume(value), err
	case "host-path":
		type plain PodmanWorkspaceLocatorHostPath
		value, err := stateVariant[plain, plain](data)
		return PodmanWorkspaceLocatorHostPath(value), err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// statePodmanStorage resolves a decoded workspace_storage field, which
// defaults to the container layer when absent.
func statePodmanStorage(field stateUnionJSON[PodmanWorkspaceLocator]) PodmanWorkspaceLocator {
	if field.V == nil {
		return PodmanWorkspaceLocatorContainerLayer{}
	}
	return field.V
}

// TargetLocator records the physical placement of a session's worker. Tag
// "kind", kebab-case.
type TargetLocator interface {
	isTargetLocator()
	// CpuContainerScope is the physical accounting identity: engine, SSH
	// destination (nil for local), container.
	CpuContainerScope() (engine string, host *string, container string, ok bool)
	// SharesCpuContainer: same physical accounting scope, including after an
	// owner moves targets.
	SharesCpuContainer(other TargetLocator) bool
	// CpuAccounting: container placement includes borrowed workers: each can
	// survive its owner moving.
	CpuAccounting() CpuAccountingMode
	// OwnsContainer reports whether this locator owns a container rather
	// than borrowing one.
	OwnsContainer() bool
	// SkillsScope: only localhost sessions can use the daemon host's CLI and config.
	SkillsScope() SkillsScope
	// SameResource: physical identity ignores borrowing, SSH discovery and
	// storage metadata.
	SameResource(other TargetLocator) bool
	validate(sessionID string) error
}

type TargetLocatorLocalBare struct {
	WorkerRoot string `json:"worker_root"`
}

type TargetLocatorLocalPodman struct {
	ContainerID      string                 `json:"container_id"`
	WorkspaceStorage PodmanWorkspaceLocator `json:"workspace_storage"`
	// BorrowedFrom is the session that owns the container when this locator
	// is a sub-agent child borrowing its parent's container; nil when the
	// session owns the container itself.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

type TargetLocatorLocalDocker struct {
	ContainerID string `json:"container_id"`
	// BorrowedFrom: see TargetLocatorLocalPodman.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

type TargetLocatorAppleContainer struct {
	ContainerID string `json:"container_id"`
	// BorrowedFrom: see TargetLocatorLocalPodman.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

type TargetLocatorAwsEc2 struct {
	InstanceID string  `json:"instance_id"`
	Address    *string `json:"address,omitempty"`
}

type TargetLocatorSshBare struct {
	Host      string  `json:"host"`
	Workspace string  `json:"workspace"`
	WorkerID  *string `json:"worker_id,omitempty"`
}

type TargetLocatorSshPodman struct {
	Host             string                 `json:"host"`
	ContainerID      string                 `json:"container_id"`
	WorkspaceStorage PodmanWorkspaceLocator `json:"workspace_storage"`
	// BorrowedFrom: see TargetLocatorLocalPodman.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

type TargetLocatorSshDocker struct {
	Host        string `json:"host"`
	ContainerID string `json:"container_id"`
	// BorrowedFrom: see TargetLocatorLocalPodman.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

type TargetLocatorSshAppleContainer struct {
	Host        string `json:"host"`
	ContainerID string `json:"container_id"`
	// BorrowedFrom: see TargetLocatorLocalPodman.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

// TargetLocatorSshNitro is a Nitro Sandbox (a Firecracker microVM) on an EC2
// host reached over SSH. The workspace is always a named volume the host
// engine owns, so unlike Podman there is no workspace storage to record.
type TargetLocatorSshNitro struct {
	Host        string `json:"host"`
	ContainerID string `json:"container_id"`
	// BorrowedFrom is the session that owns the sandbox when this locator is
	// a sub-agent child borrowing its parent's sandbox; nil when the session
	// owns the sandbox itself.
	BorrowedFrom *string `json:"borrowed_from,omitempty"`
}

func (TargetLocatorLocalBare) isTargetLocator()         {}
func (TargetLocatorLocalPodman) isTargetLocator()       {}
func (TargetLocatorLocalDocker) isTargetLocator()       {}
func (TargetLocatorAppleContainer) isTargetLocator()    {}
func (TargetLocatorAwsEc2) isTargetLocator()            {}
func (TargetLocatorSshBare) isTargetLocator()           {}
func (TargetLocatorSshPodman) isTargetLocator()         {}
func (TargetLocatorSshDocker) isTargetLocator()         {}
func (TargetLocatorSshAppleContainer) isTargetLocator() {}
func (TargetLocatorSshNitro) isTargetLocator()          {}

func (v TargetLocatorLocalBare) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorLocalBare
	return stateMarshalTagged("kind", "local-bare", plain(v))
}

func (v TargetLocatorLocalPodman) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorLocalPodman
	if v.WorkspaceStorage == nil {
		v.WorkspaceStorage = PodmanWorkspaceLocatorContainerLayer{}
	}
	return stateMarshalTagged("kind", "local-podman", plain(v))
}

func (v TargetLocatorLocalDocker) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorLocalDocker
	return stateMarshalTagged("kind", "local-docker", plain(v))
}

func (v TargetLocatorAppleContainer) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorAppleContainer
	return stateMarshalTagged("kind", "apple-container", plain(v))
}

func (v TargetLocatorAwsEc2) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorAwsEc2
	return stateMarshalTagged("kind", "aws-ec2", plain(v))
}

func (v TargetLocatorSshBare) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorSshBare
	return stateMarshalTagged("kind", "ssh-bare", plain(v))
}

func (v TargetLocatorSshPodman) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorSshPodman
	if v.WorkspaceStorage == nil {
		v.WorkspaceStorage = PodmanWorkspaceLocatorContainerLayer{}
	}
	return stateMarshalTagged("kind", "ssh-podman", plain(v))
}

func (v TargetLocatorSshDocker) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorSshDocker
	return stateMarshalTagged("kind", "ssh-docker", plain(v))
}

func (v TargetLocatorSshAppleContainer) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorSshAppleContainer
	return stateMarshalTagged("kind", "ssh-apple-container", plain(v))
}

func (v TargetLocatorSshNitro) MarshalJSON() ([]byte, error) {
	type plain TargetLocatorSshNitro
	return stateMarshalTagged("kind", "ssh-nitro", plain(v))
}

func (v *TargetLocatorLocalPodman) UnmarshalJSON(data []byte) error {
	type plain TargetLocatorLocalPodman
	aux := struct {
		*plain
		WorkspaceStorage stateUnionJSON[PodmanWorkspaceLocator] `json:"workspace_storage"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	v.WorkspaceStorage = statePodmanStorage(aux.WorkspaceStorage)
	return nil
}

func (v *TargetLocatorSshPodman) UnmarshalJSON(data []byte) error {
	type plain TargetLocatorSshPodman
	aux := struct {
		*plain
		WorkspaceStorage stateUnionJSON[PodmanWorkspaceLocator] `json:"workspace_storage"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	v.WorkspaceStorage = statePodmanStorage(aux.WorkspaceStorage)
	return nil
}

func unmarshalTargetLocator(data []byte) (TargetLocator, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "local-bare":
		return stateVariant[TargetLocatorLocalBare, TargetLocator](data)
	case "local-podman":
		var value TargetLocatorLocalPodman
		err := json.Unmarshal(data, &value)
		return value, err
	case "local-docker":
		return stateVariant[TargetLocatorLocalDocker, TargetLocator](data)
	case "apple-container":
		return stateVariant[TargetLocatorAppleContainer, TargetLocator](data)
	case "aws-ec2":
		return stateVariant[TargetLocatorAwsEc2, TargetLocator](data)
	case "ssh-bare":
		return stateVariant[TargetLocatorSshBare, TargetLocator](data)
	case "ssh-podman":
		var value TargetLocatorSshPodman
		err := json.Unmarshal(data, &value)
		return value, err
	case "ssh-docker":
		return stateVariant[TargetLocatorSshDocker, TargetLocator](data)
	case "ssh-apple-container":
		return stateVariant[TargetLocatorSshAppleContainer, TargetLocator](data)
	case "ssh-nitro":
		return stateVariant[TargetLocatorSshNitro, TargetLocator](data)
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// UnmarshalTargetLocator decodes a serialized TargetLocator.
func UnmarshalTargetLocator(data []byte) (TargetLocator, error) { return unmarshalTargetLocator(data) }

func targetLocatorCpuContainerScope(l TargetLocator) (string, *string, string, bool) {
	switch v := l.(type) {
	case TargetLocatorLocalPodman:
		return "podman", nil, v.ContainerID, true
	case TargetLocatorLocalDocker:
		return "docker", nil, v.ContainerID, true
	case TargetLocatorAppleContainer:
		return "apple", nil, v.ContainerID, true
	case TargetLocatorSshPodman:
		return "podman", &v.Host, v.ContainerID, true
	case TargetLocatorSshDocker:
		return "docker", &v.Host, v.ContainerID, true
	case TargetLocatorSshAppleContainer:
		return "apple", &v.Host, v.ContainerID, true
	case TargetLocatorSshNitro:
		return "nitro", &v.Host, v.ContainerID, true
	}
	return "", nil, "", false
}

func targetLocatorSharesCpuContainer(l, other TargetLocator) bool {
	engine, host, container, ok := l.CpuContainerScope()
	if !ok || other == nil {
		return false
	}
	otherEngine, otherHost, otherContainer, otherOK := other.CpuContainerScope()
	sameHost := (host == nil && otherHost == nil) || (host != nil && otherHost != nil && *host == *otherHost)
	return otherOK && engine == otherEngine && sameHost && container == otherContainer
}

func targetLocatorCpuAccounting(l TargetLocator) CpuAccountingMode {
	if _, _, _, ok := l.CpuContainerScope(); ok {
		return CpuAccountingModeContainer
	}
	return CpuAccountingModeProcessTree
}

func targetLocatorOwnsContainer(l TargetLocator) bool {
	switch v := l.(type) {
	case TargetLocatorLocalPodman:
		return v.BorrowedFrom == nil
	case TargetLocatorLocalDocker:
		return v.BorrowedFrom == nil
	case TargetLocatorAppleContainer:
		return v.BorrowedFrom == nil
	case TargetLocatorSshPodman:
		return v.BorrowedFrom == nil
	case TargetLocatorSshDocker:
		return v.BorrowedFrom == nil
	case TargetLocatorSshAppleContainer:
		return v.BorrowedFrom == nil
	case TargetLocatorSshNitro:
		return v.BorrowedFrom == nil
	}
	return false
}

func targetLocatorSkillsScope(l TargetLocator) SkillsScope {
	if _, ok := l.(TargetLocatorLocalBare); ok {
		return SkillsScopeLocalhost
	}
	return SkillsScopeIsolated
}

func targetLocatorValidate(l TargetLocator, sessionID string) error {
	panic("unported: mj-core/src/state.rs TargetLocator::validate")
}

func targetLocatorSameResource(l, other TargetLocator) bool {
	panic("unported: mj-core/src/state.rs TargetLocator::same_resource")
}

func (v TargetLocatorLocalBare) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorLocalPodman) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorLocalDocker) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorAppleContainer) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorAwsEc2) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorSshBare) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorSshPodman) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorSshDocker) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorSshAppleContainer) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}
func (v TargetLocatorSshNitro) CpuContainerScope() (string, *string, string, bool) {
	return targetLocatorCpuContainerScope(v)
}

func (v TargetLocatorLocalBare) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorLocalPodman) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorLocalDocker) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorAppleContainer) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorAwsEc2) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorSshBare) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorSshPodman) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorSshDocker) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorSshAppleContainer) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}
func (v TargetLocatorSshNitro) SharesCpuContainer(o TargetLocator) bool {
	return targetLocatorSharesCpuContainer(v, o)
}

func (v TargetLocatorLocalBare) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorLocalPodman) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorLocalDocker) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorAppleContainer) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorAwsEc2) CpuAccounting() CpuAccountingMode { return targetLocatorCpuAccounting(v) }
func (v TargetLocatorSshBare) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorSshPodman) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorSshDocker) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorSshAppleContainer) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}
func (v TargetLocatorSshNitro) CpuAccounting() CpuAccountingMode {
	return targetLocatorCpuAccounting(v)
}

func (v TargetLocatorLocalBare) OwnsContainer() bool         { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorLocalPodman) OwnsContainer() bool       { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorLocalDocker) OwnsContainer() bool       { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorAppleContainer) OwnsContainer() bool    { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorAwsEc2) OwnsContainer() bool            { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorSshBare) OwnsContainer() bool           { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorSshPodman) OwnsContainer() bool         { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorSshDocker) OwnsContainer() bool         { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorSshAppleContainer) OwnsContainer() bool { return targetLocatorOwnsContainer(v) }
func (v TargetLocatorSshNitro) OwnsContainer() bool          { return targetLocatorOwnsContainer(v) }

func (v TargetLocatorLocalBare) SkillsScope() SkillsScope         { return targetLocatorSkillsScope(v) }
func (v TargetLocatorLocalPodman) SkillsScope() SkillsScope       { return targetLocatorSkillsScope(v) }
func (v TargetLocatorLocalDocker) SkillsScope() SkillsScope       { return targetLocatorSkillsScope(v) }
func (v TargetLocatorAppleContainer) SkillsScope() SkillsScope    { return targetLocatorSkillsScope(v) }
func (v TargetLocatorAwsEc2) SkillsScope() SkillsScope            { return targetLocatorSkillsScope(v) }
func (v TargetLocatorSshBare) SkillsScope() SkillsScope           { return targetLocatorSkillsScope(v) }
func (v TargetLocatorSshPodman) SkillsScope() SkillsScope         { return targetLocatorSkillsScope(v) }
func (v TargetLocatorSshDocker) SkillsScope() SkillsScope         { return targetLocatorSkillsScope(v) }
func (v TargetLocatorSshAppleContainer) SkillsScope() SkillsScope { return targetLocatorSkillsScope(v) }
func (v TargetLocatorSshNitro) SkillsScope() SkillsScope          { return targetLocatorSkillsScope(v) }

func (v TargetLocatorLocalBare) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorLocalPodman) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorLocalDocker) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorAppleContainer) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorAwsEc2) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorSshBare) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorSshPodman) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorSshDocker) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorSshAppleContainer) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}
func (v TargetLocatorSshNitro) SameResource(o TargetLocator) bool {
	return targetLocatorSameResource(v, o)
}

func (v TargetLocatorLocalBare) validate(id string) error      { return targetLocatorValidate(v, id) }
func (v TargetLocatorLocalPodman) validate(id string) error    { return targetLocatorValidate(v, id) }
func (v TargetLocatorLocalDocker) validate(id string) error    { return targetLocatorValidate(v, id) }
func (v TargetLocatorAppleContainer) validate(id string) error { return targetLocatorValidate(v, id) }
func (v TargetLocatorAwsEc2) validate(id string) error         { return targetLocatorValidate(v, id) }
func (v TargetLocatorSshBare) validate(id string) error        { return targetLocatorValidate(v, id) }
func (v TargetLocatorSshPodman) validate(id string) error      { return targetLocatorValidate(v, id) }
func (v TargetLocatorSshDocker) validate(id string) error      { return targetLocatorValidate(v, id) }
func (v TargetLocatorSshAppleContainer) validate(id string) error {
	return targetLocatorValidate(v, id)
}
func (v TargetLocatorSshNitro) validate(id string) error { return targetLocatorValidate(v, id) }

// ManagedWorktreeTarget is where a managed worktree lives. Tag "kind", kebab-case.
type ManagedWorktreeTarget interface {
	isManagedWorktreeTarget()
	// SameLocation reports whether other reaches the same checkout: the same
	// kind and, over SSH, the same destination, port, and login user.
	//
	// The other ssh options (keys, ControlPath, keepalives, host-key policy)
	// say how to connect, not where the worktree lives, so they follow the
	// machine's current configuration and never make a suspended session
	// unable to resume.
	SameLocation(other ManagedWorktreeTarget) bool
}

type ManagedWorktreeTargetLocal struct{}

type ManagedWorktreeTargetSsh struct {
	Destination string   `json:"destination"`
	SSHArgs     []string `json:"ssh_args,omitempty"`
}

func (ManagedWorktreeTargetLocal) isManagedWorktreeTarget() {}
func (ManagedWorktreeTargetSsh) isManagedWorktreeTarget()   {}

func (ManagedWorktreeTargetLocal) SameLocation(other ManagedWorktreeTarget) bool {
	_, ok := other.(ManagedWorktreeTargetLocal)
	return ok
}

func (v ManagedWorktreeTargetSsh) SameLocation(other ManagedWorktreeTarget) bool {
	o, ok := other.(ManagedWorktreeTargetSsh)
	if !ok || v.Destination != o.Destination {
		return false
	}
	port, hasPort := sshLocationOption(v.SSHArgs, 'p', "port")
	otherPort, otherHasPort := sshLocationOption(o.SSHArgs, 'p', "port")
	user, hasUser := sshLocationOption(v.SSHArgs, 'l', "user")
	otherUser, otherHasUser := sshLocationOption(o.SSHArgs, 'l', "user")
	return port == otherPort && hasPort == otherHasPort && user == otherUser && hasUser == otherHasUser
}

func (ManagedWorktreeTargetLocal) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("kind", "local", nil)
}

func (v ManagedWorktreeTargetSsh) MarshalJSON() ([]byte, error) {
	type plain ManagedWorktreeTargetSsh
	return stateMarshalTagged("kind", "ssh", plain(v))
}

func unmarshalManagedWorktreeTarget(data []byte) (ManagedWorktreeTarget, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "local":
		return ManagedWorktreeTargetLocal{}, nil
	case "ssh":
		return stateVariant[ManagedWorktreeTargetSsh, ManagedWorktreeTarget](data)
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// sshLocationOption is the value ssh would use for an option that has both
// a short flag (-p 22, -p22) and an -o spelling (-o Port=22, -oPort 22).
// OpenSSH keeps the first value it sees.
func sshLocationOption(args []string, flag rune, option string) (string, bool) {
	panic("unported: mj-core/src/state.rs ssh_location_option")
}

// ManagedWorktreeOptions reports whether a selected project can create a
// session-owned Git checkout.
type ManagedWorktreeOptions struct {
	Available     bool `json:"available"`
	DefaultCreate bool `json:"default_create"`
}

func (o *ManagedWorktreeOptions) UnmarshalJSON(data []byte) error {
	type plain ManagedWorktreeOptions
	return stateDecodeStrict(data, (*plain)(o))
}

type ManagedWorktree struct {
	// Kind: old records are linked worktrees. New isolated raw sessions own
	// a clone. Omitted when it is a worktree.
	Kind                   ManagedCheckoutKind   `json:"kind,omitempty"`
	SourceProjectDirectory string                `json:"source_project_directory"`
	SourceRepository       string                `json:"source_repository"`
	WorktreeRoot           string                `json:"worktree_root"`
	Branch                 string                `json:"branch"`
	Target                 ManagedWorktreeTarget `json:"target"`
	// BaseCommit is the commit the session branch was created at. Recorded
	// so an export can diff against it in one read; sessions created before
	// this field existed fall back to the branch reflog, which expires.
	BaseCommit *string `json:"base_commit,omitempty"`
}

// MarshalJSON omits a worktree kind, as skip_serializing_if = is_worktree does.
func (w ManagedWorktree) MarshalJSON() ([]byte, error) {
	type plain ManagedWorktree
	if w.Kind.isWorktree() {
		w.Kind = ""
	}
	return json.Marshal(plain(w))
}

func (w *ManagedWorktree) UnmarshalJSON(data []byte) error {
	type plain ManagedWorktree
	aux := struct {
		*plain
		Target stateUnionJSON[ManagedWorktreeTarget] `json:"target"`
	}{plain: (*plain)(w)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	if w.Kind == "" {
		w.Kind = ManagedCheckoutKindWorktree
	}
	if aux.Target.V == nil {
		return fmt.Errorf("missing field `target`")
	}
	w.Target = aux.Target.V
	return nil
}

type ManagedCheckoutKind string

const (
	ManagedCheckoutKindWorktree ManagedCheckoutKind = "worktree"
	ManagedCheckoutKindClone    ManagedCheckoutKind = "clone"
)

func (k ManagedCheckoutKind) isWorktree() bool {
	return k == ManagedCheckoutKindWorktree || k == ""
}

func (w *ManagedWorktree) validate(sessionID string, projectDirectory *string) error {
	panic("unported: mj-core/src/state.rs ManagedWorktree::validate")
}

// SessionResourceAllocation is a session's requested size. Tag "kind",
// kebab-case.
type SessionResourceAllocation interface {
	isSessionResourceAllocation()
	Validate() error
	// ValidateMinimumUsefulContainer refuses a new container allocation that
	// is too small to be useful.
	//
	// This is deliberately separate from Validate, which also runs while
	// loading durable records. A store written by an older Mjolnir may
	// contain a smaller live allocation, and loading it must remain
	// compatible. Only a new admission applies this floor.
	ValidateMinimumUsefulContainer() error
}

type SessionResourceAllocationContainer struct {
	Cpus        uint64 `json:"cpus"`
	MemoryBytes uint64 `json:"memory_bytes"`
}

type SessionResourceAllocationAwsEc2 struct {
	InstanceType string `json:"instance_type"`
	Vcpus        uint64 `json:"vcpus"`
	MemoryBytes  uint64 `json:"memory_bytes"`
}

func (SessionResourceAllocationContainer) isSessionResourceAllocation() {}
func (SessionResourceAllocationAwsEc2) isSessionResourceAllocation()    {}

func (v SessionResourceAllocationContainer) Validate() error {
	if v.Cpus == 0 || v.MemoryBytes == 0 {
		return fmt.Errorf("container resource allocation must have non-zero CPU and memory")
	}
	return nil
}

func (v SessionResourceAllocationAwsEc2) Validate() error {
	if strings.TrimSpace(v.InstanceType) == "" || v.Vcpus == 0 || v.MemoryBytes == 0 {
		return fmt.Errorf("EC2 resource allocation must have an instance type, CPU, and memory")
	}
	return nil
}

func (v SessionResourceAllocationContainer) ValidateMinimumUsefulContainer() error {
	if v.Cpus < MinimumContainerCpus {
		return fmt.Errorf("container sessions need at least %d CPUs", MinimumContainerCpus)
	}
	if v.MemoryBytes < MinimumContainerMemoryBytes {
		return fmt.Errorf("container sessions need at least %d GiB of RAM", MinimumContainerMemoryBytes/(1024*1024*1024))
	}
	return nil
}

func (SessionResourceAllocationAwsEc2) ValidateMinimumUsefulContainer() error { return nil }

func (v SessionResourceAllocationContainer) MarshalJSON() ([]byte, error) {
	type plain SessionResourceAllocationContainer
	return stateMarshalTagged("kind", "container", plain(v))
}

func (v SessionResourceAllocationAwsEc2) MarshalJSON() ([]byte, error) {
	type plain SessionResourceAllocationAwsEc2
	return stateMarshalTagged("kind", "aws-ec2", plain(v))
}

func unmarshalSessionResourceAllocation(data []byte) (SessionResourceAllocation, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "container":
		return stateVariant[SessionResourceAllocationContainer, SessionResourceAllocation](data)
	case "aws-ec2":
		return stateVariant[SessionResourceAllocationAwsEc2, SessionResourceAllocation](data)
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// ResourceAllocationKind is the resource sizing supported by a target template.
type ResourceAllocationKind string

const (
	ResourceAllocationKindFixed     ResourceAllocationKind = "fixed"
	ResourceAllocationKindContainer ResourceAllocationKind = "container"
	ResourceAllocationKindAwsEc2    ResourceAllocationKind = "aws-ec2"
)

// AllocationCpus is the CPU count an allocation grants, regardless of target kind.
func AllocationCpus(allocation SessionResourceAllocation) uint64 {
	switch v := allocation.(type) {
	case SessionResourceAllocationContainer:
		return v.Cpus
	case SessionResourceAllocationAwsEc2:
		return v.Vcpus
	}
	return 0
}

// AllocationMemory is the memory, in bytes, an allocation grants, regardless
// of target kind.
func AllocationMemory(allocation SessionResourceAllocation) uint64 {
	switch v := allocation.(type) {
	case SessionResourceAllocationContainer:
		return v.MemoryBytes
	case SessionResourceAllocationAwsEc2:
		return v.MemoryBytes
	}
	return 0
}

type CheckpointMetadata struct {
	ArchivePath string `json:"archive_path"`
	// Sha256 is the lowercase SHA-256 digest of the verified archive.
	Sha256        string `json:"sha256"`
	CreatedAt     string `json:"created_at"`
	EventFrontier uint64 `json:"event_frontier"`
}

func (m *CheckpointMetadata) UnmarshalJSON(data []byte) error {
	type plain CheckpointMetadata
	return stateDecodeStrict(data, (*plain)(m))
}

// PublicationState reports whether every saved Git change has a verified
// durable copy outside mj.
type PublicationState string

const (
	PublicationStatePublished   PublicationState = "published"
	PublicationStateUnpublished PublicationState = "unpublished"
	PublicationStateUnknown     PublicationState = "unknown"
)

// PublicationAssessment is evidence for one exact checkpoint. A newer
// checkpoint invalidates it.
type PublicationAssessment struct {
	CheckpointSha256 string           `json:"checkpoint_sha256"`
	State            PublicationState `json:"state"`
	Dirty            bool             `json:"dirty"`
	Stashed          bool             `json:"stashed"`
	SavedCommits     []string         `json:"saved_commits"`
	Destinations     []string         `json:"destinations"`
	CheckedAt        string           `json:"checked_at"`
	Reason           *string          `json:"reason"`
}

func (m *CheckpointMetadata) validate() error {
	if m.ArchivePath == "" {
		return fmt.Errorf("checkpoint archive path is empty")
	}
	valid := len(m.Sha256) == 64
	for i := 0; valid && i < len(m.Sha256); i++ {
		c := m.Sha256[i]
		valid = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')
	}
	if !valid {
		return fmt.Errorf("checkpoint SHA-256 must be 64 lowercase hexadecimal characters")
	}
	if strings.TrimSpace(m.CreatedAt) == "" {
		return fmt.Errorf("checkpoint timestamp is empty")
	}
	return nil
}

// SessionBuildCache is the mbx build cache a container session was
// provisioned with. The directory is a host path mounted read-write at the
// same absolute path inside the container. Older containers retain their
// private binaries until recreated; the worker's bin/mbx contents identify
// that legacy scheme.
type SessionBuildCache struct {
	// Host is the container host this cache was resolved on. A session moved
	// to a different host cannot reuse it, so the decision is made again there.
	Host      string `json:"host"`
	Directory string `json:"directory"`
	// MaxSize is read compatibility for old records only. Launch never uses
	// this value; budgets belong to the shared machine configuration.
	MaxSize *string `json:"max_size,omitempty"`
	// TargetRoot is a [target] root the host's mbx configuration relocates
	// outside the cache directory, mounted read-write at the same path as well.
	TargetRoot *string `json:"target_root,omitempty"`
}

func (c *SessionBuildCache) UnmarshalJSON(data []byte) error {
	type plain SessionBuildCache
	return stateDecodeStrict(data, (*plain)(c))
}

// ArchiveSpacePreview is how much disk Mjolnir's own copies of sessions use,
// and how much an archive_after_days value would free. Settings shows this
// on the SessionWiki page so the effect of a value is visible before it is
// saved.
type ArchiveSpacePreview struct {
	// Sessions is every session record Mjolnir holds, archived or not.
	Sessions int `json:"sessions"`
	// Bytes is what those sessions' checkpoints and attachments occupy.
	Bytes uint64 `json:"bytes"`
	// ReclaimableSessions and ReclaimableBytes are the sessions an
	// archive_after_days value would catch, and their share of Bytes. Both
	// are zero when no value is set.
	ReclaimableSessions int    `json:"reclaimable_sessions"`
	ReclaimableBytes    uint64 `json:"reclaimable_bytes"`
}

// BuildCachePreview is what a container target's host resolves for its
// blank build cache settings right now. Settings shows this beside each
// "automatic" field so the values a session would actually run with are
// visible before one starts.
type BuildCachePreview struct {
	// NativeMbx is the host's own mbx version, or nil when it has none on PATH.
	NativeMbx *string `json:"native_mbx"`
	// MbxProfileFile is the login profile the host would update when mbx is installed.
	MbxProfileFile *string `json:"mbx_profile_file"`
	// MbxProfileWarning is why an unrecognized login shell may not read the
	// selected profile.
	MbxProfileWarning *string `json:"mbx_profile_warning"`
	// MbxManualPathLine is a literal POSIX PATH line for users whose shell
	// does not read the selected profile.
	MbxManualPathLine *string `json:"mbx_manual_path_line"`
	// Directory is the cache directory sessions would mount, once known.
	Directory *string `json:"directory"`
	// MaxTotalSize is the budget sessions would run with, once known.
	MaxTotalSize BuildCacheLimit `json:"max_total_size"`
	// UserManaged is true when a general-purpose host installation owns the
	// configuration.
	UserManaged bool                  `json:"user_managed"`
	Application BuildCacheApplication `json:"application"`
	// BudgetNote is an explanation of the shared storage budget.
	BudgetNote *string `json:"budget_note"`
	// Stats is what the cache on that host has done so far, when it has a tally.
	Stats *BuildCacheStats `json:"stats"`
	// OffReason is why sessions on this target run without a cache, or nil
	// when they share one.
	OffReason BuildCacheOff `json:"off_reason"`
}

func (p *BuildCachePreview) UnmarshalJSON(data []byte) error {
	type plain BuildCachePreview
	aux := struct {
		*plain
		MaxTotalSize stateUnionJSON[BuildCacheLimit]       `json:"max_total_size"`
		Application  stateUnionJSON[BuildCacheApplication] `json:"application"`
		OffReason    stateUnionJSON[BuildCacheOff]         `json:"off_reason"`
	}{plain: (*plain)(p)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	p.MaxTotalSize, p.Application, p.OffReason = aux.MaxTotalSize.V, aux.Application.V, aux.OffReason.V
	return nil
}

// BuildCacheStats are mbx's own running totals for one host's cache, read
// from the tally beside the store.
//
// These are machine-wide and cumulative: every session on that host and any
// native builds the user ran themselves are counted together, since they
// share one cache. They answer whether the cache is being used at all, not
// what one session got out of it.
type BuildCacheStats struct {
	// Builds that went through mbx. Zero means nothing has used the shim.
	Builds uint64 `json:"builds"`
	// CachedCompilations answered from the cache instead of run.
	CachedCompilations uint64 `json:"cached_compilations"`
	// AvoidedCompilerNs is the compiler time those answers avoided, as mbx
	// estimates it.
	AvoidedCompilerNs uint64 `json:"avoided_compiler_ns"`
	// ReflinkedBytes are restored output bytes that were cloned rather than copied.
	ReflinkedBytes uint64 `json:"reflinked_bytes"`
}

// BuildCacheOff is why a target's sessions run without the build cache.
// Externally tagged with Rust variant names.
type BuildCacheOff interface {
	isBuildCacheOff()
	String() string
}

// BuildCacheOffTurnedOff is this machine's own enabled = false.
type BuildCacheOffTurnedOff struct{}

// BuildCacheOffUnavailable: nothing on the machine's settings page can turn
// it on: the global switch, the host's mbx, or its filesystem. The text
// says which.
type BuildCacheOffUnavailable struct{ Value string }

// BuildCacheOffUnsupportedPlatform: the host's platform keeps its containers
// from sharing its mbx, so Mjolnir neither configures nor installs one
// there. On macOS the preview's NativeMbx is the one bare sessions on the
// Mac use. The text says what the host's own mbx still serves.
type BuildCacheOffUnsupportedPlatform struct{ Value string }

func (BuildCacheOffTurnedOff) isBuildCacheOff()           {}
func (BuildCacheOffUnavailable) isBuildCacheOff()         {}
func (BuildCacheOffUnsupportedPlatform) isBuildCacheOff() {}

func (BuildCacheOffTurnedOff) String() string             { return "turned off for this machine" }
func (v BuildCacheOffUnavailable) String() string         { return v.Value }
func (v BuildCacheOffUnsupportedPlatform) String() string { return v.Value }

func (BuildCacheOffTurnedOff) MarshalJSON() ([]byte, error) { return json.Marshal("TurnedOff") }

func (v BuildCacheOffUnavailable) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("Unavailable", v.Value)
}

func (v BuildCacheOffUnsupportedPlatform) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("UnsupportedPlatform", v.Value)
}

func unmarshalBuildCacheOff(data []byte) (BuildCacheOff, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	var text string
	switch {
	case tag == "TurnedOff" && content == nil:
		return BuildCacheOffTurnedOff{}, nil
	case tag == "Unavailable" && content != nil:
		err = json.Unmarshal(content, &text)
		return BuildCacheOffUnavailable{text}, err
	case tag == "UnsupportedPlatform" && content != nil:
		err = json.Unmarshal(content, &text)
		return BuildCacheOffUnsupportedPlatform{text}, err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// BuildCacheLimit is where a machine build cache budget comes from.
// Externally tagged with Rust variant names.
type BuildCacheLimit interface {
	isBuildCacheLimit()
}

// BuildCacheLimitSize is an explicit mj machine setting written to shared
// mbx configuration.
type BuildCacheLimitSize struct{ Value string }

// BuildCacheLimitHostConfiguration: the host's own ~/.config/mbx/config.toml
// carries the budget. The total it sets, when it sets one.
type BuildCacheLimitHostConfiguration struct{ Value *string }

// BuildCacheLimitMjDefault is an automatic total initialized by mj,
// independent of later disk growth.
type BuildCacheLimitMjDefault struct{ Value string }

// BuildCacheLimitMbxDefault is the host's native mbx default, or no combined limit.
type BuildCacheLimitMbxDefault struct{ Value *string }

func (BuildCacheLimitSize) isBuildCacheLimit()              {}
func (BuildCacheLimitHostConfiguration) isBuildCacheLimit() {}
func (BuildCacheLimitMjDefault) isBuildCacheLimit()         {}
func (BuildCacheLimitMbxDefault) isBuildCacheLimit()        {}

func (v BuildCacheLimitSize) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("Size", v.Value)
}

func (v BuildCacheLimitHostConfiguration) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("HostConfiguration", v.Value)
}

func (v BuildCacheLimitMjDefault) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("MjDefault", v.Value)
}

func (v BuildCacheLimitMbxDefault) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("MbxDefault", v.Value)
}

func unmarshalBuildCacheLimit(data []byte) (BuildCacheLimit, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	if content == nil {
		return nil, fmt.Errorf("unknown variant `%s`", tag)
	}
	var text string
	var optional *string
	switch tag {
	case "Size":
		err = json.Unmarshal(content, &text)
		return BuildCacheLimitSize{text}, err
	case "HostConfiguration":
		err = json.Unmarshal(content, &optional)
		return BuildCacheLimitHostConfiguration{optional}, err
	case "MjDefault":
		err = json.Unmarshal(content, &text)
		return BuildCacheLimitMjDefault{text}, err
	case "MbxDefault":
		err = json.Unmarshal(content, &optional)
		return BuildCacheLimitMbxDefault{optional}, err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// BuildCacheApplication is whether the shared build cache configuration was
// applied on the host. Externally tagged with Rust variant names; the Rust
// default is Pending.
type BuildCacheApplication interface {
	isBuildCacheApplication()
}

type BuildCacheApplicationPending struct{}

type BuildCacheApplicationApplied struct{}

type BuildCacheApplicationFailed struct{ Value string }

func (BuildCacheApplicationPending) isBuildCacheApplication() {}
func (BuildCacheApplicationApplied) isBuildCacheApplication() {}
func (BuildCacheApplicationFailed) isBuildCacheApplication()  {}

func (BuildCacheApplicationPending) MarshalJSON() ([]byte, error) { return json.Marshal("Pending") }
func (BuildCacheApplicationApplied) MarshalJSON() ([]byte, error) { return json.Marshal("Applied") }

func (v BuildCacheApplicationFailed) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("Failed", v.Value)
}

func unmarshalBuildCacheApplication(data []byte) (BuildCacheApplication, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	switch {
	case tag == "Pending" && content == nil:
		return BuildCacheApplicationPending{}, nil
	case tag == "Applied" && content == nil:
		return BuildCacheApplicationApplied{}, nil
	case tag == "Failed" && content != nil:
		var text string
		err = json.Unmarshal(content, &text)
		return BuildCacheApplicationFailed{text}, err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

type SessionRecord struct {
	ID string `json:"id"`
	// WorkspaceID is the owning workspace while active, or the most recent
	// workspace while inactive.
	//
	// Inactive histories are globally resumable, so this id may refer to a
	// workspace that has since been deleted. Defaults to DefaultWorkspaceID.
	WorkspaceID string      `json:"workspace_id"`
	Title       string      `json:"title"`
	HarnessKind HarnessKind `json:"harness_kind"`
	LastProfile string      `json:"last_profile"`
	BundleID    string      `json:"bundle_id"`
	// Project is the accepted bundle for every target, including a raw
	// directory's bundle of one.
	Project *ProjectBundleSnapshot `json:"project,omitempty"`
	// ProjectDirectory is an existing project directory used directly by a
	// local or SSH bare target.
	ProjectDirectory *string `json:"project_directory,omitempty"`
	// ManagedWorktree is the Git worktree created and owned by Hel for this
	// raw-project session.
	ManagedWorktree *ManagedWorktree `json:"managed_worktree,omitempty"`
	// CreateManagedWorktree: nil preserves automatic selection; false uses
	// the selected directory.
	CreateManagedWorktree *bool `json:"create_managed_worktree,omitempty"`
	// LaunchBase is the diff baseline as supplied by the caller (the API's
	// base). A raw managed worktree also starts here; a bundle checkout keeps
	// its selected remote branch tip. With checkout, it applies to the
	// checked-out repository only, and when absent that repository's base is
	// the checkout commit. The resolved baseline lands in
	// managed_worktree.base_commit or the clone's mj.baseCommit.
	LaunchBase *string `json:"launch_base,omitempty"`
	// LaunchBranch is an existing branch to check out in a new isolated
	// workspace (the API's branch without at). Never set together with
	// checkout, which carries its own branch.
	LaunchBranch *string `json:"launch_branch,omitempty"`
	// Checkout is the immutable exact starting selection for one bundle
	// repository, built from the API's at and branch. Resume preserves
	// checkpointed work rather than applying this selection again.
	Checkout *ExactCheckout `json:"checkout,omitempty"`
	// Publication is the last verified publication verdict, tied to its
	// checkpoint digest.
	Publication *PublicationAssessment `json:"publication,omitempty"`
	// Subagents is the stored delegation policy. Historical records without
	// a choice use native delegation. Alias "mjolnir_subagents"; decoded with
	// deserializeOptionalPolicy.
	Subagents *SubagentPolicy `json:"subagents"`
	// Oracle is whether this session may consult an oracle when one is
	// configured. Records written before the choice existed allow it.
	Oracle bool `json:"oracle"`
	// OracleChoice is this session's own oracle model (--subagent-oracle),
	// overriding [subagents.oracle] or [subagents.yolo.oracle].
	OracleChoice *SubagentChoice `json:"oracle_choice,omitempty"`
	// Review is the turn review chosen for this session when it was created.
	// Nil follows the global [review] section.
	Review *SessionReview `json:"review,omitempty"`
	// NoProjectMemory is chosen when the session is created (mj new
	// --no-project-memory): the session starts with no project memory. The
	// worker gets no memory directory, no memory index in its context and no
	// history tools, the controller does not stage or synchronize a memory
	// replica, and sub-agent children inherit the choice. Kept on resume and
	// Move.
	NoProjectMemory bool `json:"no_project_memory,omitempty"`
	// NoGithubEvents excludes this session from GitHub watcher delivery
	// targets. Sub-agent children inherit this choice; kept on resume and
	// Move. Reads legacy saved session JSON under alias "no_mailbox";
	// creation requests have no old-name alias.
	NoGithubEvents bool `json:"no_github_events,omitempty"`
	// NoGithubCredentials withholds GitHub credentials; inherited by children.
	NoGithubCredentials bool                      `json:"no_github_credentials,omitempty"`
	TargetTemplateID    string                    `json:"target_template_id"`
	ResourceAllocation  SessionResourceAllocation `json:"resource_allocation,omitempty"`
	AdditionalMounts    []AdditionalMount         `json:"additional_mounts,omitempty"`
	// ContainerWorkspace is the in-container workspace root this session's
	// repositories live under. Nil is a session whose container predates
	// per-session workspaces and therefore keeps the shared legacy
	// /workspace; every session created since records /workspace/<session
	// id>, so two checkouts of one project on a host never share an absolute
	// path.
	ContainerWorkspace *string `json:"container_workspace,omitempty"`
	// BuildCache is the mbx build cache this session's container runs with,
	// decided once at provisioning. Nil means the session runs without a
	// build cache; resume, move, and sub-agent children reuse the recorded
	// value.
	BuildCache *SessionBuildCache `json:"build_cache,omitempty"`
	State      SessionState       `json:"state"`
	// Archived is a legacy visibility preference, retained for record
	// compatibility. Current surfaces do not hide sessions based on this flag.
	Archived bool `json:"archived,omitempty"`
	// NotificationsIgnored: the session raises no notifications: no bell,
	// desktop, or push episode. Its attention and unread state still render
	// normally.
	NotificationsIgnored bool          `json:"notifications_ignored,omitempty"`
	Target               TargetLocator `json:"target,omitempty"`
	// TargetRuntime holds connection and worker settings captured when this
	// target was selected.
	TargetRuntime        *TargetRuntimeSettings `json:"target_runtime,omitempty"`
	NativeSessionID      *string                `json:"native_session_id,omitempty"`
	ACPSessionTitle      *string                `json:"acp_session_title,omitempty"`
	SessionTitleOverride *string                `json:"session_title_override,omitempty"`
	CreatedAt            string                 `json:"created_at"`
	UpdatedAt            string                 `json:"updated_at"`
	// ViewedThroughEventOrdinal has alias "detached_after_event_ordinal".
	ViewedThroughEventOrdinal uint64 `json:"viewed_through_event_ordinal"`
	// DraftInput is unsent chat input carried across a detach, so returning
	// to a session restores what the user was typing. Empty means no draft.
	DraftInput string `json:"draft_input,omitempty"`
	// LastError is why the last operation on this session failed.
	//
	// This is usually a raw controller error chain, which names profile
	// homes, project paths and SSH hosts, so a public projection publishes it
	// only for a session that is stopped or failed. The one exception is a
	// sentence the controller composed for the person; see CloseFailurePrefix.
	LastError *string `json:"last_error,omitempty"`
	// FailureGeneration is the database-owned failure occurrence; omitted
	// from portable session formats.
	FailureGeneration uint64 `json:"-"`
	// DismissedFailure is the database-owned dismissal receipt.
	DismissedFailure    *FailureIdentity    `json:"-"`
	LastCheckpointError *string             `json:"last_checkpoint_error,omitempty"`
	Checkpoint          *CheckpointMetadata `json:"checkpoint,omitempty"`
}

func (r *SessionRecord) UnmarshalJSON(data []byte) error {
	data, err := stateRenameAliases(data, map[string]string{
		"mjolnir_subagents":            "subagents",
		"no_mailbox":                   "no_github_events",
		"detached_after_event_ordinal": "viewed_through_event_ordinal",
	})
	if err != nil {
		return err
	}
	type plain SessionRecord
	aux := struct {
		*plain
		Subagents          json.RawMessage                           `json:"subagents"`
		ResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"resource_allocation"`
		Target             stateUnionJSON[TargetLocator]             `json:"target"`
	}{plain: (*plain)(r)}
	r.WorkspaceID = defaultSessionWorkspaceID()
	r.Oracle = defaultOracleEnabled()
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	r.ResourceAllocation = aux.ResourceAllocation.V
	r.Target = aux.Target.V
	r.Subagents = nil
	if aux.Subagents != nil {
		policy, err := deserializeOptionalPolicy(aux.Subagents)
		if err != nil {
			return err
		}
		r.Subagents = policy
	}
	return nil
}

type HostContainerSize struct {
	Cpus        uint64 `json:"cpus"`
	MemoryBytes uint64 `json:"memory_bytes"`
}

func (s *HostContainerSize) UnmarshalJSON(data []byte) error {
	type plain HostContainerSize
	return stateDecodeStrict(data, (*plain)(s))
}

// HostContainerCapacity is the capacity committed to containers on one
// physical host.
//
// Committed sums owning container limits, never borrowed sub-agent
// containers. Remaining is the admission budget left after the host reserve
// and those commitments. Complete is false when an active owner container
// has no recorded allocation; admission must not mistake that unknown usage
// for zero.
type HostContainerCapacity struct {
	Total     HostContainerSize `json:"total"`
	Reserved  HostContainerSize `json:"reserved"`
	Committed HostContainerSize `json:"committed"`
	Remaining HostContainerSize `json:"remaining"`
	Complete  bool              `json:"complete"`
}

func (c *HostContainerCapacity) UnmarshalJSON(data []byte) error {
	type plain HostContainerCapacity
	return stateDecodeStrict(data, (*plain)(c))
}

const (
	BaselineContainerCpus         uint64 = 4
	BaselineContainerMemoryBytes  uint64 = 8 * 1024 * 1024 * 1024
	MinimumContainerCpus          uint64 = 2
	MinimumContainerMemoryBytes   uint64 = 4 * 1024 * 1024 * 1024
	minimumHostMemoryReserveBytes uint64 = 2 * 1024 * 1024 * 1024
	gibBytes                      uint64 = 1024 * 1024 * 1024
)

// FloorGibTenths formats bytes as GiB rounded down to one decimal place.
//
// Capacity messages use the same string as their hard limit so a value
// shown as available is always accepted when entered back.
func FloorGibTenths(bytes uint64) string {
	panic("unported: mj-core/src/state.rs floor_gib_tenths")
}

// ReservedContainerCapacity reserves one fifth of RAM for the host operating
// system and other processes. CPU stay fully schedulable; this change sums
// CPU commitments against the logical core count rather than reserving idle
// cores.
func ReservedContainerCapacity(total HostContainerSize) HostContainerSize {
	return HostContainerSize{Cpus: 0, MemoryBytes: max(total.MemoryBytes/5, minimumHostMemoryReserveBytes)}
}

// SessionOwnsContainer reports whether an active session owns a container
// whose CPU and RAM are charged to its deployment host. A borrowed child
// runs inside its parent's limit.
func SessionOwnsContainer(session *SessionRecord) bool {
	if !session.State.IsActive() {
		return false
	}
	if session.Target != nil {
		return session.Target.OwnsContainer()
	}
	return session.State == SessionStateProvisioning || session.State == SessionStateStartupCleanup
}

// AppliedContainerSettings is the creation settings from the applied
// snapshot, or the legacy request when no snapshot was recorded. A present
// snapshot is authoritative even when its size is unknown.
type AppliedContainerSettings struct {
	ResourceAllocation SessionResourceAllocation
	AdditionalMounts   []AdditionalMount
}

// AppliedContainerSettingsOf is state.rs applied_container_settings (renamed:
// the function and its result type share a name).
func AppliedContainerSettingsOf(session *SessionRecord) AppliedContainerSettings {
	if session.TargetRuntime != nil && session.TargetRuntime.ContainerSettings != nil {
		applied := session.TargetRuntime.ContainerSettings
		return AppliedContainerSettings{applied.ResourceAllocation, applied.AdditionalMounts}
	}
	return AppliedContainerSettings{session.ResourceAllocation, session.AdditionalMounts}
}

// CountedContainerAllocation is the size a session's container is charged
// at, including unknown sizes (nil).
func CountedContainerAllocation(session *SessionRecord) SessionResourceAllocation {
	if session.Target == nil {
		// Provisioning reserves the next creation, even if a prior snapshot remains.
		return session.ResourceAllocation
	}
	return AppliedContainerSettingsOf(session).ResourceAllocation
}

// ComputeHostContainerCapacity is state.rs host_container_capacity (renamed:
// the function and its result type share a name). It sums the owning
// container allocations for one physical host.
//
// A new or resuming owner has no target locator while provisioning, so
// those states reserve their requested size. Once a target exists, a
// container child whose locator names a parent is skipped because it
// already consumes the parent's allocation.
func ComputeHostContainerCapacity(total HostContainerSize, memoryIsPool bool, targetIDs []string, sessions []*SessionRecord) HostContainerCapacity {
	panic("unported: mj-core/src/state.rs host_container_capacity")
}

// Refuses reports whether admission must refuse this allocation. CPU is
// always allowed to overcommit. RAM refuses only where memoryIsHardLimit
// (see ContainerMemoryIsHardLimit); there an incomplete budget also
// refuses, because unknown usage must not count as zero.
func (c *HostContainerCapacity) Refuses(allocation SessionResourceAllocation, memoryIsHardLimit bool) bool {
	container, ok := allocation.(SessionResourceAllocationContainer)
	if !ok {
		return true
	}
	return memoryIsHardLimit && (!c.Complete || c.Remaining.MemoryBytes < container.MemoryBytes)
}

// RefusalMessage is why Refuses refused, for the person.
func (c *HostContainerCapacity) RefusalMessage(allocation SessionResourceAllocation) string {
	panic("unported: mj-core/src/state.rs HostContainerCapacity::refusal_message")
}

// OvercommitWarning is the one-sentence warning a launch surface shows when
// this allocation asks for more RAM than is left, or false when it fits. CPU
// never warns: it is shared by design and always overcommits. An incomplete
// budget has nothing left, so it warns that the RAM left is unknown; a
// surface that already says so itself skips it.
func (c *HostContainerCapacity) OvercommitWarning(allocation SessionResourceAllocation) (string, bool) {
	panic("unported: mj-core/src/state.rs HostContainerCapacity::overcommit_warning")
}

// ClampContainerSize clamps a container size to a host reading, preserving
// the TUI's minimum of one CPU and one byte when a host reports zero totals.
func ClampContainerSize(size HostContainerSize, limits *HostContainerSize) HostContainerSize {
	if limits == nil {
		return HostContainerSize{Cpus: max(size.Cpus, 1), MemoryBytes: max(size.MemoryBytes, 1)}
	}
	return HostContainerSize{
		Cpus:        min(size.Cpus, max(limits.Cpus, 1)),
		MemoryBytes: min(size.MemoryBytes, max(limits.MemoryBytes, 1)),
	}
}

// DefaultContainerSize is the default selected for a new container target:
// its remembered host size, or 4 CPUs / 8 GiB, clamped to the latest host
// totals when available.
func DefaultContainerSize(remembered, limits *HostContainerSize) HostContainerSize {
	size := HostContainerSize{Cpus: BaselineContainerCpus, MemoryBytes: BaselineContainerMemoryBytes}
	if remembered != nil {
		size = *remembered
	}
	return ClampContainerSize(size, limits)
}

func defaultSessionWorkspaceID() string { return DefaultWorkspaceID }

func defaultOracleEnabled() bool { return true }

// OracleUnavailableNotice is what a session's creator is told when an
// explicit request for the oracle could not be honored.
const OracleUnavailableNotice = "Oracle is off for this session: no oracle model is configured; set [subagents.oracle], or [subagents.yolo.oracle] for a YOLO target, or pass --subagent-oracle."

// DestructionFailurePrefix and its siblings are how a failed lifecycle
// operation's reason begins in SessionRecord.LastError.
//
// A close that fails is non-destructive: the session goes back to the state
// it was running in. Its reason therefore has to be published for a live
// session, which the raw error chains in the same field never are. The
// controller composes a sentence for the person and tags it with this
// prefix, and the projection reads the tag to tell the two apart. Written
// in one place and read in one place, so the tag cannot drift.
const DestructionFailurePrefix = "the destruction did not finish"

const CloseFailurePrefix = "the suspension did not finish"

const MoveFailurePrefix = "the move did not finish"

const ContainerUpgradeFailurePrefix = "the container upgrade did not finish; its environment and recovery intent were retained"

// IsPublicLifecycleError recognizes safe lifecycle outcomes, including
// records saved before the rename.
func IsPublicLifecycleError(text string) bool {
	return strings.HasPrefix(text, CloseFailurePrefix) ||
		strings.HasPrefix(text, MoveFailurePrefix) ||
		strings.HasPrefix(text, ContainerUpgradeFailurePrefix) ||
		strings.HasPrefix(text, DestructionFailurePrefix) ||
		strings.HasPrefix(text, "the close did not finish")
}

// TargetLabel is how a target is named beside a session, wherever a surface
// shows one.
//
// A bare target (local-bare, ssh-bare) opens a project directory directly,
// so the target alone does not say what the session was working on and the
// project's own folder name is appended. Every other kind names a
// provisioned environment that already identifies itself, so the target id
// stands alone. A target id the configuration no longer holds is shown
// verbatim, because its kind is no longer known.
//
// Shared so the live session summary and the Resume dialog's archived rows
// cannot drift apart: SessionRecord.ProjectTarget calls this, and so does
// the archived row built from the SessionWiki index.
func TargetLabel(config *Config, targetID string, project *string) string {
	panic("unported: mj-core/src/state.rs target_label")
}

// StartSelection is a session's starting selection in the terms the API and
// CLI use.
type StartSelection struct {
	// At is the commit the workspace started checked out at.
	At *string
	// Branch is the branch created at At, or the existing branch checked out
	// without it.
	Branch *string
	// Base is the diff base; At unless the caller named another.
	Base *string
	// NoLaterHistory: the workspace started with At, Base and their
	// ancestors only.
	NoLaterHistory bool
}

// IsYoloTarget reports whether this session's harness runs without
// approvals on its recorded target (a container, or permissions = "yolo").
func (r *SessionRecord) IsYoloTarget(config *Config) bool {
	settings, err := r.TargetRuntimeSettings(config)
	return err == nil && settings.IsYolo()
}

// EffectiveOracle is the oracle this session may consult: none when its
// Oracle choice is off, else its own override or the global oracle for its
// target mode.
func (r *SessionRecord) EffectiveOracle(subagents *SubagentConfig, yolo bool) *SubagentChoice {
	panic("unported: mj-core/src/state.rs SessionRecord::effective_oracle")
}

// IsCleanlyStopped: a completed close has no failure left to report. A
// stopped record retaining an error is a failed resume and still needs
// reconciliation.
func (r *SessionRecord) IsCleanlyStopped() bool {
	return r.State == SessionStateStopped && r.LastError == nil
}

// ReviewUnavailable is why review cannot run for this session, when it
// cannot: its folder has no Git commits for review to find changes in.
func (r *SessionRecord) ReviewUnavailable() (string, bool) {
	if r.Project != nil && r.Project.PrimaryIsPlainDirectory() {
		return PlainDirectoryReviewOff, true
	}
	return "", false
}

// RecordCheckout is SessionRecord::checkout (renamed: the record has a
// Checkout field). It is the checkout shape recorded by this session,
// before State applies any sub-agent borrowing relationship. Call
// StateState.Checkout whenever State is available so a child resolves to
// its parent's owner.
func (r *SessionRecord) RecordCheckout() Checkout {
	return deriveRecordCheckout(r)
}

// StartSelection is the starting selection this record stores, as at,
// branch and base. The record keeps the older field layout, so this is the
// one place that maps it to the public names.
func (r *SessionRecord) StartSelection() StartSelection {
	panic("unported: mj-core/src/state.rs SessionRecord::start_selection")
}

// PublicationState is the cached verdict for an independent clone. Active
// checkouts are unknown until a new checkpoint binds an assessment to their
// exact contents.
func (r *SessionRecord) PublicationState() (PublicationState, bool) {
	panic("unported: mj-core/src/state.rs SessionRecord::publication_state")
}

// ExecutionEnforcement is how this session's harness realizes its target's
// execution policy: the policy recorded when its target was selected, so a
// later change to the target template does not move it, and the profile's
// approval choices (Muse auto-review, Claude permission mode). False when
// the policy cannot be resolved or the harness keeps its own configuration.
func (r *SessionRecord) ExecutionEnforcement(config *Config) (ExecutionEnforcement, bool) {
	panic("unported: mj-core/src/state.rs SessionRecord::execution_enforcement")
}

// TargetRuntimeSettings is the target access settings this session's
// commands use: the ones recorded when its target was selected, with the
// machine's current ssh options while they still reach the same host, user,
// and port, or the configured target's when nothing was recorded.
func (r *SessionRecord) TargetRuntimeSettings(config *Config) (*TargetRuntimeSettings, error) {
	return TargetRuntimeSettingsFor(r.ID, r.TargetTemplateID, r.Target, r.TargetRuntime, config)
}

// PublicError is the recorded failure that is safe to publish whatever
// state this session is in, because the controller wrote it for the person
// rather than copying an error chain into it.
func (r *SessionRecord) PublicError() (string, bool) {
	if r.LastError != nil && IsPublicLifecycleError(*r.LastError) {
		return *r.LastError, true
	}
	return "", false
}

// ConfigurationIssue: configuration drift belongs to this session, not the
// entire controller. The diagnostic contains only public identifiers, so
// both UIs can show it.
func (r *SessionRecord) ConfigurationIssue(config *Config) (string, bool) {
	panic("unported: mj-core/src/state.rs SessionRecord::configuration_issue")
}

func (r *SessionRecord) ValidateConfiguration(config *Config) error {
	if issue, ok := r.ConfigurationIssue(config); ok {
		return RefusalPrecondition(issue)
	}
	return nil
}

// ListedTitle is the name a listing shows: a name a person or the harness
// gave, else the title the session was created with, else its id. A
// session nobody named would otherwise be named by its id, which every
// listing already prints beside it; the title it was created with says more
// (launch findings F-12 and R2-8). A name a person typed beats one the
// harness generated. Every place that names a session uses this, or
// ListedSessionTitle where only the stored columns are at hand.
func (r *SessionRecord) ListedTitle() string {
	return ListedSessionTitle(r.ID, r.Title, r.SessionTitleOverride, r.ACPSessionTitle)
}

// GivenName is the manual name, harness title, or first-prompt provisional
// title, as opposed to the creation title. A manual name comes first. A
// rename prompt starts from this; everything that only needs the name uses
// ListedTitle.
func (r *SessionRecord) GivenName() (string, bool) {
	if r.SessionTitleOverride != nil {
		return *r.SessionTitleOverride, true
	}
	if r.ACPSessionTitle != nil {
		return *r.ACPSessionTitle, true
	}
	return "", false
}

// HasGivenName reports whether this session has a manual name, harness
// title, or first-prompt provisional title. Modes that number unnamed
// sessions use this.
func (r *SessionRecord) HasGivenName() bool {
	_, ok := r.GivenName()
	return ok
}

// IsNamedByID reports whether the session has no name but its id: the list
// would print the id twice, so callers fall back to a description or the
// short id.
func (r *SessionRecord) IsNamedByID() bool { return r.ListedTitle() == r.ID }

// MessageName is the listed title as one bounded line, for text that goes
// to an agent or into a message header: whitespace folds onto one line and
// a long title is cut with an ellipsis. See BoundedOneLine.
func (r *SessionRecord) MessageName() string {
	return BoundedOneLine(r.ListedTitle(), MessageNameChars)
}

// ProjectName is the project this session works in, as the session list
// and the chat header both name it: the source repository of a managed
// worktree, else the project directory, else the bundle's primary
// repository, else the bundle id.
func (r *SessionRecord) ProjectName(config *Config) string {
	panic("unported: mj-core/src/state.rs SessionRecord::project_name")
}

// ProjectTarget is the target label used by the live session summary. Bare
// targets identify the project directory they open directly; workspace
// targets already identify the provisioned environment on their own.
func (r *SessionRecord) ProjectTarget(config *Config, targetID string) string {
	panic("unported: mj-core/src/state.rs SessionRecord::project_target")
}

// ProjectSource is the stable source identity used to group sessions.
// Managed worktrees point back at their source repository, raw sessions use
// their project directory until their Git origin is resolved, and bundle
// sessions use their complete canonical repository set when configured.
func (r *SessionRecord) ProjectSource(config *Config) ProjectSourceIdentity {
	panic("unported: mj-core/src/state.rs SessionRecord::project_source")
}

// bundleSourceName resolves the display name shared by session headings,
// chat headers, and resume details for a bundle-backed session.
func (r *SessionRecord) bundleSourceName(config *Config) string {
	panic("unported: mj-core/src/state.rs SessionRecord::bundle_source_name")
}

// bundleSourceIdentity resolves the canonical identity of every repository
// in a bundle for grouping and display naming.
func (r *SessionRecord) bundleSourceIdentity(config *Config) *ProjectSourceIdentity {
	panic("unported: mj-core/src/state.rs SessionRecord::bundle_source_identity")
}

// ProjectBundle: the accepted definition survives saved-project merges and
// config edits.
func (r *SessionRecord) ProjectBundle(config *Config) *ProjectBundle {
	panic("unported: mj-core/src/state.rs SessionRecord::project_bundle")
}

// CompareByCreation orders two sessions the way the session list's sequence
// view does: oldest first by creation time, with the id as a stable
// tiebreak. A session whose timestamp does not parse sorts last.
func (r *SessionRecord) CompareByCreation(other *SessionRecord) int {
	panic("unported: mj-core/src/state.rs SessionRecord::compare_by_creation")
}

// CreationOrderKey: parse once per session when used with a cached sort key.
// The key is (timestamp unparsable, timestamp, id).
func (r *SessionRecord) CreationOrderKey() (bool, *int64, string) {
	timestamp, ok := createdAtSeconds(r.CreatedAt)
	if !ok {
		return true, nil, r.ID
	}
	return false, &timestamp, r.ID
}

func (r *SessionRecord) validate(mapID string) error {
	panic("unported: mj-core/src/state.rs SessionRecord::validate")
}

func repositorySourceIdentity(repository *ProjectRepository) *ProjectSourceIdentity {
	panic("unported: mj-core/src/state.rs repository_source_identity")
}

type ProjectSourceIdentity struct {
	Key   string `json:"key"`
	Short string `json:"short"`
	Full  string `json:"full"`
}

func (p *ProjectSourceIdentity) UnmarshalJSON(data []byte) error {
	type plain ProjectSourceIdentity
	return stateDecodeStrict(data, (*plain)(p))
}

// ProjectSourceIdentityBundle combines repository identities into one
// stable bundle identity.
func ProjectSourceIdentityBundle(sources []ProjectSourceIdentity) *ProjectSourceIdentity {
	panic("unported: mj-core/src/state.rs ProjectSourceIdentity::bundle")
}

// ProjectSourceIdentityGitRemote canonicalizes a Git remote so raw checkouts
// group as the same project even when their worktree paths differ.
// shorthand says whether a bare owner/repository is GitHub; text read from
// git config must not allow it, because there it is a relative path.
func ProjectSourceIdentityGitRemote(source string, shorthand Shorthand) *ProjectSourceIdentity {
	panic("unported: mj-core/src/state.rs ProjectSourceIdentity::git_remote")
}

// ProjectSourceIdentityPath builds a local-root identity, qualified by host
// for remote directories. The directory is the session target's, so its
// text is POSIX.
func ProjectSourceIdentityPath(path string, remote *string) ProjectSourceIdentity {
	panic("unported: mj-core/src/state.rs ProjectSourceIdentity::path")
}

// pathLeaf is the last component of a path, falling back to the whole path
// when it has none.
func pathLeaf(path string) string {
	panic("unported: mj-core/src/state.rs path_leaf")
}

func createdAtSeconds(timestamp string) (int64, bool) {
	panic("unported: mj-core/src/state.rs created_at_seconds")
}

// OperatorPresence is durable operator availability. Revisions order
// control writes across connections.
type OperatorPresence struct {
	Away     bool   `json:"away"`
	Revision uint64 `json:"revision"`
}

func (p *OperatorPresence) UnmarshalJSON(data []byte) error {
	type plain OperatorPresence
	return stateDecodeStrict(data, (*plain)(p))
}

// StateState is state.rs State (prefixed: activity/question_locator also
// declares State). It is the controller's durable state.
type StateState struct {
	OperatorPresence OperatorPresence `json:"operator_presence"`
	// LastSubagentPolicies has alias "last_subagent_policy"; decoded with
	// deserializePolicyMap.
	LastSubagentPolicies map[string]SubagentPolicy `json:"last_subagent_policies"`
	// LastOracleChoices is the most recent Oracle choice for each profile.
	LastOracleChoices map[string]bool                    `json:"last_oracle_choices,omitempty"`
	Version           uint32                             `json:"version"`
	Sessions          SnapshotMap[string, SessionRecord] `json:"sessions"`
	// Subagents are child sessions keyed by their session id. The
	// relationship lives in controller state so every control surface sees
	// the same session family.
	Subagents SnapshotMap[string, SubagentMetadata] `json:"subagents"`
	// MountHistory is recently used source directories, keyed by local or
	// SSH host name.
	MountHistory SnapshotMap[string, []string] `json:"mount_history"`
	// ContainerSizes is the most recently launched container size on each
	// physical target host.
	ContainerSizes SnapshotMap[string, HostContainerSize] `json:"container_sizes"`
}

// MarshalJSON omits empty snapshot maps, as skip_serializing_if = is_empty does.
func (s StateState) MarshalJSON() ([]byte, error) {
	type mirror struct {
		OperatorPresence     OperatorPresence                        `json:"operator_presence"`
		LastSubagentPolicies map[string]SubagentPolicy               `json:"last_subagent_policies"`
		LastOracleChoices    map[string]bool                         `json:"last_oracle_choices,omitempty"`
		Version              uint32                                  `json:"version"`
		Sessions             *SnapshotMap[string, SessionRecord]     `json:"sessions,omitempty"`
		Subagents            *SnapshotMap[string, SubagentMetadata]  `json:"subagents,omitempty"`
		MountHistory         *SnapshotMap[string, []string]          `json:"mount_history,omitempty"`
		ContainerSizes       *SnapshotMap[string, HostContainerSize] `json:"container_sizes,omitempty"`
	}
	out := mirror{
		OperatorPresence:     s.OperatorPresence,
		LastSubagentPolicies: s.LastSubagentPolicies,
		LastOracleChoices:    s.LastOracleChoices,
		Version:              s.Version,
	}
	if out.LastSubagentPolicies == nil {
		out.LastSubagentPolicies = map[string]SubagentPolicy{}
	}
	if !s.Sessions.IsEmpty() {
		out.Sessions = &s.Sessions
	}
	if !s.Subagents.IsEmpty() {
		out.Subagents = &s.Subagents
	}
	if !s.MountHistory.IsEmpty() {
		out.MountHistory = &s.MountHistory
	}
	if !s.ContainerSizes.IsEmpty() {
		out.ContainerSizes = &s.ContainerSizes
	}
	return json.Marshal(out)
}

func (s *StateState) UnmarshalJSON(data []byte) error {
	data, err := stateRenameAliases(data, map[string]string{"last_subagent_policy": "last_subagent_policies"})
	if err != nil {
		return err
	}
	type plain StateState
	aux := struct {
		*plain
		LastSubagentPolicies json.RawMessage `json:"last_subagent_policies"`
	}{plain: (*plain)(s)}
	*s = NewStateState()
	s.Version = 0
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	if aux.LastSubagentPolicies != nil {
		policies, err := deserializePolicyMap(aux.LastSubagentPolicies)
		if err != nil {
			return err
		}
		s.LastSubagentPolicies = policies
	}
	return nil
}

// NewStateState is State::default.
func NewStateState() StateState {
	return StateState{
		Version:              StateVersion,
		LastSubagentPolicies: map[string]SubagentPolicy{},
		LastOracleChoices:    map[string]bool{},
		Sessions:             NewSnapshotMap[string, SessionRecord](),
		Subagents:            NewSnapshotMap[string, SubagentMetadata](),
		MountHistory:         NewSnapshotMap[string, []string](),
		ContainerSizes:       NewSnapshotMap[string, HostContainerSize](),
	}
}

// LastSubagentPolicyFor is the remembered choice for a profile, falling
// back to the legacy global choice and then the default when that profile
// has no saved choice.
func (s *StateState) LastSubagentPolicyFor(profileID string) SubagentPolicy {
	panic("unported: mj-core/src/state.rs State::last_subagent_policy_for")
}

// LastOracleChoiceFor is the remembered Oracle choice for a profile;
// enabled when it has none.
func (s *StateState) LastOracleChoiceFor(profileID string) bool {
	if choice, ok := s.LastOracleChoices[profileID]; ok {
		return choice
	}
	return true
}

// reviewOwner is the session whose record holds the review choice that
// governs sessionID. An ordinary sub-agent reviews under the choice of the
// session that spawned it, followed up the chain to a top-level session, so
// the parent's record stays the only copy and a later change to it applies
// to its children. Every other session uses its own record.
//
// Fails with the id of a missing ancestor record, or with the session
// itself when the parent chain loops (the string result, ok false).
func (s *StateState) reviewOwner(sessionID string) (owner *SessionRecord, failed string, ok bool) {
	panic("unported: mj-core/src/state.rs State::review_owner")
}

// EffectiveReview is the stored review choice that governs sessionID,
// through reviewOwner. Reviewer selection asks this for a Legacy override;
// everything that needs the resulting configuration asks
// EffectiveReviewConfig. On failure it returns the offending session id and
// ok false.
func (s *StateState) EffectiveReview(sessionID string) (review *SessionReview, failed string, ok bool) {
	owner, failed, ok := s.reviewOwner(sessionID)
	if !ok {
		return nil, failed, false
	}
	if owner == nil {
		return nil, "", true
	}
	return owner.Review, "", true
}

// EffectiveReviewConfig is the review configuration that governs sessionID:
// the given global [review] under the stored choice, then Mjolnir's
// automatic rules (see review settings automaticReviewOff), which are
// evaluated here on every call and never stored. A parent chain that cannot
// be followed reviews as Off. Launch (which offers the review tools), the
// review host, child review requests, delegation completion and the viewer
// all read the effective review through this one rule.
func (s *StateState) EffectiveReviewConfig(review *ReviewConfig, sessionID string) ReviewConfig {
	panic("unported: mj-core/src/state.rs State::effective_review_config")
}

// Checkout is the effective code checkout for one session, including who
// owns it.
//
// These variants are derived from the durable session record and the
// State.Subagents relationship. They are not serialized. In particular, the
// creation-time create_managed_worktree choice is not evidence of current
// checkout ownership.
type Checkout interface {
	isCheckout()
	// ManagedWorktree is the managed worktree owned by this session, if any.
	// A borrowed child intentionally gets nil even when its owner uses a
	// managed checkout.
	ManagedWorktree() *ManagedWorktree
	// Effective resolves a borrowed checkout to the value that describes its
	// location. The outer CheckoutBorrowed variant still controls cleanup: a
	// child must never retire the managed checkout it borrows.
	Effective() Checkout
	// Owner is the record that owns a borrowed checkout, following nested
	// borrowing to the session that holds the workspace. Nil for a checkout
	// the session owns itself.
	Owner() *SessionRecord
	// ProjectDirectory is the concrete repository directory when the record
	// stores one. Bundle workspace paths are target-specific and are not in
	// the record.
	ProjectDirectory() (string, bool)
}

// CheckoutAttached is a user-selected raw directory. Mjolnir never creates
// or removes it.
type CheckoutAttached struct {
	Path string
}

// CheckoutManagedWorktree is a raw project worktree or clone created and
// retired by Mjolnir.
//
// ProjectDirectory can be absent only on an incomplete legacy record;
// ownership still follows the managed-worktree descriptor. For valid
// records it can be below WorktreeRoot when the project is a subdirectory
// of its repository.
type CheckoutManagedWorktree struct {
	Worktree   *ManagedWorktree
	ProjectDir *string
}

// CheckoutManagedWorkspace is a bundle workspace cloned on the target and
// retired with its target. Its target path is platform-specific and is
// derived by provisioning.
type CheckoutManagedWorkspace struct{}

// CheckoutBorrowed: a child session uses the parent's checkout and never
// owns its lifetime.
type CheckoutBorrowed struct {
	OwnerRecord *SessionRecord
	Checkout    Checkout
}

func (CheckoutAttached) isCheckout()         {}
func (CheckoutManagedWorktree) isCheckout()  {}
func (CheckoutManagedWorkspace) isCheckout() {}
func (CheckoutBorrowed) isCheckout()         {}

func (CheckoutAttached) ManagedWorktree() *ManagedWorktree          { return nil }
func (c CheckoutManagedWorktree) ManagedWorktree() *ManagedWorktree { return c.Worktree }
func (CheckoutManagedWorkspace) ManagedWorktree() *ManagedWorktree  { return nil }
func (CheckoutBorrowed) ManagedWorktree() *ManagedWorktree          { return nil }

func (c CheckoutAttached) Effective() Checkout         { return c }
func (c CheckoutManagedWorktree) Effective() Checkout  { return c }
func (c CheckoutManagedWorkspace) Effective() Checkout { return c }
func (c CheckoutBorrowed) Effective() Checkout         { return c.Checkout.Effective() }

func (CheckoutAttached) Owner() *SessionRecord         { return nil }
func (CheckoutManagedWorktree) Owner() *SessionRecord  { return nil }
func (CheckoutManagedWorkspace) Owner() *SessionRecord { return nil }
func (c CheckoutBorrowed) Owner() *SessionRecord {
	if owner := c.Checkout.Owner(); owner != nil {
		return owner
	}
	return c.OwnerRecord
}

func (c CheckoutAttached) ProjectDirectory() (string, bool) { return c.Path, true }
func (c CheckoutManagedWorktree) ProjectDirectory() (string, bool) {
	if c.ProjectDir == nil {
		return "", false
	}
	return *c.ProjectDir, true
}
func (CheckoutManagedWorkspace) ProjectDirectory() (string, bool) { return "", false }
func (c CheckoutBorrowed) ProjectDirectory() (string, bool)       { return c.Checkout.ProjectDirectory() }

// deriveRecordCheckout is the only conversion from persisted checkout
// fields to an ownership kind.
func deriveRecordCheckout(session *SessionRecord) Checkout {
	if session.ManagedWorktree != nil {
		return CheckoutManagedWorktree{Worktree: session.ManagedWorktree, ProjectDir: session.ProjectDirectory}
	}
	if session.ProjectDirectory != nil {
		return CheckoutAttached{Path: *session.ProjectDirectory}
	}
	return CheckoutManagedWorkspace{}
}

// SessionIsLive reports whether a terminal dashboard follows record live
// through the runtime feed. Stopped is the one settled state that
// accumulates, so a stopped session is left to the resume dialog unless
// something still holds it: a lifecycle operation or move in flight
// (inOperation), or a live parent whose Sub-agents view shows it
// (parentLive). Lost and data-loss sessions stay live: they are unsettled
// failures that need attention.
func SessionIsLive(record *SessionRecord, inOperation, parentLive bool) bool {
	return record.State != SessionStateStopped || inOperation || parentLive
}

// LiveSessionIDs is every session SessionIsLive keeps, given the ids that
// have an operation in flight. A stopped sub-agent follows its parent, so
// membership settles parent before child.
func LiveSessionIDs(sessions *SnapshotMap[string, SessionRecord], subagents *SnapshotMap[string, SubagentMetadata], operations map[string]struct{}) map[string]struct{} {
	panic("unported: mj-core/src/state.rs live_session_ids")
}

// ActiveChildSessionIDs is the managed children that a parent suspend or
// Move stop hook must end, including parked children and oracles.
func (s *StateState) ActiveChildSessionIDs(parentSessionID string) []string {
	panic("unported: mj-core/src/state.rs State::active_child_session_ids")
}

// SubagentsOccupyContainer reports whether a sub-agent of parentSessionID
// still has a target. A child's worker, parked or live, runs in its
// parent's container, so recreating that container would end it.
func (s *StateState) SubagentsOccupyContainer(parentSessionID string) bool {
	panic("unported: mj-core/src/state.rs State::subagents_occupy_container")
}

// Checkout derives the checkout location and its owner for a session.
//
// Raw directory sessions with no managed-worktree descriptor are attached
// even when they have no Git project snapshot (the supported non-Git
// bare-target case). A descriptor takes precedence if both raw fields are
// present, because it is the cleanup authority; the stored project path is
// retained as the checkout directory, even if an invalid legacy record
// disagrees with the descriptor's root. Record validation rejects that
// mismatch. With neither field present, old bundle-backed records map to a
// managed workspace whether their bundle came from config or a saved
// project snapshot. A managed sub-agent is resolved through its durable
// parent relation first, so its copied project_directory never changes
// ownership. Nested children keep the chain of borrowed owners. An orphan
// child whose parent record has already been removed falls back to its
// stored checkout fields, preserving the older cleanup path.
func (s *StateState) Checkout(sessionID string) (Checkout, error) {
	panic("unported: mj-core/src/state.rs State::checkout")
}

// CheckoutForRecord derives checkout ownership for a projected record while
// retaining the durable sub-agent relationship from this state snapshot.
func (s *StateState) CheckoutForRecord(sessionID string, session *SessionRecord) (Checkout, error) {
	return s.checkoutForRecordInner(sessionID, session, map[string]struct{}{})
}

func (s *StateState) checkoutForRecordInner(sessionID string, session *SessionRecord, visited map[string]struct{}) (Checkout, error) {
	panic("unported: mj-core/src/state.rs State::checkout_for_record_inner")
}

// WorkspaceOwner is the record whose workspace sessionID works in: the
// session itself, or, for a sub-agent, the parent it borrows from. Facts
// about how the workspace was created (its exact checkout, its history
// cutoff) belong to that record, so a reader asks here instead of the
// child's own fields, which a child leaves empty.
func (s *StateState) WorkspaceOwner(sessionID string) (*SessionRecord, error) {
	panic("unported: mj-core/src/state.rs State::workspace_owner")
}

// SessionNoticeName is how a notice names a session: the title the session
// list shows (ListedTitle, which includes the title it was created with),
// or its short id when it has no title or its record is gone (launch
// findings B-3, R5-5 and R8-3).
func (s *StateState) SessionNoticeName(sessionID string) string {
	panic("unported: mj-core/src/state.rs State::session_notice_name")
}

// ProjectIdentitySession is the session whose project identity names a row.
//
// A sub-agent child runs inside its parent's workspace and owns no managed
// worktree, so its own project_directory is the parent's worktree checkout,
// whose directory is named after the parent session id. Reading the project
// identity from the parent instead keeps a child under the same project
// heading and target label as the session it belongs to.
func (s *StateState) ProjectIdentitySession(session *SessionRecord) *SessionRecord {
	panic("unported: mj-core/src/state.rs State::project_identity_session")
}

// IsSubagentSession reports whether id names a sub-agent rather than a
// session the user started: a Mjolnir-managed child, or the record a client
// builds to show a harness-owned child (see native agent ViewID).
//
// Every list of top-level sessions filters with this, so the lists cannot
// disagree about what a sub-agent is.
func (s *StateState) IsSubagentSession(id string) bool {
	panic("unported: mj-core/src/state.rs State::is_subagent_session")
}

// IsStoppedSubagent reports whether id is a Mjolnir sub-agent whose session
// has no live worker, so its stored conversation can be read but not
// attached to.
//
// The terminal and the phone viewer both ask this, so they cannot disagree
// about which children are readable. Callers that also know of a transition
// bringing the session back add that check themselves.
func (s *StateState) IsStoppedSubagent(id string) bool {
	panic("unported: mj-core/src/state.rs State::is_stopped_subagent")
}

func (s *StateState) Validate() error {
	panic("unported: mj-core/src/state.rs State::validate")
}

// ValidateSubagent validates one relationship after an incremental
// committed update.
func (s *StateState) ValidateSubagent(childID string) error {
	panic("unported: mj-core/src/state.rs State::validate_subagent")
}

func (s *StateState) RememberMountSources(host string, mounts []AdditionalMount) {
	panic("unported: mj-core/src/state.rs State::remember_mount_sources")
}

func (s *StateState) RememberContainerSize(host string, size HostContainerSize) {
	s.ContainerSizes.Insert(host, size)
}

func (s *StateState) ProjectDirectories(host string) []string {
	panic("unported: mj-core/src/state.rs State::project_directories")
}

func (s *StateState) RememberProjectDirectory(host string, directory string) {
	panic("unported: mj-core/src/state.rs State::remember_project_directory")
}

func (s *StateState) DestroyStoppedSession(sessionID string) (SessionRecord, error) {
	panic("unported: mj-core/src/state.rs State::destroy_stopped_session")
}

// DestroySessionForce removes a session record from state regardless of
// its lifecycle state.
//
// Force destruction is the one caller: by the time it runs, every external
// artifact has been torn down or its loss accepted, so no state is refused
// here.
func (s *StateState) DestroySessionForce(sessionID string) (SessionRecord, error) {
	panic("unported: mj-core/src/state.rs State::destroy_session_force")
}

// BundleUsers is the sessions that still read bundleID from the config. A
// suspended session counts: resume looks its project up again. Only a
// session opened on a plain directory, or one whose data is already gone,
// does not need it.
func (s *StateState) BundleUsers(bundleID string) []*SessionRecord {
	panic("unported: mj-core/src/state.rs State::bundle_users")
}

// BundleRemovalRefusal is why bundleID cannot be removed from the config,
// or false when no session uses it.
func (s *StateState) BundleRemovalRefusal(bundleID string) (string, bool) {
	panic("unported: mj-core/src/state.rs State::bundle_removal_refusal")
}

// ValidateSetupUpdate: setup may add replacements under new names, but must
// not rewrite dependencies still owned by active sessions.
func (s *StateState) ValidateSetupUpdate(before, after *Config) error {
	panic("unported: mj-core/src/state.rs State::validate_setup_update")
}

// ValidateAgainstConfig is strict validation for callers that need all
// active references intact.
func (s *StateState) ValidateAgainstConfig(config *Config) error {
	panic("unported: mj-core/src/state.rs State::validate_against_config")
}

func projectHistoryKey(host string) string { return "project:" + host }

// NewSessionID generates an opaque, filesystem-safe stable id for a new
// logical session.
func NewSessionID() (string, error) {
	panic("unported: mj-core/src/state.rs new_session_id")
}

// HarnessSessionTitle returns the newest clean ACP session title from
// canonical worker events.
func HarnessSessionTitle(events []SequencedEvent) (string, bool) {
	panic("unported: mj-core/src/state.rs harness_session_title")
}

// DefaultSessionTitle is the title a new session gets when whoever starts
// it gives none: the name of its project directory (or its bundle id) and
// the profile, such as "project via fake". The dashboard, the HTTP API, and
// mj new all use it, so a session reads the same way whichever surface
// started it.
func DefaultSessionTitle(projectDirectory *string, bundleID, profileID string) string {
	panic("unported: mj-core/src/state.rs default_session_title")
}

const MaxSessionTitleChars = 256

// NormalizeSessionTitle cleans a title and bounds it at a word boundary,
// including the ellipsis.
//
// Titles come from agents, so terminal controls are removed with the rest
// of the cleanup: an escape sequence in a stored title would otherwise run
// in every terminal that prints it (an OSC 52 clipboard write, for example).
func NormalizeSessionTitle(title string) (string, bool) {
	visible := SanitizeTerminalText(StripHiddenPromptContext(title))
	normalized := strings.Join(strings.Fields(visible), " ")
	if normalized == "" {
		return "", false
	}
	return truncateSessionTitle(normalized, MaxSessionTitleChars), true
}

func truncateSessionTitle(title string, maximumChars int) string {
	panic("unported: mj-core/src/state.rs truncate_session_title")
}

// ListedSessionTitle is SessionRecord.ListedTitle from the stored columns,
// for code that reads a session's title fields without loading the record.
// The one place that decides which of the three titles names a session.
func ListedSessionTitle(id, title string, titleOverride, acpTitle *string) string {
	if titleOverride != nil {
		return *titleOverride
	}
	if acpTitle != nil {
		return *acpTitle
	}
	if strings.TrimSpace(title) == "" {
		return id
	}
	return title
}

// MessageNameChars is how many characters a session's name takes in text
// addressed to an agent.
const MessageNameChars = 80

// BoundedOneLine is a name as one line of at most maximumChars characters:
// terminal controls are removed as NormalizeSessionTitle removes them (the
// creation title is stored as typed), whitespace, including line breaks,
// folds to single spaces, and a longer name is cut at a word boundary and
// ends with an ellipsis. The one bounding rule for names that reach agents;
// the terminal fits names by display cells instead.
func BoundedOneLine(name string, maximumChars int) string {
	visible := SanitizeTerminalText(name)
	return truncateSessionTitle(strings.Join(strings.Fields(visible), " "), maximumChars)
}

// ProvisionalSessionTitle builds the short-lived title shown before the
// harness supplies its own.
//
// The first visible user prompt is immediately useful for identifying a
// session, but it can be arbitrarily large. Keep this fallback bounded; a
// later ACP session-info update remains authoritative and replaces it.
func ProvisionalSessionTitle(prompt string) (string, bool) {
	const maxTitleChars = 64
	normalized, ok := NormalizeSessionTitle(prompt)
	if !ok {
		return "", false
	}
	return truncateSessionTitle(normalized, maxTitleChars), true
}

// ShortIDLen is how many leading characters of a session id Mjolnir shows,
// and so the shortest prefix a person can be expected to paste back.
const ShortIDLen = 8

// ShortID is the leading ShortIDLen bytes of id, or id when it is shorter
// (or the cut would split a UTF-8 character, as str::get refuses).
func ShortID(id string) string {
	if len(id) < ShortIDLen {
		return id
	}
	if len(id) > ShortIDLen && id[ShortIDLen]&0xC0 == 0x80 {
		return id
	}
	return id[:ShortIDLen]
}

type RecoveryCandidate struct {
	SessionID        string           `json:"session_id"`
	TargetTemplateID string           `json:"target_template_id"`
	Locator          TargetLocator    `json:"locator"`
	Ownership        *WorkerOwnership `json:"ownership"`
	// InstanceID is the instance that created the worker, from its label or
	// tag, else from the ownership marker. Nil means an older build left no
	// stamp.
	InstanceID *string `json:"instance_id"`
	// TrackedSession is the state of the session this resource is labelled
	// for, when the controller still tracks that session. A leftover
	// resource the session record no longer names can only be destroyed,
	// never adopted, because the session id is already taken.
	TrackedSession *SessionState `json:"tracked_session,omitempty"`
}

func (c *RecoveryCandidate) UnmarshalJSON(data []byte) error {
	type plain RecoveryCandidate
	aux := struct {
		*plain
		Locator stateUnionJSON[TargetLocator] `json:"locator"`
	}{plain: (*plain)(c)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if aux.Locator.V == nil {
		return fmt.Errorf("missing field `locator`")
	}
	c.Locator = aux.Locator.V
	return nil
}

type RecoveryScan struct {
	Candidates []RecoveryCandidate `json:"candidates"`
	Warnings   []string            `json:"warnings"`
	// InstanceID is the identity of the instance that ran the scan.
	InstanceID string `json:"instance_id"`
	// HiddenOtherInstances counts candidates left out because another or an
	// unknown instance created them and the scan was not widened to all
	// instances.
	HiddenOtherInstances int `json:"hidden_other_instances"`
}

type ResumeRepositorySourceMismatch struct {
	SessionID        string `json:"session_id"`
	BundleID         string `json:"bundle_id"`
	RepositoryID     string `json:"repository_id"`
	MissingCommit    string `json:"missing_commit"`
	ArchivedOrigin   string `json:"archived_origin"`
	ConfiguredOrigin string `json:"configured_origin"`
}

func (m *ResumeRepositorySourceMismatch) UnmarshalJSON(data []byte) error {
	type plain ResumeRepositorySourceMismatch
	return stateDecodeStrict(data, (*plain)(m))
}

// ResumeRepositorySourcePreflight is the outcome of checking a resume's
// repository sources. Externally tagged, snake_case.
type ResumeRepositorySourcePreflight interface {
	isResumeRepositorySourcePreflight()
}

type ResumeRepositorySourcePreflightReady struct {
	Value ResumeRepositorySourceReceipt
}

type ResumeRepositorySourcePreflightRepositoryMoved struct {
	Value ResumeRepositorySourceMismatch
}

// ResumeRepositorySourcePreflightConvertingRawCheckout: the resume converts
// a local checkout into an isolated workspace. The receipt is already
// valid; the preview is what a person has to confirm before the checkout is
// snapshotted and the session moves off this machine.
type ResumeRepositorySourcePreflightConvertingRawCheckout struct {
	Receipt ResumeRepositorySourceReceipt `json:"receipt"`
	Preview *RawConversionPreview         `json:"preview"`
}

func (ResumeRepositorySourcePreflightReady) isResumeRepositorySourcePreflight()           {}
func (ResumeRepositorySourcePreflightRepositoryMoved) isResumeRepositorySourcePreflight() {}
func (ResumeRepositorySourcePreflightConvertingRawCheckout) isResumeRepositorySourcePreflight() {
}

func (v ResumeRepositorySourcePreflightReady) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("ready", v.Value)
}

func (v ResumeRepositorySourcePreflightRepositoryMoved) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("repository_moved", v.Value)
}

func (v ResumeRepositorySourcePreflightConvertingRawCheckout) MarshalJSON() ([]byte, error) {
	type plain ResumeRepositorySourcePreflightConvertingRawCheckout
	return stateMarshalExternal("converting_raw_checkout", plain(v))
}

func unmarshalResumeRepositorySourcePreflight(data []byte) (ResumeRepositorySourcePreflight, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	if content == nil {
		return nil, fmt.Errorf("unknown variant `%s`", tag)
	}
	switch tag {
	case "ready":
		var value ResumeRepositorySourceReceipt
		err := json.Unmarshal(content, &value)
		return ResumeRepositorySourcePreflightReady{value}, err
	case "repository_moved":
		var value ResumeRepositorySourceMismatch
		err := json.Unmarshal(content, &value)
		return ResumeRepositorySourcePreflightRepositoryMoved{value}, err
	case "converting_raw_checkout":
		type plain ResumeRepositorySourcePreflightConvertingRawCheckout
		var value plain
		err := stateDecodeStrict(content, &value)
		return ResumeRepositorySourcePreflightConvertingRawCheckout(value), err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// UnmarshalResumeRepositorySourcePreflight decodes a serialized preflight.
func UnmarshalResumeRepositorySourcePreflight(data []byte) (ResumeRepositorySourcePreflight, error) {
	return unmarshalResumeRepositorySourcePreflight(data)
}

type ResumeRepositorySourceReceipt struct {
	SessionID        string              `json:"session_id"`
	BundleID         string              `json:"bundle_id"`
	CheckpointSha256 string              `json:"checkpoint_sha256"`
	Repositories     []ProjectRepository `json:"repositories"`
}

func (r *ResumeRepositorySourceReceipt) UnmarshalJSON(data []byte) error {
	type plain ResumeRepositorySourceReceipt
	return stateDecodeStrict(data, (*plain)(r))
}

// TargetRuntimeSettingsFor is the access settings for a recorded physical
// placement, independent of its session.
func TargetRuntimeSettingsFor(sessionID, targetTemplateID string, target TargetLocator, runtime *TargetRuntimeSettings, config *Config) (*TargetRuntimeSettings, error) {
	panic("unported: mj-core/src/state.rs target_runtime_settings_for")
}

// Serde encodings shared by this module and its sibling modules
// (workspace, goal, attention, work_facts).

// stateMarshalTagged encodes the fields of v after an internal tag, as
// serde's #[serde(tag = "...")] does: the tag first, then the fields in
// declaration order.
func stateMarshalTagged(tagKey, tag string, v any) ([]byte, error) {
	head, err := json.Marshal(map[string]string{tagKey: tag})
	if err != nil {
		return nil, err
	}
	if v == nil {
		return head, nil
	}
	body, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	if len(body) <= 2 {
		return head, nil
	}
	out := append(head[:len(head)-1:len(head)-1], ',')
	return append(out, body[1:]...), nil
}

// stateMarshalAdjacent encodes serde's #[serde(tag = "...", content = "...")].
func stateMarshalAdjacent(tagKey, tag, contentKey string, content any) ([]byte, error) {
	head, err := json.Marshal(map[string]string{tagKey: tag})
	if err != nil {
		return nil, err
	}
	key, err := json.Marshal(contentKey)
	if err != nil {
		return nil, err
	}
	body, err := json.Marshal(content)
	if err != nil {
		return nil, err
	}
	out := append(head[:len(head)-1:len(head)-1], ',')
	out = append(append(append(out, key...), ':'), body...)
	return append(out, '}'), nil
}

// stateMarshalExternal encodes an externally tagged serde variant with data:
// {"variant": content}.
func stateMarshalExternal(tag string, content any) ([]byte, error) {
	key, err := json.Marshal(tag)
	if err != nil {
		return nil, err
	}
	body, err := json.Marshal(content)
	if err != nil {
		return nil, err
	}
	out := append(append([]byte{'{'}, key...), ':')
	return append(append(out, body...), '}'), nil
}

// stateTag reads the internal tag of a tagged serde enum.
func stateTag(data []byte, tagKey string) (string, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return "", err
	}
	raw, ok := fields[tagKey]
	if !ok {
		return "", fmt.Errorf("missing field `%s`", tagKey)
	}
	var tag string
	if err := json.Unmarshal(raw, &tag); err != nil {
		return "", err
	}
	return tag, nil
}

// stateExternal splits an externally tagged serde enum into its variant name
// and content; content is nil for a unit variant written as a bare string.
func stateExternal(data []byte) (string, json.RawMessage, error) {
	var unit string
	if err := json.Unmarshal(data, &unit); err == nil {
		return unit, nil, nil
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return "", nil, err
	}
	if len(fields) != 1 {
		return "", nil, errors.New("expected an enum variant object with exactly one key")
	}
	for tag, content := range fields {
		return tag, content, nil
	}
	return "", nil, nil
}

// stateDecodeStrict decodes data into v rejecting unknown fields, as
// #[serde(deny_unknown_fields)] does. Keys in allow (an internal tag) are
// accepted and ignored.
func stateDecodeStrict(data []byte, v any, allow ...string) error {
	if len(allow) > 0 {
		var fields map[string]json.RawMessage
		if err := json.Unmarshal(data, &fields); err != nil {
			return err
		}
		for _, key := range allow {
			delete(fields, key)
		}
		var err error
		if data, err = json.Marshal(fields); err != nil {
			return err
		}
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode(v)
}

// stateRenameAliases rewrites serde field aliases (alias -> canonical) so a
// plain decode sees the canonical name. Both spellings at once is a
// duplicate field, as in serde.
func stateRenameAliases(data []byte, aliases map[string]string) ([]byte, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return nil, err
	}
	changed := false
	for alias, canonical := range aliases {
		value, ok := fields[alias]
		if !ok {
			continue
		}
		if _, dup := fields[canonical]; dup {
			return nil, fmt.Errorf("duplicate field `%s`", canonical)
		}
		delete(fields, alias)
		fields[canonical] = value
		changed = true
	}
	if !changed {
		return data, nil
	}
	return json.Marshal(fields)
}

// stateVariant decodes one variant struct V and returns it as its union U.
func stateVariant[V any, U any](data []byte) (U, error) {
	var value V
	if err := json.Unmarshal(data, &value); err != nil {
		var zero U
		return zero, err
	}
	return any(value).(U), nil
}

// stateUnionJSON decodes one of this group's sealed-interface unions where it
// appears as a struct field; null leaves the zero (nil) value. Used only on
// the decode side, as a shadowing field in an UnmarshalJSON helper struct.
type stateUnionJSON[T any] struct {
	V   T
	Set bool
}

func (w *stateUnionJSON[T]) UnmarshalJSON(data []byte) error {
	w.Set = true
	if string(bytes.TrimSpace(data)) == "null" {
		var zero T
		w.V = zero
		return nil
	}
	var (
		value any
		err   error
	)
	switch any(&w.V).(type) {
	case *TargetLocator:
		value, err = unmarshalTargetLocator(data)
	case *PodmanWorkspaceLocator:
		value, err = unmarshalPodmanWorkspaceLocator(data)
	case *SessionResourceAllocation:
		value, err = unmarshalSessionResourceAllocation(data)
	case *ManagedWorktreeTarget:
		value, err = unmarshalManagedWorktreeTarget(data)
	case *MaterializedExecutionState:
		value, err = unmarshalMaterializedExecutionState(data)
	case *QueuedCommandKind:
		value, err = unmarshalQueuedCommandKind(data)
	case *TurnOutcomeKind:
		value, err = unmarshalTurnOutcomeKind(data)
	case *BuildCacheOff:
		value, err = unmarshalBuildCacheOff(data)
	case *BuildCacheLimit:
		value, err = unmarshalBuildCacheLimit(data)
	case *BuildCacheApplication:
		value, err = unmarshalBuildCacheApplication(data)
	case *TargetConnection:
		value, err = unmarshalTargetConnection(data)
	case *PreparedDestinationState:
		value, err = unmarshalPreparedDestinationState(data)
	case *ResumeRepositorySourcePreflight:
		value, err = unmarshalResumeRepositorySourcePreflight(data)
	case *LayoutNode:
		value, err = unmarshalLayoutNode(data)
	case *EntryMessage:
		value, err = unmarshalEntryMessage(data)
	case *NotificationDismissal:
		value, err = unmarshalNotificationDismissal(data)
	default:
		return fmt.Errorf("stateUnionJSON: unsupported union %T", w.V)
	}
	if err != nil {
		return err
	}
	w.V = value.(T)
	return nil
}

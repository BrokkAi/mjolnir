// Port of mj-core/src/activity/verdict.rs.

// Bounded turn evidence and conservative decisions for the optional Jev
// classifier.

package core

const UserPromptBytes = 1024
const AssistantTextBytes = 2048
const ToolTitleBytes = 128
const InFlightTools = 16

// ToolTitle is a tool or command title as every classifier sees it: its
// first ToolTitleBytes, where the program and its operation are.
func ToolTitle(text string) string {
	return text[:agentFloorCharBoundary(text, ToolTitleBytes)]
}

// agentFloorCharBoundary is Rust's str::floor_char_boundary: the largest UTF-8
// character boundary at or below index.
func agentFloorCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for index > 0 && text[index]&0xC0 == 0x80 {
		index--
	}
	return index
}

// agentCeilCharBoundary is Rust's str::ceil_char_boundary: the smallest UTF-8
// character boundary at or above index.
func agentCeilCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for index < len(text) && text[index]&0xC0 == 0x80 {
		index++
	}
	return index
}

type TurnPhase string

const (
	TurnPhaseRunning TurnPhase = "running"
	TurnPhaseReplied TurnPhase = "replied"
)

type ToolEvidence struct {
	Title    string `json:"title"`
	RunningS uint64 `json:"running_s"`
}

// BackgroundEvidence is one background command the agent left running, as
// the quiet judgment sees it.
type BackgroundEvidence struct {
	ID string `json:"id"`
	// Command is the command line, cut to ToolTitleBytes.
	Command     string `json:"command"`
	StartedSAgo uint64 `json:"started_s_ago"`
}

// ToolOutcome is a tool call the agent made after its last text, by name and
// outcome only. A `handback` or `spawn` here says what the reply's silence
// does not.
type ToolOutcome struct {
	Name   string `json:"name"`
	Status string `json:"status"`
}

type TurnEvidence struct {
	Authorization *ContextHistory `json:"authorization,omitempty"`
	// FinalToolCalls is the tool calls made after the last assistant text of
	// the turn, oldest first, at most InFlightTools. Empty on older workers.
	FinalToolCalls []ToolOutcome `json:"final_tool_calls,omitempty"`
	// Background is the background commands behind BackgroundCommands, sent
	// so Jev can judge whether anyone still depends on them. Empty on older
	// workers.
	Background         []BackgroundEvidence `json:"background,omitempty"`
	Harness            HarnessKind          `json:"harness"`
	Phase              TurnPhase            `json:"phase"`
	SilentForS         uint64               `json:"silent_for_s"`
	ToolsInFlight      []ToolEvidence       `json:"tools_in_flight"`
	TranscriptSummary  string               `json:"transcript_summary"`
	BackgroundCommands int                  `json:"background_commands"`
	QueuedCommands     int                  `json:"queued_commands"`
	UserPromptTail     string               `json:"user_prompt_tail"`
	AssistantTextTail  string               `json:"assistant_text_tail"`
	Completion         *CompletionEvidence  `json:"completion,omitempty"`
}

// MarshalJSON writes ToolsInFlight as a list even when it is nil, as serde
// writes a Vec.
func (e TurnEvidence) MarshalJSON() ([]byte, error) {
	type plain TurnEvidence
	if e.ToolsInFlight == nil {
		e.ToolsInFlight = []ToolEvidence{}
	}
	return agentMarshalJSON(plain(e))
}

type CompletionEvidence struct {
	StopReason string          `json:"stop_reason"`
	Diagnostic *TurnDiagnostic `json:"diagnostic"`
}

// CompletionEvidenceBounded keeps the completion within the classifier's
// evidence budget.
func CompletionEvidenceBounded(stopReason string, diagnostic *TurnDiagnostic) CompletionEvidence {
	stopReason = stopReason[:agentFloorCharBoundary(stopReason, 128)]
	var bounded *TurnDiagnostic
	if diagnostic != nil {
		copied := *diagnostic
		if len(copied.Message) > 4096 {
			copied.Message = "[Provider diagnostic omitted: exceeds 4096 bytes]"
		}
		if copied.Code != nil && len(*copied.Code) > 128 {
			copied.Code = nil
		}
		if copied.ResetAt != nil && len(*copied.ResetAt) > 256 {
			copied.ResetAt = nil
		}
		bounded = &copied
	}
	return CompletionEvidence{StopReason: stopReason, Diagnostic: bounded}
}

type Decision string

const (
	DecisionAwaitingInput      Decision = "awaiting_input"
	DecisionExpectContinuation Decision = "expect_continuation"
	DecisionInferIdle          Decision = "infer_idle"
	DecisionKeepCurrent        Decision = "keep_current"
)

// TurnCompletion is the latest prompt or harness turn that ended: its command
// and the ordinal of its completion event. `HandbackReminder` names a turn by
// this identity, and its sender takes CompletedOrdinal from the projected
// turn outcome, so the two must stay equal.
//
// Jev's verdict on a turn lives in the worker's assessment record
// (TurnAssessment), and nothing reads Decision. It is still written, always
// as KeepCurrent, because the relay protocol lets a newer worker serve an
// older daemon: daemons before #1304 require the field, and read anything but
// KeepCurrent as a verdict on this turn.
type TurnCompletion struct {
	CommandID        string   `json:"command_id"`
	CompletedOrdinal uint64   `json:"completed_ordinal"`
	Decision         Decision `json:"decision"`
}

// Port of mj-core/src/acp/claude_result.rs.

// Claude Code SDK `result` messages, which mark the end of one model cycle.
//
// Foreground prompts end only at the adapter's `session/prompt` response.
// Announced autonomous turns end at their AIR task's terminal update. Raw
// results remain the boundary for cycles without an announced task.

package core

// ClaudeTurnResult is one raw SDK result, retained in runtime journals for
// diagnostics. Only autonomous turn completion consumes it; foreground
// replies are authoritative. Keep the historical payload fields for readers
// of runtime journal records.
type ClaudeTurnResult struct {
	// OriginKind is nil when Claude Code does not report an origin (older builds).
	OriginKind *string `json:"origin_kind,omitempty"`
	Subtype    string  `json:"subtype"`
	IsError    bool    `json:"is_error"`
	StopReason *string `json:"stop_reason,omitempty"`
	// NumTurns is the model round trips in this cycle. Zero for a local
	// command, which makes no model call, and for a request the provider
	// refused.
	NumTurns uint64 `json:"num_turns"`
	// QueuedTurnCount is the user messages still waiting in Claude Code's
	// queue when the cycle ended. More than zero means another user cycle
	// follows, for example the one a steer injected. Zero when Claude Code
	// does not report it.
	QueuedTurnCount uint64 `json:"queued_turn_count"`
	// SignedOut: the result text asks the user to sign in again, which the
	// adapter reports as an `auth_required` failure of the prompt.
	SignedOut bool `json:"signed_out"`
	// Diagnostic is Claude Code's execution diagnostic, the text starting
	// with `[ede_diagnostic]`, when the result carries one.
	Diagnostic *string           `json:"diagnostic,omitempty"`
	Usage      ClaudeResultUsage `json:"usage"`
}

// ClaudeResultUsage is the token counts of one result, in the adapter's
// tally shape.
type ClaudeResultUsage struct {
	InputTokens      uint64 `json:"input_tokens"`
	OutputTokens     uint64 `json:"output_tokens"`
	CacheReadTokens  uint64 `json:"cache_read_tokens"`
	CacheWriteTokens uint64 `json:"cache_write_tokens"`
}

// ClaudeTurnResultFromSDKMessage reads an SDK message the adapter forwarded.
// A nil result with a nil error means the message is not a `result`.
func ClaudeTurnResultFromSDKMessage(message any) (*ClaudeTurnResult, error) {
	panic("unported: mj-core/src/acp/claude_result.rs ClaudeTurnResult::from_sdk_message")
}

const claudeDiagnosticPrefix = "[ede_diagnostic]"

type claudeResultPayload struct {
	Subtype         string                     `json:"subtype"`
	IsError         bool                       `json:"is_error"`
	NumTurns        uint64                     `json:"num_turns"`
	StopReason      *string                    `json:"stop_reason"`
	QueuedTurnCount *uint64                    `json:"queued_turn_count"`
	Result          *string                    `json:"result"`
	Errors          []string                   `json:"errors"`
	Origin          *claudeResultOriginPayload `json:"origin"`
	Usage           *claudeResultUsagePayload  `json:"usage"`
}

type claudeResultOriginPayload struct {
	Kind string `json:"kind"`
}

// claudeResultUsagePayload: third-party backends have been seen omitting
// token fields, so a missing or null count reads as zero instead of
// discarding the result.
type claudeResultUsagePayload struct {
	InputTokens              *uint64 `json:"input_tokens"`
	OutputTokens             *uint64 `json:"output_tokens"`
	CacheReadInputTokens     *uint64 `json:"cache_read_input_tokens"`
	CacheCreationInputTokens *uint64 `json:"cache_creation_input_tokens"`
}

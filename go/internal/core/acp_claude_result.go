// Port of mj-core/src/acp/claude_result.rs.

// Claude Code SDK `result` messages, which mark the end of one model cycle.
//
// Foreground prompts end only at the adapter's `session/prompt` response.
// Announced autonomous turns end at their AIR task's terminal update. Raw
// results remain the boundary for cycles without an announced task.

package core

import (
	"encoding/json"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

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
	if kind, ok := agentJSONString(message, "type"); !ok || kind != "result" {
		return nil, nil
	}
	data, err := jsonvalue.Marshal(message)
	if err != nil {
		return nil, err
	}
	if err := agentRequireFields(data, "subtype", "is_error", "num_turns"); err != nil {
		return nil, err
	}
	var payload claudeResultPayload
	if err := json.Unmarshal(data, &payload); err != nil {
		return nil, err
	}
	var diagnostic *string
	if payload.Subtype == "success" {
		if payload.Result != nil && strings.HasPrefix(*payload.Result, claudeDiagnosticPrefix) {
			text := *payload.Result
			diagnostic = &text
		}
	} else {
		for _, text := range payload.Errors {
			if strings.HasPrefix(text, claudeDiagnosticPrefix) {
				diagnostic = &text
				break
			}
		}
	}
	var usage ClaudeResultUsage
	if payload.Usage != nil {
		count := func(value *uint64) uint64 {
			if value == nil {
				return 0
			}
			return *value
		}
		usage = ClaudeResultUsage{
			InputTokens:      count(payload.Usage.InputTokens),
			OutputTokens:     count(payload.Usage.OutputTokens),
			CacheReadTokens:  count(payload.Usage.CacheReadInputTokens),
			CacheWriteTokens: count(payload.Usage.CacheCreationInputTokens),
		}
	}
	result := &ClaudeTurnResult{
		Subtype:    payload.Subtype,
		IsError:    payload.IsError,
		StopReason: payload.StopReason,
		NumTurns:   payload.NumTurns,
		SignedOut: payload.Subtype == "success" && payload.Result != nil &&
			strings.Contains(*payload.Result, "Please run /login"),
		Diagnostic: diagnostic,
		Usage:      usage,
	}
	if payload.Origin != nil {
		kind := payload.Origin.Kind
		result.OriginKind = &kind
	}
	if payload.QueuedTurnCount != nil {
		result.QueuedTurnCount = *payload.QueuedTurnCount
	}
	return result, nil
}

// UnmarshalJSON requires the fields serde requires.
func (r *ClaudeTurnResult) UnmarshalJSON(data []byte) error {
	if err := agentRequireFields(data, "subtype", "is_error", "num_turns"); err != nil {
		return err
	}
	type plain ClaudeTurnResult
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*r = ClaudeTurnResult(decoded)
	return nil
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

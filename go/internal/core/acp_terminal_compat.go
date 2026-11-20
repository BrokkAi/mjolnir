// Port of mj-core/src/acp/terminal_compat.rs.

// Transcript compatibility for agents that use client terminals without
// publishing the ACP tool call that owns them.

package core

import (
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
)

const fallbackTerminalMetaKey = "hel.dev/fallback-terminal"
const fallbackTerminalToolIDPrefix = "hel-terminal:"

func FallbackTerminalToolCall(terminalID string, command string) schema.ToolCall {
	kind := schema.ToolKindExecute
	status := schema.ToolCallStatusInProgress
	return schema.ToolCall{
		ToolCallID: schema.ToolCallId(FallbackTerminalToolCallID(terminalID)),
		Title:      command,
		Kind:       &kind,
		Status:     &status,
		Content: []schema.ToolCallContent{{
			Terminal: &schema.Terminal{TerminalID: schema.TerminalId(terminalID)},
		}},
		Meta: jsonvalue.ObjectOf(map[string]any{fallbackTerminalMetaKey: true}),
	}
}

func FallbackTerminalToolCallID(terminalID string) string {
	return fallbackTerminalToolIDPrefix + terminalID
}

func IsFallbackTerminalToolCall(call *schema.ToolCall) bool {
	value, ok := call.Meta.Get(fallbackTerminalMetaKey)
	flag, isBool := value.(bool)
	return ok && isBool && flag
}

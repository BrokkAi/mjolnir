// Port of mj-sessionwiki/src/sessionwiki/harness_tools.rs.
//
// Map native SessionWiki tool names to Mjolnir harnesses.

package sessionwiki

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// toolName is the SessionWiki tool name a harness's sessions are indexed
// under.
//
// This is the only table of tool names; harnessForTool reads it backwards.
func toolName(kind core.HarnessKind) string {
	switch kind {
	case core.HarnessKindClaude:
		return "claude-code"
	case core.HarnessKindCodex:
		return "codex"
	case core.HarnessKindKimi:
		return "kimi-code"
	case core.HarnessKindGrok:
		return "grok-build"
	case core.HarnessKindMuse:
		return "muse"
	case core.HarnessKindOpenCode:
		return "opencode"
	case core.HarnessKindDsh:
		return "dsh"
	case core.HarnessKindAntigravity:
		return "antigravity"
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(kind)))
}

// harnessForTool is the harness a SessionWiki tool name names, or false for
// an unsupported harness. A known harness need not have a native import
// reader yet.
func harnessForTool(name string) (core.HarnessKind, bool) {
	for _, kind := range core.HarnessKindAll {
		if toolName(kind) == name {
			return kind, true
		}
	}
	return "", false
}

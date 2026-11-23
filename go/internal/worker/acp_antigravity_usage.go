// Port of mj-worker/src/acp/antigravity_usage.rs.

// Antigravity's prompt-response usage, verified as whole-turn accounting.

package worker

import (
	"errors"
	"fmt"
	"slices"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// antigravityUsageScopeKey is Rust's antigravity_usage::USAGE_SCOPE_KEY
// (the same key as opencode_usage's usageScopeKey).
const antigravityUsageScopeKey = "mjolnir.dev/usage-scope"

// antigravityWholeTurnVersions are the Antigravity (`agy acp`) versions whose
// prompt responses were verified live against their per-step transcripts to
// report the whole turn (every model step of the prompt). Another version,
// for example one found on PATH, keeps the adapter's undeclared scope until
// checked.
var antigravityWholeTurnVersions = [1]string{"1.5.11"}

// AntigravityUsageReportsWholeTurns reports whether this Antigravity ACP
// server, as it named itself in `initialize`, reports whole turns. A nil
// agentVersion is no version.
//
// (Rust antigravity_usage::reports_whole_turns; prefixed because
// opencode_usage's is ReportsWholeTurns.)
func AntigravityUsageReportsWholeTurns(agentVersion *string) bool {
	return agentVersion != nil && slices.Contains(antigravityWholeTurnVersions[:], *agentVersion)
}

// AntigravityUsageNormalize marks a verified Antigravity report as a whole
// turn.
//
// Antigravity 1.5.11 sums every model step of a prompt into one per-turn
// `Usage` report without declaring `mjolnir.dev/usage-scope`. Its counters
// match Claude's convention: `input_tokens` is uncached input, cache tokens
// are separate tallies, `output_tokens` includes `thought_tokens`, and
// `input + output + cached_read + cached_write == total`. Verify those
// invariants before marking the report as UsageScopeTurn; a mismatch stays
// unspecified and unchanged so it never enters totals or pricing. A report
// that declares any scope, even `unspecified`, is left as declared. A nil
// usageMeta is an absent `_meta`.
//
// (Rust antigravity_usage::normalize; prefixed because grok_usage::normalize
// is normalize.)
func AntigravityUsageNormalize(usage *core.TokenUsage, usageMeta schema.Meta) error {
	if usageMeta != nil {
		if _, declared := usageMeta.Get(antigravityUsageScopeKey); declared {
			return nil
		}
	}
	if usage.ThoughtTokens != nil && *usage.ThoughtTokens > usage.OutputTokens {
		return fmt.Errorf("Antigravity thought tokens (%d) exceed output tokens (%d)",
			acpFeaturesU64Or(usage.ThoughtTokens, 0), usage.OutputTokens)
	}
	read, write := acpFeaturesU64Or(usage.CachedReadTokens, 0), acpFeaturesU64Or(usage.CachedWriteTokens, 0)
	if read+write < read {
		return errors.New("Antigravity cache token overflow")
	}
	cache := read + write
	if usage.InputTokens+usage.OutputTokens < usage.InputTokens ||
		usage.InputTokens+usage.OutputTokens+cache < usage.InputTokens+usage.OutputTokens {
		return errors.New("Antigravity usage total overflow")
	}
	total := usage.InputTokens + usage.OutputTokens + cache
	if total != usage.TotalTokens {
		return fmt.Errorf("Antigravity reported %d input, %d output, and %d cache tokens (%d), but %d in total",
			usage.InputTokens, usage.OutputTokens, cache, total, usage.TotalTokens)
	}
	usage.Scope = core.UsageScopeTurn
	return nil
}

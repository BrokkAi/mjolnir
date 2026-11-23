// Port of mj-worker/src/acp/opencode_usage.rs.

// OpenCode's prompt-response usage, converted to the convention `mj usage`
// totals use.

package worker

import (
	"errors"
	"fmt"
	"slices"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const usageScopeKey = "mjolnir.dev/usage-scope"

// wholeTurnVersions are the OpenCode versions whose prompt responses were
// checked live to report the whole turn (every model step of the prompt):
// the pinned version, and the older one that containers built from an older
// image still run. Another version, for example one found on PATH, may
// report only the last step, so its report keeps the adapter's undeclared
// scope.
var wholeTurnVersions = [3]string{"2.2.1", "2.2.6", "2.2.7"}

// ReportsWholeTurns reports whether this OpenCode, as it named itself in
// `initialize`, reports whole turns. A nil agentVersion is no version.
func ReportsWholeTurns(agentVersion *string) bool {
	return agentVersion != nil && slices.Contains(wholeTurnVersions[:], *agentVersion)
}

// OpencodeUsageNormalize folds OpenCode's counts into the full input and
// output mj totals use.
//
// OpenCode 2.2.1, 2.2.6, and 2.2.7 sum their model steps into one per-prompt
// figure but do not declare a scope, and it counts differently from the rest
// of mj: `input` excludes cache reads and writes, and `output` excludes
// reasoning. Fold both back in so input and output are the full counts, then
// mark the report as a whole turn. A report that does not add up to its own
// total is left unspecified and unchanged, so it never enters the totals or
// the pricing. A report that declares any scope, even `unspecified`, is left
// as declared. A nil usageMeta is an absent `_meta`.
//
// (Rust opencode_usage::normalize; prefixed because grok_usage::normalize is
// normalize.)
func OpencodeUsageNormalize(usage *core.TokenUsage, usageMeta schema.Meta) error {
	if usageMeta != nil {
		if _, declared := usageMeta.Get(usageScopeKey); declared {
			return nil
		}
	}
	read, write := acpFeaturesU64Or(usage.CachedReadTokens, 0), acpFeaturesU64Or(usage.CachedWriteTokens, 0)
	if read+write < read {
		return errors.New("OpenCode cache token overflow")
	}
	cache := read + write
	if usage.InputTokens+cache < usage.InputTokens {
		return errors.New("OpenCode input token overflow")
	}
	input := usage.InputTokens + cache
	thought := acpFeaturesU64Or(usage.ThoughtTokens, 0)
	if usage.OutputTokens+thought < usage.OutputTokens {
		return errors.New("OpenCode output token overflow")
	}
	output := usage.OutputTokens + thought
	if input+output < input || input+output != usage.TotalTokens {
		return fmt.Errorf("OpenCode reported %d input and %d output tokens including cache and reasoning, "+
			"but %d in total", input, output, usage.TotalTokens)
	}
	usage.InputTokens = input
	usage.OutputTokens = output
	usage.Scope = core.UsageScopeTurn
	return nil
}

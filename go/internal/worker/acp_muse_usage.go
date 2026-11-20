// Port of mj-worker/src/acp/muse_usage.rs.

// Muse's turn extras, reported inside the prompt response's usage metadata.

package worker

import (
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MuseUsageAttachProviderDetails records the model-leg accounting Muse
// reports beside the standard counters. The extras live on the `Usage`
// object's own `_meta`, not on the response `_meta`. Anything missing or
// malformed is left out rather than invented, so a partial report still
// contributes the fields it does carry. A nil usageMeta is an absent `_meta`.
//
// (Rust muse_usage::attach_provider_details; prefixed because
// claude_usage::attach_provider_details is attachProviderDetails.)
func MuseUsageAttachProviderDetails(usage core.TokenUsage, usageMeta schema.Meta) core.TokenUsage {
	panic("unported: mj-worker/src/acp/muse_usage.rs attach_provider_details")
}

// modelTokens reads one `modelUsage` row. The three counters are required; a
// row missing any of them is dropped rather than reported with an invented
// zero.
func modelTokens(report any, scope core.UsageScope) (core.TokenUsage, bool) {
	panic("unported: mj-worker/src/acp/muse_usage.rs model_tokens")
}

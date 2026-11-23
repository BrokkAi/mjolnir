// Port of mj-worker/src/acp/muse_usage.rs.

// Muse's turn extras, reported inside the prompt response's usage metadata.

package worker

import (
	"github.com/BrokkAi/acp-go/jsonvalue"
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
	if usageMeta == nil {
		return usage
	}
	muse, ok := usageMeta.Get("muse")
	if !ok {
		return usage
	}
	var modelCalls, apiDurationMs *uint64
	if value, ok := acpFeaturesGet(muse, "modelCalls"); ok {
		if calls, ok := acpFeaturesAsU64(value); ok {
			modelCalls = &calls
		}
	}
	if value, ok := acpFeaturesGet(muse, "apiDurationMs"); ok {
		if duration, ok := acpFeaturesAsU64(value); ok {
			apiDurationMs = &duration
		}
	}
	modelUsage := map[string]core.TokenUsage{}
	if value, ok := acpFeaturesGet(muse, "modelUsage"); ok {
		if models, ok := value.(jsonvalue.Object); ok {
			for _, model := range models.Keys() {
				report, _ := models.Get(model)
				if tokens, ok := modelTokens(report, usage.Scope); ok {
					modelUsage[model] = tokens
				}
			}
		}
	}
	if modelCalls == nil && apiDurationMs == nil && len(modelUsage) == 0 {
		return usage
	}
	usage.ProviderDetails = &core.ProviderTurnUsage{
		ModelCalls:    modelCalls,
		APIDurationMs: apiDurationMs,
		ModelUsage:    modelUsage,
	}
	return usage
}

// modelTokens reads one `modelUsage` row. The three counters are required; a
// row missing any of them is dropped rather than reported with an invented
// zero.
func modelTokens(report any, scope core.UsageScope) (core.TokenUsage, bool) {
	count := func(field string) *uint64 {
		value, ok := acpFeaturesGet(report, field)
		if !ok {
			return nil
		}
		number, ok := acpFeaturesAsU64(value)
		if !ok {
			return nil
		}
		return &number
	}
	total, input, output := count("totalTokens"), count("inputTokens"), count("outputTokens")
	if total == nil || input == nil || output == nil {
		return core.TokenUsage{}, false
	}
	return core.TokenUsage{
		Scope:             scope,
		TotalTokens:       *total,
		InputTokens:       *input,
		OutputTokens:      *output,
		ThoughtTokens:     count("thoughtTokens"),
		CachedReadTokens:  count("cachedReadTokens"),
		CachedWriteTokens: count("cachedWriteTokens"),
	}, true
}

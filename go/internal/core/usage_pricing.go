// Port of mj-core/src/usage/pricing.rs.
//
// Standard base token API rates checked 2026-10-14, USD per million tokens.
// Sources: https://developers.openai.com/api/docs/pricing
// https://developers.openai.com/api/docs/models/gpt-6-sol
// https://platform.claude.com/docs/en/about-claude/pricing
// https://docs.z.ai/guides/overview/pricing
// https://api-docs.deepseek.com/quick_start/pricing/
// https://docs.x.ai/developers/models/grok-4.7
// https://docs.x.ai/developers/pricing
// https://platform.kimi.ai/docs/pricing/chat
// Default 5-minute writes; excludes tools, speed/region/context premiums.

package core

import "strings"

// UsagePricingNormalizeModel normalizes provider qualifiers and context
// hints without losing versions. (Rust usage::pricing::normalize_model,
// prefixed because model_catalog has its own NormalizeModel.)
func UsagePricingNormalizeModel(model string) string {
	normalized := NormalizeModel(model)
	if trimmed, ok := strings.CutSuffix(normalized, "]"); ok {
		if index := strings.LastIndex(trimmed, "["); index >= 0 {
			return trimmed[:index]
		}
	}
	return normalized
}

func baseModel(model string) string {
	base := strings.TrimSuffix(UsagePricingNormalizeModel(model), "-latest")
	if index := strings.LastIndex(base, "-"); index >= 0 {
		date := base[index+1:]
		if len(date) == 8 && strings.Trim(date, "0123456789") == "" {
			return base[:index]
		}
	}
	return base
}

// Rates is [ordinary input, output, cache read, cache write], in 0.001 USD
// per million tokens (ten USD ticks per token). Unknown rates stay unknown.
func Rates(model string) ([4]uint64, bool) {
	panic("unported: mj-core/src/usage/pricing.rs rates")
}

func IsPriced(model string) bool {
	_, ok := Rates(model)
	return ok
}

// Estimate prices one model's counter totals; nil when the cost cannot be
// known.
func Estimate(model *string, totals map[string]UsageCounterTotal) (*uint64, error) {
	panic("unported: mj-core/src/usage/pricing.rs estimate")
}

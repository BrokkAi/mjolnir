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

import (
	"errors"
	"math/bits"
	"strings"
)

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
	switch baseModel(model) {
	// Checked 2026-11-17: https://developers.openai.com/api/docs/pricing
	case "gpt-6.2-sol":
		return [4]uint64{2000, 10000, 100, 2500}, true
	case "gpt-6.2-astra":
		return [4]uint64{8000, 40000, 400, 10000}, true
	// Checked 2026-11-11: https://developers.openai.com/api/docs/models/gpt-6.1-terra
	case "gpt-6.1-terra":
		return [4]uint64{1000, 5000, 50, 1250}, true
	case "gpt-6.1-sol":
		return [4]uint64{2000, 10000, 100, 2500}, true
	case "gpt-6-sol":
		return [4]uint64{2000, 10000, 200, 2500}, true
	case "gpt-6-astra":
		return [4]uint64{10000, 50000, 1000, 12500}, true
	case "gpt-6-luna":
		return [4]uint64{100, 500, 10, 125}, true
	case "gpt-5.6-sol":
		return [4]uint64{4000, 20000, 400, 5000}, true
	case "claude-fable-5-1", "claude-mythos-5-1":
		return [4]uint64{10000, 50000, 250, 12500}, true
	case "claude-opus-5-5":
		return [4]uint64{4000, 20000, 200, 5000}, true
	case "claude-sonnet-5-5":
		return [4]uint64{2000, 10000, 100, 2500}, true
	case "claude-haiku-5-5":
		return [4]uint64{100, 500, 10, 125}, true
	case "claude-fable-5", "claude-mythos-5":
		return [4]uint64{10000, 50000, 1000, 12500}, true
	case "claude-opus-5", "claude-opus-4-8", "claude-opus-4-7", "claude-opus-4-6",
		"claude-opus-4-5":
		return [4]uint64{5000, 25000, 500, 6250}, true
	case "claude-opus-4", "claude-opus-4-1":
		return [4]uint64{15000, 75000, 1500, 18750}, true
	case "claude-sonnet-5":
		return [4]uint64{2000, 10000, 200, 2500}, true
	case "claude-sonnet-4", "claude-sonnet-4-5", "claude-sonnet-4-6":
		return [4]uint64{3000, 15000, 300, 3750}, true
	case "claude-haiku-4-5":
		return [4]uint64{1000, 5000, 100, 1250}, true
	case "claude-3-5-haiku":
		return [4]uint64{800, 4000, 80, 1000}, true
	case "glm-5.3", "glm-5.2", "glm-5.1":
		return [4]uint64{1400, 4400, 260, 0}, true
	case "glm-5":
		return [4]uint64{1000, 3200, 200, 0}, true
	case "glm-4.7", "glm-4.6", "glm-4.5":
		return [4]uint64{600, 2200, 110, 0}, true
	case "glm-5.3-flash":
		return [4]uint64{150, 500, 30, 0}, true
	case "glm-5.3-flashx":
		return [4]uint64{370, 1250, 75, 0}, true
	case "glm-4.7-flash", "glm-4.5-flash":
		return [4]uint64{0, 0, 0, 0}, true
	// Peak rates: a stable estimate without reconstructing holidays/request times.
	// DeepSeek pricing checked 2026-11-05: https://api-docs.deepseek.com/quick_start/pricing/
	case "deepseek-flash", "deepseek-v4-flash", "deepseek-v4-flash-vision-exp":
		return [4]uint64{300, 1200, 6, 300}, true
	case "deepseek-v4-pro":
		return [4]uint64{1320, 3960, 44, 1320}, true
	case "grok-4.7", "grok-4.6":
		return [4]uint64{2000, 6000, 500, 2000}, true
	case "grok-4.5":
		return [4]uint64{2000, 6000, 300, 2000}, true
	case "grok-4.3",
		"grok-4.20-multi-agent-0309",
		"grok-4.20-0309-reasoning",
		"grok-4.20-0309-non-reasoning":
		return [4]uint64{1250, 2500, 200, 1250}, true
	case "grok-build-0.1":
		return [4]uint64{1000, 2000, 200, 1000}, true
	case "kimi-k3", "kimi-code/k3":
		return [4]uint64{3000, 15000, 300, 3000}, true
	case "kimi-k2.7-code":
		return [4]uint64{950, 4000, 190, 950}, true
	case "kimi-k2.7-code-highspeed":
		return [4]uint64{1900, 8000, 380, 1900}, true
	case "kimi-k2.6":
		return [4]uint64{950, 4000, 160, 950}, true
	}
	return [4]uint64{}, false
}

func IsPriced(model string) bool {
	_, ok := Rates(model)
	return ok
}

// Estimate prices one model's counter totals; nil when the cost cannot be
// known.
func Estimate(model *string, totals map[string]UsageCounterTotal) (*uint64, error) {
	if model == nil {
		return nil, nil
	}
	rates, ok := Rates(*model)
	if !ok {
		return nil, nil
	}
	count := func(name string) (uint64, bool) {
		counter, ok := totals[name]
		return counter.Tokens, ok
	}
	input, hasInput := count("input_tokens")
	output, hasOutput := count("output_tokens")
	total, hasTotal := count("total_tokens")
	if !hasInput || !hasOutput || !hasTotal {
		return nil, nil
	}
	read, _ := count("cached_read_tokens")
	write, _ := count("cached_write_tokens")
	caches, carry := bits.Add64(read, write, 0)
	if carry != 0 {
		return nil, errors.New("cache token overflow")
	}
	inputOutput, ioCarry := bits.Add64(input, output, 0)
	withCaches, cacheCarry := bits.Add64(inputOutput, caches, 0)
	var ordinary uint64
	switch {
	case ioCarry == 0 && inputOutput == total:
		// Codex, Muse, Kimi and Grok include cache tokens in input.
		if input < caches {
			return nil, nil
		}
		ordinary = input - caches
	case ioCarry == 0 && cacheCarry == 0 && withCaches == total:
		// Claude's input is the uncached count; caches are separate tallies.
		ordinary = input
	default:
		return nil, nil
	}
	var cost uint64
	for index, tokens := range [4]uint64{ordinary, output, read, write} {
		hi, product := bits.Mul64(tokens, rates[index])
		if hi != 0 {
			return nil, errors.New("API cost overflow")
		}
		hi, product = bits.Mul64(product, 10)
		if hi != 0 {
			return nil, errors.New("API cost overflow")
		}
		sum, carry := bits.Add64(cost, product, 0)
		if carry != 0 {
			return nil, errors.New("API cost overflow")
		}
		cost = sum
	}
	return &cost, nil
}

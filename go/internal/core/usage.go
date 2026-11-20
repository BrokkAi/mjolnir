// Port of mj-core/src/usage.rs.
//
// Provider-reported consumption. Context occupancy is deliberately excluded.

package core

import (
	"fmt"

	acp "github.com/BrokkAi/acp-go/schema/unstable"
)

type UsageScope string

const (
	UsageScopeTurn        UsageScope = "turn"
	UsageScopeLastRequest UsageScope = "last_request"
	UsageScopeUnspecified UsageScope = "unspecified"
)

type TokenUsage struct {
	ProviderDetails   *ProviderTurnUsage `json:"provider_details,omitempty"`
	Scope             UsageScope         `json:"scope"`
	TotalTokens       uint64             `json:"total_tokens"`
	InputTokens       uint64             `json:"input_tokens"`
	OutputTokens      uint64             `json:"output_tokens"`
	ThoughtTokens     *uint64            `json:"thought_tokens,omitempty"`
	CachedReadTokens  *uint64            `json:"cached_read_tokens,omitempty"`
	CachedWriteTokens *uint64            `json:"cached_write_tokens,omitempty"`
}

func (u *TokenUsage) UnmarshalJSON(data []byte) error {
	type plain TokenUsage
	return transcriptStrictDecode(data, (*plain)(u), false)
}

// TokenUsageFromACP reads an ACP usage report.
//
// Explicit adapter metadata takes precedence over legacy defaults. In
// particular, an incomplete Codex report must never enter turn totals. Any
// other adapter that declares its scope is believed. Muse 0.4.3 and later
// declare turn on the prompt response; an undeclared Muse report stays
// unspecified.
func TokenUsageFromACP(harness HarnessKind, usage acp.Usage) TokenUsage {
	var declared *string
	if usage.Meta != nil {
		if value, ok := usage.Meta.Get("mjolnir.dev/usage-scope"); ok {
			if text, ok := value.(string); ok {
				declared = &text
			}
		}
	}
	var scope UsageScope
	switch {
	case harness == HarnessKindCodex && declared != nil && *declared == "turn":
		scope = UsageScopeTurn
	case harness == HarnessKindCodex && declared != nil:
		scope = UsageScopeUnspecified
	case harness == HarnessKindCodex:
		scope = UsageScopeLastRequest
	case declared != nil && *declared == "turn":
		scope = UsageScopeTurn
	case declared != nil:
		scope = UsageScopeUnspecified
	case harness == HarnessKindClaude:
		scope = UsageScopeTurn
	default:
		scope = UsageScopeUnspecified
	}
	return TokenUsage{
		Scope:             scope,
		TotalTokens:       usage.TotalTokens,
		InputTokens:       usage.InputTokens,
		OutputTokens:      usage.OutputTokens,
		ThoughtTokens:     usage.ThoughtTokens,
		CachedReadTokens:  usage.CachedReadTokens,
		CachedWriteTokens: usage.CachedWriteTokens,
	}
}

type ProviderCost struct {
	// Amount is the cumulative amount reported for the provider's session,
	// not a turn delta. It is written as serde_json writes an f64.
	Amount       float64 `json:"amount"`
	Currency     string  `json:"currency"`
	ObservedAtMs int64   `json:"observed_at_ms"`
}

func (c ProviderCost) MarshalJSON() ([]byte, error) {
	return transcriptMarshalMembers([]transcriptMember{
		{"amount", ElicitationValueNumber(c.Amount)},
		{"currency", c.Currency},
		{"observed_at_ms", c.ObservedAtMs},
	})
}

func (c *ProviderCost) UnmarshalJSON(data []byte) error {
	type plain ProviderCost
	return transcriptStrictDecode(data, (*plain)(c), false)
}

// ProviderTurnUsage is provider-reported accounting for this turn, separate
// from session cost.
type ProviderTurnUsage struct {
	Cost          *ProviderTurnCost     `json:"cost,omitempty"`
	ModelCalls    *uint64               `json:"model_calls,omitempty"`
	APIDurationMs *uint64               `json:"api_duration_ms,omitempty"`
	ElapsedMs     *uint64               `json:"elapsed_ms,omitempty"`
	ModelUsage    map[string]TokenUsage `json:"model_usage,omitempty"`
}

type ProviderTurnCost struct {
	UsdTicks uint64 `json:"usd_ticks"`
	// Usd is the exact decimal USD amount: 10^10 ticks per USD, without
	// float rounding.
	Usd       string `json:"usd"`
	IsPartial bool   `json:"is_partial"`
}

func (c *ProviderTurnCost) UnmarshalJSON(data []byte) error {
	type plain ProviderTurnCost
	return transcriptStrictDecode(data, (*plain)(c), false)
}

func ProviderTurnCostFromUsdTicks(usdTicks uint64, isPartial bool) ProviderTurnCost {
	return ProviderTurnCost{
		UsdTicks:  usdTicks,
		Usd:       fmt.Sprintf("%d.%010d", usdTicks/10_000_000_000, usdTicks%10_000_000_000),
		IsPartial: isPartial,
	}
}

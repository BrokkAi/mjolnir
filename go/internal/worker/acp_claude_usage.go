// Port of mj-worker/src/acp/claude_usage.rs.

// Claude's per-turn model accounting includes native agents and internal
// calls.

package worker

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// attachProviderDetails replaces the main-loop counts with the response's
// fuller per-model accounting.
//
// The response's `_meta.quota.model_usage` is fuller than its flat `usage`,
// which covers only the main agent loop. The bridge already differences the
// SDK's cumulative model readings, so these rows must never be differenced
// again.
func attachProviderDetails(usage *core.TokenUsage, meta schema.Meta) error {
	panic("unported: mj-worker/src/acp/claude_usage.rs attach_provider_details")
}

// sum adds the four counters, failing on overflow.
func sum(counts [4]uint64) (uint64, error) {
	var total uint64
	for _, count := range counts {
		if total+count < total {
			return 0, errors.New("Claude token total overflow")
		}
		total += count
	}
	return total, nil
}

// ModelUsage is one row of Claude's per-model turn usage.
type ModelUsage struct {
	Model      string     `json:"model"`
	TokenCount TokenCount `json:"token_count"`
}

func (u *ModelUsage) UnmarshalJSON(data []byte) error {
	if err := acpFeaturesRequireFields(data, "model", "token_count"); err != nil {
		return err
	}
	type plain ModelUsage
	return json.Unmarshal(data, (*plain)(u))
}

type TokenCount struct {
	TotalTokens       uint64 `json:"totalTokens"`
	InputTokens       uint64 `json:"inputTokens"`
	OutputTokens      uint64 `json:"outputTokens"`
	CachedInputTokens uint64 `json:"cachedInputTokens"`
	CachedWriteTokens uint64 `json:"cachedWriteTokens"`
}

func (c *TokenCount) UnmarshalJSON(data []byte) error {
	if err := acpFeaturesRequireFields(data, "totalTokens", "inputTokens", "outputTokens",
		"cachedInputTokens", "cachedWriteTokens"); err != nil {
		return err
	}
	type plain TokenCount
	return json.Unmarshal(data, (*plain)(c))
}

// acpFeaturesRequireFields refuses a JSON object that lacks one of names,
// with serde's "missing field" error, as serde does for a field that is
// neither optional nor defaulted. A non-object is left to the decoder. It is
// shared by the acp feature modules' decoders.
func acpFeaturesRequireFields(data []byte, names ...string) error {
	var members map[string]json.RawMessage
	if json.Unmarshal(data, &members) != nil {
		return nil
	}
	for _, name := range names {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// acpFeaturesMarshalJSON encodes v without escaping HTML characters, which
// serde never escapes.
func acpFeaturesMarshalJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

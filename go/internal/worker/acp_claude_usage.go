// Port of mj-worker/src/acp/claude_usage.rs.

// Claude's per-turn model accounting includes native agents and internal
// calls.

package worker

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"reflect"
	"strconv"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
	if meta == nil {
		return nil
	}
	quota, ok := meta.Get("quota")
	if !ok {
		return nil
	}
	value, ok := acpFeaturesGet(quota, "model_usage")
	if !ok {
		return nil
	}
	var rows []ModelUsage
	if err := acpFeaturesDecodeValue(value, &rows); err != nil {
		return fmt.Errorf("decode Claude per-model turn usage: %w", err)
	}
	if len(rows) == 0 {
		return nil
	}
	models := map[string]core.TokenUsage{}
	var totals [4]uint64
	for _, row := range rows {
		count := row.TokenCount
		counters := [4]uint64{count.InputTokens, count.OutputTokens, count.CachedInputTokens, count.CachedWriteTokens}
		total, err := sum(counters)
		if err != nil {
			return err
		}
		if total != count.TotalTokens {
			return errors.New("inconsistent Claude model token total")
		}
		for index, value := range counters {
			if totals[index]+value < totals[index] {
				return errors.New("Claude model usage overflow")
			}
			totals[index] += value
		}
		cachedRead, cachedWrite := count.CachedInputTokens, count.CachedWriteTokens
		model := core.TokenUsage{
			Scope:        usage.Scope,
			TotalTokens:  total,
			InputTokens:  count.InputTokens,
			OutputTokens: count.OutputTokens,
			// Claude bills thinking within output.
			ThoughtTokens:     nil,
			CachedReadTokens:  &cachedRead,
			CachedWriteTokens: &cachedWrite,
		}
		if _, duplicate := models[row.Model]; duplicate {
			return errors.New("duplicate Claude usage model")
		}
		models[row.Model] = model
	}
	// Replace the main-loop counts with the fuller accounting, never add the
	// main loop twice. If model rows omit known usage, retain that remainder
	// in the totals: the shared attribution reader leaves it unattributed.
	input := max(usage.InputTokens, totals[0])
	output := max(usage.OutputTokens, totals[1])
	read := max(acpFeaturesU64Or(usage.CachedReadTokens, 0), totals[2])
	write := max(acpFeaturesU64Or(usage.CachedWriteTokens, 0), totals[3])
	total, err := sum([4]uint64{input, output, read, write})
	if err != nil {
		return err
	}
	total = max(total, usage.TotalTokens)
	usage.InputTokens = input
	usage.OutputTokens = output
	usage.CachedReadTokens = &read
	usage.CachedWriteTokens = &write
	usage.TotalTokens = total
	usage.ProviderDetails = &core.ProviderTurnUsage{ModelUsage: models}
	return nil
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

// acpFeaturesGet reads one member of a decoded JSON object (serde_json's
// Value::get with a string key): a jsonvalue.Object, or a Go map from
// encoding/json.
func acpFeaturesGet(value any, key string) (any, bool) {
	switch object := value.(type) {
	case jsonvalue.Object:
		return object.Get(key)
	case map[string]any:
		member, ok := object[key]
		return member, ok
	}
	return nil, false
}

// acpFeaturesPointer follows object keys, as serde_json's Value::pointer
// does for object members.
func acpFeaturesPointer(value any, path ...string) (any, bool) {
	for _, key := range path {
		member, ok := acpFeaturesGet(value, key)
		if !ok {
			return nil, false
		}
		value = member
	}
	return value, true
}

// acpFeaturesString reads a string member (Value::get(key).and_then(as_str)).
func acpFeaturesString(value any, key string) (string, bool) {
	member, _ := acpFeaturesGet(value, key)
	text, ok := member.(string)
	return text, ok
}

// acpFeaturesAsU64 is serde_json's Value::as_u64.
func acpFeaturesAsU64(value any) (uint64, bool) {
	switch number := value.(type) {
	case uint64:
		return number, true
	case int64:
		if number >= 0 {
			return uint64(number), true
		}
	case float64:
		// encoding/json's decoding of a number into an any.
		if number >= 0 && number == float64(uint64(number)) && number < 1<<64 {
			return uint64(number), true
		}
	case json.Number:
		parsed, err := strconv.ParseUint(string(number), 10, 64)
		return parsed, err == nil
	}
	return 0, false
}

// acpFeaturesAsI64 is serde_json's Value::as_i64.
func acpFeaturesAsI64(value any) (int64, bool) {
	switch number := value.(type) {
	case int64:
		return number, true
	case uint64:
		if number <= math.MaxInt64 {
			return int64(number), true
		}
	case float64:
		if number == float64(int64(number)) && number >= math.MinInt64 && number < math.MaxInt64 {
			return int64(number), true
		}
	case json.Number:
		parsed, err := strconv.ParseInt(string(number), 10, 64)
		return parsed, err == nil
	}
	return 0, false
}

// acpFeaturesAsF64 is serde_json's Value::as_f64.
func acpFeaturesAsF64(value any) (float64, bool) {
	switch number := value.(type) {
	case float64:
		return number, true
	case int64:
		return float64(number), true
	case uint64:
		return float64(number), true
	case json.Number:
		parsed, err := number.Float64()
		return parsed, err == nil
	}
	return 0, false
}

// acpFeaturesU64Or is Option<u64>::unwrap_or.
func acpFeaturesU64Or(value *uint64, fallback uint64) uint64 {
	if value == nil {
		return fallback
	}
	return *value
}

// acpFeaturesDecodeValue decodes a JSON value (as jsonvalue or encoding/json
// holds it) into target, as serde_json::from_value does. JSON null is
// refused for a slice, as serde refuses it for a Vec.
func acpFeaturesDecodeValue(value any, target any) error {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	if value == nil && reflect.TypeOf(target).Elem().Kind() == reflect.Slice {
		return errors.New("invalid type: null, expected a sequence")
	}
	return json.Unmarshal(encoded, target)
}

// acpFeaturesValue converts v to a JSON value as jsonvalue holds it (Rust
// serde_json::to_value).
func acpFeaturesValue(v any) (any, error) {
	encoded, err := acpFeaturesMarshalJSON(v)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Parse(encoded)
}

// acpFeaturesJSONEqual is serde_json's Value equality: as jsonvalue.Equal,
// except that object members compare regardless of order.
func acpFeaturesJSONEqual(a, b any) bool {
	left, leftIsObject := a.(jsonvalue.Object)
	right, rightIsObject := b.(jsonvalue.Object)
	if leftIsObject && rightIsObject && left != nil && right != nil {
		keys := left.Keys()
		if len(keys) != len(right.Keys()) {
			return false
		}
		for _, key := range keys {
			leftValue, _ := left.Get(key)
			rightValue, ok := right.Get(key)
			if !ok || !acpFeaturesJSONEqual(leftValue, rightValue) {
				return false
			}
		}
		return true
	}
	leftItems, leftIsArray := a.([]any)
	rightItems, rightIsArray := b.([]any)
	if leftIsArray && rightIsArray {
		if len(leftItems) != len(rightItems) {
			return false
		}
		for index := range leftItems {
			if !acpFeaturesJSONEqual(leftItems[index], rightItems[index]) {
				return false
			}
		}
		return true
	}
	return jsonvalue.Equal(a, b)
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

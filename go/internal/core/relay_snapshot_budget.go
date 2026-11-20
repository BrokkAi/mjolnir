// Port of mj-core/src/relay/snapshot/budget.rs.
//
// Byte budgets for relay events and snapshots, and the truncation that keeps
// a payload inside one.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"slices"
	"unicode/utf8"
)

// RelayBudgetExceeded is a measured byte limit, kept typed so callers can
// react to this failure without treating filesystem or serialization errors
// as capacity pressure.
type RelayBudgetExceeded struct {
	Description string
	Size        int
	Budget      int
}

func (e *RelayBudgetExceeded) Error() string {
	return fmt.Sprintf("%s is too large (%d bytes; maximum %d)", e.Description, e.Size, e.Budget)
}

func EnsureSerializedBudget(value any, budget int, description string) error {
	encoded, err := relayMarshalJSON(value)
	if err != nil {
		return fmt.Errorf("serialize %s for size validation: %w", description, err)
	}
	return EnsureByteBudget(len(encoded), budget, description)
}

func EnsureByteBudget(size, budget int, description string) error {
	if size > budget {
		return &RelayBudgetExceeded{Description: description, Size: size, Budget: budget}
	}
	return nil
}

// jsonSegment is one step in a JSON document, used to revisit a located
// string mutably: a key when isKey, otherwise an index.
type jsonSegment struct {
	isKey bool
	key   string
	index int
}

// jsonTree is a JSON document that keeps object members in written order, as
// serde_json's order-preserving Value does: nil, bool, json.Number, string,
// []any, or *jsonTreeObject.
type jsonTreeObject struct {
	keys   []string
	values []any
}

func decodeJSONTree(data []byte) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	value, err := decodeJSONTreeValue(decoder)
	if err != nil {
		return nil, err
	}
	if _, err := decoder.Token(); err != io.EOF {
		return nil, errors.New("trailing data after JSON document")
	}
	return value, nil
}

func decodeJSONTreeValue(decoder *json.Decoder) (any, error) {
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	switch token := token.(type) {
	case json.Delim:
		switch token {
		case '[':
			items := []any{}
			for decoder.More() {
				item, err := decodeJSONTreeValue(decoder)
				if err != nil {
					return nil, err
				}
				items = append(items, item)
			}
			_, err := decoder.Token()
			return items, err
		case '{':
			object := &jsonTreeObject{}
			for decoder.More() {
				key, err := decoder.Token()
				if err != nil {
					return nil, err
				}
				value, err := decodeJSONTreeValue(decoder)
				if err != nil {
					return nil, err
				}
				object.keys = append(object.keys, key.(string))
				object.values = append(object.values, value)
			}
			_, err := decoder.Token()
			return object, err
		}
		return nil, fmt.Errorf("unexpected JSON delimiter %v", token)
	default:
		return token, nil
	}
}

// encodeJSONTree writes a tree as serde_json writes a Value.
func encodeJSONTree(b *bytes.Buffer, value any) error {
	switch value := value.(type) {
	case []any:
		b.WriteByte('[')
		for i, item := range value {
			if i > 0 {
				b.WriteByte(',')
			}
			if err := encodeJSONTree(b, item); err != nil {
				return err
			}
		}
		b.WriteByte(']')
	case *jsonTreeObject:
		b.WriteByte('{')
		for i, key := range value.keys {
			if i > 0 {
				b.WriteByte(',')
			}
			relayWriteKey(b, key)
			if err := encodeJSONTree(b, value.values[i]); err != nil {
				return err
			}
		}
		b.WriteByte('}')
	default:
		encoded, err := relayMarshalJSON(value)
		if err != nil {
			return err
		}
		b.Write(encoded)
	}
	return nil
}

func encodedJSONTree(value any) ([]byte, error) {
	var b bytes.Buffer
	if err := encodeJSONTree(&b, value); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

// longestStringPath locates the longest string in a JSON document (a
// jsonTree), with the path to reach it. The first of equally long strings in
// document order wins.
func longestStringPath(value any) ([]jsonSegment, int, bool) {
	var best []jsonSegment
	bestLength, found := 0, false
	var path []jsonSegment
	var walk func(any)
	walk = func(value any) {
		switch value := value.(type) {
		case string:
			if !found || len(value) > bestLength {
				best, bestLength, found = slices.Clone(path), len(value), true
			}
		case []any:
			for index, item := range value {
				path = append(path, jsonSegment{index: index})
				walk(item)
				path = path[:len(path)-1]
			}
		case *jsonTreeObject:
			for i, key := range value.keys {
				path = append(path, jsonSegment{isKey: true, key: key})
				walk(value.values[i])
				path = path[:len(path)-1]
			}
		}
	}
	walk(value)
	return best, bestLength, found
}

// stringAtPath follows path and returns the container and slot holding the
// string there, so the caller can replace it.
func stringAtPath(value any, path []jsonSegment) (setter func(string), text string, ok bool) {
	set := func(string) {}
	cursor := value
	for _, segment := range path {
		switch container := cursor.(type) {
		case *jsonTreeObject:
			index := slices.Index(container.keys, segment.key)
			if !segment.isKey || index < 0 {
				return nil, "", false
			}
			cursor = container.values[index]
			set = func(text string) { container.values[index] = text }
		case []any:
			if segment.isKey || segment.index >= len(container) {
				return nil, "", false
			}
			index := segment.index
			cursor = container[index]
			set = func(text string) { container[index] = text }
		default:
			return nil, "", false
		}
	}
	text, ok = cursor.(string)
	if !ok {
		return nil, "", false
	}
	return set, text, true
}

// truncateWithMarker shortens text to at most keep bytes and describes what
// was dropped. Truncation lands on a character boundary, so the result stays
// valid UTF-8.
func truncateWithMarker(text string, keep int) string {
	end := min(keep, len(text))
	for end > 0 && end < len(text) && !utf8.RuneStart(text[end]) {
		end--
	}
	dropped := len(text) - end
	return text[:end] + fmt.Sprintf("… [mj truncated %d bytes]", dropped)
}

// TruncateStartWithMarker keeps at most the last keep bytes of *text and
// describes what was dropped. The kept part starts on a character boundary,
// so the result stays valid UTF-8. Returns whether anything was dropped.
//
// This is the mirror of truncateWithMarker for output whose end is the
// interesting part, such as a terminal's tail.
func TruncateStartWithMarker(text *string, keep int) bool {
	if len(*text) <= keep {
		return false
	}
	start := len(*text) - keep
	for start < len(*text) && !utf8.RuneStart((*text)[start]) {
		start++
	}
	*text = fmt.Sprintf("[mj dropped %d earlier bytes]\n", start) + (*text)[start:]
	return true
}

// ClampObservation fits an observation inside budget serialized bytes by
// shortening its largest text payloads.
//
// The ACP peer decides what the agent said; the relay only decides how much
// of it one durable event can carry. So an oversized payload is recorded in
// truncated form rather than rejected — refusing it would strand a live
// session over a transport limit it cannot see or control.
func ClampObservation(observation RelayObservation, budget int) (RelayObservation, error) {
	encoded, err := relayMarshalJSON(observation)
	if err != nil {
		return nil, fmt.Errorf("measure relay observation: %w", err)
	}
	size := len(encoded)
	if size <= budget {
		return observation, nil
	}
	// Only an observation that really has to shrink pays for the JSON tree
	// the truncation pass walks.
	value, err := decodeJSONTree(encoded)
	if err != nil {
		return nil, fmt.Errorf("serialize relay observation for clamping: %w", err)
	}
	original := size
	for size > budget {
		path, length, found := longestStringPath(value)
		if !found || length <= RelayTruncationFloor {
			break
		}
		set, text, ok := stringAtPath(value, path)
		if !ok {
			break
		}
		// Leave room for the marker itself so one pass usually suffices.
		keep := max(length-min(length, size-budget+64), RelayTruncationFloor)
		truncated := truncateWithMarker(text, keep)
		set(truncated)
		if len(path) == 0 {
			value = truncated
		}
		clamped, err := encodedJSONTree(value)
		if err != nil {
			return nil, fmt.Errorf("measure clamped relay observation: %w", err)
		}
		size = len(clamped)
	}
	if size > budget {
		return RelayObservationWarning{Message: fmt.Sprintf(
			"dropped an observation that cannot be recorded: %d bytes exceeds the %d byte event budget and its payload is not truncatable",
			original, budget,
		)}, nil
	}
	clamped, err := encodedJSONTree(value)
	if err == nil {
		var decoded RelayObservation
		if decoded, err = UnmarshalRelayObservationJSON(clamped); err == nil {
			slog.Warn("truncated an oversized relay observation", "original", original, "clamped", size)
			return decoded, nil
		}
	}
	return RelayObservationWarning{Message: fmt.Sprintf(
		"dropped an observation of %d bytes: it could not be re-read after truncation: %v", original, err,
	)}, nil
}

// Port of mj-core/src/relay/snapshot/budget.rs.
//
// Byte budgets for relay events and snapshots, and the truncation that keeps
// a payload inside one.

package core

import (
	"fmt"
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

// longestStringPath locates the longest string in a JSON document (decoded
// with UseNumber into any), with the path to reach it.
func longestStringPath(value any) ([]jsonSegment, int, bool) {
	panic("unported: mj-core/src/relay/snapshot/budget.rs longest_string_path")
}

// stringAtPath follows path and returns the container and slot holding the
// string there, so the caller can replace it.
func stringAtPath(value any, path []jsonSegment) (setter func(string), text string, ok bool) {
	panic("unported: mj-core/src/relay/snapshot/budget.rs string_at_path")
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
	panic("unported: mj-core/src/relay/snapshot/budget.rs clamp_observation")
}

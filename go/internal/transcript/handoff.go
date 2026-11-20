// Port of mj-transcript/src/handoff.rs.
//
// Durable provenance for a hidden, model-produced handoff.

package transcript

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const HandoffStateKey = "mj_handoff_state"
const ModelHandoffOpening = "You are continuing a coding session previously run by another ACP harness. The restored workspace is authoritative. Use the historical state below for continuity, and do not repeat completed work unless verification requires it.\n\n"
const RetainedOpen = "\n\n<retained_recent_context>\n"
const RetainedClose = "</retained_recent_context>"
const markerOpen = "\n\n<mj_handoff_state>"
const markerClose = "</mj_handoff_state>"
const version uint32 = 1

type HandoffState struct {
	Version         uint32 `json:"version"`
	ThroughPosition uint64 `json:"through_position"`
	PrefixSHA256    string `json:"prefix_sha256"`
	Summary         string `json:"summary"`
}

// MarshalJSON writes the state as serde_json does (no HTML escaping).
func (s HandoffState) MarshalJSON() ([]byte, error) {
	type plain HandoffState
	return jsonvalue.Marshal(plain(s))
}

// UnmarshalJSON requires every field, as serde does without defaults, and
// ignores unknown members.
func (s *HandoffState) UnmarshalJSON(data []byte) error {
	fields, err := handoffFields(data, false, "version", "through_position", "prefix_sha256", "summary")
	if err != nil {
		return err
	}
	return handoffDecodeFields(fields, map[string]any{
		"version": &s.Version, "through_position": &s.ThroughPosition,
		"prefix_sha256": &s.PrefixSHA256, "summary": &s.Summary,
	})
}

// marker is serde deny_unknown_fields.
type marker struct {
	Version         uint32 `json:"version"`
	ThroughPosition uint64 `json:"through_position"`
	PrefixSHA256    string `json:"prefix_sha256"`
}

func (m marker) MarshalJSON() ([]byte, error) {
	type plain marker
	return jsonvalue.Marshal(plain(m))
}

func (m *marker) UnmarshalJSON(data []byte) error {
	fields, err := handoffFields(data, true, "version", "through_position", "prefix_sha256")
	if err != nil {
		return err
	}
	return handoffDecodeFields(fields, map[string]any{
		"version": &m.Version, "through_position": &m.ThroughPosition, "prefix_sha256": &m.PrefixSHA256,
	})
}

// handoffFields reads a JSON object as serde reads a struct with the given
// fields, all required: member names match exactly (encoding/json matches
// case-insensitively), a repeated field is an error, and with denyUnknown
// any other member is an error.
func handoffFields(data []byte, denyUnknown bool, names ...string) (map[string]json.RawMessage, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	if token, err := decoder.Token(); err != nil || token != json.Delim('{') {
		return nil, errors.New("invalid type: expected struct")
	}
	known := map[string]bool{}
	for _, name := range names {
		known[name] = true
	}
	fields := map[string]json.RawMessage{}
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, err
		}
		key := token.(string)
		var value json.RawMessage
		if err := decoder.Decode(&value); err != nil {
			return nil, err
		}
		if !known[key] {
			if denyUnknown {
				return nil, fmt.Errorf("unknown field `%s`, expected one of `%s`", key, strings.Join(names, "`, `"))
			}
			continue
		}
		if _, repeated := fields[key]; repeated {
			return nil, fmt.Errorf("duplicate field `%s`", key)
		}
		fields[key] = value
	}
	if _, err := decoder.Token(); err != nil {
		return nil, err
	}
	for _, name := range names {
		if _, ok := fields[name]; !ok {
			return nil, fmt.Errorf("missing field `%s`", name)
		}
	}
	return fields, nil
}

// handoffDecodeFields decodes each field into its target. JSON null is not a
// number or a string, as serde decodes these fields.
func handoffDecodeFields(fields map[string]json.RawMessage, targets map[string]any) error {
	for name, target := range targets {
		value := fields[name]
		if string(value) == "null" {
			return fmt.Errorf("invalid type: null for field `%s`", name)
		}
		if err := json.Unmarshal(value, target); err != nil {
			return fmt.Errorf("field `%s`: %w", name, err)
		}
	}
	return nil
}

// HandoffStateCapture records the handoff state of snapshot with summary.
func HandoffStateCapture(snapshot *core.CanonicalSessionSnapshot, summary string) (HandoffState, error) {
	throughPosition := snapshot.EventFrontier
	for i := range snapshot.Transcript {
		throughPosition = max(throughPosition, snapshot.Transcript[i].Position)
	}
	digest, err := prefixDigest(snapshot, throughPosition)
	if err != nil {
		return HandoffState{}, err
	}
	return HandoffState{Version: version, ThroughPosition: throughPosition, PrefixSHA256: digest, Summary: summary}, nil
}

func (s *HandoffState) Validate(snapshot *core.CanonicalSessionSnapshot) (bool, error) {
	if s.Version != version || snapshot.CurrentContextStart() > s.ThroughPosition {
		return false, nil
	}
	digest, err := prefixDigest(snapshot, s.ThroughPosition)
	if err != nil {
		return false, err
	}
	return s.PrefixSHA256 == digest, nil
}

func (s *HandoffState) AttachToText(text string) (string, error) {
	summary, ok := modelBody(text)
	if !ok {
		return "", errors.New("not a complete model handoff")
	}
	if summary != s.Summary {
		return "", errors.New("handoff body differs from captured state")
	}
	encoded, err := marker{Version: s.Version, ThroughPosition: s.ThroughPosition, PrefixSHA256: s.PrefixSHA256}.MarshalJSON()
	if err != nil {
		return "", err
	}
	return text + markerOpen + string(encoded) + markerClose, nil
}

func HandoffStateFromHandoffText(text string) (HandoffState, bool) {
	panic("unported: mj-transcript/src/handoff.rs HandoffState::from_handoff_text")
}

func EncodeHandoff(snapshot *core.CanonicalSessionSnapshot, text string) (string, error) {
	summary, ok := modelBody(text)
	if !ok {
		return "", errors.New("not a complete model handoff")
	}
	state, err := HandoffStateCapture(snapshot, summary)
	if err != nil {
		return "", err
	}
	return state.AttachToText(text)
}

func ParseHandoff(text string) (HandoffState, bool) {
	return HandoffStateFromHandoffText(text)
}

func ReusableHandoff(snapshot *core.CanonicalSessionSnapshot) (HandoffState, bool) {
	panic("unported: mj-transcript/src/handoff.rs reusable_handoff")
}

func modelBody(text string) (string, bool) {
	body, ok := strings.CutPrefix(text, ModelHandoffOpening)
	if !ok {
		return "", false
	}
	index := strings.LastIndex(body, RetainedOpen)
	if index < 0 {
		return "", false
	}
	summary, retained := body[:index], body[index+len(RetainedOpen):]
	if strings.TrimSpace(summary) == "" || !strings.HasSuffix(retained, RetainedClose) {
		return "", false
	}
	return body, true
}

func prefixDigest(snapshot *core.CanonicalSessionSnapshot, throughPosition uint64) (string, error) {
	panic("unported: mj-transcript/src/handoff.rs prefix_digest")
}

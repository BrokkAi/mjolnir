// Port of mj-transcript/examples/jev_evidence_probe.rs.

// Command jev_evidence_probe is an offline Jev experiment adapter. It never
// opens an mj instance or changes history: it reads a request from standard
// input and prints the evidence a mode would give.
package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// request is serde deny_unknown_fields; replay_events, selected_ids and
// limit have defaults.
type request struct {
	Snapshot     core.CanonicalSessionSnapshot `json:"snapshot"`
	Mode         string                        `json:"mode"`
	ReplayEvents []core.RelayEvent             `json:"replay_events"`
	SelectedIDs  []string                      `json:"selected_ids"`
	Limit        int                           `json:"limit"`
}

func (r *request) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	*r = request{Limit: defaultLimit()}
	targets := map[string]any{
		"snapshot": &r.Snapshot, "mode": &r.Mode, "replay_events": &r.ReplayEvents,
		"selected_ids": &r.SelectedIDs, "limit": &r.Limit,
	}
	for key, value := range fields {
		target, ok := targets[key]
		if !ok {
			return fmt.Errorf("unknown field `%s`, expected one of `snapshot`, `mode`, `replay_events`, `selected_ids`, `limit`", key)
		}
		if err := json.Unmarshal(value, target); err != nil {
			return fmt.Errorf("field `%s`: %w", key, err)
		}
	}
	for _, key := range []string{"snapshot", "mode"} {
		if _, ok := fields[key]; !ok {
			return fmt.Errorf("missing field `%s`", key)
		}
	}
	return nil
}

func defaultLimit() int {
	return 48 * 1024
}

// project answers one request with a jsonvalue value.
func project(request request) (any, error) {
	if len(request.ReplayEvents) != 0 {
		state, err := transcript.MaterializedSessionFromCanonical("offline-experiment", &request.Snapshot)
		if err != nil {
			return nil, err
		}
		index := transcript.NewProjectionIndex(&state)
		for i := range request.ReplayEvents {
			event := &request.ReplayEvents[i]
			projected, err := transcript.ProjectRelayEventIndexed(&state, index, event)
			if err != nil {
				return nil, err
			}
			if err := transcript.ApplyCommittedProjectionEventIndexed(&state, index, event, projected.Mutation); err != nil {
				return nil, err
			}
		}
		if request.Snapshot, err = transcript.CanonicalSessionFromMaterialized(&state); err != nil {
			return nil, err
		}
	}
	if request.Mode == "replay" {
		encoded, err := request.Snapshot.MarshalJSON()
		if err != nil {
			return nil, err
		}
		return jsonvalue.Parse(encoded)
	}
	if request.Limit > defaultLimit() {
		return nil, errors.New("summary limit exceeds 48 KiB")
	}
	summary := transcript.TranscriptSummaryFromSnapshot(&request.Snapshot)
	user := -1
	for i := len(summary.Entries) - 1; i >= 0; i-- {
		if summary.Entries[i].Role == transcript.SummaryRoleUser {
			user = i
			break
		}
	}
	if user < 0 {
		return nil, errors.New("no delivered user message in snapshot")
	}
	userPosition := summary.Entries[user].Position
	userText := summary.Entries[user].Text
	assistant := ""
	for i := len(summary.Entries) - 1; i >= user; i-- {
		if summary.Entries[i].Role == transcript.SummaryRoleAssistant {
			assistant = summary.Entries[i].Text
			break
		}
	}
	var candidates []jsonvalue.Object
	for _, entry := range summary.Entries[user:] {
		if entry.Role != transcript.SummaryRoleTool {
			continue
		}
		callID, _ := entry.Tool.Get("toolCallId")
		candidates = append(candidates, jsonvalue.Object{
			{Key: "id", Value: entry.ID},
			{Key: "name", Value: entry.Text},
			{Key: "call_id", Value: callID},
			{Key: "position", Value: entry.Position},
		})
	}
	switch request.Mode {
	case "baseline":
	case "no_tools":
		summary = summary.LatestUserMessages()
	case "scoped", "selected":
		summary.Entries = summary.Entries[user:]
		if request.Mode == "selected" {
			for _, id := range request.SelectedIDs {
				if !slices.ContainsFunc(candidates, func(candidate jsonvalue.Object) bool {
					value, _ := candidate.Get("id")
					return value == id
				}) {
					return nil, fmt.Errorf("unknown selected tool ID: %s", id)
				}
			}
			summary.Entries = slices.DeleteFunc(summary.Entries, func(entry transcript.SummaryEntry) bool {
				return entry.Role == transcript.SummaryRoleTool && !slices.Contains(request.SelectedIDs, entry.ID)
			})
		}
	default:
		return nil, fmt.Errorf("unknown mode: %s", request.Mode)
	}
	text, retainedIDs := summary.RenderWithIDs(request.Limit)
	visible := []any{}
	for _, candidate := range candidates {
		id, _ := candidate.Get("id")
		if text, ok := id.(string); ok && slices.Contains(retainedIDs, text) {
			visible = append(visible, candidate)
		}
	}
	return jsonvalue.Object{
		{Key: "text", Value: text},
		{Key: "candidates", Value: visible},
		{Key: "latest_user_position", Value: userPosition},
		{Key: "user_text", Value: userText},
		{Key: "assistant_text", Value: assistant},
	}, nil
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	input, err := io.ReadAll(os.Stdin)
	if err != nil {
		return err
	}
	var parsed request
	if err := json.Unmarshal(input, &parsed); err != nil {
		return err
	}
	value, err := project(parsed)
	if err != nil {
		return err
	}
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	fmt.Println(string(encoded))
	return nil
}

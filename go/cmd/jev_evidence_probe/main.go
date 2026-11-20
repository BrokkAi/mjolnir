// Port of mj-transcript/examples/jev_evidence_probe.rs.

// Command jev_evidence_probe is an offline Jev experiment adapter. It never
// opens an mj instance or changes history: it reads a request from standard
// input and prints the evidence a mode would give.
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	panic("unported: mj-transcript/examples/jev_evidence_probe.rs project")
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

// Port of the tests in mj-core/src/state/session_configuration.rs.

package core

import (
	"encoding/json"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func TestLegacyConfigurationJSONKeepsItsShapeAndSeparatesOwners(t *testing.T) {
	var goal GoalState
	if err := json.Unmarshal([]byte(`{
		"known": true,
		"capability": {"version": 1, "controlMethod": "_session/goal", "actions": ["pause", "resume", "clear"]},
		"snapshot": {"objective": "finish", "status": "paused", "tokensUsed": 42, "tokenBudget": 1000},
		"execution": {"version": 1, "status": "idle"}
	}`), &goal); err != nil {
		t.Fatal(err)
	}
	encodedGoal, err := json.Marshal(goal)
	if err != nil {
		t.Fatal(err)
	}
	for _, legacy := range []string{
		`{}`,
		`{"model": "astra", "adapter_extension": {"enabled": true}}`,
		`{"model": "astra", "mj_goal_state": ` + string(encodedGoal) + `}`,
	} {
		var configuration SessionConfiguration
		if err := json.Unmarshal([]byte(legacy), &configuration); err != nil {
			t.Fatal(err)
		}
		encoded, err := json.Marshal(configuration)
		if err != nil {
			t.Fatal(err)
		}
		want, _ := jsonvalue.Parse([]byte(legacy))
		got, err := jsonvalue.Parse(encoded)
		if err != nil {
			t.Fatal(err)
		}
		if !draftValueEqual(got, want) {
			t.Fatalf("round trip of %s gave %s", legacy, encoded)
		}
		if _, ok := configuration.Values[ProjectionKey]; ok {
			t.Fatal("goal state leaked into the values")
		}
		_, hasGoal := want.(jsonvalue.Object).Get("mj_goal_state")
		if (configuration.Goal != nil) != hasGoal {
			t.Fatalf("goal presence = %v, want %v", configuration.Goal != nil, hasGoal)
		}
		var legacyReader map[string]json.RawMessage
		if err := json.Unmarshal(encoded, &legacyReader); err != nil {
			t.Fatal(err)
		}
		reencoded, err := json.Marshal(legacyReader)
		if err != nil {
			t.Fatal(err)
		}
		reparsed, _ := jsonvalue.Parse(reencoded)
		if !draftValueEqual(reparsed, want) {
			t.Fatalf("legacy reader of %s gave %s", legacy, reencoded)
		}
	}
}

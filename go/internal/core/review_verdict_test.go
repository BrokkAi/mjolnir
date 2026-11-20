// Port of the tests in mj-core/src/review/verdict.rs.

package core

import (
	"encoding/json"
	"testing"
)

// Reviews persisted while the extended tier still ran an intent analyst
// carry its brief in their evidence. Those fields are no longer read, and
// the stored record still loads.
func TestEvidenceStoredWithAnIntentBriefStillLoads(t *testing.T) {
	lanes, err := json.Marshal([]ReviewLaneEvidence{{ID: "tests", Outcome: LaneOutcomeCompleted{}}})
	if err != nil {
		t.Fatal(err)
	}
	stored := `{"intent_brief": "Goal: add a retry", "intent_available": true, "lanes": ` + string(lanes) + `}`
	var evidence ReviewPassEvidence
	if err := json.Unmarshal([]byte(stored), &evidence); err != nil {
		t.Fatal(err)
	}
	if len(evidence.Lanes) != 1 || evidence.Lanes[0].ID != "tests" || evidence.Lanes[0].Outcome != (LaneOutcomeCompleted{}) {
		t.Fatalf("evidence = %+v", evidence)
	}
}

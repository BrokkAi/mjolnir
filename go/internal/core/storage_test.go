// Port of the declined_findings_tests in mj-core/src/storage.rs.

package core

import (
	"encoding/json"
	"reflect"
	"slices"
	"strings"
	"testing"
)

func storageEntry(finding, reason string) ReviewOverride {
	return ReviewOverride{Finding: finding, Reason: reason}
}

// Hard-won: 9ebd246b2: clearing one repository erased other ranges.
func TestABaseMoveStopsCoveringOnlyItsOwnRepository(t *testing.T) {
	var state TurnReviewState
	if err := json.Unmarshal([]byte(`{
		"active": null, "declined_findings": [{"finding": "legacy", "reason": "all repositories"}]
	}`), &state); err != nil {
		t.Fatal(err)
	}
	if !state.DeclinedFindings[0].Covers("/b") {
		t.Fatal("an unscoped entry covers every repository")
	}
	state.DeclineFindings([]ReviewOverride{storageEntry("scoped", "both")}, []string{"/a", "/b"})
	state.DeclineFindings([]ReviewOverride{storageEntry("scoped", "both")}, []string{"/a", "/b"})
	if len(state.DeclinedFindings) != 2 {
		t.Fatalf("a retried override is not duplicated: %d", len(state.DeclinedFindings))
	}
	state.AdvanceReviewBases([]string{"/a"})
	if len(state.DeclinedFindings) != 1 {
		t.Fatalf("an unscoped entry ends at any move: %d", len(state.DeclinedFindings))
	}
	if !slices.Equal(state.DeclinedFindings[0].Repositories, []string{"/b"}) {
		t.Fatalf("repositories %v", state.DeclinedFindings[0].Repositories)
	}
	encoded, err := json.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	var restored TurnReviewState
	if err := json.Unmarshal(encoded, &restored); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(restored, state) {
		t.Fatalf("restored %+v, want %+v", restored, state)
	}
	state.AdvanceReviewBases([]string{"/b"})
	if len(state.DeclinedFindings) != 0 {
		t.Fatalf("left %v", state.DeclinedFindings)
	}
}

func TestTheDeclinedListKeepsTheNewestEntriesWithinItsByteBudget(t *testing.T) {
	var state TurnReviewState
	state.DeclineFindings([]ReviewOverride{storageEntry("first", "kept order")}, nil)
	state.DeclineFindings([]ReviewOverride{storageEntry("second", "kept order")}, nil)
	if len(state.DeclinedFindings) != 2 {
		t.Fatalf("within budget nothing goes: %d", len(state.DeclinedFindings))
	}

	half := DeclinedFindingsBytes/2 - 2
	state.DeclineFindings([]ReviewOverride{
		storageEntry(strings.Repeat("a", half), "x"),
		storageEntry(strings.Repeat("b", half), "y"),
	}, nil)
	var kept []byte
	size := 0
	for _, entry := range state.DeclinedFindings {
		kept = append(kept, entry.Finding[0])
		size += len(entry.Finding) + len(entry.Reason)
	}
	if string(kept) != "ab" {
		t.Fatalf("the oldest entries are dropped first: %q", kept)
	}
	if size > DeclinedFindingsBytes {
		t.Fatalf("size %d", size)
	}
}

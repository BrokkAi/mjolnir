// Port of the tests in mj-core/src/targets/diagnostics.rs.

package core

import (
	"slices"
	"sort"
	"strings"
	"testing"
)

func TestDiagnosticsKeepConcurrentOperationsUntilEachOwnerFinishes(t *testing.T) {
	first := BlockingOperationStart("diagnostics-first", "ssh")
	second := BlockingOperationStart("diagnostics-second", "git")
	snapshot := func() []string {
		blockingOperationsMu.Lock()
		defer blockingOperationsMu.Unlock()
		ids := make([]uint64, 0, len(blockingOperationsByID))
		for id := range blockingOperationsByID {
			ids = append(ids, id)
		}
		sort.Slice(ids, func(i, j int) bool { return ids[i] < ids[j] })
		purposes := []string{}
		for _, id := range ids {
			if purpose := blockingOperationsByID[id].purpose; strings.HasPrefix(purpose, "diagnostics-") {
				purposes = append(purposes, purpose)
			}
		}
		return purposes
	}
	if got := snapshot(); !slices.Equal(got, []string{"diagnostics-first", "diagnostics-second"}) {
		t.Fatalf("%v", got)
	}
	first.End()
	if got := snapshot(); !slices.Equal(got, []string{"diagnostics-second"}) {
		t.Fatalf("%v", got)
	}
	second.End()
	if got := snapshot(); len(got) != 0 {
		t.Fatalf("%v", got)
	}
}

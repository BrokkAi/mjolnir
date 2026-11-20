// Port of the tests in mj-core/src/lock_wait.rs.

package core

import (
	"slices"
	"testing"
	"time"
)

func TestReportsAfterFiveSecondsThenOnceAMinute(t *testing.T) {
	wait := NewLockWait("a test lock")
	var due []uint64
	for second := uint64(0); second <= 190; second++ {
		if wait.due(time.Duration(second) * time.Second) {
			due = append(due, second)
		}
	}
	if !slices.Equal(due, []uint64{5, 65, 125, 185}) {
		t.Fatalf("due = %v", due)
	}
}

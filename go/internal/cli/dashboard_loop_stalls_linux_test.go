// Port of mj-cli/src/dashboard/loop_stalls.rs tests (the Linux scheduler
// statistics).

package cli

import (
	"runtime"
	"testing"
	"time"
)

func TestTheKernelReportsThisThreadsCPUAndReadsAgainFromTheStart(t *testing.T) {
	// The statistics are the OS thread's, and a goroutine moves between
	// threads; Rust's test thread never moves.
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	sched := openSchedstat()
	if sched == nil {
		t.Fatal("open /proc/thread-self/schedstat")
	}
	first := sched.read()
	if first == nil {
		t.Fatal("parse schedstat")
	}
	// Spin on CPU time, not wall time: a loaded host can leave this thread
	// waiting for most of any wall-clock window.
	deadline := time.Now().Add(10 * time.Second)
	second := *first
	for second.since(*first).cpu < 5*time.Millisecond && time.Now().Before(deadline) {
		sample := sched.read()
		if sample == nil {
			t.Fatal("parse schedstat again")
		}
		second = *sample
	}
	if cpu := second.since(*first).cpu; cpu < 5*time.Millisecond {
		t.Fatalf("cpu %v", cpu)
	}
	if second.runqueue < first.runqueue {
		t.Fatalf("runqueue went back from %v to %v", first.runqueue, second.runqueue)
	}
}

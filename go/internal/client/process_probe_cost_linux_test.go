// Port of mj-client/tests/process_probe_cost.rs.
//
// The liveness probe runs every 250 ms while a new client waits for an older
// daemon's upgrade handoff, for as long as that daemon has work. It must cost
// one small read, not a scan of every process on the machine.

package client

import (
	"os"
	"testing"
	"time"
)

func probeThreadCount(t *testing.T) int {
	t.Helper()
	tasks, err := os.ReadDir("/proc/self/task")
	if err != nil {
		t.Fatal(err)
	}
	return len(tasks)
}

// Hard-won: 0317441d9980: H-1: daemon liveness probes spawned 241 threads and drove CPU to 1881 percent
func TestRepeatedLivenessProbesStartNoThreadsAndStayCheap(t *testing.T) {
	pid := uint32(os.Getpid())
	probe := func() {
		if !ProcessIsAlive(pid) {
			t.Fatal("this process is not alive")
		}
		if ProcessIsZombie(pid) {
			t.Fatal("this process is a zombie")
		}
	}
	// Rust runs this test alone in its own process. A Go test binary shares
	// its process with the package's other tests, and the Go runtime starts
	// the threads its scheduler needs for blocking reads on first use, so
	// one warm-up round runs before the count is taken.
	for range 200 {
		probe()
	}
	threads := probeThreadCount(t)
	started := time.Now()
	for range 200 {
		probe()
	}
	if got := probeThreadCount(t); got != threads {
		t.Fatalf("a liveness probe must not start a thread pool: %d threads, then %d", threads, got)
	}
	// 200 probes of one /proc file take well under a millisecond each; a full
	// process-table scan per probe takes far longer on a busy host.
	if elapsed := time.Since(started); elapsed >= time.Second {
		t.Fatalf("200 probes took %v", elapsed)
	}
}

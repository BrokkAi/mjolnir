// Port of mj-worker/src/worker_stop/process_table.rs (the shared part).

// One process's facts, read from the kernel without running `ps`.

//go:build linux || darwin

package worker

// processEntry is what a stop needs to know about a process. start is an
// opaque start-time stamp: a pid whose stamp changed is a different process.
type processEntry struct {
	pid    uint32
	ppid   uint32
	pgid   uint32
	start  uint64
	zombie bool
}

// isLiveInstanceOf reports whether e is the process earlier described, still
// running.
func (e *processEntry) isLiveInstanceOf(earlier *processEntry) bool {
	return e.pid == earlier.pid && e.start == earlier.start && !e.zombie
}

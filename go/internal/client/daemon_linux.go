// Port of mj-client/src/daemon.rs: the Linux zombie probe.

package client

import (
	"bytes"
	"fmt"
	"os"
)

// ProcessIsZombie reports whether a non-child process has exited but not yet
// been reaped.
//
// A zombie still answers `kill(pid, 0)`, because its process-table entry
// survives until its parent waits for it — so an existence probe alone calls
// it alive forever and anything waiting for it to leave waits forever. That
// is exactly the shape of `mj daemon restart` refusing to restart a daemon
// that had already stopped: spawning detached used to leave the daemon a
// child of a long-lived Mjolnir process that never reaped it. It now
// double-forks, so the daemon is init's to reap, but any other unreaped child
// of this process would look the same, and the check stays cheap.
//
// Treating a zombie as gone is also safe in the direction that matters: a
// zombie's PID cannot be reused until it is reaped, so nothing else can be
// occupying that number while this returns true.
//
// On Linux this reads the one `/proc/<pid>/stat` file, not a scan of every
// process: this probe runs every 250 ms while an upgrade waits on an older
// daemon.
func ProcessIsZombie(pid uint32) bool {
	stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return false
	}
	// The command name in parentheses may itself contain ") ", so the state
	// is the first field after the last closing parenthesis.
	end := bytes.LastIndexByte(stat, ')')
	return end >= 0 && end+2 < len(stat) && stat[end+2] == 'Z'
}

// daemonNonChildIsAlive is the liveness of a daemon this process cannot
// reap.
func daemonNonChildIsAlive(pid uint32) bool {
	return ProcessIsAlive(pid)
}

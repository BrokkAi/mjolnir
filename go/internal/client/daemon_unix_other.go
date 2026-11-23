// Port of mj-client/src/daemon.rs: the zombie probe on Unix systems other
// than Linux and macOS.

//go:build unix && !linux && !darwin

package client

import (
	"context"
	"os/exec"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProcessIsZombie reports whether a non-child process has exited but not yet
// been reaped (see the Linux version for why this matters). The Rust build
// asks its process table library for the one process's status; these
// systems share no process-table interface in Go, so this asks ps(1), whose
// state column starts with Z for a zombie.
func ProcessIsZombie(pid uint32) bool {
	output, err := core.Capture(context.Background(),
		exec.Command("ps", "-o", "stat=", "-p", strconv.FormatUint(uint64(pid), 10)))
	if err != nil || !output.Status.Success() {
		return false
	}
	return strings.HasPrefix(strings.TrimSpace(string(output.Stdout)), "Z")
}

// daemonNonChildIsAlive is the liveness of a daemon this process cannot
// reap.
func daemonNonChildIsAlive(pid uint32) bool {
	return ProcessIsAlive(pid)
}

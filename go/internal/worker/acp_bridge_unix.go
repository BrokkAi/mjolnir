//go:build unix

// Part of the port of mj-worker/src/acp.rs: the signal names Rust's
// ExitStatus Display shows on Unix.

package worker

import (
	"syscall"

	"golang.org/x/sys/unix"
)

// bridgeSignalName is the name Rust's ExitStatus Display gives a signal,
// such as SIGKILL; empty for one it does not name.
func bridgeSignalName(signal syscall.Signal) string {
	return unix.SignalName(signal)
}

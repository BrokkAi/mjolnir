// Port of mj-client/src/daemon.rs: the Unix process liveness probes.

//go:build unix

package client

import (
	"errors"
	"fmt"
	"os"
	"syscall"

	"golang.org/x/sys/unix"
)

// DaemonProcessIsAlive reports whether a daemon that Mjolnir launched in its
// own process group is alive.
//
// Reaping is deliberately confined to this daemon-specific path. Attachment
// PIDs are merely observations and may alias unrelated children owned by
// this process, so their liveness probe (ProcessIsAlive) must never call
// waitpid.
func DaemonProcessIsAlive(pid uint32) bool {
	if pid == 0 || pid > 1<<31-1 {
		return false
	}
	rawPID := int(pid)
	var status unix.WaitStatus
	// WNOHANG never blocks. A non-child fails with ECHILD without changing
	// any process state.
	waited, err := unix.Wait4(rawPID, &status, unix.WNOHANG, nil)
	if err == nil && waited == rawPID {
		return false
	}
	if err == nil && waited == 0 {
		return true
	}
	if !errors.Is(err, unix.ECHILD) {
		return true
	}
	return daemonNonChildIsAlive(pid)
}

// daemonConnectionRefused is io::ErrorKind::ConnectionRefused: nothing
// listens at the address dialed.
func daemonConnectionRefused(err error) bool {
	return errors.Is(err, unix.ECONNREFUSED)
}

// daemonExitStatus formats a process exit as Rust's ExitStatus Display does
// on Unix: "exit status: N", or "signal: N (SIGNAME)" with " (core dumped)"
// when it dumped core.
func daemonExitStatus(state *os.ProcessState) string {
	status, ok := state.Sys().(syscall.WaitStatus)
	if !ok || !status.Signaled() {
		return fmt.Sprintf("exit status: %d", state.ExitCode())
	}
	signal := status.Signal()
	text := fmt.Sprintf("signal: %d", int(signal))
	if name := unix.SignalName(signal); name != "" {
		text += " (" + name + ")"
	}
	if status.CoreDump() {
		text += " (core dumped)"
	}
	return text
}

// ProcessIsAlive reports whether pid names a live process: kill(pid, 0) is
// the standard existence probe, EPERM still means the process exists, and a
// zombie counts as gone.
func ProcessIsAlive(pid uint32) bool {
	if pid == 0 || pid > 1<<31-1 {
		return false
	}
	err := unix.Kill(int(pid), 0)
	exists := err == nil || errors.Is(err, unix.EPERM)
	return exists && !ProcessIsZombie(pid)
}

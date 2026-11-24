//go:build unix

// Port of mj-cli/src/daemon/wait_exit.rs: the Unix parts.

package cli

import (
	"os"
	"os/exec"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// configureWaiterChild has nothing to add on Unix.
func configureWaiterChild(*exec.Cmd) {}

// waiterExitSignal is the signal that ended the daemon, its name, and
// whether it dumped core; all nil for a daemon that exited.
func waiterExitSignal(status *os.ProcessState) (*int32, *string, *bool) {
	wait, ok := status.Sys().(syscall.WaitStatus)
	if !ok || !wait.Signaled() {
		return nil, nil, nil
	}
	signal := int32(wait.Signal())
	var name *string
	if text, ok := core.UnixSignalName(signal); ok {
		name = &text
	}
	dumped := wait.CoreDump()
	return &signal, name, &dumped
}

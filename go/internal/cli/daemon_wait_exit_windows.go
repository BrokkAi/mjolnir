//go:build windows

// Port of mj-cli/src/daemon/wait_exit.rs: the Windows parts.

package cli

import (
	"os"
	"os/exec"
	"syscall"

	"golang.org/x/sys/windows"
)

// configureWaiterChild keeps the daemon in its own hidden console and
// process group: the creation flags of spawn_detached are not inherited by
// this child.
func configureWaiterChild(command *exec.Cmd) {
	command.SysProcAttr = &syscall.SysProcAttr{
		CreationFlags: windows.CREATE_NO_WINDOW | windows.CREATE_NEW_PROCESS_GROUP,
	}
}

// waiterExitSignal is always nil on Windows, which has no signals.
func waiterExitSignal(*os.ProcessState) (*int32, *string, *bool) {
	return nil, nil, nil
}

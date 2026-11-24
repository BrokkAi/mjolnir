//go:build windows

// Port of the Windows half of mj-cli/src/dashboard/surface.rs
// windows_toast_command.

package cli

import (
	"os/exec"
	"syscall"

	"golang.org/x/sys/windows"
)

// withoutConsoleWindow starts command without a console of its own, so it
// cannot change the mode or title of the console the dashboard draws on.
func withoutConsoleWindow(command *exec.Cmd) {
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.CreationFlags |= windows.CREATE_NO_WINDOW
}

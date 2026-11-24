//go:build windows

// Port of the Windows parts of mj-accounts/src/login_terminal.rs.

package accounts

import (
	"errors"
	"fmt"
	"os/exec"

	"github.com/charmbracelet/x/xpty"
	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loginConsoleClosesOnExit: closing ConPTY sends CTRL_CLOSE_EVENT, which the
// root's exit has to trigger.
const loginConsoleClosesOnExit = true

// startLoginTerminal opens a ConPTY and starts command on it.
func startLoginTerminal(command *exec.Cmd, cols, rows uint16) (loginPty, *loginProcess, error) {
	pty, err := xpty.NewConPty(loginTerminalSize(cols, rows))
	if err != nil {
		return nil, nil, err
	}
	if err := pty.Start(command); err != nil {
		_ = pty.Close()
		return nil, nil, err
	}
	return pty, &loginProcess{child: command, armed: true}, nil
}

// loginReadEnded: a closed ConPTY output pipe reads as the end of output.
func loginReadEnded(err error) bool {
	return errors.Is(err, windows.ERROR_BROKEN_PIPE)
}

// loginKillGroup: Windows has no process group to kill; the child is killed
// alone.
func loginKillGroup(int) {}

// loginHangup: Windows has no hangup signal.
func loginHangup(int) {}

// loginAbandonedCleanup: the reap that follows is the whole cleanup.
func loginAbandonedCleanup(int) {}

// loginWaitChild waits for the root and reports its exit code.
func loginWaitChild(child *exec.Cmd) (uint32, error) {
	state, err := child.Process.Wait()
	if err != nil {
		return 0, err
	}
	return uint32(state.ExitCode()), nil
}

// observeExit waits for the root's process handle on its own duplicate, so
// aborting the connection cannot close a handle being waited on.
func observeExit(process *loginProcess) (*core.JoinHandle[error], error) {
	if process.child == nil || process.child.Process == nil {
		return nil, errors.New("login child missing")
	}
	handle, err := windows.OpenProcess(windows.SYNCHRONIZE, false, uint32(process.child.Process.Pid))
	if err != nil {
		return nil, fmt.Errorf("login child has no process handle: %w", err)
	}
	return core.SpawnBlocking(func() error {
		defer windows.CloseHandle(handle)
		// INFINITE waits for process termination, not an elapsed budget.
		event, err := windows.WaitForSingleObject(handle, windows.INFINITE)
		if err != nil {
			return err
		}
		if event != windows.WAIT_OBJECT_0 {
			return fmt.Errorf("wait for the login child returned %d", event)
		}
		return nil
	}), nil
}

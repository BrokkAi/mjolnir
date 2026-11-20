// Port of the `cfg(unix)` parts of mj-worker/src/terminal.rs.

//go:build unix

package worker

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// spawnTerminalShell starts line under `sh -c` in its own process group,
// with exactly spawn's environment, a null stdin and piped stdout and
// stderr.
func spawnTerminalShell(line string, spawn *TerminalSpawn) (*exec.Cmd, io.ReadCloser, io.ReadCloser, error) {
	panic("unported: mj-worker/src/terminal.rs spawn_shell")
}

// setOwnProcessGroup puts command's child in a process group of its own
// (Rust's `process_group(0)`), so a kill reaches descendants holding its
// pipes open.
func setOwnProcessGroup(command *exec.Cmd) {
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.Setpgid = true
	command.SysProcAttr.Pgid = 0
}

func killTerminalProcessGroup(pid int) {
	core.TerminateProcessGroup(pid, syscall.SIGKILL)
}

// terminalExitFromStatus is the ACP exit of a reaped terminal child.
func terminalExitFromStatus(state *os.ProcessState) TerminalExit {
	var exit TerminalExit
	if code := state.ExitCode(); code >= 0 {
		exitCode := uint32(code)
		exit.ExitCode = &exitCode
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
		name := terminalSignalName(status.Signal())
		exit.Signal = &name
	}
	return exit
}

// terminalSignalName is the name ACP reports for signal. It covers what a
// terminal actually sees; anything else keeps its number so the report stays
// truthful.
func terminalSignalName(signal syscall.Signal) string {
	switch signal {
	case syscall.SIGHUP:
		return "SIGHUP"
	case syscall.SIGINT:
		return "SIGINT"
	case syscall.SIGQUIT:
		return "SIGQUIT"
	case syscall.SIGILL:
		return "SIGILL"
	case syscall.SIGABRT:
		return "SIGABRT"
	case syscall.SIGBUS:
		return "SIGBUS"
	case syscall.SIGFPE:
		return "SIGFPE"
	case syscall.SIGKILL:
		return "SIGKILL"
	case syscall.SIGUSR1:
		return "SIGUSR1"
	case syscall.SIGSEGV:
		return "SIGSEGV"
	case syscall.SIGUSR2:
		return "SIGUSR2"
	case syscall.SIGPIPE:
		return "SIGPIPE"
	case syscall.SIGALRM:
		return "SIGALRM"
	case syscall.SIGTERM:
		return "SIGTERM"
	case syscall.SIGSTOP:
		return "SIGSTOP"
	case syscall.SIGTSTP:
		return "SIGTSTP"
	case syscall.SIGCONT:
		return "SIGCONT"
	}
	return fmt.Sprintf("SIG%d", int(signal))
}

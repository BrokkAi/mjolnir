//go:build unix

// Port of the Unix parts of mj-core/src/targets.rs.

package core

import (
	"errors"
	"log/slog"
	"os"
	"os/exec"
	"syscall"
)

// targetsSetProcessGroup starts the child in a process group of its own, so
// cancellation can kill the whole group (Rust: process_group(0)).
func targetsSetProcessGroup(process *exec.Cmd) {
	if process.SysProcAttr == nil {
		process.SysProcAttr = &syscall.SysProcAttr{}
	}
	process.SysProcAttr.Setpgid = true
	process.SysProcAttr.Pgid = 0
}

// targetsKillChild kills a cancelled command: only its leader when its
// children are preserved, otherwise its whole process group.
func targetsKillChild(child *targetChild, preserveChildren bool) {
	if preserveChildren {
		if err := child.process.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
			slog.Warn("could not terminate cancelled command", "pid", child.pid(), "error", err)
		}
		return
	}
	if err := SignalProcessGroup(child.pid(), syscall.SIGKILL); err != nil {
		// A group that is already gone is the wanted outcome, not a failure.
		slog.Warn("could not terminate cancelled command process group", "pid", child.pid(), "error", err)
	}
}

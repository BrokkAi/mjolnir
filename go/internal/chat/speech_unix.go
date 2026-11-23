//go:build unix

// Port of mj-chat/src/speech.rs (Unix process groups).

package chat

import (
	"os/exec"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// prepareVoiceWorkerGroup puts the worker in a process group of its own
// (Rust's `process_group(0)`), so stopping it also stops what it started.
func prepareVoiceWorkerGroup(command *exec.Cmd) {
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.Setpgid = true
	command.SysProcAttr.Pgid = 0
}

// stopVoiceWorker stops the worker and any subprocesses it owns.
func stopVoiceWorker(child *exec.Cmd) {
	// The shared helper also handles a group that exited between the timeout
	// check and this call.
	core.TerminateProcessGroup(child.Process.Pid, syscall.SIGKILL)
}

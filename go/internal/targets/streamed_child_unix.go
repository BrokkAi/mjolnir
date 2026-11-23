//go:build unix

// Port of mj-targets/src/streamed_child.rs: the Unix process group (Rust's
// `process_group(0)`).

package targets

import (
	"os/exec"
	"syscall"
)

// setProxyProcessGroup starts the proxy in a process group of its own.
func setProxyProcessGroup(command *exec.Cmd) {
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.Setpgid = true
	command.SysProcAttr.Pgid = 0
}

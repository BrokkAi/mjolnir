//go:build unix

// Port of the Unix parts of mj-accounts/src/login_terminal.rs.

package accounts

import "github.com/BrokkAi/mjolnir/go/internal/core"

// observeExit observes the root's exit without reaping it: the process id
// remains reserved until the terminal owner has signaled the group and
// drained the PTY.
func observeExit(process *loginProcess) (*core.JoinHandle[error], error) {
	panic("unported: mj-accounts/src/login_terminal.rs observe_exit (unix)")
}

// waitForExit waits for the unreaped child pid to exit, with WNOWAIT so its
// group id stays reserved.
func waitForExit(pid int) error {
	panic("unported: mj-accounts/src/login_terminal.rs wait_for_exit")
}

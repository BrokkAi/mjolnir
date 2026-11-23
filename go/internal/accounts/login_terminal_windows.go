//go:build windows

// Port of the Windows parts of mj-accounts/src/login_terminal.rs.

package accounts

import "github.com/BrokkAi/mjolnir/go/internal/core"

// observeExit waits for the root's process handle on its own duplicate, so
// aborting the connection cannot close a handle being waited on.
func observeExit(process *loginProcess) (*core.JoinHandle[error], error) {
	panic("unported: mj-accounts/src/login_terminal.rs observe_exit (windows)")
}

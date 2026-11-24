// Port of mj-accounts/src/login_terminal.rs's wait_for_exit on the other
// Unix systems, which Go's bindings give no waitid.

//go:build unix && !linux && !darwin

package accounts

import "errors"

// waitForExit would wait for the unreaped child pid to exit with WNOWAIT;
// without waitid the exit cannot be observed without reaping it.
func waitForExit(int) error {
	return errors.New("observing a login's exit needs waitid, which Go offers on Linux and macOS only")
}

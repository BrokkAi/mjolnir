// Port of mj-accounts/src/login_terminal.rs's wait_for_exit on Linux.

package accounts

import (
	"errors"

	"golang.org/x/sys/unix"
)

// waitForExit waits for the unreaped child pid to exit, with WNOWAIT so its
// group id stays reserved.
func waitForExit(pid int) error {
	for {
		var info unix.Siginfo
		// This is our unreaped child; WNOWAIT reserves its group id.
		err := unix.Waitid(unix.P_PID, pid, &info, unix.WEXITED|unix.WNOWAIT, nil)
		if err == nil {
			return nil
		}
		if !errors.Is(err, unix.EINTR) {
			return err
		}
	}
}

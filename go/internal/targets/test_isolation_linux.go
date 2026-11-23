// Port of mj-targets/src/test_isolation.rs's wait_for_exit_without_reaping
// on Linux.

package targets

import (
	"errors"

	"golang.org/x/sys/unix"
)

func waitForExitWithoutReaping(pid int) {
	for {
		var info unix.Siginfo
		err := unix.Waitid(unix.P_PID, pid, &info, unix.WEXITED|unix.WNOWAIT, nil)
		if err == nil || !errors.Is(err, unix.EINTR) {
			return
		}
	}
}

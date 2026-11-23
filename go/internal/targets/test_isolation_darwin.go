// Port of mj-targets/src/test_isolation.rs's wait_for_exit_without_reaping
// on macOS, where golang.org/x/sys/unix has no Waitid wrapper.

package targets

import (
	"unsafe"

	"golang.org/x/sys/unix"
)

// darwinPPID is P_PID in <sys/wait.h>'s idtype_t (P_ALL, P_PID, P_PGID).
const darwinPPID = 1

func waitForExitWithoutReaping(pid int) {
	for {
		// siginfo_t is 104 bytes on macOS; waitid writes it on success.
		var info [104]byte
		_, _, errno := unix.Syscall6(unix.SYS_WAITID, darwinPPID, uintptr(pid),
			uintptr(unsafe.Pointer(&info[0])), unix.WEXITED|unix.WNOWAIT, 0, 0)
		if errno != unix.EINTR {
			return
		}
	}
}

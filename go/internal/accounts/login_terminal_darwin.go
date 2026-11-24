// Port of mj-accounts/src/login_terminal.rs's wait_for_exit on macOS, where
// golang.org/x/sys/unix has no Waitid wrapper.

package accounts

import (
	"unsafe"

	"golang.org/x/sys/unix"
)

// loginDarwinPPID is P_PID in <sys/wait.h>'s idtype_t (P_ALL, P_PID,
// P_PGID).
const loginDarwinPPID = 1

// waitForExit waits for the unreaped child pid to exit, with WNOWAIT so its
// group id stays reserved.
func waitForExit(pid int) error {
	for {
		// siginfo_t is 104 bytes on macOS; waitid writes it on success.
		var info [104]byte
		_, _, errno := unix.Syscall6(unix.SYS_WAITID, loginDarwinPPID, uintptr(pid),
			uintptr(unsafe.Pointer(&info[0])), unix.WEXITED|unix.WNOWAIT, 0, 0)
		switch errno {
		case 0:
			return nil
		case unix.EINTR:
			continue
		}
		return errno
	}
}

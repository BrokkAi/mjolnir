// Port of mj-client/src/daemon.rs: the Windows process liveness probes.

package client

import (
	"errors"
	"fmt"
	"os"

	"golang.org/x/sys/windows"
)

// windowsStillActive is STILL_ACTIVE, the exit code of a running process.
const windowsStillActive = 259

// daemonConnectionRefused is io::ErrorKind::ConnectionRefused: nothing
// listens at the address dialed.
func daemonConnectionRefused(err error) bool {
	return errors.Is(err, windows.WSAECONNREFUSED)
}

// daemonExitStatus formats a process exit as Rust's ExitStatus Display does
// on Windows: "exit code: N", in hexadecimal when the code is an NTSTATUS
// with its high bit set.
func daemonExitStatus(state *os.ProcessState) string {
	code := uint32(state.ExitCode())
	if code&0x80000000 != 0 {
		return fmt.Sprintf("exit code: %#x", code)
	}
	return fmt.Sprintf("exit code: %d", code)
}

// DaemonProcessIsAlive reports whether a daemon that Mjolnir launched is
// alive. Windows has no zombies to reap.
func DaemonProcessIsAlive(pid uint32) bool {
	return ProcessIsAlive(pid)
}

// ProcessIsAlive reports whether pid names a running process. A process this
// user may not query still exists.
func ProcessIsAlive(pid uint32) bool {
	if pid == 0 {
		return false
	}
	handle, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
	if err != nil {
		return err == windows.ERROR_ACCESS_DENIED
	}
	defer windows.CloseHandle(handle)
	var code uint32
	if err := windows.GetExitCodeProcess(handle, &code); err != nil {
		return false
	}
	return code == windowsStillActive
}

//go:build unix

// Port of mj-cli/src/daemon.rs: the Unix daemon signal.

package cli

import (
	"errors"
	"fmt"
	"syscall"
)

// signalHostDaemon sends SIGTERM to the daemon at pid, which every
// supported daemon handles as graceful cancellation. A PID that no longer
// runs the daemon is left alone.
func signalHostDaemon(pid uint32) error {
	daemon, err := hostPIDIsDaemon(pid)
	if err != nil {
		return err
	}
	if !daemon {
		return nil
	}
	// The PID comes from owner-only daemon metadata.
	if err := syscall.Kill(int(pid), syscall.SIGTERM); err != nil && !errors.Is(err, syscall.ESRCH) {
		return fmt.Errorf("stop superseded Mjolnir daemon: %w", err)
	}
	return nil
}

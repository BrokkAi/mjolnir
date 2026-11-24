//go:build linux

package cli

import "golang.org/x/sys/unix"

// flushTerminalInput is tcflush(fd, TCIFLUSH).
func flushTerminalInput(fd int) { _ = unix.IoctlSetInt(fd, unix.TCFLSH, unix.TCIFLUSH) }

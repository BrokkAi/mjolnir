//go:build unix && !linux

package cli

import "golang.org/x/sys/unix"

// flushTerminalInput is tcflush(fd, TCIFLUSH), which macOS and the BSDs
// implement as TIOCFLUSH with FREAD.
func flushTerminalInput(fd int) { _ = unix.IoctlSetPointerInt(fd, unix.TIOCFLUSH, unix.FREAD) }

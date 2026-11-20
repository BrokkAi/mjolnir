//go:build darwin

package rsio

import (
	"syscall"

	"golang.org/x/sys/unix"
)

func sysPipe() (pipeEnd, pipeEnd, error) {
	var fds [2]int

	// macOS has no pipe2: the two descriptors are made and then marked
	// close-on-exec. Rust leaves a window in which a fork inherits them; Go
	// starts processes under syscall.ForkLock, and holding it closes the
	// window.
	syscall.ForkLock.RLock()
	defer syscall.ForkLock.RUnlock()
	if err := unix.Pipe(fds[:]); err != nil {
		return pipeEnd{}, pipeEnd{}, From(err)
	}
	fd0 := OwnedFdFromRawFd(RawFd(fds[0]))
	fd1 := OwnedFdFromRawFd(RawFd(fds[1]))
	for _, fd := range fds {
		if _, err := unix.FcntlInt(uintptr(fd), unix.F_SETFD, unix.FD_CLOEXEC); err != nil {
			fd0.Drop()
			fd1.Drop()
			return pipeEnd{}, pipeEnd{}, From(err)
		}
	}
	return pipeEnd{fd: fd0}, pipeEnd{fd: fd1}, nil
}

//go:build linux

package rsio

import "golang.org/x/sys/unix"

func sysPipe() (pipeEnd, pipeEnd, error) {
	var fds [2]int

	// The only known way right now to create atomically set the CLOEXEC flag is
	// to use the `pipe2` syscall. This was added to Linux in 2.6.27, glibc 2.9
	// and musl 0.9.3, and some other targets also have it.
	if err := unix.Pipe2(fds[:], unix.O_CLOEXEC); err != nil {
		return pipeEnd{}, pipeEnd{}, From(err)
	}
	return pipeEnd{fd: OwnedFdFromRawFd(RawFd(fds[0]))}, pipeEnd{fd: OwnedFdFromRawFd(RawFd(fds[1]))}, nil
}

//go:build unix

package rsio

import "syscall"

const (
	// deadlineExceededCode is what a read or write on a socket with a
	// timeout fails with on Unix: the code Rust decodes as WouldBlock.
	deadlineExceededCode = RawOsError(syscall.EAGAIN)
	// closedDescriptorCode is the system's error for a descriptor that is
	// not open.
	closedDescriptorCode = RawOsError(syscall.EBADF)
)

// fromErrno is the io::Error for an errno a system call returned.
func fromErrno(errno syscall.Errno) *Error {
	return ErrorFromRawOsError(rawOsErrorOf(errno))
}

//go:build windows

package rsio

import (
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"
)

const (
	// deadlineExceededCode is what a read or write on a socket with a
	// timeout fails with on Windows: the code Rust decodes as TimedOut.
	deadlineExceededCode = RawOsError(windows.WSAETIMEDOUT)
	// closedDescriptorCode is the system's error for a handle that is not
	// open.
	closedDescriptorCode = RawOsError(windows.ERROR_INVALID_HANDLE)
)

func uintptrOf(p *uint16) uintptr { return uintptr(unsafe.Pointer(p)) }

// fromErrno is the io::Error for an error code a system call returned.
//
// Go's syscall package also invents error numbers on Windows, above the
// range the system uses, for the POSIX names package os expects
// (syscall.EINVAL, syscall.EISDIR, ...). Windows never returns them and Rust
// never sees them; one of them becomes an error of the kind the POSIX error
// has, holding Go's error.
func fromErrno(errno syscall.Errno) *Error {
	if errno < syscall.APPLICATION_ERROR {
		return ErrorFromRawOsError(rawOsErrorOf(errno))
	}
	kind := ErrorKind_Uncategorized
	switch errno {
	case syscall.E2BIG:
		kind = ErrorKind_ArgumentListTooLong
	case syscall.EADDRINUSE:
		kind = ErrorKind_AddrInUse
	case syscall.EADDRNOTAVAIL:
		kind = ErrorKind_AddrNotAvailable
	case syscall.EBUSY:
		kind = ErrorKind_ResourceBusy
	case syscall.ECONNABORTED:
		kind = ErrorKind_ConnectionAborted
	case syscall.ECONNREFUSED:
		kind = ErrorKind_ConnectionRefused
	case syscall.ECONNRESET:
		kind = ErrorKind_ConnectionReset
	case syscall.EDEADLK:
		kind = ErrorKind_Deadlock
	case syscall.EDQUOT:
		kind = ErrorKind_QuotaExceeded
	case syscall.EEXIST:
		kind = ErrorKind_AlreadyExists
	case syscall.EFBIG:
		kind = ErrorKind_FileTooLarge
	case syscall.EHOSTUNREACH:
		kind = ErrorKind_HostUnreachable
	case syscall.EINTR:
		kind = ErrorKind_Interrupted
	case syscall.EINVAL:
		kind = ErrorKind_InvalidInput
	case syscall.EISDIR:
		kind = ErrorKind_IsADirectory
	case syscall.ELOOP:
		kind = ErrorKind_FilesystemLoop
	case syscall.ENOMEM:
		kind = ErrorKind_OutOfMemory
	case syscall.ENOSPC:
		kind = ErrorKind_StorageFull
	case syscall.ENOSYS, syscall.ENOTSUP, syscall.EOPNOTSUPP, syscall.EWINDOWS:
		kind = ErrorKind_Unsupported
	case syscall.EMLINK:
		kind = ErrorKind_TooManyLinks
	case syscall.ENAMETOOLONG:
		kind = ErrorKind_InvalidFilename
	case syscall.ENETDOWN:
		kind = ErrorKind_NetworkDown
	case syscall.ENETUNREACH:
		kind = ErrorKind_NetworkUnreachable
	case syscall.ENOTCONN:
		kind = ErrorKind_NotConnected
	case syscall.ENOTEMPTY:
		kind = ErrorKind_DirectoryNotEmpty
	case syscall.EPIPE:
		kind = ErrorKind_BrokenPipe
	case syscall.EROFS:
		kind = ErrorKind_ReadOnlyFilesystem
	case syscall.ESPIPE:
		kind = ErrorKind_NotSeekable
	case syscall.ESTALE:
		kind = ErrorKind_StaleNetworkFileHandle
	case syscall.ETIMEDOUT:
		kind = ErrorKind_TimedOut
	case syscall.ETXTBSY:
		kind = ErrorKind_ExecutableFileBusy
	case syscall.EXDEV:
		kind = ErrorKind_CrossesDevices
	case syscall.EINPROGRESS:
		kind = ErrorKind_InProgress
	case syscall.EMFILE, syscall.ENFILE:
		kind = ErrorKind_TooManyOpenFiles
	case syscall.EIO:
		kind = ErrorKind_InputOutputError
	case syscall.EACCES, syscall.EPERM:
		kind = ErrorKind_PermissionDenied
	case syscall.EAGAIN, syscall.EWOULDBLOCK:
		kind = ErrorKind_WouldBlock
	}
	return ErrorNew(kind, errno)
}

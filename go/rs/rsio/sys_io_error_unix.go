// Port of std 1.99.1 src/sys/io/error/unix.rs

//go:build unix

package rsio

import "syscall"

// isInterrupted is sys::io::is_interrupted.
func isInterrupted(errno RawOsError) bool {
	return syscall.Errno(errno) == syscall.EINTR
}

// decodeErrorKind is sys::io::decode_error_kind: the ErrorKind of an errno.
// The constants are the operating system's own, so the same text gives the
// Linux table on Linux and the Darwin table on macOS.
func decodeErrorKind(errno RawOsError) ErrorKind {
	if errno < 0 {
		return ErrorKind_Uncategorized
	}
	switch syscall.Errno(errno) {
	case syscall.E2BIG:
		return ErrorKind_ArgumentListTooLong
	case syscall.EADDRINUSE:
		return ErrorKind_AddrInUse
	case syscall.EADDRNOTAVAIL:
		return ErrorKind_AddrNotAvailable
	case syscall.EBUSY:
		return ErrorKind_ResourceBusy
	case syscall.ECONNABORTED:
		return ErrorKind_ConnectionAborted
	case syscall.ECONNREFUSED:
		return ErrorKind_ConnectionRefused
	case syscall.ECONNRESET:
		return ErrorKind_ConnectionReset
	case syscall.EDEADLK:
		return ErrorKind_Deadlock
	case syscall.EDQUOT:
		return ErrorKind_QuotaExceeded
	case syscall.EEXIST:
		return ErrorKind_AlreadyExists
	case syscall.EFBIG:
		return ErrorKind_FileTooLarge
	case syscall.EHOSTUNREACH:
		return ErrorKind_HostUnreachable
	case syscall.EINTR:
		return ErrorKind_Interrupted
	case syscall.EINVAL:
		return ErrorKind_InvalidInput
	case syscall.EISDIR:
		return ErrorKind_IsADirectory
	case syscall.ELOOP:
		return ErrorKind_FilesystemLoop
	case syscall.ENOENT:
		return ErrorKind_NotFound
	case syscall.ENOMEM:
		return ErrorKind_OutOfMemory
	case syscall.ENOSPC:
		return ErrorKind_StorageFull
	case syscall.ENOSYS:
		return ErrorKind_Unsupported
	case syscall.EMLINK:
		return ErrorKind_TooManyLinks
	case syscall.ENAMETOOLONG:
		return ErrorKind_InvalidFilename
	case syscall.ENETDOWN:
		return ErrorKind_NetworkDown
	case syscall.ENETUNREACH:
		return ErrorKind_NetworkUnreachable
	case syscall.ENOTCONN:
		return ErrorKind_NotConnected
	case syscall.ENOTDIR:
		return ErrorKind_NotADirectory
	case syscall.ENOTEMPTY:
		return ErrorKind_DirectoryNotEmpty
	case syscall.EPIPE:
		return ErrorKind_BrokenPipe
	case syscall.EROFS:
		return ErrorKind_ReadOnlyFilesystem
	case syscall.ESPIPE:
		return ErrorKind_NotSeekable
	case syscall.ESTALE:
		return ErrorKind_StaleNetworkFileHandle
	case syscall.ETIMEDOUT:
		return ErrorKind_TimedOut
	case syscall.ETXTBSY:
		return ErrorKind_ExecutableFileBusy
	case syscall.EXDEV:
		return ErrorKind_CrossesDevices
	case syscall.EINPROGRESS:
		return ErrorKind_InProgress
	case syscall.EMFILE, syscall.ENFILE:
		return ErrorKind_TooManyOpenFiles
	case syscall.EOPNOTSUPP:
		return ErrorKind_Unsupported
	case syscall.EIO:
		return ErrorKind_InputOutputError

	case syscall.EACCES, syscall.EPERM:
		return ErrorKind_PermissionDenied
	}

	// These two constants can have the same value on some systems,
	// but different values on others, so we can't use a match
	// clause
	if x := syscall.Errno(errno); x == syscall.EAGAIN || x == syscall.EWOULDBLOCK {
		return ErrorKind_WouldBlock
	}

	return ErrorKind_Uncategorized
}

// rawOsErrorOf is the error code of an errno the system returned.
func rawOsErrorOf(errno syscall.Errno) RawOsError {
	return RawOsError(errno)
}

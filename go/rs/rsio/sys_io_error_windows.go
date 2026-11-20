// Port of std 1.99.1 src/sys/io/error/windows.rs

//go:build windows

package rsio

import (
	"strconv"
	"syscall"
	"unicode/utf16"

	"golang.org/x/sys/windows"
)

// isInterrupted is sys::io::is_interrupted.
func isInterrupted(errno RawOsError) bool {
	return false
}

// decodeErrorKind is sys::io::decode_error_kind: the ErrorKind of a Windows
// error code.
func decodeErrorKind(errno RawOsError) ErrorKind {
	switch syscall.Errno(uint32(errno)) {
	case windows.ERROR_ACCESS_DENIED:
		return ErrorKind_PermissionDenied
	case windows.ERROR_ALREADY_EXISTS:
		return ErrorKind_AlreadyExists
	case windows.ERROR_FILE_EXISTS:
		return ErrorKind_AlreadyExists
	case windows.ERROR_BROKEN_PIPE:
		return ErrorKind_BrokenPipe
	case windows.ERROR_FILE_NOT_FOUND,
		windows.ERROR_PATH_NOT_FOUND,
		windows.ERROR_INVALID_DRIVE,
		windows.ERROR_BAD_NETPATH,
		windows.ERROR_BAD_NET_NAME:
		return ErrorKind_NotFound
	case windows.ERROR_NO_DATA:
		return ErrorKind_BrokenPipe
	case windows.ERROR_INVALID_NAME, windows.ERROR_BAD_PATHNAME:
		return ErrorKind_InvalidFilename
	case windows.ERROR_INVALID_PARAMETER:
		return ErrorKind_InvalidInput
	case windows.ERROR_NOT_ENOUGH_MEMORY, windows.ERROR_OUTOFMEMORY:
		return ErrorKind_OutOfMemory
	case windows.ERROR_SEM_TIMEOUT,
		windows.WAIT_TIMEOUT,
		windows.ERROR_DRIVER_CANCEL_TIMEOUT,
		windows.ERROR_OPERATION_ABORTED,
		windows.ERROR_SERVICE_REQUEST_TIMEOUT,
		windows.ERROR_COUNTER_TIMEOUT,
		windows.ERROR_TIMEOUT,
		windows.ERROR_RESOURCE_CALL_TIMED_OUT,
		windows.ERROR_CTX_MODEM_RESPONSE_TIMEOUT,
		windows.ERROR_CTX_CLIENT_QUERY_TIMEOUT,
		windows.FRS_ERR_SYSVOL_POPULATE_TIMEOUT,
		windows.ERROR_DS_TIMELIMIT_EXCEEDED,
		windows.DNS_ERROR_RECORD_TIMED_OUT,
		windows.ERROR_IPSEC_IKE_TIMED_OUT,
		windows.ERROR_RUNLEVEL_SWITCH_TIMEOUT,
		windows.ERROR_RUNLEVEL_SWITCH_AGENT_TIMEOUT:
		return ErrorKind_TimedOut
	case windows.ERROR_CALL_NOT_IMPLEMENTED:
		return ErrorKind_Unsupported
	case windows.ERROR_HOST_UNREACHABLE:
		return ErrorKind_HostUnreachable
	case windows.ERROR_NETWORK_UNREACHABLE:
		return ErrorKind_NetworkUnreachable
	case windows.ERROR_DIRECTORY:
		return ErrorKind_NotADirectory
	case windows.ERROR_DIRECTORY_NOT_SUPPORTED:
		return ErrorKind_IsADirectory
	case windows.ERROR_DIR_NOT_EMPTY:
		return ErrorKind_DirectoryNotEmpty
	case windows.ERROR_WRITE_PROTECT:
		return ErrorKind_ReadOnlyFilesystem
	case windows.ERROR_DISK_FULL, windows.ERROR_HANDLE_DISK_FULL:
		return ErrorKind_StorageFull
	case windows.ERROR_SEEK_ON_DEVICE:
		return ErrorKind_NotSeekable
	case windows.ERROR_DISK_QUOTA_EXCEEDED:
		return ErrorKind_QuotaExceeded
	case windows.ERROR_FILE_TOO_LARGE:
		return ErrorKind_FileTooLarge
	case windows.ERROR_BUSY:
		return ErrorKind_ResourceBusy
	case windows.ERROR_POSSIBLE_DEADLOCK:
		return ErrorKind_Deadlock
	case windows.ERROR_NOT_SAME_DEVICE:
		return ErrorKind_CrossesDevices
	case windows.ERROR_TOO_MANY_LINKS:
		return ErrorKind_TooManyLinks
	case windows.ERROR_TOO_MANY_OPEN_FILES:
		return ErrorKind_TooManyOpenFiles
	case windows.ERROR_FILENAME_EXCED_RANGE:
		return ErrorKind_InvalidFilename
	case windows.ERROR_CANT_RESOLVE_FILENAME:
		return ErrorKind_FilesystemLoop
	case windows.ERROR_IO_DEVICE:
		return ErrorKind_InputOutputError
	case windows.ERROR_NEGATIVE_SEEK:
		return ErrorKind_InvalidInput
	}

	if errno < 0 {
		return ErrorKind_Uncategorized
	}
	switch syscall.Errno(errno) {
	case windows.WSAEACCES:
		return ErrorKind_PermissionDenied
	case windows.WSAEADDRINUSE:
		return ErrorKind_AddrInUse
	case windows.WSAEADDRNOTAVAIL:
		return ErrorKind_AddrNotAvailable
	case windows.WSAECONNABORTED:
		return ErrorKind_ConnectionAborted
	case windows.WSAECONNREFUSED:
		return ErrorKind_ConnectionRefused
	case windows.WSAECONNRESET:
		return ErrorKind_ConnectionReset
	case windows.WSAEINVAL:
		return ErrorKind_InvalidInput
	case windows.WSAENOTCONN:
		return ErrorKind_NotConnected
	case windows.WSAEWOULDBLOCK:
		return ErrorKind_WouldBlock
	case windows.WSAETIMEDOUT:
		return ErrorKind_TimedOut
	case windows.WSAEHOSTUNREACH:
		return ErrorKind_HostUnreachable
	case windows.WSAENETDOWN:
		return ErrorKind_NetworkDown
	case windows.WSAENETUNREACH:
		return ErrorKind_NetworkUnreachable
	case windows.WSAEDQUOT:
		return ErrorKind_QuotaExceeded
	case windows.WSAEMFILE:
		return ErrorKind_TooManyOpenFiles
	// Not a perfect mapping but this error is only returned when writing to
	// a socket after shutting down the write-end. On Unix targets, EPIPE is
	// returned in those cases.
	case windows.WSAESHUTDOWN:
		return ErrorKind_BrokenPipe
	}
	return ErrorKind_Uncategorized
}

var (
	modkernel32          = windows.NewLazySystemDLL("kernel32.dll")
	procGetModuleHandleW = modkernel32.NewProc("GetModuleHandleW")
)

// errorString is sys::io::error_string: the system's text for an error
// code, in the language of the user, without the line break FormatMessageW
// ends it with.
func errorString(errnum RawOsError) string {
	var buf [2048]uint16

	var module uintptr
	var flags uint32

	// NTSTATUS errors may be encoded as HRESULT, which may returned from
	// GetLastError. For more information about Windows error codes, see
	// `[MS-ERREF]`: https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-erref/0642cb2f-2075-4469-918c-4441e69c548a
	if errnum&windows.FACILITY_NT_BIT != 0 {
		// format according to https://support.microsoft.com/en-us/help/259693
		ntdll, _ := windows.UTF16PtrFromString("NTDLL.DLL")
		module = getModuleHandle(ntdll)

		if module != 0 {
			errnum ^= windows.FACILITY_NT_BIT
			flags = windows.FORMAT_MESSAGE_FROM_HMODULE
		}
	}

	res, err := windows.FormatMessage(
		flags|windows.FORMAT_MESSAGE_FROM_SYSTEM|windows.FORMAT_MESSAGE_IGNORE_INSERTS,
		module,
		uint32(errnum),
		0,
		buf[:],
		nil,
	)
	if res == 0 {
		// Sometimes FormatMessageW can fail e.g., system doesn't like 0 as langId,
		fmErr := int32(0)
		if code, ok := err.(syscall.Errno); ok {
			fmErr = int32(code)
		}
		return "OS Error " + strconv.FormatInt(int64(errnum), 10) +
			" (FormatMessageW() returned error " + strconv.FormatInt(int64(fmErr), 10) + ")"
	}

	message, ok := stringFromUtf16(buf[:res])
	if !ok {
		return "OS Error " + strconv.FormatInt(int64(errnum), 10) +
			" (FormatMessageW() returned invalid UTF-16)"
	}
	// Trim trailing CRLF inserted by FormatMessageW
	end := len(message)
	for end > 0 {
		switch message[end-1] {
		case '\t', '\n', '\x0C', '\r', ' ':
			end--
			continue
		}
		break
	}
	return message[:end]
}

func getModuleHandle(name *uint16) uintptr {
	handle, _, _ := procGetModuleHandleW.Call(uintptrOf(name))
	return handle
}

// stringFromUtf16 is String::from_utf16: it fails on an unpaired surrogate.
func stringFromUtf16(units []uint16) (string, bool) {
	for i := 0; i < len(units); i++ {
		switch unit := units[i]; {
		case unit >= 0xD800 && unit <= 0xDBFF:
			if i+1 >= len(units) || units[i+1] < 0xDC00 || units[i+1] > 0xDFFF {
				return "", false
			}
			i++
		case unit >= 0xDC00 && unit <= 0xDFFF:
			return "", false
		}
	}
	return string(utf16.Decode(units)), true
}

// rawOsErrorOf is the error code of an error the system returned.
func rawOsErrorOf(errno syscall.Errno) RawOsError {
	return RawOsError(uint32(errno))
}

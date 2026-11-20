// Port of std 1.99.1 src/sys/pal/windows/mod.rs (to_u16s, fill_utf16_buf,
// os2path)
//
// go/rs/rspath (absolute_windows.go) has its own copy of to_u16s and
// fill_utf16_buf for std::path::absolute; rspath cannot import this package,
// which imports it.

package rsenv

import (
	"math"
	"syscall"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

var (
	kernel32                     = windows.NewLazySystemDLL("kernel32.dll")
	userenv                      = windows.NewLazySystemDLL("userenv.dll")
	procGetEnvironmentVariableW  = kernel32.NewProc("GetEnvironmentVariableW")
	procGetCurrentDirectoryW     = kernel32.NewProc("GetCurrentDirectoryW")
	procGetModuleFileNameW       = kernel32.NewProc("GetModuleFileNameW")
	procGetTempPath2W            = kernel32.NewProc("GetTempPath2W")
	procGetTempPathW             = kernel32.NewProc("GetTempPathW")
	procGetUserProfileDirectoryW = userenv.NewProc("GetUserProfileDirectoryW")
)

// toU16s is sys::pal::windows::to_u16s: the string as UTF-16 with a
// terminating zero, refusing one that holds a zero itself.
func toU16s(s string) ([]uint16, error) {
	wide := rspath.OsStrEncodeWide(s)
	for _, unit := range wide {
		if unit == 0 {
			return nil, rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput,
				"strings passed to WinAPI cannot contain NULs")
		}
	}
	return append(wide, 0), nil
}

// lastErrorOf is the thread's last error after a LazyProc.Call. The Go
// runtime clears the last error before every system call and reads it
// after, which is the SetLastError(0) and GetLastError fill_utf16_buf wraps
// the call in.
//
// The calls are written out where they are made: LazyProc.Call keeps a
// pointer converted to uintptr alive only when the conversion is in its own
// argument list.
func lastErrorOf(err error) syscall.Errno {
	return err.(syscall.Errno)
}

// fillUtf16Buf is sys::pal::windows::fill_utf16_buf.
//
// Many Windows APIs follow a pattern of where we hand a buffer and then they
// will report back to us how large the buffer should be or how many bytes
// currently reside in the buffer. This function is an abstraction over these
// functions by making them easier to call.
//
// f1 is passed a (pointer, len) pair which can be passed to a syscall. The
// pointer is valid for len items. It returns what the call returned and the
// thread's last error after it:
//   - On success, the actual length of the written data *without* the null
//     terminator. This can be 0. In this case the last error must be left
//     unchanged.
//   - On insufficient buffer space, either the buffer size or the required
//     length *including* the null terminator.
//   - On other failure, 0, with the last error set.
func fillUtf16Buf(f1 func(buffer *uint16, size uint32) (uint32, syscall.Errno)) ([]uint16, error) {
	// Start off with a stack buf but then spill over to the heap if we end up
	// needing more space.
	//
	// This initial size also works around `GetFullPathNameW` returning
	// incorrect size hints for some short paths:
	// https://github.com/dylni/normpath/issues/5
	n := 512
	var buf []uint16
	for {
		if len(buf) < n {
			buf = make([]uint16, n)
		}

		// This function is typically called on windows API functions which
		// will return the correct length of the string, but these functions
		// also return the `0` on error. In some cases, however, the
		// returned "correct length" may actually be 0!
		//
		// To handle this case we call `SetLastError` to reset it to 0 and
		// then check it again if we get the "0 error value". If the "last
		// error" is still 0 then we interpret it as a 0 length buffer and
		// not an actual error.
		returned, lastError := f1(&buf[0], uint32(n))
		if returned == 0 && lastError != 0 {
			return nil, rsio.ErrorLastOsError(lastError)
		}
		k := int(returned)
		if k == n && lastError == windows.ERROR_INSUFFICIENT_BUFFER {
			n = int(min(rs.SaturatingMul(uint64(n), 2), math.MaxUint32))
		} else if k > n {
			n = k
		} else if k == n {
			// It is impossible to reach this point.
			// On success, k is the returned string length excluding the null.
			// On failure, k is the required buffer length including the null.
			// Therefore k never equals n.
			panic(rs.Unreachable())
		} else {
			return buf[:k], nil
		}
	}
}

// os2path is sys::pal::windows::os2path.
func os2path(s []uint16) string {
	return rspath.OsStringFromWide(s)
}

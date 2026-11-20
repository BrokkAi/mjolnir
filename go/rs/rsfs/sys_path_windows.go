// Port of std 1.99.1 src/sys/path/windows.rs (with_native_path,
// maybe_verbatim, get_long_path), src/sys/pal/windows/mod.rs (to_u16s,
// fill_utf16_buf, truncate_utf16_at_nul) and src/sys/args/windows.rs
// (from_wide_to_user_path)

//go:build windows

package rsfs

import (
	"math"
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// go/rs/rspath has its own copies of to_u16s and fill_utf16_buf for
// std::path::absolute; they are not exported, so this package keeps these.

var (
	modkernel32 = windows.NewLazySystemDLL("kernel32.dll")
	modntdll    = windows.NewLazySystemDLL("ntdll.dll")

	procCopyFileExW               = modkernel32.NewProc("CopyFileExW")
	procCreateFileW               = modkernel32.NewProc("CreateFileW")
	procFindFirstFileExW          = modkernel32.NewProc("FindFirstFileExW")
	procFindNextFileW             = modkernel32.NewProc("FindNextFileW")
	procGetFileSizeEx             = modkernel32.NewProc("GetFileSizeEx")
	procGetFinalPathNameByHandleW = modkernel32.NewProc("GetFinalPathNameByHandleW")
	procGetFullPathNameW          = modkernel32.NewProc("GetFullPathNameW")
	procSetFilePointerEx          = modkernel32.NewProc("SetFilePointerEx")
	procSwitchToThread            = modkernel32.NewProc("SwitchToThread")
	procUnlockFile                = modkernel32.NewProc("UnlockFile")
	procNtOpenFile                = modntdll.NewProc("NtOpenFile")
	procNtReadFile                = modntdll.NewProc("NtReadFile")
	procNtWriteFile               = modntdll.NewProc("NtWriteFile")
	procRtlNtStatusToDosError     = modntdll.NewProc("RtlNtStatusToDosError")
)

// lastErrorOf is the thread's last error after a call made with
// LazyProc.Call, which reads it right after the call (and clears it right
// before), as Rust's get_last_error does.
func lastErrorOf(err error) uint32 {
	if errno, ok := err.(syscall.Errno); ok {
		return uint32(errno)
	}
	return 0
}

// winErrorOf is the error code behind an error golang.org/x/sys/windows
// returned. That package reports a failure that left the last error at 0
// as syscall.EINVAL, a number Go invents; Rust reads the 0.
func winErrorOf(err error) uint32 {
	if errno, ok := err.(syscall.Errno); ok {
		if errno == syscall.EINVAL {
			return 0
		}
		return uint32(errno)
	}
	return 0
}

// osError is io::Error::last_os_error after a call failed with err.
func osError(err error) error {
	if err == nil {
		return nil
	}
	if _, ok := err.(syscall.Errno); ok {
		return rsio.ErrorFromRawOsError(int32(winErrorOf(err)))
	}
	return rsio.From(err)
}

// ioResult is IoResult::io_result for a WinError: the io::Error of its
// code.
func ioResult(code uint32) error {
	return rsio.ErrorFromRawOsError(int32(code))
}

// errNulWinAPI is what to_u16s reports for a string holding a NUL.
var errNulWinAPI = rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput,
	"strings passed to WinAPI cannot contain NULs")

// toU16s is sys::pal::windows::to_u16s: the string as UTF-16 with a
// terminating zero, refusing one that holds a zero itself.
func toU16s(s string) ([]uint16, error) {
	maybeResult := rspath.OsStrEncodeWide(s)
	for _, unit := range maybeResult {
		if unit == 0 {
			return nil, errNulWinAPI
		}
	}
	return append(maybeResult, 0), nil
}

// truncateUtf16AtNul is truncate_utf16_at_nul.
func truncateUtf16AtNul(v []uint16) []uint16 {
	for i, unit := range v {
		if unit == 0 {
			// don't include the 0
			return v[:i]
		}
	}
	return v
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
// thread's last error after it (LazyProc.Call clears the last error before
// the call and reads it after, which is the SetLastError(0) and
// GetLastError Rust wraps the call in). On success it is the length of the
// data without the terminating null, which can be 0 with the last error
// left at 0; on insufficient buffer space, the length needed with the null,
// or len with ERROR_INSUFFICIENT_BUFFER; on other failure, 0 and the error.
func fillUtf16Buf(f1 func(buffer *uint16, size uint32) (uint32, uint32)) ([]uint16, error) {
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
			return nil, ioResult(lastError)
		}
		k := int(returned)
		if k == n && lastError == uint32(windows.ERROR_INSUFFICIENT_BUFFER) {
			if n > math.MaxUint32/2 {
				n = math.MaxUint32
			} else {
				n *= 2
			}
		} else if k > n {
			n = k
		} else if k == n {
			// It is impossible to reach this point.
			// On success, k is the returned string length excluding the null.
			// On failure, k is the required buffer length including the null.
			// Therefore k never equals n.
			panic("internal error: entered unreachable code")
		} else {
			return buf[:k], nil
		}
	}
}

// getFullPathNameW is fill_utf16_buf over GetFullPathNameW of a
// NUL-terminated path.
func getFullPathNameW(path []uint16) ([]uint16, error) {
	return fillUtf16Buf(func(buffer *uint16, size uint32) (uint32, uint32) {
		r, _, err := procGetFullPathNameW.Call(uintptr(unsafe.Pointer(&path[0])), uintptr(size),
			uintptr(unsafe.Pointer(buffer)), 0)
		return uint32(r), lastErrorOf(err)
	})
}

// nativePath is what with_native_path hands on: the path in UTF-16 with a
// terminating zero, verbatim (`\\?\`) where that lifts the length limit.
type nativePath = []uint16

// withNativePath is sys::path::with_native_path: f with the path made by
// maybe_verbatim.
func withNativePath[T any](path string, f func(p nativePath) (T, error)) (T, error) {
	native, err := maybeVerbatim(path)
	if err != nil {
		var zero T
		return zero, err
	}
	return f(native)
}

// maybeVerbatim is sys::path::maybe_verbatim.
func maybeVerbatim(path string) ([]uint16, error) {
	wide, err := toU16s(path)
	if err != nil {
		return nil, err
	}
	return getLongPath(wide, true)
}

// UTF-16 encoded code points, used in parsing and building UTF-16 paths.
// All of these are in the ASCII range so they can be cast directly to `u16`.
const (
	wSep    = uint16('\\')
	wAltSep = uint16('/')
	wQuery  = uint16('?')
	wColon  = uint16(':')
	wDot    = uint16('.')
	wU      = uint16('U')
	wN      = uint16('N')
	wC      = uint16('C')
)

var (
	// \\?\
	verbatimPrefix = []uint16{wSep, wSep, wQuery, wSep}
	// \??\
	ntPrefix = []uint16{wSep, wQuery, wQuery, wSep}
	// \\?\UNC\
	uncPrefix = []uint16{wSep, wSep, wQuery, wSep, wU, wN, wC, wSep}
)

func hasPrefixU16(s, prefix []uint16) bool {
	if len(s) < len(prefix) {
		return false
	}
	for i, unit := range prefix {
		if s[i] != unit {
			return false
		}
	}
	return true
}

func equalU16(a, b []uint16) bool {
	return len(a) == len(b) && hasPrefixU16(a, b)
}

// getLongPath is sys::path::get_long_path: a normalized absolute path that
// can bypass path length limits. path ends with its terminating zero.
//
// Setting prefer_verbatim to true suggests a stronger preference for verbatim
// paths even when not strictly necessary. This allows the Windows API to avoid
// repeating our work. However, if the path may be given back to users or
// passed to other application then it's preferable to use non-verbatim paths
// when possible. Non-verbatim paths are better understood by users and handled
// by more software.
func getLongPath(path []uint16, preferVerbatim bool) ([]uint16, error) {
	// Normally the MAX_PATH is 260 UTF-16 code units (including the NULL).
	// However, for APIs such as CreateDirectory[1], the limit is 248.
	//
	// [1]: https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createdirectorya#parameters
	const legacyMaxPath = 248

	if hasPrefixU16(path, verbatimPrefix) || hasPrefixU16(path, ntPrefix) || equalU16(path, []uint16{0}) {
		// Early return for paths that are already verbatim or empty.
		return path, nil
	} else if len(path) < legacyMaxPath {
		// Early return if an absolute path is less < 260 UTF-16 code units.
		// This is an optimization to avoid calling `GetFullPathNameW` unnecessarily.
		switch {
		// Starts with `D:`, `D:\`, `D:/`, etc.
		// Does not match if the path starts with a `\` or `/`.
		case (len(path) == 3 && path[1] == wColon && path[2] == 0 ||
			len(path) >= 3 && path[1] == wColon && (path[2] == wSep || path[2] == wAltSep)) &&
			path[0] != wSep && path[0] != wAltSep:
			return path, nil
		// Starts with `\\`, `//`, etc
		case len(path) >= 2 && (path[0] == wSep || path[0] == wAltSep) && (path[1] == wSep || path[1] == wAltSep):
			return path, nil
		}
	}

	// Firstly, get the absolute path using `GetFullPathNameW`.
	// https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfullpathnamew
	absolute, err := getFullPathNameW(path)
	if err != nil {
		return nil, err
	}

	var out []uint16
	// Only prepend the prefix if needed.
	if preferVerbatim || len(absolute)+1 >= legacyMaxPath {
		// Secondly, add the verbatim prefix. This is easier here because we know the
		// path is now absolute and fully normalized (e.g. `/` has been changed to `\`).
		var prefix []uint16
		switch {
		// C:\ => \\?\C:\
		case len(absolute) >= 3 && absolute[1] == wColon && absolute[2] == wSep:
			prefix = verbatimPrefix
		// \\.\ => \\?\
		case hasPrefixU16(absolute, []uint16{wSep, wSep, wDot, wSep}):
			absolute = absolute[4:]
			prefix = verbatimPrefix
		// Leave \\?\ and \??\ as-is.
		case hasPrefixU16(absolute, verbatimPrefix) || hasPrefixU16(absolute, ntPrefix):
			prefix = nil
		// \\ => \\?\UNC\
		case hasPrefixU16(absolute, []uint16{wSep, wSep}):
			absolute = absolute[2:]
			prefix = uncPrefix
		// Anything else we leave alone.
		default:
			prefix = nil
		}

		out = make([]uint16, 0, len(prefix)+len(absolute)+1)
		out = append(out, prefix...)
	} else {
		out = make([]uint16, 0, len(absolute)+1)
	}
	out = append(out, absolute...)
	out = append(out, 0)
	return out, nil
}

// fromWideToUserPath is sys::args::from_wide_to_user_path: a verbatim path
// (with its terminating zero) without the verbatim prefix, when removing it
// does not change what the path names.
func fromWideToUserPath(path []uint16) ([]uint16, error) {
	// Early return if the path is too long to remove the verbatim prefix.
	const legacyMaxPath = 260
	if len(path) > legacyMaxPath {
		return path, nil
	}

	switch {
	// `\\?\C:\...` => `C:\...`
	case len(path) >= 7 && hasPrefixU16(path, verbatimPrefix) && path[5] == wColon && path[6] == wSep:
		fullPath, err := getFullPathNameW(path[4:])
		if err != nil {
			return nil, err
		}
		if equalU16(fullPath, path[4:len(path)-1]) {
			return append(append([]uint16(nil), fullPath...), 0), nil
		}
		return path, nil
	// `\\?\UNC\...` => `\\...`
	case hasPrefixU16(path, uncPrefix):
		// Change the `C` in `UNC\` to `\` so we can get a slice that starts with `\\`.
		path[6] = wSep
		fullPath, err := getFullPathNameW(path[6:])
		if err != nil {
			return nil, err
		}
		if equalU16(fullPath, path[6:len(path)-1]) {
			return append(append([]uint16(nil), fullPath...), 0), nil
		}
		// Restore the 'C' in "UNC".
		path[6] = wC
		return path, nil
	// For everything else, leave the path unchanged.
	default:
		return getLongPath(path, false)
	}
}

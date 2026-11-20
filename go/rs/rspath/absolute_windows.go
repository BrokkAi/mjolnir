// Port of std 1.99.1 src/sys/path/windows.rs (absolute) and
// src/sys/pal/windows/mod.rs (to_u16s, fill_utf16_buf)

//go:build windows

package rspath

import (
	"math"
	"strings"
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"
)

var procGetFullPathNameW = windows.NewLazySystemDLL("kernel32.dll").NewProc("GetFullPathNameW")

// sysAbsolute is sys::path::absolute: make a Windows path absolute.
func sysAbsolute(path string) (string, error) {
	prefix := Windows.parsePrefix(path)
	// Verbatim paths should not be modified.
	if p, ok := prefix.Get(); ok && p.IsVerbatim() {
		// NULs in verbatim paths are rejected for consistency.
		if strings.IndexByte(path, 0) >= 0 {
			return "", ioErrorInvalidInput("strings passed to WinAPI cannot contain NULs")
		}
		return path, nil
	}

	wide, err := toU16s(path)
	if err != nil {
		return "", err
	}
	full, err := fillUtf16Buf(func(buffer *uint16, size uint32) (uint32, syscall.Errno) {
		n, _, lastError := procGetFullPathNameW.Call(
			uintptr(unsafe.Pointer(&wide[0])), uintptr(size), uintptr(unsafe.Pointer(buffer)), 0)
		return uint32(n), lastError.(syscall.Errno)
	})
	if err != nil {
		return "", err
	}
	return OsStringFromWide(full), nil
}

// toU16s is sys::pal::windows::to_u16s: the string as UTF-16 with a
// terminating zero, refusing one that holds a zero itself.
func toU16s(s string) ([]uint16, error) {
	wide := OsStrEncodeWide(s)
	for _, unit := range wide {
		if unit == 0 {
			return nil, ioErrorInvalidInput("strings passed to WinAPI cannot contain NULs")
		}
	}
	return append(wide, 0), nil
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
// thread's last error after it. The Go runtime clears the last error before
// every system call and reads it after, which is the SetLastError(0) and
// GetLastError Rust wraps the call in.
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
		// If the "last error" is still 0 then we interpret it as a 0 length
		// buffer and not an actual error.
		returned, lastError := f1(&buf[0], uint32(n))
		if returned == 0 && lastError != 0 {
			return nil, ioErrorFromOs(lastError)
		}
		k := int(returned)
		if k == n && lastError == windows.ERROR_INSUFFICIENT_BUFFER {
			n = min(saturatingDouble(n), math.MaxUint32)
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

func saturatingDouble(n int) int {
	if n > math.MaxInt/2 {
		return math.MaxInt
	}
	return n * 2
}

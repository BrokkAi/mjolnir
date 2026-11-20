// Port of std 1.99.1 src/sys/stdio/windows.rs and
// src/sys/io/is_terminal/windows.rs

//go:build windows

package rsio

import (
	"io"
	"os"
	"strings"
	"unicode/utf16"
	"unsafe"

	"golang.org/x/sys/windows"
)

// stdinBufSize is sys::stdio::STDIN_BUF_SIZE on Windows.
//
// Apparently Windows doesn't handle large reads on stdin or writes to
// stdout/stderr well (see #13304 for details). As a result the buffer is
// limited to MAX_BUFFER_SIZE u16s, and the read buffer is sized so that
// they always decode to UTF-8 completely.
const stdinBufSize = 8192 / 2 * 3

// The standard streams go through Go's own os.Stdin, os.Stdout and
// os.Stderr. Rust's Windows code converts between UTF-8 and the UTF-16 of a
// console itself; Go's files do the same conversion for a console handle and
// read and write bytes for any other handle. They differ only on a console
// given text that is not UTF-8, which Rust refuses and Go writes with
// replacement characters.

func stdinRead(buf []byte) (int, error) {
	n, err := os.Stdin.Read(buf)
	if err == nil || err == io.EOF {
		return n, nil
	}
	return n, From(err)
}

func stdoutWrite(buf []byte) (int, error) {
	n, err := os.Stdout.Write(buf)
	if err != nil {
		return n, From(err)
	}
	return n, nil
}

func stderrWrite(buf []byte) (int, error) {
	n, err := os.Stderr.Write(buf)
	if err != nil {
		return n, From(err)
	}
	return n, nil
}

// isEbadf is sys::stdio::is_ebadf.
func isEbadf(err error) bool {
	code, ok := From(err).RawOsError().Get()
	return ok && code == RawOsError(windows.ERROR_INVALID_HANDLE)
}

// stdioIsTerminal is sys::io::is_terminal for a standard stream.
func stdioIsTerminal(fd int) bool {
	switch fd {
	case 0:
		return HandleIsTerminal(stdHandle(windows.STD_INPUT_HANDLE))
	case 1:
		return HandleIsTerminal(stdHandle(windows.STD_OUTPUT_HANDLE))
	default:
		return HandleIsTerminal(stdHandle(windows.STD_ERROR_HANDLE))
	}
}

// HandleIsTerminal is sys::io::is_terminal: whether a handle is a console,
// or the pipe an MSYS or Cygwin terminal stands behind.
func HandleIsTerminal(handle RawHandle) bool {
	// A null handle means the process has no console.
	if handle == 0 {
		return false
	}

	var out uint32
	if windows.GetConsoleMode(handle, &out) == nil {
		// False positives aren't possible. If we got a console then we definitely have a console.
		return true
	}

	// Otherwise, we fall back to an msys hack to see if we can detect the presence of a pty.
	return msysTtyOn(handle)
}

func msysTtyOn(handle RawHandle) bool {
	// Early return if the handle is not a pipe.
	if fileType, _ := windows.GetFileType(handle); fileType != windows.FILE_TYPE_PIPE {
		return false
	}

	// Mirrors FILE_NAME_INFO, giving it a fixed length that we can stack
	// allocate
	var nameInfo struct {
		FileNameLength uint32
		FileName       [windows.MAX_PATH]uint16
	}
	err := windows.GetFileInformationByHandleEx(
		handle,
		windows.FileNameInfo,
		(*byte)(unsafe.Pointer(&nameInfo)),
		uint32(unsafe.Sizeof(nameInfo)),
	)
	if err != nil {
		return false
	}

	// Use `get` because `FileNameLength` can be out of range.
	units := int(nameInfo.FileNameLength) / 2
	if units > len(nameInfo.FileName) {
		return false
	}
	name := string(utf16.Decode(nameInfo.FileName[:units]))
	// Get the file name only.
	if i := strings.LastIndexByte(name, '\\'); i >= 0 {
		name = name[i+1:]
	}
	// This checks whether 'pty' exists in the file name, which indicates that
	// a pseudo-terminal is attached. To mitigate against false positives
	// (e.g., an actual file name that contains 'pty'), we also require that
	// the file name begins with either the strings 'msys-' or 'cygwin-'.)
	isMsys := strings.HasPrefix(name, "msys-") || strings.HasPrefix(name, "cygwin-")
	isPty := strings.Contains(name, "-pty")
	return isMsys && isPty
}

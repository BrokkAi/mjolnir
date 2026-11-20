// Port of std 1.99.1 src/sys/paths/windows.rs: current_exe, getcwd, chdir,
// temp_dir and home_dir

package rsenv

import (
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

func sysCurrentExe() (string, error) {
	path, err := fillUtf16Buf(func(buf *uint16, size uint32) (uint32, syscall.Errno) {
		r, _, err := procGetModuleFileNameW.Call(0, uintptr(unsafe.Pointer(buf)), uintptr(size))
		return uint32(r), lastErrorOf(err)
	})
	if err != nil {
		return "", err
	}
	return os2path(path), nil
}

func sysGetcwd() (string, error) {
	directory, err := fillUtf16Buf(func(buf *uint16, size uint32) (uint32, syscall.Errno) {
		r, _, err := procGetCurrentDirectoryW.Call(uintptr(size), uintptr(unsafe.Pointer(buf)))
		return uint32(r), lastErrorOf(err)
	})
	if err != nil {
		return "", err
	}
	return os2path(directory), nil
}

// sysChdir is sys::paths::chdir. Rust does not check the path for a NUL
// here: SetCurrentDirectoryW reads it up to the first one.
func sysChdir(p string) error {
	wide := append(rspath.OsStrEncodeWide(p), 0)

	if err := windows.SetCurrentDirectory(&wide[0]); err != nil {
		return rsio.ErrorLastOsError(err)
	}
	return nil
}

// sysTempDir is sys::paths::temp_dir: GetTempPath2W, which Rust replaces by
// GetTempPathW where the system lacks it (before Windows 11 and Server
// 2022). It panics as Rust's `.unwrap()` does when the call fails.
func sysTempDir() string {
	proc := procGetTempPath2W
	if proc.Find() != nil {
		proc = procGetTempPathW
	}
	path := rs.Unwrap(fillUtf16Buf(func(buf *uint16, size uint32) (uint32, syscall.Errno) {
		r, _, err := proc.Call(uintptr(size), uintptr(unsafe.Pointer(buf)))
		return uint32(r), lastErrorOf(err)
	}))
	return os2path(path)
}

// homeDirCrt is home_dir_crt: the profile directory of the user the process
// runs as, which GetUserProfileDirectoryW gives for the current process
// token.
func homeDirCrt() rs.Option[string] {
	// Defined in processthreadsapi.h.
	currentProcessToken := windows.GetCurrentProcessToken()

	path, err := fillUtf16Buf(func(buf *uint16, size uint32) (uint32, syscall.Errno) {
		// GetUserProfileDirectoryW does not quite use the usual protocol for
		// negotiating the buffer size, so we have to translate.
		sz := size
		r, _, err := procGetUserProfileDirectoryW.Call(
			uintptr(currentProcessToken), uintptr(unsafe.Pointer(buf)), uintptr(unsafe.Pointer(&sz)))
		lastError := lastErrorOf(err)
		switch {
		case r == 0 && lastError != windows.ERROR_INSUFFICIENT_BUFFER:
			return 0, lastError
		case r == 0:
			return sz, lastError
		default:
			return sz - 1, lastError // sz includes the null terminator
		}
	})
	if err != nil {
		return rs.None[string]()
	}
	return rs.Some(os2path(path))
}

func sysHomeDir() rs.Option[string] {
	if profile, ok := VarOs("USERPROFILE").Get(); ok && profile != "" {
		return rs.Some(profile)
	}
	return homeDirCrt()
}

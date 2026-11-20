// Port of std 1.99.1 src/sys/paths/unix.rs: getcwd, chdir, temp_dir and
// home_dir

//go:build unix

package rsenv

import (
	"os"
	"os/user"
	"strconv"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// sysGetcwd is sys::paths::getcwd. syscall.Getwd is getcwd(3) with a buffer
// of PATH_MAX bytes, which is as long as the kernel names a directory; Rust
// grows its buffer from 512 bytes until getcwd stops failing with ERANGE.
// (os.Getwd is not this: it answers with $PWD when that names the same
// directory, keeping a symbolic link the shell came through.)
func sysGetcwd() (string, error) {
	directory, err := syscall.Getwd()
	if err != nil {
		return "", rsio.ErrorLastOsError(err)
	}
	return directory, nil
}

// sysChdir is sys::paths::chdir.
func sysChdir(p string) error {
	if containsNul(p) {
		return errNulInCString()
	}
	if err := syscall.Chdir(p); err != nil {
		return rsio.ErrorLastOsError(err)
	}
	return nil
}

// sysTempDir is sys::paths::temp_dir.
func sysTempDir() string {
	if value, ok := VarOs("TMPDIR").Get(); ok {
		return value
	}
	return sysDefaultTempDir()
}

// sysHomeDir is sys::paths::home_dir.
func sysHomeDir() rs.Option[string] {
	if home, ok := VarOs("HOME").Get(); ok && home != "" {
		return rs.Some(home)
	}
	return homeDirFallback()
}

// homeDirFallback is the fallback of sys::paths::home_dir: the home
// directory of the user database entry of the real user ID, as getpwuid_r
// gives it, or None when there is none.
//
// os/user asks getpwuid_r through the C library on macOS. On Linux a Go
// program without C reads /etc/passwd, which is what the C library reads
// unless nsswitch.conf names another source (LDAP, SSSD, systemd-homed).
func homeDirFallback() rs.Option[string] {
	entry, err := user.LookupId(strconv.Itoa(os.Getuid()))
	if err != nil {
		return rs.None[string]()
	}
	return rs.Some(entry.HomeDir)
}

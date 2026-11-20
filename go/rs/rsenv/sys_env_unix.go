// Port of std 1.99.1 src/sys/env/unix.rs

//go:build unix

package rsenv

import (
	"runtime"
	"strings"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// A Go program built without C has no C library environment: the Go runtime
// holds the environment and syscall.Getenv, Setenv and Unsetenv change it
// under its own lock. So the ENV_LOCK Rust takes around the C calls has no
// counterpart here, and what the C library would check is checked below.

func sysGetenv(k string) rs.Option[string] {
	// environment variables with a nul byte can't be set, so their value is
	// always None as well
	if containsNul(k) {
		return rs.None[string]()
	}
	return libcGetenv(k)
}

// libcGetenv is getenv(3) over the Go runtime's environment.
//
// For a name without `=` that is syscall.Getenv. A name holding `=` is
// looked up as the C library would: glibc takes the first entry that begins
// with the whole name followed by `=`, and the C library of macOS reads the
// name only up to its first `=`.
func libcGetenv(name string) rs.Option[string] {
	if name == "" {
		return rs.None[string]()
	}
	if strings.IndexByte(name, '=') < 0 {
		value, ok := syscall.Getenv(name)
		return rs.OptionOf(value, ok)
	}
	if runtime.GOOS == "darwin" {
		name = name[:strings.IndexByte(name, '=')]
		if name != "" {
			value, ok := syscall.Getenv(name)
			return rs.OptionOf(value, ok)
		}
		// With nothing before the `=`, the first entry that begins with `=`.
		for _, entry := range syscall.Environ() {
			if strings.HasPrefix(entry, "=") {
				return rs.Some(entry[1:])
			}
		}
		return rs.None[string]()
	}
	for _, entry := range syscall.Environ() {
		if len(entry) > len(name) && entry[len(name)] == '=' && entry[:len(name)] == name {
			return rs.Some(entry[len(name)+1:])
		}
	}
	return rs.None[string]()
}

func sysSetenv(k, v string) error {
	if containsNul(k) || containsNul(v) {
		return errNulInCString()
	}
	// setenv(3) refuses an empty name and a name holding `=` with EINVAL;
	// syscall.Setenv refuses the same names with the same code.
	if err := syscall.Setenv(k, v); err != nil {
		return rsio.ErrorLastOsError(err)
	}
	return nil
}

func sysUnsetenv(n string) error {
	if containsNul(n) {
		return errNulInCString()
	}
	// unsetenv(3) refuses an empty name and a name holding `=` with EINVAL.
	// syscall.Unsetenv does not check, so it is checked here.
	if n == "" || strings.IndexByte(n, '=') >= 0 {
		return rsio.ErrorFromRawOsError(rsio.RawOsError(syscall.EINVAL))
	}
	if err := syscall.Unsetenv(n); err != nil {
		return rsio.ErrorLastOsError(err)
	}
	return nil
}

// errNulInCString is the error sys::helpers::run_with_cstr gives for bytes
// holding a NUL, which no C string can hold.
func errNulInCString() error {
	return rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput, "file name contained an unexpected NUL byte")
}

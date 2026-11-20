// Port of std 1.99.1 src/sys/env/windows.rs

package rsenv

import (
	"syscall"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

func sysGetenv(k string) rs.Option[string] {
	key, err := toU16s(k)
	if err != nil {
		return rs.None[string]()
	}
	value, err := fillUtf16Buf(func(buf *uint16, size uint32) (uint32, syscall.Errno) {
		r, _, err := procGetEnvironmentVariableW.Call(
			uintptr(unsafe.Pointer(&key[0])), uintptr(unsafe.Pointer(buf)), uintptr(size))
		return uint32(r), lastErrorOf(err)
	})
	if err != nil {
		return rs.None[string]()
	}
	return rs.Some(os2path(value))
}

// sysSetenv is sys::env::setenv: SetEnvironmentVariableW, made through
// syscall.Setenv so that the Go runtime sees the change too (it reads
// GODEBUG from it). The strings are checked first, as to_u16s checks them.
func sysSetenv(k, v string) error {
	if _, err := toU16s(k); err != nil {
		return err
	}
	if _, err := toU16s(v); err != nil {
		return err
	}
	if err := syscall.Setenv(k, v); err != nil {
		return rsio.ErrorLastOsError(err)
	}
	return nil
}

// sysUnsetenv is sys::env::unsetenv: SetEnvironmentVariableW with no value,
// made through syscall.Unsetenv.
func sysUnsetenv(n string) error {
	if _, err := toU16s(n); err != nil {
		return err
	}
	if err := syscall.Unsetenv(n); err != nil {
		return rsio.ErrorLastOsError(err)
	}
	return nil
}

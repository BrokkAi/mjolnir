// Port of std 1.99.1 src/sys/io/error/unix.rs (error_string, on Linux)

//go:build linux

package rsio

import "strconv"

// errorString is sys::io::error_string: the text the C library gives for an
// error number. Rust calls __xpg_strerror_r with a buffer of 128 bytes; no
// text of either C library comes near that length.
func errorString(errno RawOsError) string {
	switch LinuxLibc {
	case Libc_Musl:
		if errno > 0 && int(errno) < len(strerrorMusl) && strerrorMusl[errno] != "" {
			return strerrorMusl[errno]
		}
		return "No error information"
	default:
		if errno >= 0 && int(errno) < len(strerrorGlibc) && strerrorGlibc[errno] != "" {
			return strerrorGlibc[errno]
		}
		return "Unknown error " + strconv.FormatInt(int64(errno), 10)
	}
}

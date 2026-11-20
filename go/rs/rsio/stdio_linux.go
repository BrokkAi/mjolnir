//go:build linux

package rsio

import (
	"math"

	"golang.org/x/sys/unix"
)

const ioctlReadTermios = unix.TCGETS

// readLimit is sys::fd::READ_LIMIT: the most one read or write asks for.
//
// The maximum read limit on most POSIX-like systems is `SSIZE_MAX`,
// with the man page quoting that if the count of bytes to read is
// greater than `SSIZE_MAX` the result is "unspecified".
const readLimit = math.MaxInt

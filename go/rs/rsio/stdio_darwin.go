//go:build darwin

package rsio

import (
	"math"

	"golang.org/x/sys/unix"
)

const ioctlReadTermios = unix.TIOCGETA

// readLimit is sys::fd::READ_LIMIT: the most one read or write asks for.
//
// On Apple targets however, apparently the 64-bit libc is either buggy or
// intentionally showing odd behavior by rejecting any read with a size
// larger than INT_MAX. To handle both of these the read size is capped on
// both platforms.
const readLimit = math.MaxInt32

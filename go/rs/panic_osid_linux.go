// Port of std 1.99.1 src/sys/thread/unix.rs (current_os_id)

//go:build linux

package rs

import "syscall"

// currentOSThreadID is thread::current_os_id, which the default panic hook
// prints after the thread's name. On Linux it is gettid(): the id `top` and
// debuggers show for the thread running the hook.
func currentOSThreadID() uint64 {
	return uint64(syscall.Gettid())
}

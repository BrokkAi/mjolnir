// Port of std 1.99.1 src/sys/thread/unix.rs (current_os_id)

//go:build darwin

package rs

import "syscall"

// currentOSThreadID is thread::current_os_id, which the default panic hook
// prints after the thread's name. Rust asks pthread_threadid_np for the
// current thread; the thread_selfid system call it is built on returns the
// same 64-bit id without C.
func currentOSThreadID() uint64 {
	id, _, errno := syscall.Syscall(syscall.SYS_THREAD_SELFID, 0, 0, 0)
	if errno != 0 {
		// Rust falls back to its own ThreadId, which this port does not have.
		return 0
	}
	return uint64(id)
}

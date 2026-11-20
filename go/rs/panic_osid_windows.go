// Port of std 1.99.1 src/sys/thread/windows.rs (current_os_id)

//go:build windows

package rs

import "golang.org/x/sys/windows"

// currentOSThreadID is thread::current_os_id, which the default panic hook
// prints after the thread's name: GetCurrentThreadId.
func currentOSThreadID() uint64 {
	return uint64(windows.GetCurrentThreadId())
}

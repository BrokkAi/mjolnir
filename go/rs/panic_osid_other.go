// Port of std 1.99.1 src/sys/thread/unsupported.rs (current_os_id)

//go:build !linux && !darwin && !windows

package rs

// currentOSThreadID is thread::current_os_id on a system the port does not
// target. Rust falls back to its own ThreadId there, which this port does
// not have, so it is 0.
func currentOSThreadID() uint64 {
	return 0
}

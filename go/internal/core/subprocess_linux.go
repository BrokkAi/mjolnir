//go:build linux

// Port of mj-core/src/subprocess.rs: the Linux parts (pidfd exit watches,
// close_range, /proc/self/fd and fs.nr_open).

package core

import (
	"errors"
	"os"
	"runtime"
	"strconv"
	"strings"
	"time"
	"unsafe"

	"golang.org/x/sys/unix"
)

// NewExitWatch must be called before the child is reaped, so the pid cannot
// be reused.
func NewExitWatch(process *os.Process) *ExitWatch {
	fd, err := unix.PidfdOpen(process.Pid, 0)
	if err != nil {
		fd = -1
	}
	return &ExitWatch{pidfd: fd}
}

// Wait sleeps up to timeout, returning early once the child exits. Returns
// whether the child may have exited.
func (w *ExitWatch) Wait(timeout time.Duration) bool {
	if w.pidfd >= 0 {
		deadline := time.Now().Add(timeout)
		for {
			remaining := max(time.Until(deadline), 0)
			millis := int(min(remaining.Milliseconds(), int64(^uint32(0)>>1)))
			poll := []unix.PollFd{{Fd: int32(w.pidfd), Events: unix.POLLIN}}
			ready, err := unix.Poll(poll, millis)
			if err == nil {
				return ready != 0
			}
			if errors.Is(err, unix.EINTR) {
				// A handled signal: keep waiting for the rest of the time.
				continue
			}
			// A poll refused outright (for example by a syscall filter) keeps
			// the plain pace and reports a possible exit, so the caller reaps
			// instead of spinning on a descriptor it cannot use.
			time.Sleep(max(time.Until(deadline), 0))
			return true
		}
	}
	if timeout > 0 {
		time.Sleep(timeout)
	}
	return true
}

// Close releases the pidfd.
func (w *ExitWatch) Close() {
	if w.pidfd >= 0 {
		_ = unix.Close(w.pidfd)
		w.pidfd = -1
	}
}

// markRangeCloexec marks the whole range in one close_range call (Linux
// 5.11+). Older kernels reject it.
func markRangeCloexec() bool {
	return unix.CloseRange(3, ^uint(0), unix.CLOSE_RANGE_CLOEXEC) == nil
}

// markListedDescriptorsCloexec marks the descriptors the kernel lists for
// this process, and only those. Returns false when the listing was
// unavailable, so the caller must scan by number instead.
func markListedDescriptorsCloexec() bool {
	return markListedDescriptors("/proc/self/fd")
}

// openFileLimitTarget is the soft limit to ask for given the hard limit.
// Linux refuses anything above fs.nr_open, which is what an unlimited hard
// limit really means there.
func openFileLimitTarget(hard uint64) (uint64, error) {
	if hard != rlimitInfinity {
		return hard, nil
	}
	text, err := os.ReadFile("/proc/sys/fs/nr_open")
	if err != nil {
		return 0, err
	}
	return strconv.ParseUint(strings.TrimSpace(string(text)), 10, 64)
}

// blockSIGTTOU blocks SIGTTOU on this thread until the returned function
// restores the thread's mask.
func blockSIGTTOU() func() {
	runtime.LockOSThread()
	var blocked, previous unix.Sigset_t
	bits := uint(unsafe.Sizeof(blocked.Val[0]) * 8)
	signal := uint(unix.SIGTTOU) - 1
	blocked.Val[signal/bits] |= 1 << (signal % bits)
	if err := unix.PthreadSigmask(unix.SIG_BLOCK, &blocked, &previous); err != nil {
		runtime.UnlockOSThread()
		return blockSIGTTOUByIgnoring()
	}
	return func() {
		_ = unix.PthreadSigmask(unix.SIG_SETMASK, &previous, nil)
		runtime.UnlockOSThread()
	}
}

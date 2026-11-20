//go:build darwin

// Port of mj-core/src/subprocess.rs: the macOS parts (the descriptor
// listing and kern.maxfilesperproc).

package core

import "golang.org/x/sys/unix"

// markRangeCloexec has no macOS equivalent of Linux's close_range.
func markRangeCloexec() bool { return false }

// markListedDescriptorsCloexec marks the descriptors /dev/fd lists for this
// process. Rust reads proc_pidinfo because it runs after fork, where
// opendir is not safe; the Go intermediate is an ordinary process.
func markListedDescriptorsCloexec() bool {
	return markListedDescriptors("/dev/fd")
}

// openFileLimitTarget is the soft limit to ask for given the hard limit.
// macOS reports an unlimited hard limit but refuses a soft limit above
// kern.maxfilesperproc; Go's runtime applies the same ceiling.
func openFileLimitTarget(hard uint64) (uint64, error) {
	perProcess, err := unix.SysctlUint32("kern.maxfilesperproc")
	if err != nil {
		return 0, err
	}
	return min(hard, uint64(perProcess)), nil
}

// blockSIGTTOU ignores SIGTTOU until the returned function restores it.
func blockSIGTTOU() func() { return blockSIGTTOUByIgnoring() }

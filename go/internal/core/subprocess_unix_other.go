//go:build unix && !linux && !darwin

// Port of mj-core/src/subprocess.rs: other Unix systems take the bounded
// descriptor scan and keep their hard open-file limit.

package core

func markRangeCloexec() bool { return false }

func markListedDescriptorsCloexec() bool { return false }

func openFileLimitTarget(hard uint64) (uint64, error) { return hard, nil }

// blockSIGTTOU ignores SIGTTOU until the returned function restores it.
func blockSIGTTOU() func() { return blockSIGTTOUByIgnoring() }

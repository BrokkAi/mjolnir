// Port of mj-client/src/daemon.rs: the zombie probe on Unix systems other
// than Linux and macOS.

//go:build unix && !linux && !darwin

package client

// ProcessIsZombie reports whether a non-child process has exited but not yet
// been reaped (see the Linux version for why this matters). The Rust build
// asks its process table library for the one process's status.
func ProcessIsZombie(pid uint32) bool {
	panic("unported: mj-client/src/daemon.rs process_is_zombie (other unix)")
}

// daemonNonChildIsAlive is the liveness of a daemon this process cannot
// reap.
func daemonNonChildIsAlive(pid uint32) bool {
	return ProcessIsAlive(pid)
}

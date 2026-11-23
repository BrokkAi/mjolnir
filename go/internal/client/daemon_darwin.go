// Port of mj-client/src/daemon.rs: the macOS zombie and process-group
// probes.

package client

import (
	"errors"

	"golang.org/x/sys/unix"
)

// darwinZombieStatus is SZOMB in <sys/proc.h>, the p_stat of a process that
// has exited but not been reaped.
const darwinZombieStatus = 5

// ProcessIsZombie reports whether a non-child process has exited but not yet
// been reaped (see the Linux version for why this matters). On macOS this
// asks the kernel for the one process's state. The Rust build's process
// table library cannot be used: its lookups skip zombies, so it reports an
// exited, unreaped process as absent rather than as a zombie, and
// `kill(pid, 0)` then calls it alive forever.
//
// The kernel's process table, read by `kern.proc.pid`, includes zombies;
// Rust reads the same state through proc_pidinfo's PROC_PIDTBSDINFO.
func ProcessIsZombie(pid uint32) bool {
	if pid > 1<<31-1 {
		return false
	}
	info, err := unix.SysctlKinfoProc("kern.proc.pid", int(pid))
	if err != nil || info == nil || info.Proc.P_pid != int32(pid) {
		return false
	}
	return info.Proc.P_stat == darwinZombieStatus
}

// OwnedDaemonGroupIsAlive probes the process group a daemon leads.
//
// Spawning detached makes the daemon a process-group leader. Darwin excludes
// zombies from group signal probes: ESRCH means the group is gone and EPERM
// means only exiting members remain. The latter is safe here because this is
// a group we created for our own same-user child, not an arbitrary process
// group. Signal 0 is only an existence probe, and the negative PID targets
// the daemon-owned group rather than another process.
func OwnedDaemonGroupIsAlive(pid int) bool {
	err := unix.Kill(-pid, 0)
	if err == nil {
		return true
	}
	return !errors.Is(err, unix.ESRCH) && !errors.Is(err, unix.EPERM)
}

// daemonNonChildIsAlive is the liveness of a daemon this process cannot
// reap.
func daemonNonChildIsAlive(pid uint32) bool {
	return OwnedDaemonGroupIsAlive(int(pid))
}

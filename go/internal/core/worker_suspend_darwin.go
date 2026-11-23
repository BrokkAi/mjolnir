// Port of mj-core/src/worker_suspend.rs: WorkerIdentity::is_alive on macOS.
// Rust reads proc_pidinfo(PROC_PIDTBSDINFO); kern.proc.pid gives the same
// start time and status.

package core

import "golang.org/x/sys/unix"

// darwinSZOMB is the process status of a zombie (sys/proc.h SZOMB).
const darwinSZOMB = 5

func workerIdentityIsAlive(w WorkerIdentity) (bool, error) {
	info, err := unix.SysctlKinfoProc("kern.proc.pid", int(w.Pid))
	if err != nil || info.Proc.P_pid != int32(w.Pid) {
		return false, nil
	}
	started := info.Proc.P_starttime
	start := uint64(started.Sec)*1_000_000 + uint64(started.Usec)
	return start == w.Start && info.Proc.P_stat != darwinSZOMB, nil
}

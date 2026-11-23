// Port of mj-worker/src/cpu_usage/macos.rs, plus the macOS branches of
// online_cpus and CpuCounter in mj-worker/src/cpu_usage.rs.
//
// Rust calls libproc's proc_pid_rusage and proc_listchildpids and Mach's
// mach_timebase_info through libc. Go reaches the same kernel calls without
// cgo: both libproc functions are thin wrappers of the proc_info system call
// (see libproc.c in Apple's Libc), and the time base is the ratio of a
// nanosecond to the timebase frequency the kernel reports as hw.tbfrequency.

package worker

import (
	"errors"
	"fmt"
	"math/big"
	"time"
	"unsafe"

	"golang.org/x/sys/unix"
)

// proc_info call numbers and flavors, from <sys/proc_info.h>.
const (
	darwinProcInfoCallListPids  = 1
	darwinProcInfoCallPidRusage = 9
	darwinProcPpidOnly          = 6
	darwinRusageInfoFlavorV2    = 2
)

// darwinRusageInfoV2 is `struct rusage_info_v2` from <sys/resource.h>; its
// time fields count Mach absolute-time ticks.
type darwinRusageInfoV2 struct {
	UUID                [16]byte
	UserTime            uint64
	SystemTime          uint64
	PkgIdleWkups        uint64
	InterruptWkups      uint64
	Pageins             uint64
	WiredSize           uint64
	ResidentSize        uint64
	PhysFootprint       uint64
	ProcStartAbstime    uint64
	ProcExitAbstime     uint64
	ChildUserTime       uint64
	ChildSystemTime     uint64
	ChildPkgIdleWkups   uint64
	ChildInterruptWkups uint64
	ChildPageins        uint64
	ChildElapsedAbstime uint64
	DiskioBytesread     uint64
	DiskioByteswritten  uint64
}

// darwinProcInfo is the proc_info system call behind libproc.
func darwinProcInfo(call, pid, flavor uintptr, arg uint64, buffer unsafe.Pointer, size uintptr) (int, error) {
	result, _, errno := unix.Syscall6(unix.SYS_PROC_INFO, call, pid, flavor, uintptr(arg), uintptr(buffer), size)
	if errno != 0 {
		return -1, errno
	}
	return int(int32(result)), nil
}

// darwinChildPids lists pid's children, growing the buffer until the result
// fits, since a process can spawn children between two calls.
func darwinChildPids(pid int32) ([]int32, error) {
	capacity := 64
	for {
		children := make([]int32, capacity)
		bytes := uintptr(len(children)) * unsafe.Sizeof(children[0])
		// proc_listchildpids is proc_listpids(PROC_PPID_ONLY), whose system
		// call answers in bytes; libproc divides that into a PID count.
		written, err := darwinProcInfo(darwinProcInfoCallListPids, darwinProcPpidOnly, uintptr(uint32(pid)), 0, unsafe.Pointer(&children[0]), bytes)
		if err != nil {
			return nil, err
		}
		count := written / int(unsafe.Sizeof(children[0]))
		if count < capacity {
			return children[:count], nil
		}
		if capacity > int(^uint(0)>>2) {
			return nil, errors.New("too many child processes")
		}
		capacity *= 2
	}
}

// processTreeCpuTime sums `rusage_info_v2` CPU time (own and children's),
// converted from Mach absolute-time ticks with the host's time base.
func processTreeCpuTime(root uint32) (time.Duration, error) {
	// `rusage_info_v2` reports CPU time in Mach absolute-time ticks, so the
	// nanoseconds-per-tick time base has to come from the same host.
	frequency, err := unix.SysctlUint64("hw.tbfrequency")
	if err != nil || frequency == 0 {
		return 0, errors.New("could not read Mach time base")
	}
	if root > 1<<31-1 {
		return 0, errors.New("out of range integral type conversion attempted")
	}
	pending := []int32{int32(root)}
	visited := map[int32]bool{}
	ticks := new(big.Int)
	for len(pending) > 0 {
		pid := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if visited[pid] {
			continue
		}
		visited[pid] = true
		var usage darwinRusageInfoV2
		if _, err := darwinProcInfo(darwinProcInfoCallPidRusage, uintptr(uint32(pid)), darwinRusageInfoFlavorV2, 0, unsafe.Pointer(&usage), 0); err != nil {
			if uint32(pid) != root && errors.Is(err, unix.ESRCH) {
				continue
			}
			return 0, fmt.Errorf("read process CPU time: %w", err)
		}
		for _, value := range []uint64{usage.UserTime, usage.SystemTime, usage.ChildUserTime, usage.ChildSystemTime} {
			ticks.Add(ticks, new(big.Int).SetUint64(value))
		}
		children, err := darwinChildPids(pid)
		if err != nil {
			if uint32(pid) != root && errors.Is(err, unix.ESRCH) {
				continue
			}
			return 0, fmt.Errorf("list process children: %w", err)
		}
		pending = append(pending, children...)
	}
	// nanoseconds = ticks * numer / denom, where numer/denom is one
	// nanosecond's share of a tick: 1e9 / hw.tbfrequency.
	nanos := ticks.Mul(ticks, big.NewInt(int64(time.Second)))
	nanos.Quo(nanos, new(big.Int).SetUint64(frequency))
	if !nanos.IsInt64() {
		return 0, errors.New("out of range integral type conversion attempted")
	}
	return time.Duration(nanos.Int64()), nil
}

// onlineCpus is sysconf(_SC_NPROCESSORS_ONLN), which Apple's libc answers
// from hw.activecpu: the host's online CPUs.
func onlineCpus() (uint32, error) {
	count, err := unix.SysctlUint32("hw.activecpu")
	if err != nil || count == 0 {
		return 0, errors.New("could not read the machine's online CPU count")
	}
	return count, nil
}

func discoverContainerCpuCounter() (cpuCounter, error) {
	return nil, errors.New("container CPU sampling is not supported on this platform")
}

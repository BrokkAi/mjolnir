// Port of the macOS `imp` of mj-worker/src/worker_stop/process_table.rs.
//
// Rust calls libproc's proc_pidinfo and proc_listallpids. Both are thin
// wrappers of the proc_info system call (libproc.c in Apple's Libc), which
// Go reaches without cgo through darwinProcInfo.

package worker

import (
	"errors"
	"unsafe"

	"golang.org/x/sys/unix"
)

// proc_info call numbers and flavors, from <sys/proc_info.h>.
const (
	darwinProcInfoCallPidInfo = 2
	darwinProcAllPids         = 1
	darwinProcPidTBSDInfo     = 3
	// darwinSZomb is SZOMB from <sys/proc.h>: a zombie awaiting collection.
	darwinSZomb = 5
)

// darwinProcBSDInfo is `struct proc_bsdinfo` from <sys/proc_info.h>.
type darwinProcBSDInfo struct {
	Flags       uint32
	Status      uint32
	Xstatus     uint32
	Pid         uint32
	Ppid        uint32
	UID         uint32
	GID         uint32
	RUID        uint32
	RGID        uint32
	SVUID       uint32
	SVGID       uint32
	Rfu1        uint32
	Comm        [16]byte
	Name        [32]byte
	Nfiles      uint32
	Pgid        uint32
	Pjobc       uint32
	ETdev       uint32
	ETpgid      uint32
	Nice        int32
	StartTvsec  uint64
	StartTvusec uint64
}

// processTableLookup is pid's entry; false when no such process exists.
func processTableLookup(pid uint32) (processEntry, bool, error) {
	var info darwinProcBSDInfo
	size := int(unsafe.Sizeof(info))
	written, err := darwinProcInfo(darwinProcInfoCallPidInfo, uintptr(pid), darwinProcPidTBSDInfo, 0, unsafe.Pointer(&info), uintptr(size))
	if err == nil && written == size {
		start := saturatingMul(info.StartTvsec, 1_000_000)
		start = saturatingAdd(start, info.StartTvusec)
		return processEntry{
			pid:    pid,
			ppid:   info.Ppid,
			pgid:   info.Pgid,
			start:  start,
			zombie: info.Status == darwinSZomb,
		}, true, nil
	}
	// Libproc reports a missing process as ESRCH, or as zero bytes.
	if (err == nil && written == 0) || errors.Is(err, unix.ESRCH) {
		return processEntry{}, false, nil
	}
	if err == nil {
		return processEntry{}, false, errors.New("proc_pidinfo returned a partial process record")
	}
	return processEntry{}, false, err
}

// processTableList is every process the kernel lists. Processes can start
// while the list is read, so it retries when the list filled its buffer.
func processTableList() ([]processEntry, error) {
	pidSize := int(unsafe.Sizeof(int32(0)))
	// A null buffer asks only for the size. The system call answers in bytes,
	// which libproc's proc_listallpids divides into a PID count.
	bytes, err := darwinProcInfo(darwinProcInfoCallListPids, darwinProcAllPids, 0, 0, nil, 0)
	if err != nil {
		return nil, err
	}
	capacity := bytes/pidSize + 64
	for {
		pids := make([]int32, capacity)
		written, err := darwinProcInfo(darwinProcInfoCallListPids, darwinProcAllPids, 0, 0, unsafe.Pointer(&pids[0]), uintptr(capacity*pidSize))
		if err != nil {
			return nil, err
		}
		count := written / pidSize
		if count < capacity {
			var entries []processEntry
			for _, pid := range pids[:count] {
				if pid <= 0 {
					continue
				}
				entry, found, err := processTableLookup(uint32(pid))
				if err != nil {
					return nil, err
				}
				if found {
					entries = append(entries, entry)
				}
			}
			return entries, nil
		}
		capacity *= 2
	}
}

func saturatingMul(a, b uint64) uint64 {
	if a != 0 && b > ^uint64(0)/a {
		return ^uint64(0)
	}
	return a * b
}

func saturatingAdd(a, b uint64) uint64 {
	if a > ^uint64(0)-b {
		return ^uint64(0)
	}
	return a + b
}

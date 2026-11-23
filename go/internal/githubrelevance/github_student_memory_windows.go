// Port of mj-github-relevance/src/github_student/inference.rs: sysinfo's
// System::available_memory on Windows, which parallelism reads:
// GlobalMemoryStatusEx's available physical memory.

package githubrelevance

import (
	"unsafe"

	"golang.org/x/sys/windows"
)

// memoryStatusEx is MEMORYSTATUSEX.
type memoryStatusEx struct {
	length               uint32
	memoryLoad           uint32
	totalPhys            uint64
	availPhys            uint64
	totalPageFile        uint64
	availPageFile        uint64
	totalVirtual         uint64
	availVirtual         uint64
	availExtendedVirtual uint64
}

var globalMemoryStatusEx = windows.NewLazySystemDLL("kernel32.dll").NewProc("GlobalMemoryStatusEx")

func availableMemory() (uint64, error) {
	status := memoryStatusEx{length: uint32(unsafe.Sizeof(memoryStatusEx{}))}
	if result, _, _ := globalMemoryStatusEx.Call(uintptr(unsafe.Pointer(&status))); result == 0 {
		// sysinfo leaves the figure at zero when the call fails.
		return 0, nil
	}
	return status.availPhys, nil
}

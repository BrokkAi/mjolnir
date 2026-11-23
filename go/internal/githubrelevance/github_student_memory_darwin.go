// Port of mj-github-relevance/src/github_student/inference.rs: sysinfo's
// System::available_memory on macOS, which parallelism reads:
// host_statistics64's free, inactive and purgeable pages less the
// compressor's, in bytes. purego calls libSystem without cgo.

package githubrelevance

import (
	"unsafe"

	"github.com/ebitengine/purego"
	"golang.org/x/sys/unix"
)

// vmStatistics64 is struct vm_statistics64 (mach/vm_statistics.h).
type vmStatistics64 struct {
	freeCount                          uint32
	activeCount                        uint32
	inactiveCount                      uint32
	wireCount                          uint32
	zeroFillCount                      uint64
	reactivations                      uint64
	pageins                            uint64
	pageouts                           uint64
	faults                             uint64
	cowFaults                          uint64
	lookups                            uint64
	hits                               uint64
	purges                             uint64
	purgeableCount                     uint32
	speculativeCount                   uint32
	decompressions                     uint64
	compressions                       uint64
	swapins                            uint64
	swapouts                           uint64
	compressorPageCount                uint32
	throttledCount                     uint32
	externalPageCount                  uint32
	internalPageCount                  uint32
	totalUncompressedPagesInCompressor uint64
}

// hostVMInfo64 is HOST_VM_INFO64.
const hostVMInfo64 = 4

func availableMemory() (uint64, error) {
	library, err := purego.Dlopen("/usr/lib/libSystem.B.dylib", purego.RTLD_LAZY|purego.RTLD_GLOBAL)
	if err != nil {
		return 0, err
	}
	var machHostSelf func() uint32
	var hostStatistics64 func(host uint32, flavor int32, info unsafe.Pointer, count *uint32) int32
	purego.RegisterLibFunc(&machHostSelf, library, "mach_host_self")
	purego.RegisterLibFunc(&hostStatistics64, library, "host_statistics64")
	var stat vmStatistics64
	count := uint32(unsafe.Sizeof(stat) / 4)
	if hostStatistics64(machHostSelf(), hostVMInfo64, unsafe.Pointer(&stat), &count) != 0 {
		// sysinfo leaves the figure at zero when the call fails.
		return 0, nil
	}
	pages := uint64(stat.freeCount) + uint64(stat.inactiveCount) + uint64(stat.purgeableCount)
	if pages < uint64(stat.compressorPageCount) {
		return 0, nil
	}
	pages -= uint64(stat.compressorPageCount)
	pageSize := uint64(unix.Getpagesize())
	if pageSize != 0 && pages > ^uint64(0)/pageSize {
		return ^uint64(0), nil
	}
	return pages * pageSize, nil
}

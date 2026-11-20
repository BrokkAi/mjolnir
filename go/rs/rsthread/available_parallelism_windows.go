// Port of std 1.99.1 src/sys/thread/windows.rs (available_parallelism)

package rsthread

import (
	"unsafe"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

var getSystemInfo = windows.NewLazySystemDLL("kernel32.dll").NewProc("GetSystemInfo")

// systemInfo is SYSTEM_INFO.
type systemInfo struct {
	processorArchitecture     uint16
	reserved                  uint16
	pageSize                  uint32
	minimumApplicationAddress uintptr
	maximumApplicationAddress uintptr
	activeProcessorMask       uintptr
	numberOfProcessors        uint32
	processorType             uint32
	allocationGranularity     uint32
	processorLevel            uint16
	processorRevision         uint16
}

func availableParallelism() (int, error) {
	var info systemInfo
	// GetSystemInfo returns nothing and cannot fail.
	getSystemInfo.Call(uintptr(unsafe.Pointer(&info)))
	if info.numberOfProcessors == 0 {
		return 0, rsio.ErrorUnknownThreadCount()
	}
	return int(info.numberOfProcessors), nil
}

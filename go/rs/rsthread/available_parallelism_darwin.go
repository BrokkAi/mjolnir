// Port of std 1.99.1 src/sys/thread/unix.rs (available_parallelism) for
// macOS

package rsthread

import (
	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

func availableParallelism() (int, error) {
	// sysconf(_SC_NPROCESSORS_ONLN), which is the sysctl hw.activecpu.
	cpus, err := unix.SysctlUint32("hw.activecpu")
	if err != nil {
		if errno, ok := err.(unix.Errno); ok {
			return 0, rsio.ErrorFromRawOsError(rsio.RawOsError(errno))
		}
		return 0, rsio.ErrorUnknownThreadCount()
	}
	if cpus == 0 {
		return 0, rsio.ErrorUnknownThreadCount()
	}
	return int(cpus), nil
}

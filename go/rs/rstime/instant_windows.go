// Port of std 1.99.1 src/sys/pal/windows/time.rs (perf_counter::epsilon)

package rstime

import (
	"sync"
	"time"
	"unsafe"

	"golang.org/x/sys/windows"
)

var (
	queryPerformanceFrequency = windows.NewLazySystemDLL("kernel32.dll").NewProc("QueryPerformanceFrequency")
	epsilonOnce               sync.Once
	epsilon                   time.Duration
)

// instantEpsilon is the margin of error for comparing two readings of the
// performance counter: one tick, which is one second over the counter's
// frequency. Two instants closer than this are treated as the same.
func instantEpsilon() time.Duration {
	epsilonOnce.Do(func() {
		var frequency int64
		// The call cannot fail on Windows XP or later, and the frequency is
		// never zero there.
		result, _, _ := queryPerformanceFrequency.Call(uintptr(unsafe.Pointer(&frequency)))
		if result != 0 && frequency > 0 {
			epsilon = time.Duration(nanosPerSec / frequency)
		}
	})
	return epsilon
}

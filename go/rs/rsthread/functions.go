// Port of std 1.99.1 src/thread/functions.rs

package rsthread

import (
	"runtime"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Spawn is std::thread::spawn: it starts f on a new thread and returns a
// handle to join it. The thread is a goroutine; a panic in it does not end
// the process but is delivered to Join, after the panic hook has run with
// the thread's name.
func Spawn[T any](f func() T) *JoinHandle[T] {
	handle, _ := BuilderSpawn(BuilderNew(), f)
	return handle
}

// YieldNow is std::thread::yield_now.
func YieldNow() { runtime.Gosched() }

// Panicking is std::thread::panicking: whether the calling goroutine is
// unwinding because of a panic. It is true in the deferred calls a panic
// runs (destructors, in Rust's terms), in what they call, and in the panic
// hook; false in normal code, after the panic was recovered, and while a
// goroutine ends through runtime.Goexit, which is not a panic.
//
// It reads the goroutine's stack, which holds the Go runtime's panic frame
// exactly while a panic unwinds it, so it costs a stack walk. One case
// differs from Rust: in the deferred function that recovers a panic, it is
// still true after recover until that function returns.
func Panicking() bool {
	pcs := make([]uintptr, 64)
	for {
		n := runtime.Callers(1, pcs)
		if n < len(pcs) {
			pcs = pcs[:n]
			break
		}
		pcs = make([]uintptr, 2*len(pcs))
	}
	frames := runtime.CallersFrames(pcs)
	for {
		frame, more := frames.Next()
		if frame.Function == "runtime.gopanic" {
			return true
		}
		if !more {
			return false
		}
	}
}

// checkDuration panics on a negative duration, which a Rust Duration cannot
// be: it comes from Go's own subtraction where the Rust code would have
// panicked.
func checkDuration(dur time.Duration) {
	if dur < 0 {
		rs.PanicAt(2, "rsthread: negative Duration (a Rust Duration is unsigned; subtract with rstime.DurationSub, DurationCheckedSub or DurationSaturatingSub)")
	}
}

// Sleep is std::thread::sleep: it blocks the calling thread for at least
// dur. std::thread::sleep does not follow tokio's paused clock, and neither
// does this.
func Sleep(dur time.Duration) {
	checkDuration(dur)
	time.Sleep(dur)
}

// Park is std::thread::park: it blocks until the calling thread's token is
// available (see Thread.Unpark), and takes it. Rust allows park to return
// without a token; this one does not, and code must not rely on either.
func Park() { currentInner().parker.park() }

// ParkTimeout is std::thread::park_timeout: Park, giving up after dur.
func ParkTimeout(dur time.Duration) {
	checkDuration(dur)
	currentInner().parker.parkTimeout(dur)
}

// AvailableParallelism is std::thread::available_parallelism: an estimate of
// how many threads the program can run at once. On Linux it is the number of
// CPUs the process may run on, lowered to the cgroup CPU quota, as Rust
// computes it; elsewhere the number of CPUs the system reports. The error is
// an io::Error (go/rs/rsio) when the number cannot be found.
func AvailableParallelism() (int, error) { return availableParallelism() }

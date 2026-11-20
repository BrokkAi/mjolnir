// Port of std 1.99.1 tests/thread.rs

package rsthread

// Not ported:
//   - sleep_until: thread::sleep_until is unstable.
//   - thread_local_hygeiene: a compilation test of the thread_local! macro.

import (
	"math"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rssync"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

func TestItThread_sleep_very_long(t *testing.T) {
	finished := rssync.MutexNew(false)
	tFinished := finished
	Spawn(func() rs.Unit {
		Sleep(rstime.DurationNew(math.MaxUint64, 0))
		*tFinished.Lock().Get() = true
		return rs.Unit{}
	})
	Sleep(rstime.DurationFromMillis(100))
	guard := finished.Lock()
	defer guard.Drop()
	assertEq(t, *guard.Get(), false)
}

func TestItThread_thread_local_containing_const_statements(t *testing.T) {
	// This exercises the `const $init:block` cases of the thread_local macro.
	// Despite overlapping with expression syntax, the `const { ... }` is not
	// parsed as `$init:expr`.
	cell := LocalKeyNew(func() rs.Cell[uint32] {
		value := uint32(1)
		return rs.CellNew(value)
	})
	refcell := LocalKeyNew(func() rs.RefCell[uint32] {
		value := uint32(1)
		return rs.RefCellNew(value)
	})

	assertEq(t, LocalKeyCellGet(cell), 1)
	assertEq(t, LocalKeyRefCellTake(refcell), 1)
}

func TestItThread_available_parallelism(t *testing.T) {
	// check that std::thread::available_parallelism() returns a valid value
	_, err := AvailableParallelism()
	assert(t, err == nil, err)
}

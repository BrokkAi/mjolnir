// Port of std 1.99.1 src/sync/poison.rs

package rssync

import (
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// How poisoning is detected.
//
// Rust poisons a lock when its guard is dropped while the thread is panicking
// and the guard was taken before the panic began. Go has no flag that says a
// goroutine is panicking, but a deferred function that the panic itself calls
// can ask with recover. So a guard's Drop calls recover: when it returns a
// value, Drop is running because the frame that deferred it is being unwound
// by a panic, which is the same condition. Drop then marks the lock poisoned,
// unlocks it, and panics again with the same value, so the panic continues as
// if it had not been touched.
//
// This sees exactly the guards Rust sees:
//
//   - `defer guard.Drop()` in any frame the panic unwinds poisons the lock.
//   - A lock taken and released inside a destructor that runs during the
//     unwinding does not: recover returns nil there, because that guard's
//     Drop is run by its own function returning, not by the panic. Rust does
//     not poison it either, since the thread was already panicking when the
//     guard was taken.
//   - runtime.Goexit is not a panic and poisons nothing.
//
// It has one blind spot. recover only answers the function the panic calls
// directly, so a guard that is dropped by another deferred function (a struct
// that owns a guard and drops it in its own Drop) is released without
// poisoning. Such a type calls recover itself; see MutexGuard.DropPanicking.

// poisonFlag is poison::Flag.
type poisonFlag struct {
	failed atomic.Bool
}

func (f *poisonFlag) get() bool { return f.failed.Load() }

func (f *poisonFlag) set() { f.failed.Store(true) }

func (f *poisonFlag) clear() { f.failed.Store(false) }

const (
	poisonErrorDisplay = "poisoned lock: another task failed inside"
	// unwrapPoisoned is the panic of `.unwrap()` on a LockResult's Err: the
	// Debug form of a PoisonError is `PoisonError { .. }`.
	unwrapPoisoned = "called `Result::unwrap()` on an `Err` value: PoisonError { .. }"
	// expectPoisoned follows the message of `.expect(message)`.
	expectPoisoned = ": PoisonError { .. }"
)

// lockMode says what a locking call does with a poisoned lock.
type lockMode uint8

const (
	// lockUnwrap is `.unwrap()`: panic.
	lockUnwrap lockMode = iota
	// lockExpect is `.expect(message)`: panic with the message.
	lockExpect
	// lockReport is everything else: hand the guard over and say so.
	lockReport
)

// panicPoisoned raises the panic of unwrap or expect on a poisoned lock,
// attributed to the caller skip frames above the caller of panicPoisoned.
func panicPoisoned(mode lockMode, message string, skip int) {
	if mode == lockExpect {
		rs.PanicAt(skip+1, message+expectPoisoned)
	}
	rs.PanicAt(skip+1, unwrapPoisoned)
}

// panicUninitialised reports the use of a zero-value handle, attributed to
// the caller skip frames above the caller of panicUninitialised.
func panicUninitialised(name string, skip int) {
	rs.PanicAt(skip+1, "rssync: use of an uninitialised "+name+" (the zero value is not a "+name+"; create one with rssync."+name+"New)")
}

// PoisonError is std::sync::PoisonError<T>: the error a lock returns once a
// thread has panicked while holding it. It carries what the call would have
// returned, usually the guard, which IntoInner hands over.
//
// The locking methods of this package do not return it: LockResult and its
// relatives return the guard and a flag, and TryLock wraps it in a
// TryLockError_Poisoned. Build one with PoisonErrorNew where the Rust code
// builds its own LockResult.
type PoisonError[T any] struct {
	data T
}

// PoisonErrorNew is PoisonError::new.
func PoisonErrorNew[T any](data T) *PoisonError[T] {
	return &PoisonError[T]{data: data}
}

// IntoInner is PoisonError::into_inner: the data the error carries.
func (e *PoisonError[T]) IntoInner() T { return e.data }

// GetRef is PoisonError::get_ref.
func (e *PoisonError[T]) GetRef() *T { return &e.data }

// GetMut is PoisonError::get_mut.
func (e *PoisonError[T]) GetMut() *T { return &e.data }

// String is the error's Display text.
func (e *PoisonError[T]) String() string { return poisonErrorDisplay }

// Error makes the value a Go error with its Display text.
func (e *PoisonError[T]) Error() string { return poisonErrorDisplay }

// FmtDisplay is the Display impl.
func (e *PoisonError[T]) FmtDisplay(f *rs.Formatter) error { return f.Pad(poisonErrorDisplay) }

// FmtDebug is the Debug impl: `PoisonError { .. }`.
func (e *PoisonError[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("PoisonError").FinishNonExhaustive()
}

// TryLockError is std::sync::TryLockError<Guard>: why TryLock, TryRead or
// TryWrite did not hand over a clean lock. It is one of
// *TryLockError_Poisoned[G] and *TryLockError_WouldBlock[G], with G the
// guard's type (*MutexGuard[T], *RwLockReadGuard[T], *RwLockWriteGuard[T]).
type TryLockError[G any] interface {
	error
	isTryLockError(G)
}

// TryLockError_Poisoned is TryLockError::Poisoned: the lock was acquired, and
// another thread failed while holding it. F0 carries the guard.
type TryLockError_Poisoned[G any] struct {
	F0 *PoisonError[G]
}

// TryLockError_WouldBlock is TryLockError::WouldBlock: the lock is held and
// was not acquired.
type TryLockError_WouldBlock[G any] struct{}

func (e *TryLockError_Poisoned[G]) isTryLockError(G)   {}
func (e *TryLockError_WouldBlock[G]) isTryLockError(G) {}

const wouldBlockDisplay = "try_lock failed because the operation would block"

// String is the error's Display text.
func (e *TryLockError_Poisoned[G]) String() string { return poisonErrorDisplay }

// Error makes the value a Go error with its Display text.
func (e *TryLockError_Poisoned[G]) Error() string { return poisonErrorDisplay }

// FmtDisplay is the Display impl.
func (e *TryLockError_Poisoned[G]) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(poisonErrorDisplay)
}

// FmtDebug is the Debug impl. Rust formats the string "Poisoned(..)" with
// Debug, so the output carries the quotation marks.
func (e *TryLockError_Poisoned[G]) FmtDebug(f *rs.Formatter) error {
	return rs.DebugFmt("Poisoned(..)", f)
}

// String is the error's Display text.
func (e *TryLockError_WouldBlock[G]) String() string { return wouldBlockDisplay }

// Error makes the value a Go error with its Display text.
func (e *TryLockError_WouldBlock[G]) Error() string { return wouldBlockDisplay }

// FmtDisplay is the Display impl.
func (e *TryLockError_WouldBlock[G]) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(wouldBlockDisplay)
}

// FmtDebug is the Debug impl. Rust formats the string "WouldBlock" with
// Debug, so the output carries the quotation marks.
func (e *TryLockError_WouldBlock[G]) FmtDebug(f *rs.Formatter) error {
	return rs.DebugFmt("WouldBlock", f)
}

// TryLockErrorFromPoisonError is `impl From<PoisonError<T>> for TryLockError<T>`.
func TryLockErrorFromPoisonError[G any](err *PoisonError[G]) TryLockError[G] {
	return &TryLockError_Poisoned[G]{F0: err}
}

// rawDebug is a value whose Debug output is its text as it stands, the way
// `format_args!("<locked>")` prints.
type rawDebug string

func (r rawDebug) FmtDebug(f *rs.Formatter) error { return f.WriteStr(string(r)) }

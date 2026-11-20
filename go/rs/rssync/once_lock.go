// Port of std 1.99.1 src/sync/once_lock.rs

package rssync

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

type onceLockState[T any] struct {
	once onceInner
	// value is set by the initializing call before the Once completes, and
	// read only once it has.
	value *T
}

// OnceLock is std::sync::OnceLock<T>: a cell that is written at most once,
// by whichever thread gets there first.
//
// A OnceLock is a small handle: copies are the same cell, and the zero value
// is not one (create one with OnceLockNew; rs.Default builds a real one for a
// field of a struct whose Default is derived).
type OnceLock[T any] struct {
	state *onceLockState[T]
}

// OnceLockNew is OnceLock::new: an empty cell.
func OnceLockNew[T any]() OnceLock[T] {
	return OnceLock[T]{state: &onceLockState[T]{}}
}

// OnceLockDefault is `impl Default for OnceLock<T>`: an empty cell.
func OnceLockDefault[T any]() OnceLock[T] { return OnceLockNew[T]() }

// OnceLockFrom is `impl From<T> for OnceLock<T>`: a cell that holds value.
func OnceLockFrom[T any](value T) OnceLock[T] {
	state := &onceLockState[T]{value: &value}
	state.once.state.Store(onceComplete)
	return OnceLock[T]{state: state}
}

// ReflectSetDefault makes rs.Default build a real cell for a OnceLock field.
func (o *OnceLock[T]) ReflectSetDefault() { *o = OnceLockNew[T]() }

func (o OnceLock[T]) must(skip int) *onceLockState[T] {
	if o.state == nil {
		panicUninitialised("OnceLock", skip+1)
	}
	return o.state
}

// Get is OnceLock::get: the value, or None when the cell is empty or is
// being initialized.
func (o OnceLock[T]) Get() rs.Option[*T] {
	state := o.must(1)
	if state.once.isCompleted() {
		return rs.Some(state.value)
	}
	return rs.None[*T]()
}

// GetMut is OnceLock::get_mut, which in the port is Get.
func (o OnceLock[T]) GetMut() rs.Option[*T] {
	state := o.must(1)
	if state.once.isCompleted() {
		return rs.Some(state.value)
	}
	return rs.None[*T]()
}

// Wait is OnceLock::wait: it blocks until the cell is initialized.
func (o OnceLock[T]) Wait() *T {
	state := o.must(1)
	if !state.once.isCompleted() {
		state.once.wait(true, 1)
	}
	return state.value
}

// Set is OnceLock::set: it stores value if the cell is empty, and reports
// whether it did. Like Rust's it waits for an initialization that is running.
//
// Rust hands the value back in Err when the cell was already full; the Go
// caller still has it.
func (o OnceLock[T]) Set(value T) bool {
	return o.set(value, 1)
}

func (o OnceLock[T]) set(value T, skip int) bool {
	state := o.must(skip + 1)
	if state.once.isCompleted() {
		return false
	}
	stored := false
	state.once.call(true, func(*OnceState) {
		state.value = &value
		stored = true
	}, skip+1)
	return stored
}

// SetUnwrap is `o.set(value).unwrap()`: it panics, showing the value, when
// the cell was already full.
func (o OnceLock[T]) SetUnwrap(value T) {
	if !o.set(value, 1) {
		rs.PanicAt(1, "called `Result::unwrap()` on an `Err` value: "+rs.DebugString(value))
	}
}

// SetExpect is `o.set(value).expect(message)`.
func (o OnceLock[T]) SetExpect(value T, message string) {
	if !o.set(value, 1) {
		rs.PanicAt(1, message+": "+rs.DebugString(value))
	}
}

// GetOrInit is OnceLock::get_or_init: the value, after initializing the cell
// with f() if it was empty. Many threads may call it at once; one f runs and
// the others wait for it. If f panics the cell stays empty and the panic
// reaches the caller. f must not initialize the same cell: like Rust's, that
// deadlocks.
func (o OnceLock[T]) GetOrInit(f func() T) *T {
	state := o.must(1)
	if state.once.isCompleted() {
		return state.value
	}
	// Ignore poisoning from other threads
	// If another thread panics, then we'll be able to run our closure
	state.once.call(true, func(*OnceState) {
		value := f()
		state.value = &value
	}, 1)
	return state.value
}

// IntoInner is OnceLock::into_inner: the value, taken out of the cell.
func (o OnceLock[T]) IntoInner() rs.Option[T] {
	return o.take(1)
}

// Take is OnceLock::take: the value, leaving the cell empty and ready to be
// set again. It is for code that has the cell to itself.
func (o OnceLock[T]) Take() rs.Option[T] {
	return o.take(1)
}

func (o OnceLock[T]) take(skip int) rs.Option[T] {
	state := o.must(skip + 1)
	if !state.once.isCompleted() {
		return rs.None[T]()
	}
	value := *state.value
	state.once.reset()
	state.value = nil
	return rs.Some(value)
}

// Clone is `impl Clone for OnceLock<T>`: a new cell holding a clone of the
// value, or an empty one.
func (o OnceLock[T]) Clone() OnceLock[T] {
	state := o.must(1)
	if state.once.isCompleted() {
		return OnceLockFrom(rs.Clone(*state.value))
	}
	return OnceLockNew[T]()
}

// Equal is `impl PartialEq for OnceLock<T>`: two cells are equal when both
// are empty or their values are equal.
func (o OnceLock[T]) Equal(other OnceLock[T]) bool {
	a, b := o.must(1), other.must(1)
	aSet, bSet := a.once.isCompleted(), b.once.isCompleted()
	if !aSet || !bSet {
		return aSet == bSet
	}
	return rs.Eq(*a.value, *b.value)
}

// FmtDebug is the Debug impl: `OnceLock(5)` or `OnceLock(<uninit>)`.
func (o OnceLock[T]) FmtDebug(f *rs.Formatter) error {
	state := o.must(1)
	d := f.DebugTuple("OnceLock")
	if state.once.isCompleted() {
		d.Field(*state.value)
	} else {
		d.Field(rawDebug("<uninit>"))
	}
	return d.Finish()
}

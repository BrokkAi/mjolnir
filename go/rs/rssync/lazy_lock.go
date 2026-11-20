// Port of std 1.99.1 src/sync/lazy_lock.rs

package rssync

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

type lazyLockState[T any] struct {
	once onceInner
	// f is the initializer until it has been taken to run.
	f func() T
	// value is set by the initializing call before the Once completes.
	value *T
}

// LazyLock is std::sync::LazyLock<T>: a value that is computed the first
// time it is used, by whichever thread gets there first.
//
// A Rust `static NAME: LazyLock<T> = LazyLock::new(|| ..)` is a package
// variable `var name = rssync.LazyLockNew(func() T { .. })`. Rust reaches the
// value by dereferencing the LazyLock; here that is written out: Deref (or
// Force) returns a pointer to it.
//
// A LazyLock is a small handle: copies are the same value, and the zero
// value is not one. If the initializer panics the LazyLock is poisoned, and
// every later use panics.
type LazyLock[T any] struct {
	state *lazyLockState[T]
}

// LazyLockNew is LazyLock::new.
func LazyLockNew[T any](f func() T) LazyLock[T] {
	return LazyLock[T]{state: &lazyLockState[T]{f: f}}
}

// LazyLockFrom is `impl From<T> for LazyLock<T>`: a LazyLock that already
// holds value.
func LazyLockFrom[T any](value T) LazyLock[T] {
	state := &lazyLockState[T]{value: &value}
	state.once.state.Store(onceComplete)
	return LazyLock[T]{state: state}
}

// LazyLockDefault is `impl Default for LazyLock<T>`: a LazyLock that
// computes T's Default.
func LazyLockDefault[T any]() LazyLock[T] { return LazyLockNew(rs.Default[T]) }

// ReflectSetDefault makes rs.Default build a real LazyLock for a LazyLock
// field.
func (l *LazyLock[T]) ReflectSetDefault() { *l = LazyLockDefault[T]() }

const lazyLockPoisoned = "LazyLock instance has previously been poisoned"

func (l LazyLock[T]) force(skip int) *T {
	state := l.state
	if state == nil {
		panicUninitialised("LazyLock", skip+1)
	}
	if state.once.isCompleted() {
		return state.value
	}
	state.once.call(true, func(onceState *OnceState) {
		if onceState.IsPoisoned() {
			rs.PanicAt(skip+5, lazyLockPoisoned)
		}
		f := state.f
		state.f = nil
		value := f()
		state.value = &value
	}, skip+1)
	return state.value
}

// Force is LazyLock::force: the value, computed now if it was not yet. It
// panics when an earlier initialization panicked.
func (l LazyLock[T]) Force() *T { return l.force(1) }

// Deref is the `impl Deref`: `*lazy`, and every use of the value through the
// LazyLock, is `*lazy.Deref()`.
func (l LazyLock[T]) Deref() *T { return l.force(1) }

// ForceMut is LazyLock::force_mut, which in the port is Force.
func (l LazyLock[T]) ForceMut() *T { return l.force(1) }

// Get is LazyLock::get: the value if it has been computed, without computing
// it.
func (l LazyLock[T]) Get() rs.Option[*T] {
	state := l.state
	if state == nil {
		panicUninitialised("LazyLock", 1)
	}
	if state.once.isCompleted() {
		return rs.Some(state.value)
	}
	return rs.None[*T]()
}

// GetMut is LazyLock::get_mut, which in the port is Get.
func (l LazyLock[T]) GetMut() rs.Option[*T] {
	state := l.state
	if state == nil {
		panicUninitialised("LazyLock", 1)
	}
	if state.once.isCompleted() {
		return rs.Some(state.value)
	}
	return rs.None[*T]()
}

// LazyLockForce is `LazyLock::force(&lazy)`, as the associated function Rust
// code calls.
func LazyLockForce[T any](lazy LazyLock[T]) *T { return lazy.force(1) }

// LazyLockGet is `LazyLock::get(&lazy)`.
func LazyLockGet[T any](lazy LazyLock[T]) rs.Option[*T] { return lazy.Get() }

// FmtDebug is the Debug impl: `LazyLock(5)` or `LazyLock(<uninit>)`.
func (l LazyLock[T]) FmtDebug(f *rs.Formatter) error {
	d := f.DebugTuple("LazyLock")
	if value, ok := l.Get().Get(); ok {
		d.Field(*value)
	} else {
		d.Field(rawDebug("<uninit>"))
	}
	return d.Finish()
}

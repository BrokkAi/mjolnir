// Port of std 1.99.1 src/sync/poison/mutex.rs

package rssync

import (
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// mutexState is the mutex itself: the lock, its poison flag and the value it
// protects. Every copy of a Mutex handle points to one.
type mutexState[T any] struct {
	inner  sync.Mutex
	poison poisonFlag
	data   T
}

// Mutex is std::sync::Mutex<T>: a mutual-exclusion lock that owns the value
// it protects, which is reached only through the guard a locking call
// returns.
//
// A Mutex is a small handle to the lock. Copying the handle, or a struct that
// holds it, does not copy the lock: both copies are the same mutex, as a
// moved Mutex is in Rust. The zero value is not a mutex, and using one
// panics; create one with MutexNew. (rs.Default builds a real one for a field
// of a struct whose Default is derived.) An Arc<Mutex<T>> is a *Mutex[T].
//
// Each of Rust's ways of handling a poisoned lock has a method:
//
//	m.lock().unwrap()                              m.Lock()
//	m.lock().expect("..")                          m.LockExpect("..")
//	m.lock().unwrap_or_else(|e| e.into_inner())    m.LockInner()
//	match m.lock() { Ok(g) => .., Err(p) => .. }   g, poisoned := m.LockResult()
//	m.try_lock()                                   g, err := m.TryLock()
//
// A guard that Rust holds for a single statement is With, WithInner or
// WithExpect:
//
//	state.lock().unwrap().push(x);                 state.With(func(v *[]int) { *v = append(*v, x) })
type Mutex[T any] struct {
	state *mutexState[T]
}

// MutexNew is Mutex::new: a mutex in an unlocked state, holding t.
func MutexNew[T any](t T) Mutex[T] {
	return Mutex[T]{state: &mutexState[T]{data: t}}
}

// MutexFrom is `impl From<T> for Mutex<T>`, which is MutexNew.
func MutexFrom[T any](t T) Mutex[T] { return MutexNew(t) }

// MutexDefault is `impl Default for Mutex<T>`: a mutex holding T's Default.
func MutexDefault[T any]() Mutex[T] { return MutexNew(rs.Default[T]()) }

// ReflectSetDefault makes rs.Default build a real mutex for a Mutex field.
func (m *Mutex[T]) ReflectSetDefault() { *m = MutexDefault[T]() }

// MutexGuard is std::sync::MutexGuard: the proof that the mutex is locked,
// and the way to its value. The mutex stays locked until Drop.
//
// Write `defer guard.Drop()` on the line after taking it, as for every value
// with a destructor (translation-rules.md, rule 4); an earlier Drop where
// Rust drops earlier makes the deferred one do nothing. That deferred call is
// also what poisons the mutex when the function panics. Rust's guard cannot
// leave its thread; this one is not checked, and must not be handed to
// another goroutine while a deferred Drop still refers to it.
type MutexGuard[T any] struct {
	// lock is the mutex, and nil once the guard was dropped.
	lock *mutexState[T]
}

// acquire locks the mutex into guard and reports whether it is poisoned. With
// lockUnwrap or lockExpect a poisoned mutex is unlocked again and the call
// panics, attributed to the caller skip frames above the caller of acquire.
func (m Mutex[T]) acquire(guard *MutexGuard[T], mode lockMode, message string, skip int) bool {
	state := m.state
	if state == nil {
		panicUninitialised("Mutex", skip+1)
	}
	state.inner.Lock()
	if state.poison.get() {
		if mode != lockReport {
			// The guard inside the PoisonError is dropped as unwrap panics.
			state.inner.Unlock()
			panicPoisoned(mode, message, skip+1)
		}
		guard.lock = state
		return true
	}
	guard.lock = state
	return false
}

// Lock is `m.lock().unwrap()`: it blocks until the mutex is acquired and
// returns its guard. It panics when the mutex is poisoned.
func (m Mutex[T]) Lock() *MutexGuard[T] {
	guard := &MutexGuard[T]{}
	m.acquire(guard, lockUnwrap, "", 1)
	return guard
}

// LockExpect is `m.lock().expect(message)`.
func (m Mutex[T]) LockExpect(message string) *MutexGuard[T] {
	guard := &MutexGuard[T]{}
	m.acquire(guard, lockExpect, message, 1)
	return guard
}

// LockInner is `m.lock().unwrap_or_else(|e| e.into_inner())` (and
// `.unwrap_or_else(PoisonError::into_inner)`): the guard, poisoned or not.
func (m Mutex[T]) LockInner() *MutexGuard[T] {
	guard := &MutexGuard[T]{}
	m.acquire(guard, lockReport, "", 1)
	return guard
}

// LockResult is `m.lock()` for code that looks at the result: the guard,
// which holds the lock either way, and whether the mutex is poisoned. Rust's
// Err(PoisonError) carries the same guard.
func (m Mutex[T]) LockResult() (guard *MutexGuard[T], poisoned bool) {
	guard = &MutexGuard[T]{}
	poisoned = m.acquire(guard, lockReport, "", 1)
	return guard, poisoned
}

// TryLock is `m.try_lock()`. The error is nil when the mutex was acquired
// clean, a *TryLockError_WouldBlock[*MutexGuard[T]] when it is held (the
// guard is then nil), and a *TryLockError_Poisoned[*MutexGuard[T]] when it
// was acquired and is poisoned. In that last case the lock is held: the
// guard is returned as well as carried in the error.
//
// Drop on a nil guard does nothing, so `defer guard.Drop()` after TryLock is
// right in all three cases.
func (m Mutex[T]) TryLock() (*MutexGuard[T], error) {
	state := m.state
	if state == nil {
		panicUninitialised("Mutex", 1)
	}
	if !state.inner.TryLock() {
		return nil, &TryLockError_WouldBlock[*MutexGuard[T]]{}
	}
	guard := &MutexGuard[T]{lock: state}
	if state.poison.get() {
		return guard, &TryLockError_Poisoned[*MutexGuard[T]]{F0: PoisonErrorNew(guard)}
	}
	return guard, nil
}

// With is `m.lock().unwrap()` held for one statement: it locks the mutex,
// calls f with the value, and unlocks it. It panics when the mutex is
// poisoned, and a panic in f poisons it.
func (m Mutex[T]) With(f func(v *T)) {
	var guard MutexGuard[T]
	m.acquire(&guard, lockUnwrap, "", 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// WithExpect is `m.lock().expect(message)` held for one statement.
func (m Mutex[T]) WithExpect(message string, f func(v *T)) {
	var guard MutexGuard[T]
	m.acquire(&guard, lockExpect, message, 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// WithInner is `m.lock().unwrap_or_else(|e| e.into_inner())` held for one
// statement.
func (m Mutex[T]) WithInner(f func(v *T)) {
	var guard MutexGuard[T]
	m.acquire(&guard, lockReport, "", 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// IsPoisoned is Mutex::is_poisoned.
func (m Mutex[T]) IsPoisoned() bool {
	if m.state == nil {
		panicUninitialised("Mutex", 1)
	}
	return m.state.poison.get()
}

// ClearPoison is Mutex::clear_poison.
func (m Mutex[T]) ClearPoison() {
	if m.state == nil {
		panicUninitialised("Mutex", 1)
	}
	m.state.poison.clear()
}

// IntoInner is `m.into_inner().unwrap()`: the value, for the owner of a
// mutex nobody else can reach any more. It panics when the mutex is
// poisoned.
func (m Mutex[T]) IntoInner() T {
	if m.state == nil {
		panicUninitialised("Mutex", 1)
	}
	if m.state.poison.get() {
		rs.PanicAt(1, unwrapPoisoned)
	}
	return m.state.data
}

// IntoInnerResult is `m.into_inner()`: the value, and whether the mutex is
// poisoned.
func (m Mutex[T]) IntoInnerResult() (value T, poisoned bool) {
	if m.state == nil {
		panicUninitialised("Mutex", 1)
	}
	return m.state.data, m.state.poison.get()
}

// GetMut is `m.get_mut().unwrap()`: the value, without locking, for code
// that has the mutex to itself. It panics when the mutex is poisoned.
func (m Mutex[T]) GetMut() *T {
	if m.state == nil {
		panicUninitialised("Mutex", 1)
	}
	if m.state.poison.get() {
		rs.PanicAt(1, unwrapPoisoned)
	}
	return &m.state.data
}

// GetMutResult is `m.get_mut()`: the value, and whether the mutex is
// poisoned.
func (m Mutex[T]) GetMutResult() (value *T, poisoned bool) {
	if m.state == nil {
		panicUninitialised("Mutex", 1)
	}
	return &m.state.data, m.state.poison.get()
}

// FmtDebug is the Debug impl: `Mutex { data: 5, poisoned: false, .. }`, with
// "<locked>" for the data of a mutex that is held.
func (m Mutex[T]) FmtDebug(f *rs.Formatter) error {
	d := f.DebugStruct("Mutex")
	guard, err := m.TryLock()
	defer guard.Drop()
	if guard != nil {
		d.Field("data", *guard.Get())
	} else {
		_ = err
		d.Field("data", "<locked>")
	}
	d.Field("poisoned", m.state.poison.get())
	return d.FinishNonExhaustive()
}

// Get returns the value the mutex protects. It is the guard's Deref and
// DerefMut: `*guard = x` is `*guard.Get() = x`. The pointer must not be used
// after Drop.
func (g *MutexGuard[T]) Get() *T {
	if g.lock == nil {
		rs.PanicAt(1, "rssync: use of a MutexGuard after Drop")
	}
	return &g.lock.data
}

// Deref is Get under the name the translation rules give `impl Deref`.
func (g *MutexGuard[T]) Deref() *T {
	if g.lock == nil {
		rs.PanicAt(1, "rssync: use of a MutexGuard after Drop")
	}
	return &g.lock.data
}

// Drop unlocks the mutex. It is idempotent, and does nothing on a nil guard.
//
// When Drop runs as a deferred call while its goroutine panics, it poisons
// the mutex before unlocking it, and the panic goes on.
func (g *MutexGuard[T]) Drop() {
	if g == nil || g.lock == nil {
		return
	}
	lock := g.lock
	// See "How poisoning is detected" in poison.go.
	if recovered := recover(); recovered != nil {
		g.lock = nil
		lock.poison.set()
		lock.inner.Unlock()
		panic(recovered)
	}
	g.lock = nil
	lock.inner.Unlock()
}

// DropPanicking drops the guard the way Rust does on a panicking thread: it
// poisons the mutex and unlocks it. It is for the Drop of a type that owns a
// guard, where the guard's own Drop cannot see the panic:
//
//	func (g *RelayGuard) Drop() {
//		if recovered := recover(); recovered != nil {
//			g.guard.DropPanicking()
//			panic(recovered)
//		}
//		g.guard.Drop()
//	}
func (g *MutexGuard[T]) DropPanicking() {
	if g == nil || g.lock == nil {
		return
	}
	lock := g.lock
	g.lock = nil
	lock.poison.set()
	lock.inner.Unlock()
}

// FmtDebug is the Debug impl, which is the value's.
func (g *MutexGuard[T]) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(*g.Get(), f) }

// FmtDisplay is the Display impl, which is the value's.
func (g *MutexGuard[T]) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(*g.Get(), f) }

// condvarLock gives a Condvar the lock this guard holds.
func (g *MutexGuard[T]) condvarLock() (*sync.Mutex, *poisonFlag) {
	if g == nil || g.lock == nil {
		return nil, nil
	}
	return &g.lock.inner, &g.lock.poison
}

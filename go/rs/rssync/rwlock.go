// Port of std 1.99.1 src/sync/poison/rwlock.rs

package rssync

import (
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// rwLockInner is the lock of an RwLock: any number of readers or one writer.
// Like Rust's, it prefers writers: a reader waits while a writer is waiting,
// so a stream of readers cannot starve a writer. (Go's sync.RWMutex cannot
// turn a write lock into a read lock without letting another writer in, which
// RwLockWriteGuardDowngrade must not do.)
type rwLockInner struct {
	mu             sync.Mutex
	changed        sync.Cond
	readers        int
	writer         bool
	writersWaiting int
}

func (l *rwLockInner) read() {
	l.mu.Lock()
	for l.writer || l.writersWaiting > 0 {
		l.changed.Wait()
	}
	l.readers++
	l.mu.Unlock()
}

func (l *rwLockInner) tryRead() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.writer || l.writersWaiting > 0 {
		return false
	}
	l.readers++
	return true
}

func (l *rwLockInner) write() {
	l.mu.Lock()
	l.writersWaiting++
	for l.writer || l.readers > 0 {
		l.changed.Wait()
	}
	l.writersWaiting--
	l.writer = true
	l.mu.Unlock()
}

func (l *rwLockInner) tryWrite() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.writer || l.readers > 0 {
		return false
	}
	l.writer = true
	return true
}

func (l *rwLockInner) readUnlock() {
	l.mu.Lock()
	l.readers--
	if l.readers == 0 {
		l.changed.Broadcast()
	}
	l.mu.Unlock()
}

func (l *rwLockInner) writeUnlock() {
	l.mu.Lock()
	l.writer = false
	l.changed.Broadcast()
	l.mu.Unlock()
}

// downgrade turns the write lock into a read lock in one step.
func (l *rwLockInner) downgrade() {
	l.mu.Lock()
	l.writer = false
	l.readers = 1
	l.changed.Broadcast()
	l.mu.Unlock()
}

type rwLockState[T any] struct {
	inner  rwLockInner
	poison poisonFlag
	data   T
}

// RwLock is std::sync::RwLock<T>: a reader-writer lock that owns the value
// it protects. It allows any number of readers or one writer at a time.
//
// Like Mutex, an RwLock is a small handle: copies are the same lock, and the
// zero value is not a lock (create one with RwLockNew). It is poisoned only
// by a panic while it is locked for writing; a poisoned lock fails readers
// too. Its methods follow Mutex's:
//
//	l.read().unwrap()                              l.Read()
//	l.read().expect("..")                          l.ReadExpect("..")
//	l.read().unwrap_or_else(|e| e.into_inner())    l.ReadInner()
//	match l.read() { .. }                          g, poisoned := l.ReadResult()
//	l.try_read()                                   g, err := l.TryRead()
//
// and the same with Write.
type RwLock[T any] struct {
	state *rwLockState[T]
}

// RwLockNew is RwLock::new: an unlocked lock holding t.
func RwLockNew[T any](t T) RwLock[T] {
	state := &rwLockState[T]{data: t}
	state.inner.changed.L = &state.inner.mu
	return RwLock[T]{state: state}
}

// RwLockFrom is `impl From<T> for RwLock<T>`, which is RwLockNew.
func RwLockFrom[T any](t T) RwLock[T] { return RwLockNew(t) }

// RwLockDefault is `impl Default for RwLock<T>`: a lock holding T's Default.
func RwLockDefault[T any]() RwLock[T] { return RwLockNew(rs.Default[T]()) }

// ReflectSetDefault makes rs.Default build a real lock for an RwLock field.
func (l *RwLock[T]) ReflectSetDefault() { *l = RwLockDefault[T]() }

// RwLockReadGuard is std::sync::RwLockReadGuard: shared access to the value
// until Drop. The value must not be changed through it.
type RwLockReadGuard[T any] struct {
	// lock is the lock, and nil once the guard was dropped.
	lock *rwLockState[T]
}

// RwLockWriteGuard is std::sync::RwLockWriteGuard: exclusive access to the
// value until Drop. Take it with `defer guard.Drop()`, which is also what
// poisons the lock when the function panics (see MutexGuard).
type RwLockWriteGuard[T any] struct {
	// lock is the lock, and nil once the guard was dropped.
	lock *rwLockState[T]
}

func (l RwLock[T]) acquireRead(guard *RwLockReadGuard[T], mode lockMode, message string, skip int) bool {
	state := l.state
	if state == nil {
		panicUninitialised("RwLock", skip+1)
	}
	state.inner.read()
	if state.poison.get() {
		if mode != lockReport {
			state.inner.readUnlock()
			panicPoisoned(mode, message, skip+1)
		}
		guard.lock = state
		return true
	}
	guard.lock = state
	return false
}

func (l RwLock[T]) acquireWrite(guard *RwLockWriteGuard[T], mode lockMode, message string, skip int) bool {
	state := l.state
	if state == nil {
		panicUninitialised("RwLock", skip+1)
	}
	state.inner.write()
	if state.poison.get() {
		if mode != lockReport {
			state.inner.writeUnlock()
			panicPoisoned(mode, message, skip+1)
		}
		guard.lock = state
		return true
	}
	guard.lock = state
	return false
}

// Read is `l.read().unwrap()`: it blocks until the lock can be shared and
// returns a read guard. It panics when the lock is poisoned.
func (l RwLock[T]) Read() *RwLockReadGuard[T] {
	guard := &RwLockReadGuard[T]{}
	l.acquireRead(guard, lockUnwrap, "", 1)
	return guard
}

// ReadExpect is `l.read().expect(message)`.
func (l RwLock[T]) ReadExpect(message string) *RwLockReadGuard[T] {
	guard := &RwLockReadGuard[T]{}
	l.acquireRead(guard, lockExpect, message, 1)
	return guard
}

// ReadInner is `l.read().unwrap_or_else(|e| e.into_inner())`.
func (l RwLock[T]) ReadInner() *RwLockReadGuard[T] {
	guard := &RwLockReadGuard[T]{}
	l.acquireRead(guard, lockReport, "", 1)
	return guard
}

// ReadResult is `l.read()` for code that looks at the result: the guard,
// which holds the lock either way, and whether the lock is poisoned.
func (l RwLock[T]) ReadResult() (guard *RwLockReadGuard[T], poisoned bool) {
	guard = &RwLockReadGuard[T]{}
	poisoned = l.acquireRead(guard, lockReport, "", 1)
	return guard, poisoned
}

// TryRead is `l.try_read()`. The error is nil, a
// *TryLockError_WouldBlock[*RwLockReadGuard[T]] (the guard is then nil), or
// a *TryLockError_Poisoned[*RwLockReadGuard[T]], in which case the lock is
// held and the guard is returned as well as carried in the error.
func (l RwLock[T]) TryRead() (*RwLockReadGuard[T], error) {
	state := l.state
	if state == nil {
		panicUninitialised("RwLock", 1)
	}
	if !state.inner.tryRead() {
		return nil, &TryLockError_WouldBlock[*RwLockReadGuard[T]]{}
	}
	guard := &RwLockReadGuard[T]{lock: state}
	if state.poison.get() {
		return guard, &TryLockError_Poisoned[*RwLockReadGuard[T]]{F0: PoisonErrorNew(guard)}
	}
	return guard, nil
}

// Write is `l.write().unwrap()`: it blocks until the lock is exclusive and
// returns a write guard. It panics when the lock is poisoned.
func (l RwLock[T]) Write() *RwLockWriteGuard[T] {
	guard := &RwLockWriteGuard[T]{}
	l.acquireWrite(guard, lockUnwrap, "", 1)
	return guard
}

// WriteExpect is `l.write().expect(message)`.
func (l RwLock[T]) WriteExpect(message string) *RwLockWriteGuard[T] {
	guard := &RwLockWriteGuard[T]{}
	l.acquireWrite(guard, lockExpect, message, 1)
	return guard
}

// WriteInner is `l.write().unwrap_or_else(|e| e.into_inner())`.
func (l RwLock[T]) WriteInner() *RwLockWriteGuard[T] {
	guard := &RwLockWriteGuard[T]{}
	l.acquireWrite(guard, lockReport, "", 1)
	return guard
}

// WriteResult is `l.write()` for code that looks at the result: the guard,
// which holds the lock either way, and whether the lock is poisoned.
func (l RwLock[T]) WriteResult() (guard *RwLockWriteGuard[T], poisoned bool) {
	guard = &RwLockWriteGuard[T]{}
	poisoned = l.acquireWrite(guard, lockReport, "", 1)
	return guard, poisoned
}

// TryWrite is `l.try_write()`, with TryRead's three outcomes.
func (l RwLock[T]) TryWrite() (*RwLockWriteGuard[T], error) {
	state := l.state
	if state == nil {
		panicUninitialised("RwLock", 1)
	}
	if !state.inner.tryWrite() {
		return nil, &TryLockError_WouldBlock[*RwLockWriteGuard[T]]{}
	}
	guard := &RwLockWriteGuard[T]{lock: state}
	if state.poison.get() {
		return guard, &TryLockError_Poisoned[*RwLockWriteGuard[T]]{F0: PoisonErrorNew(guard)}
	}
	return guard, nil
}

// WithRead is `l.read().unwrap()` held for one statement.
func (l RwLock[T]) WithRead(f func(v *T)) {
	var guard RwLockReadGuard[T]
	l.acquireRead(&guard, lockUnwrap, "", 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// WithReadInner is `l.read().unwrap_or_else(|e| e.into_inner())` held for
// one statement.
func (l RwLock[T]) WithReadInner(f func(v *T)) {
	var guard RwLockReadGuard[T]
	l.acquireRead(&guard, lockReport, "", 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// WithWrite is `l.write().unwrap()` held for one statement. A panic in f
// poisons the lock.
func (l RwLock[T]) WithWrite(f func(v *T)) {
	var guard RwLockWriteGuard[T]
	l.acquireWrite(&guard, lockUnwrap, "", 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// WithWriteInner is `l.write().unwrap_or_else(|e| e.into_inner())` held for
// one statement.
func (l RwLock[T]) WithWriteInner(f func(v *T)) {
	var guard RwLockWriteGuard[T]
	l.acquireWrite(&guard, lockReport, "", 1)
	defer guard.Drop()
	f(&guard.lock.data)
}

// IsPoisoned is RwLock::is_poisoned.
func (l RwLock[T]) IsPoisoned() bool {
	if l.state == nil {
		panicUninitialised("RwLock", 1)
	}
	return l.state.poison.get()
}

// ClearPoison is RwLock::clear_poison.
func (l RwLock[T]) ClearPoison() {
	if l.state == nil {
		panicUninitialised("RwLock", 1)
	}
	l.state.poison.clear()
}

// IntoInner is `l.into_inner().unwrap()`: the value, for the owner of a lock
// nobody else can reach any more. It panics when the lock is poisoned.
func (l RwLock[T]) IntoInner() T {
	if l.state == nil {
		panicUninitialised("RwLock", 1)
	}
	if l.state.poison.get() {
		rs.PanicAt(1, unwrapPoisoned)
	}
	return l.state.data
}

// IntoInnerResult is `l.into_inner()`: the value, and whether the lock is
// poisoned.
func (l RwLock[T]) IntoInnerResult() (value T, poisoned bool) {
	if l.state == nil {
		panicUninitialised("RwLock", 1)
	}
	return l.state.data, l.state.poison.get()
}

// GetMut is `l.get_mut().unwrap()`: the value, without locking, for code
// that has the lock to itself. It panics when the lock is poisoned.
func (l RwLock[T]) GetMut() *T {
	if l.state == nil {
		panicUninitialised("RwLock", 1)
	}
	if l.state.poison.get() {
		rs.PanicAt(1, unwrapPoisoned)
	}
	return &l.state.data
}

// GetMutResult is `l.get_mut()`: the value, and whether the lock is
// poisoned.
func (l RwLock[T]) GetMutResult() (value *T, poisoned bool) {
	if l.state == nil {
		panicUninitialised("RwLock", 1)
	}
	return &l.state.data, l.state.poison.get()
}

// FmtDebug is the Debug impl: `RwLock { data: 5, poisoned: false, .. }`, with
// <locked> for the data of a lock that is held for writing.
func (l RwLock[T]) FmtDebug(f *rs.Formatter) error {
	d := f.DebugStruct("RwLock")
	guard, _ := l.TryRead()
	defer guard.Drop()
	if guard != nil {
		d.Field("data", *guard.Get())
	} else {
		d.Field("data", rawDebug("<locked>"))
	}
	d.Field("poisoned", l.state.poison.get())
	return d.FinishNonExhaustive()
}

// Get returns the value the lock protects, for reading. The pointer must not
// be used after Drop.
func (g *RwLockReadGuard[T]) Get() *T {
	if g.lock == nil {
		rs.PanicAt(1, "rssync: use of an RwLockReadGuard after Drop")
	}
	return &g.lock.data
}

// Deref is Get under the name the translation rules give `impl Deref`.
func (g *RwLockReadGuard[T]) Deref() *T {
	if g.lock == nil {
		rs.PanicAt(1, "rssync: use of an RwLockReadGuard after Drop")
	}
	return &g.lock.data
}

// Drop releases the read lock. It is idempotent, and does nothing on a nil
// guard. A reader never poisons the lock.
func (g *RwLockReadGuard[T]) Drop() {
	if g == nil || g.lock == nil {
		return
	}
	lock := g.lock
	g.lock = nil
	lock.inner.readUnlock()
}

// FmtDebug is the Debug impl, which is the value's.
func (g *RwLockReadGuard[T]) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(*g.Get(), f) }

// FmtDisplay is the Display impl, which is the value's.
func (g *RwLockReadGuard[T]) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(*g.Get(), f) }

// Get returns the value the lock protects. `*guard = x` is
// `*guard.Get() = x`. The pointer must not be used after Drop.
func (g *RwLockWriteGuard[T]) Get() *T {
	if g.lock == nil {
		rs.PanicAt(1, "rssync: use of an RwLockWriteGuard after Drop")
	}
	return &g.lock.data
}

// Deref is Get under the name the translation rules give `impl Deref`.
func (g *RwLockWriteGuard[T]) Deref() *T {
	if g.lock == nil {
		rs.PanicAt(1, "rssync: use of an RwLockWriteGuard after Drop")
	}
	return &g.lock.data
}

// Drop releases the write lock. It is idempotent, and does nothing on a nil
// guard. When it runs as a deferred call while its goroutine panics, it
// poisons the lock first, and the panic goes on.
func (g *RwLockWriteGuard[T]) Drop() {
	if g == nil || g.lock == nil {
		return
	}
	lock := g.lock
	// See "How poisoning is detected" in poison.go.
	if recovered := recover(); recovered != nil {
		g.lock = nil
		lock.poison.set()
		lock.inner.writeUnlock()
		panic(recovered)
	}
	g.lock = nil
	lock.inner.writeUnlock()
}

// DropPanicking drops the guard the way Rust does on a panicking thread: it
// poisons the lock and releases it. See MutexGuard.DropPanicking.
func (g *RwLockWriteGuard[T]) DropPanicking() {
	if g == nil || g.lock == nil {
		return
	}
	lock := g.lock
	g.lock = nil
	lock.poison.set()
	lock.inner.writeUnlock()
}

// FmtDebug is the Debug impl, which is the value's.
func (g *RwLockWriteGuard[T]) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(*g.Get(), f) }

// FmtDisplay is the Display impl, which is the value's.
func (g *RwLockWriteGuard[T]) FmtDisplay(f *rs.Formatter) error {
	return rs.DisplayFmt(*g.Get(), f)
}

// RwLockWriteGuardDowngrade is RwLockWriteGuard::downgrade: it turns the
// write guard into a read guard without letting another writer in between.
// The write guard is spent; its Drop does nothing afterwards. Like Rust's,
// the read guard is returned whether or not the lock is poisoned.
func RwLockWriteGuardDowngrade[T any](guard *RwLockWriteGuard[T]) *RwLockReadGuard[T] {
	if guard.lock == nil {
		rs.PanicAt(1, "rssync: use of an RwLockWriteGuard after Drop")
	}
	lock := guard.lock
	// We don't want to call the destructor since that calls `write_unlock`.
	guard.lock = nil
	lock.inner.downgrade()
	return &RwLockReadGuard[T]{lock: lock}
}

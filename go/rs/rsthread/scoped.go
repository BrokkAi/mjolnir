// Port of std 1.99.1 src/thread/scoped.rs

package rsthread

import (
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ScopeHandle is std::thread::Scope: what the function given to Scope
// receives, to spawn threads that may borrow from the enclosing function.
// (The function std::thread::scope has the name Scope here, so the type has
// a suffix, as rsio's types do where a function holds the name.)
type ScopeHandle struct {
	data *scopeData
}

// ScopedJoinHandle is std::thread::ScopedJoinHandle<'scope, T>: permission
// to join a thread spawned in a scope. A thread nobody joins is joined when
// the scope ends.
type ScopedJoinHandle[T any] struct {
	inner *joinInner[T]
}

// scopeData is ScopeData.
type scopeData struct {
	mu sync.Mutex
	// numRunningThreads is Rust's num_running_threads: the packets of the
	// scope's threads that have not been dropped, which is when both the
	// thread has ended and its handle has been joined or dropped.
	numRunningThreads int
	// aThreadPanicked is set when a packet is dropped with a panic nobody
	// took.
	aThreadPanicked bool
	// threadsRunning counts the threads that have not let go of their
	// packet yet, and changed is closed when it drops to zero while the
	// scope is waiting for that.
	threadsRunning int
	changed        chan struct{}
	// handles are the packets whose handle may not have been released yet.
	handles []interface{ releaseHandle() bool }
	// mainThread is the thread that called Scope.
	mainThread Thread
}

func (s *scopeData) incrementNumRunningThreads(p interface{ releaseHandle() bool }) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.numRunningThreads++
	s.threadsRunning++
	s.handles = append(s.handles, p)
}

func (s *scopeData) decrementNumRunningThreads(panic bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if panic {
		s.aThreadPanicked = true
	}
	s.numRunningThreads--
}

// threadEnded is called by a scoped thread once it has let go of its
// packet.
func (s *scopeData) threadEnded() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.threadsRunning--
	if s.threadsRunning == 0 && s.changed != nil {
		close(s.changed)
		s.changed = nil
	}
}

// waitThreads blocks until no thread of the scope is running.
func (s *scopeData) waitThreads() {
	s.mu.Lock()
	if s.threadsRunning == 0 {
		s.mu.Unlock()
		return
	}
	changed := make(chan struct{})
	s.changed = changed
	s.mu.Unlock()
	<-changed
}

// waitAll is the end of the scope: Rust's wait until num_running_threads is
// zero.
//
// Rust drops every ScopedJoinHandle before that, because none can outlive
// the scope's function or the scoped thread that holds it. Go cannot see a
// handle go away, so once the function has returned and every thread has
// ended (no code is left that could join a handle), the scope releases the
// handles nobody joined or dropped, as Rust would already have. That drops
// their results, whose destructors may spawn more scoped threads, so it
// repeats until nothing is left.
func (s *scopeData) waitAll() {
	for {
		s.waitThreads()
		s.mu.Lock()
		pending := s.handles
		s.handles = nil
		s.mu.Unlock()
		for _, handle := range pending {
			handle.releaseHandle()
		}
		s.mu.Lock()
		done := s.threadsRunning == 0 && len(s.handles) == 0
		s.mu.Unlock()
		if done {
			return
		}
	}
}

// Scope is std::thread::scope: it calls f with a ScopeHandle to spawn
// threads with, waits for every one of them to end, and returns f's result.
//
// If f panics, Scope waits for the threads and then continues the panic. If
// a thread panicked and nobody joined it, Scope panics with "a scoped thread
// panicked" once all have ended, attributed to Scope's caller as Rust's
// #[track_caller] does.
func Scope[T any](f func(scope *ScopeHandle) T) (result T) {
	// We put the `ScopeData` into an `Arc` so that other threads can finish their
	// `decrement_num_running_threads` even after this function returns.
	scope := &ScopeHandle{data: &scopeData{mainThread: currentOrUnnamed()}}

	completed := false
	defer func() {
		if completed {
			return
		}
		// f panicked, or its goroutine is ending through runtime.Goexit:
		// wait for the threads before the panic or the exit goes on.
		if recovered := recover(); recovered != nil {
			caught := rs.PanicInfoOf(recovered)
			rs.RunPanicHook(caught, scope.data.mainThread.hookName())
			// Wait until all the threads are finished.
			scope.data.waitAll()
			// Throw any panic from `f`.
			rs.ResumeUnwind(caught)
		}
		scope.data.waitAll()
	}()
	result = f(scope)
	completed = true

	// Wait until all the threads are finished.
	scope.data.waitAll()

	s := scope.data
	s.mu.Lock()
	panicked := s.aThreadPanicked
	s.mu.Unlock()
	if panicked {
		rs.PanicAt(1, "a scoped thread panicked")
	}
	return result
}

// ScopeSpawn is Scope::spawn: it starts f on a new thread of the scope.
func ScopeSpawn[T any](s *ScopeHandle, f func() T) ScopedJoinHandle[T] {
	handle, _ := BuilderSpawnScoped(BuilderNew(), s, f)
	return handle
}

// BuilderSpawnScoped is Builder::spawn_scoped. The error is always nil, as
// for BuilderSpawn.
func BuilderSpawnScoped[T any](b Builder, s *ScopeHandle, f func() T) (ScopedJoinHandle[T], error) {
	return ScopedJoinHandle[T]{inner: spawnUnchecked(b.name, s.data, f)}, nil
}

// Thread is ScopedJoinHandle::thread.
func (h ScopedJoinHandle[T]) Thread() *Thread { return &h.inner.thread }

// Join is ScopedJoinHandle::join: it waits for the thread to end and returns
// its result or its panic. A panic taken here does not make the scope
// panic. See JoinHandle.Join.
func (h ScopedJoinHandle[T]) Join() (T, *rs.PanicInfo) { return h.inner.join(1) }

// JoinUnwrap is `handle.join().unwrap()`.
func (h ScopedJoinHandle[T]) JoinUnwrap() T {
	result, caught := h.inner.join(1)
	if caught != nil {
		rs.PanicAt(1, joinUnwrapPanic)
	}
	return result
}

// JoinExpect is `handle.join().expect(message)`.
func (h ScopedJoinHandle[T]) JoinExpect(message string) T {
	result, caught := h.inner.join(1)
	if caught != nil {
		rs.PanicAt(1, message+joinExpectPanic)
	}
	return result
}

// IsFinished is ScopedJoinHandle::is_finished.
func (h ScopedJoinHandle[T]) IsFinished() bool { return h.inner.isFinished() }

// Drop is dropping the handle without joining: the thread's result is
// dropped when it ends, and a panic in it makes the scope panic. It is
// idempotent, and does nothing after Join. A handle that is never dropped is
// released when the scope ends.
func (h ScopedJoinHandle[T]) Drop() { h.inner.packet.releaseHandle() }

// FmtDebug is the Debug impl:
// `Scope { num_running_threads: 0, a_thread_panicked: false, main_thread: Thread { .. }, .. }`.
func (s *ScopeHandle) FmtDebug(f *rs.Formatter) error {
	s.data.mu.Lock()
	running := s.data.numRunningThreads
	panicked := s.data.aThreadPanicked
	s.data.mu.Unlock()
	return f.DebugStruct("Scope").
		Field("num_running_threads", running).
		Field("a_thread_panicked", panicked).
		Field("main_thread", s.data.mainThread).
		FinishNonExhaustive()
}

// FmtDebug is the Debug impl: `ScopedJoinHandle { .. }`.
func (h ScopedJoinHandle[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ScopedJoinHandle").FinishNonExhaustive()
}

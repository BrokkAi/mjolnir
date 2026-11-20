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
	// numRunningThreads counts the threads whose function has not ended.
	numRunningThreads int
	// done is closed when numRunningThreads drops to zero while the scope
	// is waiting for it.
	done chan struct{}
	// packets are the scope's threads, to find a panic nobody joined.
	packets []interface{ unhandledPanic() bool }
	// mainThread is the thread that called Scope.
	mainThread Thread
}

func (s *scopeData) incrementNumRunningThreads(p interface{ unhandledPanic() bool }) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.numRunningThreads++
	s.packets = append(s.packets, p)
}

func (s *scopeData) decrementNumRunningThreads() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.numRunningThreads--
	if s.numRunningThreads == 0 && s.done != nil {
		close(s.done)
		s.done = nil
	}
}

// wait blocks until every thread of the scope has ended.
func (s *scopeData) wait() {
	s.mu.Lock()
	if s.numRunningThreads == 0 {
		s.mu.Unlock()
		return
	}
	done := make(chan struct{})
	s.done = done
	s.mu.Unlock()
	<-done
}

// aThreadPanicked is a_thread_panicked, once every thread has ended: Rust
// sets it as each thread's packet is dropped with a panic in it, which is a
// panic its handle never took with join.
func (s *scopeData) aThreadPanicked() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, p := range s.packets {
		if p.unhandledPanic() {
			return true
		}
	}
	return false
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
			scope.data.wait()
			// Throw any panic from `f`.
			rs.ResumeUnwind(caught)
		}
		scope.data.wait()
	}()
	result = f(scope)
	completed = true

	// Wait until all the threads are finished.
	scope.data.wait()

	if scope.data.aThreadPanicked() {
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

// Drop is dropping the handle without joining. The scope still waits for
// the thread, and panics if it panicked.
func (h ScopedJoinHandle[T]) Drop() {}

// FmtDebug is the Debug impl:
// `Scope { num_running_threads: 0, a_thread_panicked: false, main_thread: Thread { .. }, .. }`.
func (s *ScopeHandle) FmtDebug(f *rs.Formatter) error {
	s.data.mu.Lock()
	running := s.data.numRunningThreads
	panicked := false
	for _, p := range s.data.packets {
		if p.unhandledPanic() {
			panicked = true
		}
	}
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

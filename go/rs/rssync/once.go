// Port of std 1.99.1 src/sync/once.rs and src/sys/sync/once/futex.rs

package rssync

import (
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The states of a Once.
const (
	// No initialization has run yet, and no thread is currently using the Once.
	onceIncomplete uint32 = iota
	// Some thread is currently attempting to run initialization. It may succeed,
	// so all future threads need to wait for it to finish.
	onceRunning
	// Some thread has previously attempted to initialize the Once, but it panicked,
	// so the Once is now poisoned. There are no other threads currently accessing
	// this Once.
	oncePoisoned
	// Initialization has completed and all future calls should finish immediately.
	onceComplete
)

// onceInner is sys::Once. Its zero value is a new Once.
type onceInner struct {
	// state is read without the lock on the fast path and changed under it.
	state atomic.Uint32
	mu    sync.Mutex
	// finished wakes the threads that wait for the running one.
	finished *sync.Cond
}

func (o *onceInner) isCompleted() bool { return o.state.Load() == onceComplete }

// waitLocked blocks until the running thread finishes. The lock is held on
// entry and on return.
func (o *onceInner) waitLocked() {
	if o.finished == nil {
		o.finished = sync.NewCond(&o.mu)
	}
	o.finished.Wait()
}

// finish is CompletionGuard's destructor: it stores the final state and wakes
// the waiting threads.
func (o *onceInner) finish(state uint32) {
	o.mu.Lock()
	o.state.Store(state)
	if o.finished != nil {
		o.finished.Broadcast()
	}
	o.mu.Unlock()
}

// call is sys::Once::call. A panic for a poisoned Once is attributed to the
// caller skip frames above the caller of call.
func (o *onceInner) call(ignorePoisoning bool, f func(state *OnceState), skip int) {
	o.mu.Lock()
	for {
		state := o.state.Load()
		switch state {
		case onceComplete:
			o.mu.Unlock()
			return
		case onceRunning:
			o.waitLocked()
			continue
		}
		// INCOMPLETE or POISONED.
		if state == oncePoisoned && !ignorePoisoning {
			o.mu.Unlock()
			// Panic to propagate the poison.
			rs.PanicAt(skip+1, "Once instance has previously been poisoned")
		}
		// Register the current thread as the one running.
		o.state.Store(onceRunning)
		o.mu.Unlock()
		break
	}

	// Run the function, letting it know if we're poisoned or not. Unless it
	// returns, the Once is left poisoned.
	fState := OnceState{poisoned: o.state.Load() == oncePoisoned, setStateTo: onceComplete}
	final := oncePoisoned
	defer func() { o.finish(final) }()
	f(&fState)
	final = fState.setStateTo
}

// wait is sys::Once::wait.
func (o *onceInner) wait(ignorePoisoning bool, skip int) {
	o.mu.Lock()
	for {
		switch o.state.Load() {
		case onceComplete:
			o.mu.Unlock()
			return
		case oncePoisoned:
			if !ignorePoisoning {
				o.mu.Unlock()
				// Panic to propagate the poison.
				rs.PanicAt(skip+1, "Once instance has previously been poisoned")
			}
		}
		o.waitLocked()
	}
}

// reset is `self.once = Once::new()`, for an owner that has the Once to
// itself.
func (o *onceInner) reset() {
	o.mu.Lock()
	o.state.Store(onceIncomplete)
	o.mu.Unlock()
}

// Once is std::sync::Once: a way to run a one-time initialization, however
// many threads ask for it.
//
// A Once is a small handle: copies are the same Once, and the zero value is
// not one (create one with OnceNew).
type Once struct {
	inner *onceInner
}

// OnceNew is Once::new.
func OnceNew() Once {
	return Once{inner: &onceInner{}}
}

// OnceState is std::sync::OnceState: what CallOnceForce tells its function
// about the Once.
type OnceState struct {
	poisoned   bool
	setStateTo uint32
}

// IsPoisoned is OnceState::is_poisoned: whether an earlier call panicked.
func (s *OnceState) IsPoisoned() bool { return s.poisoned }

// poison is OnceState::poison: leave the Once poisoned although the function
// returned.
func (s *OnceState) poison() { s.setStateTo = oncePoisoned }

// FmtDebug is the Debug impl: `OnceState { poisoned: false }`.
func (s *OnceState) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("OnceState").Field("poisoned", s.poisoned).Finish()
}

func (o Once) must(skip int) *onceInner {
	if o.inner == nil {
		panicUninitialised("Once", skip+1)
	}
	return o.inner
}

// CallOnce is Once::call_once: it runs f if no call has completed yet, and
// otherwise returns once the running call, if any, has finished. If f
// panics the Once is poisoned, and every later CallOnce panics.
func (o Once) CallOnce(f func()) {
	inner := o.must(1)
	// Fast path check
	if inner.isCompleted() {
		return
	}
	inner.call(false, func(*OnceState) { f() }, 1)
}

// CallOnceForce is Once::call_once_force: CallOnce that also runs f when the
// Once is poisoned, telling it so.
func (o Once) CallOnceForce(f func(state *OnceState)) {
	inner := o.must(1)
	// Fast path check
	if inner.isCompleted() {
		return
	}
	inner.call(true, f, 1)
}

// IsCompleted is Once::is_completed.
func (o Once) IsCompleted() bool { return o.must(1).isCompleted() }

// Wait is Once::wait: it blocks until a call has completed. It panics when
// the Once is poisoned.
func (o Once) Wait() {
	inner := o.must(1)
	if !inner.isCompleted() {
		inner.wait(false, 1)
	}
}

// WaitForce is Once::wait_force: Wait that keeps waiting while the Once is
// poisoned.
func (o Once) WaitForce() {
	inner := o.must(1)
	if !inner.isCompleted() {
		inner.wait(true, 1)
	}
}

// FmtDebug is the Debug impl: `Once { .. }`.
func (o Once) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Once").FinishNonExhaustive()
}

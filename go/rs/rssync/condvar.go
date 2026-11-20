// Port of std 1.99.1 src/sync/poison/condvar.rs, and WaitTimeoutResult from
// src/sync/mod.rs

package rssync

import (
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// WaitTimeoutResult is std::sync::WaitTimeoutResult: whether a timed wait on
// a condition variable returned because the time ran out.
type WaitTimeoutResult bool

// TimedOut is WaitTimeoutResult::timed_out.
func (r WaitTimeoutResult) TimedOut() bool { return bool(r) }

// FmtDebug is the derived Debug impl: `WaitTimeoutResult(true)`.
func (r WaitTimeoutResult) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("WaitTimeoutResult").Field(bool(r)).Finish()
}

// condvarWaiter is one thread blocked in a wait.
type condvarWaiter struct {
	// wake receives the notification. It holds one, so a notifier never
	// blocks.
	wake chan struct{}
}

type condvarState struct {
	mu sync.Mutex
	// waiters are the blocked threads, the longest waiting first.
	waiters []*condvarWaiter
	// mutex is the lock the condition variable was first used with. Rust
	// keeps it, and checks it, where condition variables are pthread ones.
	mutex atomic.Pointer[sync.Mutex]
}

// remove takes waiter out of the queue and reports whether it was there. A
// waiter that is not there has been notified.
func (s *condvarState) remove(waiter *condvarWaiter) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i, queued := range s.waiters {
		if queued == waiter {
			s.waiters = append(s.waiters[:i], s.waiters[i+1:]...)
			return true
		}
	}
	return false
}

// Condvar is std::sync::Condvar: a condition variable, which blocks a thread
// until another notifies it, releasing a mutex while it waits.
//
// A Condvar is a small handle: copies are the same condition variable, and
// the zero value is not one (create one with CondvarNew).
//
// A wait takes the guard and, in Rust, returns it. Here the guard is the same
// object before and after: the mutex is released and locked again inside it,
// and a deferred Drop on it stays right.
//
//	guard = cvar.wait(guard).unwrap();                    cvar.Wait(guard)
//	guard = cvar.wait(guard).unwrap_or_else(into_inner);  cvar.WaitInner(guard)
//	(guard, result) = cvar.wait_timeout(guard, d).unwrap();
//	                                                      result := cvar.WaitTimeout(guard, d)
//
// As in Rust, a wait may return without a notification (here it does not, but
// code must not rely on that), and each wait reports the mutex's poison.
type Condvar struct {
	state *condvarState
}

// CondvarNew is Condvar::new.
func CondvarNew() Condvar {
	return Condvar{state: &condvarState{}}
}

// CondvarDefault is `impl Default for Condvar`.
func CondvarDefault() Condvar { return CondvarNew() }

// ReflectSetDefault makes rs.Default build a real condition variable for a
// Condvar field.
func (c *Condvar) ReflectSetDefault() { *c = CondvarNew() }

// CondvarGuard is what a Condvar waits with: a *MutexGuard[T] of any T.
type CondvarGuard interface {
	condvarLock() (*sync.Mutex, *poisonFlag)
	Drop()
}

// wait is Condvar::wait and wait_timeout: it releases the guard's mutex,
// blocks until notified or until timeout has passed (when timed), and locks
// the mutex again. It reports whether it was notified and whether the mutex
// is poisoned.
func (c Condvar) wait(guard CondvarGuard, timeout time.Duration, timed bool, skip int) (notified bool, poisoned bool) {
	state := c.state
	if state == nil {
		panicUninitialised("Condvar", skip+1)
	}
	inner, poison := guard.condvarLock()
	if inner == nil {
		rs.PanicAt(skip+1, "rssync: Condvar wait with a MutexGuard that was dropped")
	}
	if rs.CfgMacos {
		// Rust's condition variable is a pthread one on macOS, which may
		// only ever be used with one mutex.
		if !state.mutex.CompareAndSwap(nil, inner) && state.mutex.Load() != inner {
			rs.PanicAt(skip+1, "attempted to use a condition variable with two mutexes")
		}
	}

	waiter := &condvarWaiter{wake: make(chan struct{}, 1)}
	// Queue before unlocking, so that a notification sent once the mutex is
	// free finds this waiter.
	state.mu.Lock()
	state.waiters = append(state.waiters, waiter)
	state.mu.Unlock()
	inner.Unlock()

	if !timed {
		<-waiter.wake
		notified = true
	} else {
		timer := time.NewTimer(max(timeout, 0))
		select {
		case <-waiter.wake:
			timer.Stop()
			notified = true
		case <-timer.C:
			if state.remove(waiter) {
				notified = false
			} else {
				// A notifier chose this waiter as the time ran out: the
				// notification is taken, not lost.
				<-waiter.wake
				notified = true
			}
		}
	}

	inner.Lock()
	return notified, poison.get()
}

// Wait is `guard = c.wait(guard).unwrap()`: it blocks until the condition
// variable is notified. It panics, dropping the guard, when the mutex is
// poisoned.
func (c Condvar) Wait(guard CondvarGuard) {
	if _, poisoned := c.wait(guard, 0, false, 1); poisoned {
		guard.Drop()
		panicPoisoned(lockUnwrap, "", 1)
	}
}

// WaitExpect is `guard = c.wait(guard).expect(message)`.
func (c Condvar) WaitExpect(guard CondvarGuard, message string) {
	if _, poisoned := c.wait(guard, 0, false, 1); poisoned {
		guard.Drop()
		panicPoisoned(lockExpect, message, 1)
	}
}

// WaitInner is `guard = c.wait(guard).unwrap_or_else(|e| e.into_inner())`.
func (c Condvar) WaitInner(guard CondvarGuard) {
	c.wait(guard, 0, false, 1)
}

// WaitResult is `c.wait(guard)` for code that looks at the result: whether
// the mutex is poisoned. The guard holds the lock either way.
func (c Condvar) WaitResult(guard CondvarGuard) (poisoned bool) {
	_, poisoned = c.wait(guard, 0, false, 1)
	return poisoned
}

// WaitTimeout is `(guard, result) = c.wait_timeout(guard, dur).unwrap()`: it
// blocks until the condition variable is notified or dur has passed, and
// says which. It panics, dropping the guard, when the mutex is poisoned.
func (c Condvar) WaitTimeout(guard CondvarGuard, dur time.Duration) WaitTimeoutResult {
	notified, poisoned := c.wait(guard, dur, true, 1)
	if poisoned {
		guard.Drop()
		panicPoisoned(lockUnwrap, "", 1)
	}
	return WaitTimeoutResult(!notified)
}

// WaitTimeoutExpect is `c.wait_timeout(guard, dur).expect(message)`.
func (c Condvar) WaitTimeoutExpect(guard CondvarGuard, dur time.Duration, message string) WaitTimeoutResult {
	notified, poisoned := c.wait(guard, dur, true, 1)
	if poisoned {
		guard.Drop()
		panicPoisoned(lockExpect, message, 1)
	}
	return WaitTimeoutResult(!notified)
}

// WaitTimeoutInner is
// `c.wait_timeout(guard, dur).unwrap_or_else(|e| e.into_inner())`.
func (c Condvar) WaitTimeoutInner(guard CondvarGuard, dur time.Duration) WaitTimeoutResult {
	notified, _ := c.wait(guard, dur, true, 1)
	return WaitTimeoutResult(!notified)
}

// WaitTimeoutResult is `c.wait_timeout(guard, dur)` for code that looks at
// the result: whether the time ran out, and whether the mutex is poisoned.
func (c Condvar) WaitTimeoutResult(guard CondvarGuard, dur time.Duration) (result WaitTimeoutResult, poisoned bool) {
	notified, poisoned := c.wait(guard, dur, true, 1)
	return WaitTimeoutResult(!notified), poisoned
}

// NotifyOne is Condvar::notify_one: it wakes the thread that has waited
// longest, if any is waiting.
func (c Condvar) NotifyOne() {
	state := c.state
	if state == nil {
		panicUninitialised("Condvar", 1)
	}
	state.mu.Lock()
	var waiter *condvarWaiter
	if len(state.waiters) > 0 {
		waiter = state.waiters[0]
		state.waiters = state.waiters[1:]
	}
	state.mu.Unlock()
	if waiter != nil {
		waiter.wake <- struct{}{}
	}
}

// NotifyAll is Condvar::notify_all: it wakes every waiting thread.
func (c Condvar) NotifyAll() {
	state := c.state
	if state == nil {
		panicUninitialised("Condvar", 1)
	}
	state.mu.Lock()
	waiters := state.waiters
	state.waiters = nil
	state.mu.Unlock()
	for _, waiter := range waiters {
		waiter.wake <- struct{}{}
	}
}

// FmtDebug is the Debug impl: `Condvar { .. }`.
func (c Condvar) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Condvar").FinishNonExhaustive()
}

// CondvarWaitWhile is `guard = c.wait_while(guard, condition).unwrap()`: it
// waits for as long as condition holds. It panics, dropping the guard, when
// a wait finds the mutex poisoned.
func CondvarWaitWhile[T any](c Condvar, guard *MutexGuard[T], condition func(v *T) bool) {
	if condvarWaitWhile(c, guard, condition) {
		guard.Drop()
		panicPoisoned(lockUnwrap, "", 1)
	}
}

// CondvarWaitWhileResult is `c.wait_while(guard, condition)` for code that
// looks at the result (or takes the guard with into_inner): whether a wait
// found the mutex poisoned, in which case it returned at once, with the
// condition possibly still true.
func CondvarWaitWhileResult[T any](c Condvar, guard *MutexGuard[T], condition func(v *T) bool) (poisoned bool) {
	return condvarWaitWhile(c, guard, condition)
}

func condvarWaitWhile[T any](c Condvar, guard *MutexGuard[T], condition func(v *T) bool) bool {
	for condition(guard.Get()) {
		if _, poisoned := c.wait(guard, 0, false, 2); poisoned {
			return true
		}
	}
	return false
}

// CondvarWaitTimeoutWhile is
// `(guard, result) = c.wait_timeout_while(guard, dur, condition).unwrap()`:
// it waits for as long as condition holds, for at most dur, and says whether
// the time ran out with the condition still true. It panics, dropping the
// guard, when a wait finds the mutex poisoned.
func CondvarWaitTimeoutWhile[T any](c Condvar, guard *MutexGuard[T], dur time.Duration, condition func(v *T) bool) WaitTimeoutResult {
	result, poisoned := condvarWaitTimeoutWhile(c, guard, dur, condition)
	if poisoned {
		guard.Drop()
		panicPoisoned(lockUnwrap, "", 1)
	}
	return result
}

// CondvarWaitTimeoutWhileResult is `c.wait_timeout_while(guard, dur,
// condition)` for code that looks at the result: whether the time ran out,
// and whether a wait found the mutex poisoned.
func CondvarWaitTimeoutWhileResult[T any](c Condvar, guard *MutexGuard[T], dur time.Duration, condition func(v *T) bool) (result WaitTimeoutResult, poisoned bool) {
	return condvarWaitTimeoutWhile(c, guard, dur, condition)
}

func condvarWaitTimeoutWhile[T any](c Condvar, guard *MutexGuard[T], dur time.Duration, condition func(v *T) bool) (WaitTimeoutResult, bool) {
	start := time.Now()
	for {
		if !condition(guard.Get()) {
			return false, false
		}
		timeout := dur - time.Since(start)
		if timeout < 0 {
			return true, false
		}
		if notified, poisoned := c.wait(guard, timeout, true, 2); poisoned {
			// The error carries the result of the wait that found the
			// poison.
			return WaitTimeoutResult(!notified), true
		}
	}
}

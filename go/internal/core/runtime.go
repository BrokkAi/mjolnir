// Port of mj-core/src/runtime.rs.

// Runtime shutdown coordination for blocking work that awaits other work.
//
// In the Rust build, a `spawn_blocking` thread running `Handle::block_on`
// sits outside the tokio scheduler's shutdown ordering and keeps polling
// timers while the driver disappears. Go has no timer driver to outlive, but
// the shutdown contract stays: every guarded BlockOn registers here, and
// shutdown cancels those calls (through their context) and waits up to a
// grace period for them to return before the process leaves.
//
// The tokio task helpers become goroutine helpers: Spawn and SpawnBlocking
// start a goroutine and return a JoinHandle that reports a panic as an error
// instead of dropping it. Watch and Unbounded stand in for tokio's watch and
// unbounded mpsc channels.

package core

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
	"time"
)

// BlockingWork says how to treat blocking work that is not guarded by
// BlockOn.
type BlockingWork interface{ isBlockingWork() }

// BlockingWorkAwait waits for the blocking work to finish (a plain runtime
// drop).
type BlockingWorkAwait struct{}

// BlockingWorkAbandon leaves the remaining blocking work behind
// (`shutdown_background`).
type BlockingWorkAbandon struct{}

// BlockingWorkAwaitFor waits up to Limit for the blocking work, then leaves
// what is still running behind (`shutdown_timeout`). The returned report says
// whether anything was left, so the caller can name it.
type BlockingWorkAwaitFor struct{ Limit time.Duration }

func (BlockingWorkAwait) isBlockingWork()    {}
func (BlockingWorkAbandon) isBlockingWork()  {}
func (BlockingWorkAwaitFor) isBlockingWork() {}

// ShutdownReport is what a runtime shutdown could not finish.
type ShutdownReport struct {
	// AwaitsLeft is the guarded BlockOn calls still running when the grace
	// ran out.
	AwaitsLeft int
	// BlockingLeft is whether BlockingWorkAwaitFor ran out of time with
	// blocking work still running.
	BlockingLeft bool
}

// ShutdownState tracks the shutdown signal and the guarded BlockOn calls in
// flight.
type ShutdownState struct {
	signalled  atomic.Bool
	notifyOnce sync.Once
	// notify is closed when shutdown is signalled.
	notify chan struct{}
	mu     sync.Mutex
	// inFlight counts guarded BlockOn calls; drained is closed and replaced
	// whenever it returns to zero.
	inFlight int
	drained  chan struct{}
}

// NewShutdownState is a state with no shutdown signalled and nothing in
// flight.
func NewShutdownState() *ShutdownState {
	return &ShutdownState{notify: make(chan struct{}), drained: make(chan struct{})}
}

// runtimeShutdown is the process's one shutdown state (Rust's SHUTDOWN).
var runtimeShutdown = NewShutdownState()

// signalledChan is closed once shutdown has been signalled, whether before
// or after the caller starts waiting.
func (s *ShutdownState) signalledChan() <-chan struct{} { return s.notify }

// errRuntimeShuttingDown is the error of a BlockOn that shutdown cancelled
// or refused.
var errRuntimeShuttingDown = errors.New("runtime is shutting down")

// BlockOnWith runs future to completion from blocking work, treating runtime
// shutdown as a cancellation: future's context is cancelled when shutdown is
// signalled, and the call then fails with "runtime is shutting down".
// (Rust's ShutdownState::block_on_with; Go methods cannot be generic.)
func BlockOnWith[T any](s *ShutdownState, future func(ctx context.Context) T) (T, error) {
	panic("unported: mj-core/src/runtime.rs ShutdownState::block_on_with")
}

// ShutdownWith signals shutdown, waits up to grace for guarded BlockOn calls
// to return, then shuts runtime down as work says.
func (s *ShutdownState) ShutdownWith(runtime *Runtime, work BlockingWork, grace time.Duration) ShutdownReport {
	panic("unported: mj-core/src/runtime.rs ShutdownState::shutdown_with")
}

// inFlightGuard holds one registration of a guarded BlockOn call.
type inFlightGuard struct {
	state *ShutdownState
}

func registerInFlight(state *ShutdownState) inFlightGuard {
	state.mu.Lock()
	state.inFlight++
	state.mu.Unlock()
	return inFlightGuard{state: state}
}

// release is the guard's Drop.
func (g inFlightGuard) release() {
	g.state.mu.Lock()
	defer g.state.mu.Unlock()
	g.state.inFlight--
	if g.state.inFlight == 0 {
		close(g.state.drained)
		g.state.drained = make(chan struct{})
	}
}

// BlockOn runs future from blocking work, with runtime shutdown as a
// cancellation. Errors with "runtime is shutting down" when the process has
// begun shutting its runtime down before or while the future runs.
func BlockOn[T any](future func(ctx context.Context) T) (T, error) {
	return BlockOnWith(runtimeShutdown, future)
}

// Runtime stands in for the tokio runtime Rust shuts down: it owns the
// blocking work started through SpawnBlocking, so shutdown can wait for it,
// leave it, or wait a bounded time.
type Runtime struct {
	blocking sync.WaitGroup
}

func NewRuntime() *Runtime { return &Runtime{} }

// currentRuntime is the runtime Spawn and SpawnBlocking hand work to (Rust's
// Handle::current()).
var currentRuntime = NewRuntime()

// CurrentRuntime is the process's runtime, which the process's main shuts
// down with Shutdown.
func CurrentRuntime() *Runtime { return currentRuntime }

// OffAsyncWorker runs blocking work, such as waiting for a child process,
// without holding an async worker. Goroutines never hold a scheduler thread
// while they block in a system call, so in Go the work simply runs.
func OffAsyncWorker[R any](work func() R) R { return work() }

// SpawnOffAsyncWorkers runs future, whose synchronous waits must not hold an
// async worker, on its own goroutine through BlockOn. The result is an error
// when the runtime begins shutting down before the future finishes.
func SpawnOffAsyncWorkers[T any](future func(ctx context.Context) T) *JoinHandle[T] {
	return SpawnBlocking(func() T {
		value, err := BlockOn(future)
		if err != nil {
			panic(joinError{err: err})
		}
		return value
	})
}

// Spawn starts a task, as `tokio::spawn` does.
func Spawn[T any](future func() T) *JoinHandle[T] {
	handle := newJoinHandle[T]()
	go handle.run(future)
	return handle
}

// SpawnBlocking runs blocking work on the current runtime, as
// `tokio::task::spawn_blocking` does.
func SpawnBlocking[T any](work func() T) *JoinHandle[T] {
	return SpawnBlockingOn(currentRuntime, work)
}

// SpawnBlockingOn runs blocking work owned by runtime (Rust's
// `runtime.spawn_blocking`).
func SpawnBlockingOn[T any](runtime *Runtime, work func() T) *JoinHandle[T] {
	handle := newJoinHandle[T]()
	runtime.blocking.Add(1)
	go func() {
		defer runtime.blocking.Done()
		handle.run(work)
	}()
	return handle
}

// Shutdown shuts the process's runtime down in the order that keeps BlockOn
// callers safe: raise the shutdown signal, wait up to grace for every
// in-flight BlockOn to return, then shut the runtime down.
func Shutdown(runtime *Runtime, work BlockingWork, grace time.Duration) ShutdownReport {
	return runtimeShutdown.ShutdownWith(runtime, work, grace)
}

// JoinHandle is the result of a spawned task. A task that panics reports the
// panic as its error, so a failure is never silently dropped.
type JoinHandle[T any] struct {
	done  chan struct{}
	value T
	err   error
}

// joinError carries an error out of a task through a panic, so it is
// reported as that error rather than as a panic.
type joinError struct{ err error }

func newJoinHandle[T any]() *JoinHandle[T] {
	return &JoinHandle[T]{done: make(chan struct{})}
}

func (h *JoinHandle[T]) run(work func() T) {
	defer close(h.done)
	defer func() {
		if recovered := recover(); recovered != nil {
			if failure, ok := recovered.(joinError); ok {
				h.err = failure.err
			} else {
				h.err = fmt.Errorf("task panicked: %v", recovered)
			}
		}
	}()
	h.value = work()
}

// Done is closed once the task has finished.
func (h *JoinHandle[T]) Done() <-chan struct{} { return h.done }

// Join waits for the task's result, or for ctx to end.
func (h *JoinHandle[T]) Join(ctx context.Context) (T, error) {
	select {
	case <-h.done:
		return h.value, h.err
	case <-ctx.Done():
		var zero T
		return zero, ctx.Err()
	}
}

// Watch is tokio's watch channel: one current value whose readers are told
// when it changes.
type Watch[T any] struct {
	mu      sync.Mutex
	value   T
	changed chan struct{}
}

func NewWatch[T any](initial T) *Watch[T] {
	return &Watch[T]{value: initial, changed: make(chan struct{})}
}

// Borrow is the current value.
func (w *Watch[T]) Borrow() T {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.value
}

// SendReplace stores value, wakes every waiter, and returns the old value.
func (w *Watch[T]) SendReplace(value T) T {
	w.mu.Lock()
	defer w.mu.Unlock()
	old := w.value
	w.value = value
	close(w.changed)
	w.changed = make(chan struct{})
	return old
}

// Changed is closed at the next SendReplace after this call.
func (w *Watch[T]) Changed() <-chan struct{} {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.changed
}

// Unbounded is tokio's unbounded mpsc channel: Send never blocks, and fails
// only once the receiver has closed it.
type Unbounded[T any] struct {
	mu     sync.Mutex
	queue  []T
	closed bool
	// ready is closed and replaced whenever a value arrives or the channel
	// closes.
	ready chan struct{}
}

func NewUnbounded[T any]() *Unbounded[T] {
	return &Unbounded[T]{ready: make(chan struct{})}
}

// errUnboundedClosed is the error of a Send after the receiver closed.
var errUnboundedClosed = errors.New("channel closed")

// Send queues value, or fails when the receiver has closed the channel.
func (u *Unbounded[T]) Send(value T) error {
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.closed {
		return errUnboundedClosed
	}
	u.queue = append(u.queue, value)
	close(u.ready)
	u.ready = make(chan struct{})
	return nil
}

// Recv waits for the next value; false when the channel is closed and empty,
// or ctx ended.
func (u *Unbounded[T]) Recv(ctx context.Context) (T, bool) {
	panic("unported: mj-core/src/runtime.rs tokio::sync::mpsc::UnboundedReceiver::recv")
}

// Close stops further sends, as dropping the receiver does.
func (u *Unbounded[T]) Close() {
	u.mu.Lock()
	defer u.mu.Unlock()
	if !u.closed {
		u.closed = true
		close(u.ready)
		u.ready = make(chan struct{})
	}
}

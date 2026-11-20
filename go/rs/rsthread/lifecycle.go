// Port of std 1.99.1 src/thread/lifecycle.rs

package rsthread

import (
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// packet is lifecycle's Packet: how the thread hands its result to whoever
// joins it.
type packet[T any] struct {
	// scope is the scope the thread was spawned in, if any.
	scope *scopeData
	// result and caught are the thread's return value or its panic. They
	// are written once, before finished is set, and read by join after the
	// thread has ended.
	result T
	caught *rs.PanicInfo
	// finished is set when the result is stored: is_finished.
	finished atomic.Bool
	// taken is set by join, which consumes the result.
	taken atomic.Bool
}

// store is the end of the thread's main function: the result goes into the
// packet, and a scope learns that one of its threads is done.
func (p *packet[T]) store(result T, caught *rs.PanicInfo) {
	p.result = result
	p.caught = caught
	p.finished.Store(true)
	if p.scope != nil {
		p.scope.decrementNumRunningThreads()
	}
}

// unhandledPanic reports whether the thread has ended with a panic that
// nobody took with join, which makes its scope panic.
func (p *packet[T]) unhandledPanic() bool {
	// finished is set after caught is written.
	return p.finished.Load() && p.caught != nil && !p.taken.Load()
}

// joinInner is lifecycle's JoinInner: what a JoinHandle or a
// ScopedJoinHandle holds.
type joinInner[T any] struct {
	thread Thread
	packet *packet[T]
	// exited is closed when the thread's goroutine has finished, its
	// thread-locals included: the native join.
	exited chan struct{}
}

// spawnUnchecked is lifecycle's spawn_unchecked: it starts f on a new
// goroutine that is a thread of its own.
func spawnUnchecked[T any](name rs.Option[string], scope *scopeData, f func() T) *joinInner[T] {
	id := threadIdNew()
	thread := threadNew(id, name)

	myPacket := &packet[T]{scope: scope}
	if scope != nil {
		scope.incrementNumRunningThreads(myPacket)
	}
	inner := &joinInner[T]{thread: thread, packet: myPacket, exited: make(chan struct{})}
	go threadMain(inner, f)
	return inner
}

// threadMain is the entry point of a spawned thread (rust_start, and what
// the platform's thread start runs around it). No panic leaves it: a panic
// in f is recovered, the panic hook runs with the thread's name, and the
// panic goes to whoever joins the thread.
func threadMain[T any](inner *joinInner[T], f func() T) {
	thread := inner.thread
	setCurrent(thread.inner)
	// These run last to first once the result is stored: the
	// thread-locals are dropped, the goroutine leaves the table, and join
	// may return.
	defer close(inner.exited)
	defer dropCurrent()
	defer thread.inner.locals.destroy()

	returned := false
	defer func() {
		if returned {
			return
		}
		var zero T
		var caught *rs.PanicInfo
		if recovered := recover(); recovered != nil {
			caught = rs.PanicInfoOf(recovered)
		} else {
			// Not a panic: the goroutine is ending through
			// runtime.Goexit, which Rust code cannot do. Report it as a
			// panic so that join does not wait for a result that never
			// comes.
			caught = &rs.PanicInfo{Message: "rsthread: the thread ended through runtime.Goexit"}
		}
		rs.RunPanicHook(caught, thread.hookName())
		inner.packet.store(zero, caught)
	}()
	result := f()
	returned = true
	inner.packet.store(result, nil)
}

// isFinished is JoinInner::is_finished.
func (j *joinInner[T]) isFinished() bool { return j.packet.finished.Load() }

// join is JoinInner::join: it waits for the thread to end and takes its
// result.
func (j *joinInner[T]) join(skip int) (T, *rs.PanicInfo) {
	<-j.exited
	if !j.packet.taken.CompareAndSwap(false, true) {
		rs.PanicAt(skip+1, "rsthread: a thread was joined twice (Rust's join consumes the handle)")
	}
	result, caught := j.packet.result, j.packet.caught
	var zero T
	j.packet.result = zero
	return result, caught
}

// The panics of `handle.join().unwrap()` and `.expect(..)`: the Debug form of
// the Box<dyn Any + Send> a panicking thread leaves is `Any { .. }`.
const (
	joinUnwrapPanic = "called `Result::unwrap()` on an `Err` value: Any { .. }"
	joinExpectPanic = ": Any { .. }"
)

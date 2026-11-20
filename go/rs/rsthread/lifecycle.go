// Port of std 1.99.1 src/thread/lifecycle.rs

package rsthread

import (
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The states of the handle side of a packet.
const (
	handleLive uint32 = iota
	handleJoined
	handleDropped
)

// packet is lifecycle's Packet: how the thread hands its result to whoever
// joins it.
//
// Rust shares the packet through an Arc between the thread and its handle,
// and its destructor runs when the second of them lets go: it drops a result
// nobody took and, for a scoped thread, tells the scope. The port counts the
// same two references: the thread releases its own when its function has
// ended, and the handle releases its own by Join or Drop (or, for a scoped
// thread, when the scope ends; see scopeData.waitAll).
type packet[T any] struct {
	// scope is the scope the thread was spawned in, if any.
	scope *scopeData

	mu sync.Mutex
	// refs counts the thread's reference and the handle's.
	refs int
	// result and caught are the thread's return value or its panic.
	result T
	caught *rs.PanicInfo
	// taken is set when join consumed the result.
	taken bool

	// finished is set when the result is stored: is_finished.
	finished atomic.Bool
	// handle is handleLive until the handle joins or is dropped.
	handle atomic.Uint32
}

// store is the end of the thread's main function: the result goes into the
// packet.
func (p *packet[T]) store(result T, caught *rs.PanicInfo) {
	p.mu.Lock()
	p.result = result
	p.caught = caught
	p.mu.Unlock()
	p.finished.Store(true)
}

// release gives up one reference, and runs the packet's destructor when it
// was the last.
func (p *packet[T]) release() {
	p.mu.Lock()
	p.refs--
	last := p.refs == 0
	p.mu.Unlock()
	if last {
		p.drop()
	}
}

// drop is Packet's destructor.
func (p *packet[T]) drop() {
	p.mu.Lock()
	// If this packet was for a thread that ran in a scope, the thread
	// panicked, and nobody consumed the panic payload, we make sure
	// the scope function will panic.
	unhandledPanic := p.caught != nil && !p.taken
	// Drop the result. This is only relevant for threads that aren't
	// join()ed, as join() takes the result, such that there is nothing left
	// to drop here.
	result, caught, taken := p.result, p.caught, p.taken
	var zero T
	p.result = zero
	p.caught = nil
	p.mu.Unlock()
	if !taken {
		if caught == nil {
			dropValue(&result)
		} else {
			dropValue(&caught.Payload)
		}
	}

	// Book-keeping so the scope knows when it's done.
	if p.scope != nil {
		// Now that there will be no more user code running on this thread
		// that can use 'scope, mark the thread as 'finished'.
		// It's important we only do this after the `result` has been dropped,
		// since dropping it might still use things it borrowed from 'scope.
		p.scope.decrementNumRunningThreads(unhandledPanic)
	}
}

// releaseHandle is dropping the handle without joining. It reports whether
// the handle was still live; a second release does nothing.
func (p *packet[T]) releaseHandle() bool {
	if !p.handle.CompareAndSwap(handleLive, handleDropped) {
		return false
	}
	p.release()
	return true
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

	myPacket := &packet[T]{scope: scope, refs: 2}
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
	// These run last to first once the thread has let go of its packet:
	// the thread-locals are dropped, the goroutine leaves the table, and
	// join may return.
	defer close(inner.exited)
	defer dropCurrent()
	defer thread.inner.locals.destroy()

	returned := false
	defer func() {
		if !returned {
			var zero T
			var caught *rs.PanicInfo
			if recovered := recover(); recovered != nil {
				caught = rs.PanicInfoOf(recovered)
			} else {
				// Not a panic: the goroutine is ending through
				// runtime.Goexit, which Rust code cannot do. Report it as
				// a panic so that join does not wait for a result that
				// never comes.
				caught = &rs.PanicInfo{Message: "rsthread: the thread ended through runtime.Goexit"}
			}
			rs.RunPanicHook(caught, thread.hookName())
			inner.packet.store(zero, caught)
		}
		// Here `their_packet` gets dropped, and if this is the last `Arc`
		// for that packet that will call `decrement_num_running_threads` and
		// therefore signal that this thread is done.
		inner.packet.release()
		if scope := inner.packet.scope; scope != nil {
			scope.threadEnded()
		}
	}()
	result := f()
	returned = true
	inner.packet.store(result, nil)
}

// isFinished is JoinInner::is_finished.
func (j *joinInner[T]) isFinished() bool { return j.packet.finished.Load() }

// join is JoinInner::join: it waits for the thread to end, takes its result
// and lets go of the packet.
func (j *joinInner[T]) join(skip int) (T, *rs.PanicInfo) {
	if !j.packet.handle.CompareAndSwap(handleLive, handleJoined) {
		rs.PanicAt(skip+1, "rsthread: a thread handle was used after Join or Drop (Rust's join consumes the handle)")
	}
	<-j.exited
	p := j.packet
	p.mu.Lock()
	p.taken = true
	result, caught := p.result, p.caught
	var zero T
	p.result = zero
	p.mu.Unlock()
	p.release()
	return result, caught
}

// The panics of `handle.join().unwrap()` and `.expect(..)`: the Debug form of
// the Box<dyn Any + Send> a panicking thread leaves is `Any { .. }`.
const (
	joinUnwrapPanic = "called `Result::unwrap()` on an `Err` value: Any { .. }"
	joinExpectPanic = ": Any { .. }"
)

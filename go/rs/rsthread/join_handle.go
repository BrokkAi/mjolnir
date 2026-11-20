// Port of std 1.99.1 src/thread/join_handle.rs

package rsthread

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// JoinHandle is std::thread::JoinHandle<T>: permission to join a thread,
// that is to wait for it to end and take its result.
//
// Dropping the handle detaches the thread, which runs on; the thread's
// result is then dropped (its Drop runs) when the thread ends, as in Rust.
type JoinHandle[T any] struct {
	inner *joinInner[T]
}

// Thread is JoinHandle::thread: the thread's handle.
func (h *JoinHandle[T]) Thread() *Thread { return &h.inner.thread }

// Join is JoinHandle::join: it waits for the thread to end and returns its
// result, or the panic that ended it. A panic in a thread does not end the
// process; it is delivered here. The handle is spent: Rust's join consumes
// it, and joining again panics.
//
// The panic is a *rs.PanicInfo, not an error: do not pass Join's results to
// rs.Unwrap, where a nil *rs.PanicInfo would be a non-nil error. Use
// JoinUnwrap and JoinExpect for `.join().unwrap()` and `.join().expect(..)`.
func (h *JoinHandle[T]) Join() (T, *rs.PanicInfo) { return h.inner.join(1) }

// JoinUnwrap is `handle.join().unwrap()`: the result, or a panic with Rust's
// text when the thread panicked.
func (h *JoinHandle[T]) JoinUnwrap() T {
	result, caught := h.inner.join(1)
	if caught != nil {
		rs.PanicAt(1, joinUnwrapPanic)
	}
	return result
}

// JoinExpect is `handle.join().expect(message)`.
func (h *JoinHandle[T]) JoinExpect(message string) T {
	result, caught := h.inner.join(1)
	if caught != nil {
		rs.PanicAt(1, message+joinExpectPanic)
	}
	return result
}

// IsFinished is JoinHandle::is_finished: whether the thread's function has
// returned or panicked. Join may still wait briefly while the thread's
// thread-locals are dropped.
func (h *JoinHandle[T]) IsFinished() bool { return h.inner.isFinished() }

// Drop is dropping the handle without joining, which detaches the thread.
// It is idempotent, and does nothing after Join.
func (h *JoinHandle[T]) Drop() { h.inner.packet.releaseHandle() }

// FmtDebug is the Debug impl: `JoinHandle { .. }`.
func (h *JoinHandle[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("JoinHandle").FinishNonExhaustive()
}

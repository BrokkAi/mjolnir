// Port of std 1.99.1 src/sync/mpsc.rs, and of the channel flavours it is
// built on: src/sync/mpmc/{mod,counter,list,array,zero}.rs

package rssync

import (
	"iter"
	"reflect"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// The three flavours of channel, as mpmc has them.
type mpscFlavor uint8

const (
	// mpscList is the unbounded channel of `channel()` (mpmc/list.rs).
	mpscList mpscFlavor = iota
	// mpscArray is the bounded channel of `sync_channel(n)` for n > 0
	// (mpmc/array.rs).
	mpscArray
	// mpscZero is the rendezvous channel of `sync_channel(0)`
	// (mpmc/zero.rs): a send waits until a receive takes the message.
	mpscZero
)

// mpscWaiter is one thread blocked on a channel: Rust's registered
// Operation with its Context.
type mpscWaiter[T any] struct {
	// wake receives the notification. It holds one, so a notifier never
	// blocks.
	wake chan struct{}
	// selected is set, under the channel's lock, when the other side paired
	// with this waiter (Selected::Operation). Only the rendezvous flavour
	// pairs.
	selected bool
	// msg is the packet of the rendezvous flavour: a waiting sender's
	// message until a receiver takes it, or the message a sender handed to a
	// waiting receiver.
	msg T
}

func newMpscWaiter[T any]() *mpscWaiter[T] {
	return &mpscWaiter[T]{wake: make(chan struct{}, 1)}
}

func (w *mpscWaiter[T]) notify() {
	select {
	case w.wake <- struct{}{}:
	default:
	}
}

// wait blocks until the waiter is notified or the deadline, if any, has
// passed.
func (w *mpscWaiter[T]) wait(deadline rs.Option[rstime.Instant]) {
	until, timed := deadline.Get()
	if !timed {
		<-w.wake
		return
	}
	remaining := until.SaturatingDurationSince(rstime.InstantNow())
	if remaining == 0 {
		select {
		case <-w.wake:
		default:
		}
		return
	}
	timer := time.NewTimer(remaining)
	defer timer.Stop()
	select {
	case <-w.wake:
	case <-timer.C:
	}
}

// removeWaiter takes w out of queue and reports whether it was there.
func removeWaiter[T any](queue *[]*mpscWaiter[T], w *mpscWaiter[T]) bool {
	for i, queued := range *queue {
		if queued == w {
			*queue = append((*queue)[:i], (*queue)[i+1:]...)
			return true
		}
	}
	return false
}

// popWaiter takes the longest-waiting waiter out of queue, or returns nil.
func popWaiter[T any](queue *[]*mpscWaiter[T]) *mpscWaiter[T] {
	if len(*queue) == 0 {
		return nil
	}
	w := (*queue)[0]
	(*queue)[0] = nil
	*queue = (*queue)[1:]
	return w
}

// mpscChannel is the channel the handles share, with mpmc/counter.rs's
// counts.
type mpscChannel[T any] struct {
	mu     sync.Mutex
	flavor mpscFlavor
	// capacity is the bound of the array flavour.
	capacity int
	// queue holds the messages of the list and array flavours, oldest
	// first, from index head.
	queue []T
	head  int
	// senders and receivers count the live handles of each side.
	senders   int
	receivers int
	// disconnected is the channel's mark bit: set once either side has no
	// handles left.
	disconnected bool
	// recvWaiters and sendWaiters are the blocked threads, longest waiting
	// first. A thread is taken out of its queue by whoever wakes it, or by
	// itself when its deadline passes.
	recvWaiters []*mpscWaiter[T]
	sendWaiters []*mpscWaiter[T]
}

func (c *mpscChannel[T]) len() int { return len(c.queue) - c.head }

func (c *mpscChannel[T]) push(msg T) { c.queue = append(c.queue, msg) }

func (c *mpscChannel[T]) pop() T {
	msg := c.queue[c.head]
	var zero T
	c.queue[c.head] = zero
	c.head++
	if c.head == len(c.queue) {
		c.queue = c.queue[:0]
		c.head = 0
	} else if c.head > 32 && c.head*2 > len(c.queue) {
		c.queue = append(c.queue[:0], c.queue[c.head:]...)
		c.head = 0
	}
	return msg
}

// notifyOne wakes the longest-waiting thread of queue. The notifier takes
// the waiter out of the queue, as Rust's notify selects it, so that a second
// notification goes to the next waiter instead of to one that has not run
// yet.
func notifyOne[T any](queue *[]*mpscWaiter[T]) {
	if w := popWaiter(queue); w != nil {
		w.notify()
	}
}

// notifyAll wakes every thread of queue and empties it.
func notifyAll[T any](queue *[]*mpscWaiter[T]) {
	for _, w := range *queue {
		w.notify()
	}
	*queue = nil
}

// disconnectLocked is the flavours' disconnect: it sets the mark and wakes
// every blocked thread, which then finds the channel disconnected. It
// returns the messages a receiver-side disconnect discards, for the caller
// to drop once the lock is released.
func (c *mpscChannel[T]) disconnectLocked(receiverSide bool) []T {
	c.disconnected = true
	notifyAll(&c.recvWaiters)
	notifyAll(&c.sendWaiters)
	if !receiverSide || c.len() == 0 {
		return nil
	}
	// disconnect_receivers discards the messages still in the channel.
	discarded := append([]T(nil), c.queue[c.head:]...)
	c.queue = nil
	c.head = 0
	return discarded
}

// send is the flavours' send. It returns the message back when the channel
// is disconnected.
func (c *mpscChannel[T]) send(msg T) (T, bool) {
	c.mu.Lock()
	for {
		if c.disconnected {
			c.mu.Unlock()
			return msg, false
		}
		switch c.flavor {
		case mpscList:
			c.push(msg)
			notifyOne(&c.recvWaiters)
			c.mu.Unlock()
			var zero T
			return zero, true
		case mpscArray:
			if c.len() < c.capacity {
				c.push(msg)
				notifyOne(&c.recvWaiters)
				c.mu.Unlock()
				var zero T
				return zero, true
			}
			// Block until a receiver makes room or the channel is
			// disconnected, then try again.
			w := newMpscWaiter[T]()
			c.sendWaiters = append(c.sendWaiters, w)
			c.mu.Unlock()
			w.wait(rs.None[rstime.Instant]())
			c.mu.Lock()
			removeWaiter(&c.sendWaiters, w)
		case mpscZero:
			// If there's a waiting receiver, pair up with it.
			if w := popWaiter(&c.recvWaiters); w != nil {
				w.msg = msg
				w.selected = true
				w.notify()
				c.mu.Unlock()
				var zero T
				return zero, true
			}
			// Block until a receiver takes the message or the channel is
			// disconnected.
			w := newMpscWaiter[T]()
			w.msg = msg
			c.sendWaiters = append(c.sendWaiters, w)
			c.mu.Unlock()
			w.wait(rs.None[rstime.Instant]())
			c.mu.Lock()
			if w.selected {
				c.mu.Unlock()
				var zero T
				return zero, true
			}
			removeWaiter(&c.sendWaiters, w)
			msg = w.msg
			var zero T
			w.msg = zero
		default:
			panic(rs.Unreachable())
		}
	}
}

// trySendResult says what trySend did.
type trySendResult uint8

const (
	trySendSent trySendResult = iota
	trySendFull
	trySendDisconnected
)

// trySend is the array and rendezvous flavours' try_send.
func (c *mpscChannel[T]) trySend(msg T) trySendResult {
	c.mu.Lock()
	defer c.mu.Unlock()
	switch c.flavor {
	case mpscList:
		if c.disconnected {
			return trySendDisconnected
		}
		c.push(msg)
		notifyOne(&c.recvWaiters)
		return trySendSent
	case mpscArray:
		if c.disconnected {
			return trySendDisconnected
		}
		if c.len() >= c.capacity {
			return trySendFull
		}
		c.push(msg)
		notifyOne(&c.recvWaiters)
		return trySendSent
	case mpscZero:
		if w := popWaiter(&c.recvWaiters); w != nil {
			w.msg = msg
			w.selected = true
			w.notify()
			return trySendSent
		}
		if c.disconnected {
			return trySendDisconnected
		}
		return trySendFull
	default:
		panic(rs.Unreachable())
	}
}

// recvResult says what tryRecv and recv did.
type recvResult uint8

const (
	recvReceived recvResult = iota
	recvEmpty
	recvTimeout
	recvDisconnected
)

// takeLocked takes the next message, if there is one: from the queue, or
// from a waiting sender of the rendezvous flavour.
func (c *mpscChannel[T]) takeLocked() (T, bool) {
	if c.flavor == mpscZero {
		// If there's a waiting sender, pair up with it.
		if w := popWaiter(&c.sendWaiters); w != nil {
			msg := w.msg
			var zero T
			w.msg = zero
			w.selected = true
			w.notify()
			return msg, true
		}
		var zero T
		return zero, false
	}
	if c.len() > 0 {
		msg := c.pop()
		// A slot is free: wake a sender blocked on a full channel.
		notifyOne(&c.sendWaiters)
		return msg, true
	}
	var zero T
	return zero, false
}

// tryRecv is the flavours' try_recv.
func (c *mpscChannel[T]) tryRecv() (T, recvResult) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if msg, ok := c.takeLocked(); ok {
		return msg, recvReceived
	}
	var zero T
	if c.disconnected {
		return zero, recvDisconnected
	}
	return zero, recvEmpty
}

// recv is the flavours' recv, waiting until deadline when there is one.
func (c *mpscChannel[T]) recv(deadline rs.Option[rstime.Instant]) (T, recvResult) {
	var zero T
	c.mu.Lock()
	for {
		// Try receiving a message.
		if msg, ok := c.takeLocked(); ok {
			c.mu.Unlock()
			return msg, recvReceived
		}
		if c.disconnected {
			c.mu.Unlock()
			return zero, recvDisconnected
		}
		if until, ok := deadline.Get(); ok && !rstime.InstantNow().Before(until) {
			c.mu.Unlock()
			return zero, recvTimeout
		}

		// Prepare for blocking until a sender wakes us up.
		w := newMpscWaiter[T]()
		c.recvWaiters = append(c.recvWaiters, w)
		c.mu.Unlock()
		w.wait(deadline)
		c.mu.Lock()
		if w.selected {
			// A sender of the rendezvous flavour handed the message over.
			msg := w.msg
			c.mu.Unlock()
			return msg, recvReceived
		}
		removeWaiter(&c.recvWaiters, w)
		if c.flavor == mpscZero {
			// The rendezvous flavour waits once: it was woken by the
			// deadline or by the disconnect.
			c.mu.Unlock()
			if c.disconnectedNow() {
				return zero, recvDisconnected
			}
			return zero, recvTimeout
		}
		// If the channel was disconnected, we still have to check for
		// remaining messages.
	}
}

func (c *mpscChannel[T]) disconnectedNow() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.disconnected
}

// mpscHandle is one sender or receiver: Rust's counter::Sender or
// counter::Receiver. Copies of the Go handle value share it, so a moved
// handle is dropped once.
type mpscHandle[T any] struct {
	ch      *mpscChannel[T]
	dropped atomic.Bool
}

func (h *mpscHandle[T]) channel(kind string, skip int) *mpscChannel[T] {
	if h == nil {
		rs.PanicAt(skip+1, "rssync: use of an uninitialised Mpsc"+kind+" (create one with rssync.MpscChannel or rssync.MpscSyncChannel)")
	}
	if h.dropped.Load() {
		rs.PanicAt(skip+1, "rssync: use of an Mpsc"+kind+" after Drop")
	}
	return h.ch
}

// releaseSender is counter::Sender::release.
func (h *mpscHandle[T]) releaseSender() {
	if h == nil || !h.dropped.CompareAndSwap(false, true) {
		return
	}
	c := h.ch
	c.mu.Lock()
	c.senders--
	if c.senders == 0 {
		c.disconnectLocked(false)
	}
	c.mu.Unlock()
}

// releaseReceiver is counter::Receiver::release. The messages still in the
// channel are dropped.
func (h *mpscHandle[T]) releaseReceiver() {
	if h == nil || !h.dropped.CompareAndSwap(false, true) {
		return
	}
	c := h.ch
	c.mu.Lock()
	c.receivers--
	var discarded []T
	if c.receivers == 0 {
		discarded = c.disconnectLocked(true)
	}
	c.mu.Unlock()
	for i := range discarded {
		dropMessage(&discarded[i])
	}
}

// acquireSender is counter::Sender::acquire: another handle on the same
// channel.
func (h *mpscHandle[T]) acquireSender(kind string, skip int) *mpscHandle[T] {
	c := h.channel(kind, skip+1)
	c.mu.Lock()
	c.senders++
	c.mu.Unlock()
	return &mpscHandle[T]{ch: c}
}

// dropMessage runs the destructor of a message the channel discards, as
// dropping it does in Rust.
func dropMessage[T any](msg *T) {
	if dropper, ok := any(msg).(rs.Dropper); ok {
		dropper.Drop()
		return
	}
	// A message that is an interface or a pointer holds a value whose own
	// type has the destructor.
	switch reflect.TypeFor[T]().Kind() {
	case reflect.Pointer:
		if *(*unsafe.Pointer)(unsafe.Pointer(msg)) == nil {
			return
		}
		fallthrough
	case reflect.Interface:
		if dropper, ok := any(*msg).(rs.Dropper); ok {
			dropper.Drop()
		}
	}
}

// MpscSender is std::sync::mpsc::Sender<T>: the sending half of an
// unbounded channel. Send never blocks.
//
// A Sender is a handle: copying the Go value is a Rust move, and both
// copies are the same sender. Clone makes another sender. Drop gives this
// one up; when the last sender is dropped the channel is disconnected, and
// the receiver gets RecvError once it has taken what is left. Drop is
// idempotent. The zero value is not a sender.
type MpscSender[T any] struct {
	h *mpscHandle[T]
}

// MpscSyncSender is std::sync::mpsc::SyncSender<T>: the sending half of a
// bounded channel. Send blocks while the channel is full; with a bound of
// zero it blocks until a receiver takes the message. It is a handle like
// MpscSender.
type MpscSyncSender[T any] struct {
	h *mpscHandle[T]
}

// MpscReceiver is std::sync::mpsc::Receiver<T>: the receiving half of a
// channel. It is a handle: copying the Go value is a Rust move. Drop it when
// Rust drops it; the senders' Send then fails, and the messages still in
// the channel are dropped. Drop is idempotent. The zero value is not a
// receiver.
type MpscReceiver[T any] struct {
	h *mpscHandle[T]
}

// MpscChannel is std::sync::mpsc::channel: a new unbounded channel.
func MpscChannel[T any]() (MpscSender[T], MpscReceiver[T]) {
	c := &mpscChannel[T]{flavor: mpscList, senders: 1, receivers: 1}
	return MpscSender[T]{h: &mpscHandle[T]{ch: c}}, MpscReceiver[T]{h: &mpscHandle[T]{ch: c}}
}

// MpscSyncChannel is std::sync::mpsc::sync_channel: a new channel that holds
// at most bound messages. A bound of zero makes a rendezvous channel, in
// which every send waits for a receive.
func MpscSyncChannel[T any](bound int) (MpscSyncSender[T], MpscReceiver[T]) {
	if bound < 0 {
		rs.PanicAt(1, "rssync: negative bound for MpscSyncChannel (a Rust usize is never negative)")
	}
	c := &mpscChannel[T]{flavor: mpscArray, capacity: bound, senders: 1, receivers: 1}
	if bound == 0 {
		c.flavor = mpscZero
	}
	return MpscSyncSender[T]{h: &mpscHandle[T]{ch: c}}, MpscReceiver[T]{h: &mpscHandle[T]{ch: c}}
}

// Send is Sender::send: it queues t for the receiver. The error is a
// *MpscSendError[T] carrying t back when the receiver has been dropped.
func (s MpscSender[T]) Send(t T) error {
	if msg, ok := s.h.channel("Sender", 1).send(t); !ok {
		return &MpscSendError[T]{F0: msg}
	}
	return nil
}

// Clone is `impl Clone for Sender`: another sender on the same channel.
func (s MpscSender[T]) Clone() MpscSender[T] {
	return MpscSender[T]{h: s.h.acquireSender("Sender", 1)}
}

// Drop gives the sender up. It is idempotent.
func (s MpscSender[T]) Drop() { s.h.releaseSender() }

// FmtDebug is the Debug impl: `Sender { .. }`.
func (s MpscSender[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Sender").FinishNonExhaustive()
}

// Send is SyncSender::send: it queues t, waiting while the channel is full
// (or, with a bound of zero, until a receiver takes it). The error is a
// *MpscSendError[T] carrying t back when the receiver has been dropped,
// including while this call waited.
func (s MpscSyncSender[T]) Send(t T) error {
	if msg, ok := s.h.channel("SyncSender", 1).send(t); !ok {
		return &MpscSendError[T]{F0: msg}
	}
	return nil
}

// TrySend is SyncSender::try_send: Send without waiting. The error is a
// *MpscTrySendError_Full[T] when the channel is full (with a bound of zero:
// when no receiver is waiting), or a *MpscTrySendError_Disconnected[T] when
// the receiver has been dropped; either carries t back.
func (s MpscSyncSender[T]) TrySend(t T) error {
	switch s.h.channel("SyncSender", 1).trySend(t) {
	case trySendSent:
		return nil
	case trySendFull:
		return &MpscTrySendError_Full[T]{F0: t}
	case trySendDisconnected:
		return &MpscTrySendError_Disconnected[T]{F0: t}
	default:
		panic(rs.Unreachable())
	}
}

// Clone is `impl Clone for SyncSender`: another sender on the same channel.
func (s MpscSyncSender[T]) Clone() MpscSyncSender[T] {
	return MpscSyncSender[T]{h: s.h.acquireSender("SyncSender", 1)}
}

// Drop gives the sender up. It is idempotent.
func (s MpscSyncSender[T]) Drop() { s.h.releaseSender() }

// FmtDebug is the Debug impl: `SyncSender { .. }`.
func (s MpscSyncSender[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("SyncSender").FinishNonExhaustive()
}

// TryRecv is Receiver::try_recv: the next message if one is waiting. The
// error is MpscTryRecvError_Empty, or MpscTryRecvError_Disconnected when the
// channel is empty and every sender has been dropped.
func (r MpscReceiver[T]) TryRecv() (T, error) {
	msg, result := r.h.channel("Receiver", 1).tryRecv()
	switch result {
	case recvReceived:
		return msg, nil
	case recvEmpty:
		return msg, MpscTryRecvError_Empty
	case recvDisconnected:
		return msg, MpscTryRecvError_Disconnected
	default:
		panic(rs.Unreachable())
	}
}

// Recv is Receiver::recv: the next message, waiting for one. The error is
// MpscRecvError{} when the channel is empty and every sender has been
// dropped; messages sent before that are still received.
func (r MpscReceiver[T]) Recv() (T, error) {
	msg, result := r.h.channel("Receiver", 1).recv(rs.None[rstime.Instant]())
	switch result {
	case recvReceived:
		return msg, nil
	case recvDisconnected:
		return msg, MpscRecvError{}
	default:
		panic(rs.Unreachable())
	}
}

// RecvTimeout is Receiver::recv_timeout: Recv, waiting at most timeout. The
// error is MpscRecvTimeoutError_Timeout or MpscRecvTimeoutError_Disconnected.
// A message that is waiting is received even with a zero timeout. A timeout
// too long for an Instant waits without one, as Rust's does.
func (r MpscReceiver[T]) RecvTimeout(timeout time.Duration) (T, error) {
	c := r.h.channel("Receiver", 1)
	if timeout < 0 {
		rs.PanicAt(1, "rssync: negative Duration (a Rust Duration is unsigned)")
	}
	msg, result := c.recv(rstime.InstantNow().CheckedAdd(timeout))
	switch result {
	case recvReceived:
		return msg, nil
	case recvTimeout:
		return msg, MpscRecvTimeoutError_Timeout
	case recvDisconnected:
		return msg, MpscRecvTimeoutError_Disconnected
	default:
		panic(rs.Unreachable())
	}
}

// Iter is Receiver::iter (and `for x in &rx`): the messages as they come,
// waiting for each, until the channel is disconnected.
func (r MpscReceiver[T]) Iter() iter.Seq[T] {
	c := r.h.channel("Receiver", 1)
	return func(yield func(T) bool) {
		for {
			msg, result := c.recv(rs.None[rstime.Instant]())
			if result != recvReceived || !yield(msg) {
				return
			}
		}
	}
}

// TryIter is Receiver::try_iter: the messages that are waiting now, without
// waiting for more.
func (r MpscReceiver[T]) TryIter() iter.Seq[T] {
	c := r.h.channel("Receiver", 1)
	return func(yield func(T) bool) {
		for {
			msg, result := c.tryRecv()
			if result != recvReceived || !yield(msg) {
				return
			}
		}
	}
}

// IntoIter is `impl IntoIterator for Receiver` (`for x in rx`): Iter, which
// owns the receiver and drops it when the loop ends.
func (r MpscReceiver[T]) IntoIter() iter.Seq[T] {
	c := r.h.channel("Receiver", 1)
	return func(yield func(T) bool) {
		defer r.Drop()
		for {
			msg, result := c.recv(rs.None[rstime.Instant]())
			if result != recvReceived || !yield(msg) {
				return
			}
		}
	}
}

// Drop gives the receiver up. The senders' sends fail from now on, and the
// messages still in the channel are dropped. It is idempotent.
func (r MpscReceiver[T]) Drop() { r.h.releaseReceiver() }

// FmtDebug is the Debug impl: `Receiver { .. }`.
func (r MpscReceiver[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Receiver").FinishNonExhaustive()
}

// MpscSendError is std::sync::mpsc::SendError<T>: a send on a channel whose
// receiver has been dropped. F0 is the message, handed back.
type MpscSendError[T any] struct {
	F0 T
}

const sendErrorDisplay = "sending on a closed channel"

// String is the error's Display text.
func (e *MpscSendError[T]) String() string { return sendErrorDisplay }

// Error makes the value a Go error with its Display text.
func (e *MpscSendError[T]) Error() string { return sendErrorDisplay }

// FmtDisplay is the Display impl.
func (e *MpscSendError[T]) FmtDisplay(f *rs.Formatter) error { return f.Pad(sendErrorDisplay) }

// FmtDebug is the Debug impl: `SendError { .. }`, which does not show the
// message.
func (e *MpscSendError[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("SendError").FinishNonExhaustive()
}

// MpscTrySendError is std::sync::mpsc::TrySendError<T>: why TrySend did not
// send. It is one of *MpscTrySendError_Full[T] and
// *MpscTrySendError_Disconnected[T], each carrying the message back.
type MpscTrySendError[T any] interface {
	error
	isMpscTrySendError(T)
}

// MpscTrySendError_Full is TrySendError::Full: the channel is full, or a
// rendezvous channel has no receiver waiting.
type MpscTrySendError_Full[T any] struct {
	F0 T
}

// MpscTrySendError_Disconnected is TrySendError::Disconnected: the receiver
// has been dropped.
type MpscTrySendError_Disconnected[T any] struct {
	F0 T
}

func (e *MpscTrySendError_Full[T]) isMpscTrySendError(T)         {}
func (e *MpscTrySendError_Disconnected[T]) isMpscTrySendError(T) {}

const trySendFullDisplay = "sending on a full channel"

// String is the error's Display text.
func (e *MpscTrySendError_Full[T]) String() string { return trySendFullDisplay }

// Error makes the value a Go error with its Display text.
func (e *MpscTrySendError_Full[T]) Error() string { return trySendFullDisplay }

// FmtDisplay is the Display impl.
func (e *MpscTrySendError_Full[T]) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(trySendFullDisplay)
}

// FmtDebug is the Debug impl: `TrySendError::Full(..)`.
func (e *MpscTrySendError_Full[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("TrySendError::Full").FinishNonExhaustive()
}

// String is the error's Display text.
func (e *MpscTrySendError_Disconnected[T]) String() string { return sendErrorDisplay }

// Error makes the value a Go error with its Display text.
func (e *MpscTrySendError_Disconnected[T]) Error() string { return sendErrorDisplay }

// FmtDisplay is the Display impl.
func (e *MpscTrySendError_Disconnected[T]) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(sendErrorDisplay)
}

// FmtDebug is the Debug impl: `TrySendError::Disconnected(..)`.
func (e *MpscTrySendError_Disconnected[T]) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("TrySendError::Disconnected").FinishNonExhaustive()
}

// MpscTrySendErrorFromSendError is `impl From<SendError<T>> for
// TrySendError<T>`: a Disconnected carrying the same message.
func MpscTrySendErrorFromSendError[T any](err *MpscSendError[T]) MpscTrySendError[T] {
	return &MpscTrySendError_Disconnected[T]{F0: err.F0}
}

// MpscRecvError is std::sync::mpsc::RecvError: Recv on a channel that is
// empty and has no senders left. It is a comparable value.
type MpscRecvError struct{}

var _ = rs.RegisterUnitStruct[MpscRecvError]("RecvError")

const recvErrorDisplay = "receiving on a closed channel"

// String is the error's Display text.
func (MpscRecvError) String() string { return recvErrorDisplay }

// Error makes the value a Go error with its Display text.
func (MpscRecvError) Error() string { return recvErrorDisplay }

// FmtDisplay is the Display impl.
func (MpscRecvError) FmtDisplay(f *rs.Formatter) error { return f.Pad(recvErrorDisplay) }

// FmtDebug is the derived Debug impl: `RecvError`.
func (MpscRecvError) FmtDebug(f *rs.Formatter) error { return f.WriteStr("RecvError") }

// MpscTryRecvError is std::sync::mpsc::TryRecvError: why TryRecv returned no
// message. Its values are comparable with ==.
type MpscTryRecvError uint8

const (
	// MpscTryRecvError_Empty is TryRecvError::Empty: no message is waiting
	// now.
	MpscTryRecvError_Empty MpscTryRecvError = iota
	// MpscTryRecvError_Disconnected is TryRecvError::Disconnected: no
	// message is waiting and no sender is left.
	MpscTryRecvError_Disconnected
)

var _ = rs.RegisterEnum[MpscTryRecvError]("TryRecvError",
	rs.Const(MpscTryRecvError_Empty, "Empty"),
	rs.Const(MpscTryRecvError_Disconnected, "Disconnected"),
)

// String is the error's Display text.
func (e MpscTryRecvError) String() string {
	switch e {
	case MpscTryRecvError_Empty:
		return "receiving on an empty channel"
	case MpscTryRecvError_Disconnected:
		return recvErrorDisplay
	default:
		panic(rs.Unreachable(int(e)))
	}
}

// Error makes the value a Go error with its Display text.
func (e MpscTryRecvError) Error() string { return e.String() }

// FmtDisplay is the Display impl.
func (e MpscTryRecvError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived Debug impl: the variant's name.
func (e MpscTryRecvError) FmtDebug(f *rs.Formatter) error {
	switch e {
	case MpscTryRecvError_Empty:
		return f.WriteStr("Empty")
	case MpscTryRecvError_Disconnected:
		return f.WriteStr("Disconnected")
	default:
		panic(rs.Unreachable(int(e)))
	}
}

// MpscTryRecvErrorFromRecvError is `impl From<RecvError> for TryRecvError`.
func MpscTryRecvErrorFromRecvError(MpscRecvError) MpscTryRecvError {
	return MpscTryRecvError_Disconnected
}

// MpscRecvTimeoutError is std::sync::mpsc::RecvTimeoutError: why
// RecvTimeout returned no message. Its values are comparable with ==.
type MpscRecvTimeoutError uint8

const (
	// MpscRecvTimeoutError_Timeout is RecvTimeoutError::Timeout: no message
	// came in time.
	MpscRecvTimeoutError_Timeout MpscRecvTimeoutError = iota
	// MpscRecvTimeoutError_Disconnected is RecvTimeoutError::Disconnected:
	// the channel is empty and no sender is left.
	MpscRecvTimeoutError_Disconnected
)

var _ = rs.RegisterEnum[MpscRecvTimeoutError]("RecvTimeoutError",
	rs.Const(MpscRecvTimeoutError_Timeout, "Timeout"),
	rs.Const(MpscRecvTimeoutError_Disconnected, "Disconnected"),
)

// String is the error's Display text.
func (e MpscRecvTimeoutError) String() string {
	switch e {
	case MpscRecvTimeoutError_Timeout:
		return "timed out waiting on channel"
	case MpscRecvTimeoutError_Disconnected:
		return "channel is empty and sending half is closed"
	default:
		panic(rs.Unreachable(int(e)))
	}
}

// Error makes the value a Go error with its Display text.
func (e MpscRecvTimeoutError) Error() string { return e.String() }

// FmtDisplay is the Display impl.
func (e MpscRecvTimeoutError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived Debug impl: the variant's name.
func (e MpscRecvTimeoutError) FmtDebug(f *rs.Formatter) error {
	switch e {
	case MpscRecvTimeoutError_Timeout:
		return f.WriteStr("Timeout")
	case MpscRecvTimeoutError_Disconnected:
		return f.WriteStr("Disconnected")
	default:
		panic(rs.Unreachable(int(e)))
	}
}

// MpscRecvTimeoutErrorFromRecvError is `impl From<RecvError> for
// RecvTimeoutError`.
func MpscRecvTimeoutErrorFromRecvError(MpscRecvError) MpscRecvTimeoutError {
	return MpscRecvTimeoutError_Disconnected
}

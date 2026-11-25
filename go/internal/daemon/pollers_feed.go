// Port of mj-daemon/src/pollers/feed.rs.

package daemon

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
)

// FeedSource is something a control loop waits on and then drains: one
// awaited receive for the select arm, and a non-blocking receive for the
// batch that follows.
type FeedSource[T any] interface {
	// Wait blocks for the next message; false when the source has closed or
	// ctx ended. Cancel-safe: a wait that loses the race must not drop a
	// message.
	Wait(ctx context.Context) (T, bool)
	// PollNow takes a message that is already queued, without waiting.
	PollNow() (T, bool)
}

// ChannelSource is a bounded channel as a feed source (tokio's
// mpsc::Receiver).
type ChannelSource[T any] struct {
	channel <-chan T
}

// NewChannelSource is the feed source over channel.
func NewChannelSource[T any](channel <-chan T) *ChannelSource[T] {
	return &ChannelSource[T]{channel: channel}
}

func (s *ChannelSource[T]) Wait(ctx context.Context) (T, bool) {
	select {
	case message, ok := <-s.channel:
		return message, ok
	case <-ctx.Done():
		var zero T
		return zero, false
	}
}

func (s *ChannelSource[T]) PollNow() (T, bool) {
	select {
	case message, ok := <-s.channel:
		return message, ok
	default:
		var zero T
		return zero, false
	}
}

// UnboundedSource is an unbounded channel as a feed source (tokio's
// mpsc::UnboundedReceiver).
type UnboundedSource[T any] struct {
	channel *core.Unbounded[T]
}

// NewUnboundedSource is the feed source over channel.
func NewUnboundedSource[T any](channel *core.Unbounded[T]) *UnboundedSource[T] {
	return &UnboundedSource[T]{channel: channel}
}

func (s *UnboundedSource[T]) Wait(ctx context.Context) (T, bool) {
	return s.channel.Recv(ctx)
}

func (s *UnboundedSource[T]) PollNow() (T, bool) {
	message, ok, _ := s.channel.TryRecv()
	return message, ok
}

// WatchSource is one receiver of a watch as a feed source (tokio's
// watch::Receiver): Wait answers with the current value once it changed
// since this receiver last read it.
type WatchSource[T any] struct {
	watch *core.Watch[T]
}

// NewWatchSource is a receiver of watch that has seen its current value.
func NewWatchSource[T any](watch *core.Watch[T]) *WatchSource[T] {
	panic("unported: mj-daemon/src/pollers/feed.rs FeedSource for watch::Receiver")
}

func (s *WatchSource[T]) Wait(ctx context.Context) (T, bool) {
	panic("unported: mj-daemon/src/pollers/feed.rs FeedSource::wait for watch::Receiver")
}

func (s *WatchSource[T]) PollNow() (T, bool) {
	panic("unported: mj-daemon/src/pollers/feed.rs FeedSource::poll_now for watch::Receiver")
}

// MarkChanged makes the current value count as unseen, so the next Wait
// returns it (watch::Receiver::mark_changed).
func (s *WatchSource[T]) MarkChanged() {
	panic("unported: watch::Receiver::mark_changed")
}

// Borrow is the current value, without marking it seen.
func (s *WatchSource[T]) Borrow() T { return s.watch.Borrow() }

// IsClosed reports whether the sending side is gone (watch::Receiver's
// has_changed erring).
func (s *WatchSource[T]) IsClosed() bool {
	panic("unported: watch::Receiver::has_changed is_err")
}

// SessionManagerSource is the session manager's update feed as a feed
// source.
type SessionManagerSource struct {
	updates *sessionruntime.SessionManagerUpdates
}

// NewSessionManagerSource is the feed source over updates.
func NewSessionManagerSource(updates *sessionruntime.SessionManagerUpdates) *SessionManagerSource {
	return &SessionManagerSource{updates: updates}
}

func (s *SessionManagerSource) Wait(ctx context.Context) (sessionruntime.SessionManagerUpdate, bool) {
	return s.updates.Recv(ctx)
}

func (s *SessionManagerSource) PollNow() (sessionruntime.SessionManagerUpdate, bool) {
	return s.updates.TryRecv()
}

// Feed is one background feed as a control loop uses it.
//
// The select arm hands the message that woke the loop to Accept, and the
// drain that follows walks NextReady until the feed is empty, so a burst of
// updates costs one draw. A closed channel reports nothing for ever, which
// would leave its arm permanently ready; Accept retires the feed instead,
// and IsOpen gates the arm.
type Feed[T any] struct {
	source  FeedSource[T]
	pending *T
	open    bool
	// delivered is whether the drain has taken a message since it was last
	// asked. A loop that gates a frame on a timer still has to draw when a
	// message rode along with that timer.
	delivered bool
}

// NewFeed is an open feed over source.
func NewFeed[T any](source FeedSource[T]) *Feed[T] {
	return &Feed[T]{source: source, open: true}
}

func (f *Feed[T]) IsOpen() bool { return f.open }

// Wait blocks for the source's next message.
func (f *Feed[T]) Wait(ctx context.Context) (T, bool) { return f.source.Wait(ctx) }

// Accept latches the message that won the select and reports whether one
// arrived. Applying it determines whether the visible state needs a redraw.
func (f *Feed[T]) Accept(message T, ok bool) bool {
	if !ok {
		f.open = false
		return false
	}
	f.pending = &message
	return true
}

// NextReady is the next message for the batch drain: the one that won the
// select first, then whatever queued behind it.
func (f *Feed[T]) NextReady() (T, bool) {
	if f.pending != nil {
		message := *f.pending
		f.pending = nil
		f.delivered = true
		return message, true
	}
	message, ok := f.source.PollNow()
	f.delivered = f.delivered || ok
	return message, ok
}

// TakeDelivered reports whether NextReady produced a message since the last
// call.
func (f *Feed[T]) TakeDelivered() bool {
	delivered := f.delivered
	f.delivered = false
	return delivered
}

// Latest reads the latest publication of a feed over a watch without
// waiting for the render loop to drain it. Only a feed built over a
// WatchSource has one.
func (f *Feed[T]) Latest() T {
	return f.source.(*WatchSource[T]).Borrow()
}

// SourceIsClosed reports whether the watch behind a feed has lost its
// sender. Only a feed built over a WatchSource has one.
func (f *Feed[T]) SourceIsClosed() bool {
	return f.source.(*WatchSource[T]).IsClosed()
}

// Port of tracing-core 0.1.36 src/subscriber.rs

package tracing

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Subscriber is tracing::Subscriber: what collects spans and events.
//
// Every method of the Rust trait is here. Those it gives a default body are
// OnRegisterDispatch, RegisterCallsite, MaxLevelHint, EventEnabled,
// CloneSpan, DropSpan, TryClose and CurrentSpan; an implementation that
// relies on a default forwards to the matching SubscriberDefault function.
//
// The methods are called from any goroutine, as the Rust ones are called from
// any thread.
type Subscriber interface {
	// OnRegisterDispatch is called when the subscriber is wrapped in a
	// Dispatch.
	OnRegisterDispatch(subscriber *Dispatch)
	// RegisterCallsite reports whether the subscriber is always, sometimes
	// or never interested in a callsite. Rust asks once for each callsite
	// and caches the answer; here it is asked for each event and span, so it
	// must depend only on the metadata.
	RegisterCallsite(metadata *Metadata) SubscriberInterest
	// Enabled reports whether a span or event with this metadata would be
	// recorded. It is asked when RegisterCallsite answered "sometimes".
	Enabled(metadata *Metadata) bool
	// MaxLevelHint returns the most verbose level the subscriber enables,
	// or None when it does not know.
	MaxLevelHint() rs.Option[LevelFilter]
	// NewSpan records a new span and returns its ID.
	NewSpan(span *SpanAttributes) SpanId
	// Record records values for fields of a span.
	Record(span SpanId, values *SpanRecord)
	// RecordFollowsFrom records that span follows from the span follows.
	RecordFollowsFrom(span SpanId, follows SpanId)
	// EventEnabled reports whether an event should be recorded, once its
	// values are known.
	EventEnabled(event *Event) bool
	// Event records an event.
	Event(event *Event)
	// Enter records that a span has been entered.
	Enter(span SpanId)
	// Exit records that a span has been exited.
	Exit(span SpanId)
	// CloneSpan notifies the subscriber that a span ID has been cloned.
	CloneSpan(id SpanId) SpanId
	// DropSpan is the deprecated form of TryClose.
	DropSpan(id SpanId)
	// TryClose notifies the subscriber that a span ID has been dropped, and
	// returns true when that closed the span.
	TryClose(id SpanId) bool
	// CurrentSpan returns the span the subscriber considers current.
	CurrentSpan() CurrentSpan
}

// SubscriberDefaultOnRegisterDispatch is the default body of
// Subscriber::on_register_dispatch.
func SubscriberDefaultOnRegisterDispatch(self Subscriber, subscriber *Dispatch) {}

// SubscriberDefaultRegisterCallsite is the default body of
// Subscriber::register_callsite.
func SubscriberDefaultRegisterCallsite(self Subscriber, metadata *Metadata) SubscriberInterest {
	if self.Enabled(metadata) {
		return SubscriberInterestAlways()
	}
	return SubscriberInterestNever()
}

// SubscriberDefaultMaxLevelHint is the default body of
// Subscriber::max_level_hint.
func SubscriberDefaultMaxLevelHint(self Subscriber) rs.Option[LevelFilter] {
	return rs.None[LevelFilter]()
}

// SubscriberDefaultEventEnabled is the default body of
// Subscriber::event_enabled.
func SubscriberDefaultEventEnabled(self Subscriber, event *Event) bool {
	return true
}

// SubscriberDefaultCloneSpan is the default body of Subscriber::clone_span.
func SubscriberDefaultCloneSpan(self Subscriber, id SpanId) SpanId {
	return id
}

// SubscriberDefaultDropSpan is the default body of Subscriber::drop_span.
func SubscriberDefaultDropSpan(self Subscriber, id SpanId) {}

// SubscriberDefaultTryClose is the default body of Subscriber::try_close.
func SubscriberDefaultTryClose(self Subscriber, id SpanId) bool {
	self.DropSpan(id)
	return false
}

// SubscriberDefaultCurrentSpan is the default body of
// Subscriber::current_span.
func SubscriberDefaultCurrentSpan(self Subscriber) CurrentSpan {
	return currentSpanUnknown()
}

// SubscriberDowncaster is Subscriber::downcast_raw for a subscriber made of
// several parts: it returns the part whose type is target. A subscriber that
// is only itself needs no method; DispatchDowncastRef tries the subscriber's
// own type first.
type SubscriberDowncaster interface {
	DowncastRaw(target reflect.Type) (any, bool)
}

// SubscriberInterest is tracing::subscriber::Interest: whether a subscriber
// wants to be notified of a callsite.
type SubscriberInterest struct {
	kind interestKind
}

type interestKind uint8

const (
	interestNever interestKind = iota
	interestSometimes
	interestAlways
)

// SubscriberInterestNever is Interest::never: the subscriber is never
// interested in the callsite.
func SubscriberInterestNever() SubscriberInterest {
	return SubscriberInterest{kind: interestNever}
}

// SubscriberInterestSometimes is Interest::sometimes: the subscriber is asked
// with Enabled each time.
func SubscriberInterestSometimes() SubscriberInterest {
	return SubscriberInterest{kind: interestSometimes}
}

// SubscriberInterestAlways is Interest::always: the subscriber is always
// interested in the callsite.
func SubscriberInterestAlways() SubscriberInterest {
	return SubscriberInterest{kind: interestAlways}
}

// IsNever reports whether the interest is "never".
func (i SubscriberInterest) IsNever() bool { return i.kind == interestNever }

// IsSometimes reports whether the interest is "sometimes".
func (i SubscriberInterest) IsSometimes() bool { return i.kind == interestSometimes }

// IsAlways reports whether the interest is "always".
func (i SubscriberInterest) IsAlways() bool { return i.kind == interestAlways }

// and is Interest::and: the interest of two subscribers together, which is
// "sometimes" unless they agree.
func (i SubscriberInterest) and(rhs SubscriberInterest) SubscriberInterest {
	if i.kind == rhs.kind {
		return i
	}
	return SubscriberInterestSometimes()
}

// FmtDebug is the derived Debug of Interest.
func (i SubscriberInterest) FmtDebug(f *rs.Formatter) error {
	name := "Never"
	switch i.kind {
	case interestSometimes:
		name = "Sometimes"
	case interestAlways:
		name = "Always"
	}
	return f.WriteStr("Interest(" + name + ")")
}

// SubscriberNoSubscriber is tracing::subscriber::NoSubscriber: it enables
// nothing and records nothing.
type SubscriberNoSubscriber struct{}

// SubscriberNoSubscriberNew is NoSubscriber::new.
func SubscriberNoSubscriberNew() SubscriberNoSubscriber { return SubscriberNoSubscriber{} }

func (SubscriberNoSubscriber) OnRegisterDispatch(*Dispatch) {}

func (SubscriberNoSubscriber) RegisterCallsite(*Metadata) SubscriberInterest {
	return SubscriberInterestNever()
}

func (SubscriberNoSubscriber) NewSpan(*SpanAttributes) SpanId { return SpanIdFromU64(0xDEAD) }

func (SubscriberNoSubscriber) Event(*Event) {}

func (SubscriberNoSubscriber) Record(SpanId, *SpanRecord) {}

func (SubscriberNoSubscriber) RecordFollowsFrom(SpanId, SpanId) {}

func (SubscriberNoSubscriber) Enabled(*Metadata) bool { return false }

func (SubscriberNoSubscriber) Enter(SpanId) {}

func (SubscriberNoSubscriber) Exit(SpanId) {}

func (s SubscriberNoSubscriber) MaxLevelHint() rs.Option[LevelFilter] {
	return SubscriberDefaultMaxLevelHint(s)
}

func (s SubscriberNoSubscriber) EventEnabled(event *Event) bool {
	return SubscriberDefaultEventEnabled(s, event)
}

func (s SubscriberNoSubscriber) CloneSpan(id SpanId) SpanId { return SubscriberDefaultCloneSpan(s, id) }

func (s SubscriberNoSubscriber) DropSpan(id SpanId) { SubscriberDefaultDropSpan(s, id) }

func (s SubscriberNoSubscriber) TryClose(id SpanId) bool { return SubscriberDefaultTryClose(s, id) }

func (s SubscriberNoSubscriber) CurrentSpan() CurrentSpan { return SubscriberDefaultCurrentSpan(s) }

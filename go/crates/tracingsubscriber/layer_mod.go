// Port of tracing-subscriber 0.3.23 src/layer/mod.rs

package tracingsubscriber

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Layer is tracing_subscriber::Layer: a composable piece of a subscriber,
// layered over a subscriber that keeps the span data (Registry). Every
// method of the Rust trait is here; an implementation that relies on a
// default body forwards to the matching LayerDefault function.
//
// Per-layer filtering (with_filter, Filter, Filtered) is not ported:
// Mjolnir filters the whole subscriber.
type Layer interface {
	// OnRegisterDispatch is called when the subscriber the layer is part of
	// is wrapped in a Dispatch.
	OnRegisterDispatch(subscriber *tracing.Dispatch)
	// OnLayer is called when the layer is layered over subscriber.
	OnLayer(subscriber tracing.Subscriber)
	// RegisterCallsite reports the layer's interest in a callsite.
	RegisterCallsite(metadata *tracing.Metadata) tracing.SubscriberInterest
	// Enabled reports whether the layer wants a span or event with this
	// metadata.
	Enabled(metadata *tracing.Metadata, ctx LayerContext) bool
	// OnNewSpan is told of a new span and its ID.
	OnNewSpan(attrs *tracing.SpanAttributes, id tracing.SpanId, ctx LayerContext)
	// MaxLevelHint returns the most verbose level the layer enables, or
	// None when it does not know.
	MaxLevelHint() rs.Option[tracing.LevelFilter]
	// OnRecord is told of values recorded on a span.
	OnRecord(span tracing.SpanId, values *tracing.SpanRecord, ctx LayerContext)
	// OnFollowsFrom is told that span follows from follows.
	OnFollowsFrom(span tracing.SpanId, follows tracing.SpanId, ctx LayerContext)
	// EventEnabled reports whether the layer wants an event, once its
	// values are known.
	EventEnabled(event *tracing.Event, ctx LayerContext) bool
	// OnEvent is told of an event.
	OnEvent(event *tracing.Event, ctx LayerContext)
	// OnEnter is told that a span was entered.
	OnEnter(id tracing.SpanId, ctx LayerContext)
	// OnExit is told that a span was exited.
	OnExit(id tracing.SpanId, ctx LayerContext)
	// OnClose is told that a span closed. Its data is still there.
	OnClose(id tracing.SpanId, ctx LayerContext)
	// OnIdChange is told that a span's ID changed.
	OnIdChange(old tracing.SpanId, new tracing.SpanId, ctx LayerContext)
	// DowncastRaw returns the layer, or the part of it, whose type is
	// target.
	DowncastRaw(target reflect.Type) (any, bool)
}

// LayerDefaultOnRegisterDispatch is the default body of
// Layer::on_register_dispatch.
func LayerDefaultOnRegisterDispatch(self Layer, subscriber *tracing.Dispatch) {}

// LayerDefaultOnLayer is the default body of Layer::on_layer.
func LayerDefaultOnLayer(self Layer, subscriber tracing.Subscriber) {}

// LayerDefaultRegisterCallsite is the default body of
// Layer::register_callsite.
func LayerDefaultRegisterCallsite(self Layer, metadata *tracing.Metadata) tracing.SubscriberInterest {
	if self.Enabled(metadata, layerContextNone()) {
		return tracing.SubscriberInterestAlways()
	}
	return tracing.SubscriberInterestNever()
}

// LayerDefaultEnabled is the default body of Layer::enabled.
func LayerDefaultEnabled(self Layer, metadata *tracing.Metadata, ctx LayerContext) bool {
	return true
}

// LayerDefaultOnNewSpan is the default body of Layer::on_new_span.
func LayerDefaultOnNewSpan(self Layer, attrs *tracing.SpanAttributes, id tracing.SpanId, ctx LayerContext) {
}

// LayerDefaultMaxLevelHint is the default body of Layer::max_level_hint.
func LayerDefaultMaxLevelHint(self Layer) rs.Option[tracing.LevelFilter] {
	return rs.None[tracing.LevelFilter]()
}

// LayerDefaultOnRecord is the default body of Layer::on_record.
func LayerDefaultOnRecord(self Layer, span tracing.SpanId, values *tracing.SpanRecord, ctx LayerContext) {
}

// LayerDefaultOnFollowsFrom is the default body of Layer::on_follows_from.
func LayerDefaultOnFollowsFrom(self Layer, span tracing.SpanId, follows tracing.SpanId, ctx LayerContext) {
}

// LayerDefaultEventEnabled is the default body of Layer::event_enabled.
func LayerDefaultEventEnabled(self Layer, event *tracing.Event, ctx LayerContext) bool {
	return true
}

// LayerDefaultOnEvent is the default body of Layer::on_event.
func LayerDefaultOnEvent(self Layer, event *tracing.Event, ctx LayerContext) {}

// LayerDefaultOnEnter is the default body of Layer::on_enter.
func LayerDefaultOnEnter(self Layer, id tracing.SpanId, ctx LayerContext) {}

// LayerDefaultOnExit is the default body of Layer::on_exit.
func LayerDefaultOnExit(self Layer, id tracing.SpanId, ctx LayerContext) {}

// LayerDefaultOnClose is the default body of Layer::on_close.
func LayerDefaultOnClose(self Layer, id tracing.SpanId, ctx LayerContext) {}

// LayerDefaultOnIdChange is the default body of Layer::on_id_change.
func LayerDefaultOnIdChange(self Layer, old tracing.SpanId, new tracing.SpanId, ctx LayerContext) {}

// LayerDefaultDowncastRaw is the default body of Layer::downcast_raw: the
// layer itself.
func LayerDefaultDowncastRaw(self Layer, target reflect.Type) (any, bool) {
	if reflect.TypeOf(self) == target {
		return self, true
	}
	return nil, false
}

// LayerWithSubscriber is Layer::with_subscriber: the layer over inner, as one
// subscriber.
func LayerWithSubscriber(layer Layer, inner tracing.Subscriber) *LayerLayered {
	innerHasLayerFilter := subscriberHasPlf(inner)
	layer.OnLayer(inner)
	return layeredNew(layer, inner, innerHasLayerFilter)
}

// SubscriberExtWith is SubscriberExt::with: layer over the subscriber.
//
//	subscriber := tracingsubscriber.SubscriberExtWith(tracingsubscriber.RegistryDefault(), filter)
func SubscriberExtWith(inner tracing.Subscriber, layer Layer) *LayerLayered {
	return LayerWithSubscriber(layer, inner)
}

// subscriberHasPlf is filter::subscriber_has_plf: whether a subscriber has
// per-layer filters. They are not ported, so none has.
func subscriberHasPlf(subscriber tracing.Subscriber) bool {
	return false
}

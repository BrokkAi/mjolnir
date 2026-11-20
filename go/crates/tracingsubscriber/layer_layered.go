// Port of tracing-subscriber 0.3.23 src/layer/layered.rs

package tracingsubscriber

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// LayerLayered is tracing_subscriber::layer::Layered: a layer over a
// subscriber, which is a subscriber itself. Only the list shape is ported (a
// layer over a subscriber), not a layer over a layer (`Layer::and_then`).
type LayerLayered struct {
	layer Layer
	inner tracing.Subscriber
	// These booleans are used to determine how to combine `Interest`s and max
	// level hints when per-layer filters are in use.
	innerIsRegistry     bool
	hasLayerFilter      bool
	innerHasLayerFilter bool
}

func layeredNew(layer Layer, inner tracing.Subscriber, innerHasLayerFilter bool) *LayerLayered {
	_, innerIsRegistry := inner.(*Registry)
	innerHasLayerFilter = innerHasLayerFilter || innerIsRegistry
	// filter::layer_has_plf: per-layer filters are not ported.
	hasLayerFilter := false
	return &LayerLayered{
		layer:               layer,
		inner:               inner,
		hasLayerFilter:      hasLayerFilter,
		innerHasLayerFilter: innerHasLayerFilter,
		innerIsRegistry:     innerIsRegistry,
	}
}

// LayerLayeredIs is Layered::is.
func LayerLayeredIs[T any](l *LayerLayered) bool {
	return LayerLayeredDowncastRef[T](l).IsSome()
}

// LayerLayeredDowncastRef is Layered::downcast_ref.
func LayerLayeredDowncastRef[T any](l *LayerLayered) rs.Option[T] {
	raw, ok := l.DowncastRaw(reflect.TypeFor[T]())
	if !ok {
		return rs.None[T]()
	}
	typed, ok := raw.(T)
	return rs.OptionOf(typed, ok)
}

// OnRegisterDispatch is `impl Subscriber for Layered`.
func (l *LayerLayered) OnRegisterDispatch(subscriber *tracing.Dispatch) {
	l.inner.OnRegisterDispatch(subscriber)
	l.layer.OnRegisterDispatch(subscriber)
}

// RegisterCallsite is `impl Subscriber for Layered`.
func (l *LayerLayered) RegisterCallsite(metadata *tracing.Metadata) tracing.SubscriberInterest {
	return l.pickInterest(l.layer.RegisterCallsite(metadata), func() tracing.SubscriberInterest {
		return l.inner.RegisterCallsite(metadata)
	})
}

// Enabled is `impl Subscriber for Layered`.
func (l *LayerLayered) Enabled(metadata *tracing.Metadata) bool {
	if l.layer.Enabled(metadata, l.ctx()) {
		// if the outer layer enables the callsite metadata, ask the subscriber.
		return l.inner.Enabled(metadata)
	}
	// otherwise, the callsite is disabled by the layer
	return false
}

// MaxLevelHint is `impl Subscriber for Layered`.
func (l *LayerLayered) MaxLevelHint() rs.Option[tracing.LevelFilter] {
	return l.pickLevelHint(l.layer.MaxLevelHint(), l.inner.MaxLevelHint(), subscriberIsNone(l.inner))
}

// NewSpan is `impl Subscriber for Layered`.
func (l *LayerLayered) NewSpan(span *tracing.SpanAttributes) tracing.SpanId {
	id := l.inner.NewSpan(span)
	l.layer.OnNewSpan(span, id, l.ctx())
	return id
}

// Record is `impl Subscriber for Layered`.
func (l *LayerLayered) Record(span tracing.SpanId, values *tracing.SpanRecord) {
	l.inner.Record(span, values)
	l.layer.OnRecord(span, values, l.ctx())
}

// RecordFollowsFrom is `impl Subscriber for Layered`.
func (l *LayerLayered) RecordFollowsFrom(span tracing.SpanId, follows tracing.SpanId) {
	l.inner.RecordFollowsFrom(span, follows)
	l.layer.OnFollowsFrom(span, follows, l.ctx())
}

// EventEnabled is `impl Subscriber for Layered`.
func (l *LayerLayered) EventEnabled(event *tracing.Event) bool {
	if l.layer.EventEnabled(event, l.ctx()) {
		// if the outer layer enables the event, ask the inner subscriber.
		return l.inner.EventEnabled(event)
	}
	// otherwise, the event is disabled by this layer
	return false
}

// Event is `impl Subscriber for Layered`.
func (l *LayerLayered) Event(event *tracing.Event) {
	l.inner.Event(event)
	l.layer.OnEvent(event, l.ctx())
}

// Enter is `impl Subscriber for Layered`.
func (l *LayerLayered) Enter(span tracing.SpanId) {
	l.inner.Enter(span)
	l.layer.OnEnter(span, l.ctx())
}

// Exit is `impl Subscriber for Layered`.
func (l *LayerLayered) Exit(span tracing.SpanId) {
	l.inner.Exit(span)
	l.layer.OnExit(span, l.ctx())
}

// CloneSpan is `impl Subscriber for Layered`.
func (l *LayerLayered) CloneSpan(old tracing.SpanId) tracing.SpanId {
	new := l.inner.CloneSpan(old)
	if new != old {
		l.layer.OnIdChange(old, new, l.ctx())
	}
	return new
}

// DropSpan is `impl Subscriber for Layered`.
func (l *LayerLayered) DropSpan(id tracing.SpanId) {
	l.TryClose(id)
}

// TryClose is `impl Subscriber for Layered`: when the inner subscriber says
// the span closed, the layer is told, with the span's data still in the
// registry until the outermost close is done.
func (l *LayerLayered) TryClose(id tracing.SpanId) bool {
	var guard *closeGuard
	if registry, ok := subscriberDowncastRegistry(l.inner); ok {
		guard = registry.startClose(id)
		defer guard.drop()
	}
	if l.inner.TryClose(id) {
		// If we have a registry's close guard, indicate that the span is
		// closing.
		if guard != nil {
			guard.setClosing()
		}
		l.layer.OnClose(id, l.ctx())
		return true
	}
	return false
}

// CurrentSpan is `impl Subscriber for Layered`.
func (l *LayerLayered) CurrentSpan() tracing.CurrentSpan {
	return l.inner.CurrentSpan()
}

// DowncastRaw is `impl Subscriber for Layered`: the Layered itself, or a part
// of the layer, or a part of the inner subscriber.
func (l *LayerLayered) DowncastRaw(target reflect.Type) (any, bool) {
	// If downcasting to `Self`, return a pointer to `self`.
	if target == reflect.TypeFor[*LayerLayered]() {
		return l, true
	}
	if raw, ok := l.layer.DowncastRaw(target); ok {
		return raw, true
	}
	return subscriberDowncastRaw(l.inner, target)
}

// SpanData is `impl LookupSpan for Layered`.
func (l *LayerLayered) SpanData(id tracing.SpanId) rs.Option[*RegistryData] {
	if lookup, ok := l.inner.(RegistryLookupSpan); ok {
		return lookup.SpanData(id)
	}
	return rs.None[*RegistryData]()
}

func (l *LayerLayered) ctx() LayerContext {
	return layerContextNew(l.inner)
}

func (l *LayerLayered) pickInterest(outer tracing.SubscriberInterest, inner func() tracing.SubscriberInterest) tracing.SubscriberInterest {
	if l.hasLayerFilter {
		return inner()
	}

	// If the outer layer has disabled the callsite, return now so that
	// the inner layer/subscriber doesn't get its hopes up.
	if outer.IsNever() {
		return outer
	}

	// The `inner` closure will call `inner.register_callsite()`. We do this
	// before the `if` statement to  ensure that the inner subscriber is
	// informed that the callsite exists regardless of the outer layer's
	// filtering decision.
	innerInterest := inner()
	if outer.IsSometimes() {
		// if this interest is "sometimes", return "sometimes" to ensure that
		// filters are reevaluated.
		return outer
	}

	// If there is a per-layer filter in the `inner` stack, and it returns
	// `never`, change the interest to `sometimes`, because the `outer`
	// layer didn't return `never`. This means that _some_ layer still wants
	// to see that callsite, even though the inner stack's per-layer filter
	// didn't want it. Therefore, returning `sometimes` will ensure
	// `enabled` is called so that the per-layer filter can skip that
	// span/event, while the `outer` layer still gets to see it.
	if innerInterest.IsNever() && l.innerHasLayerFilter {
		return tracing.SubscriberInterestSometimes()
	}

	// otherwise, allow the inner subscriber or subscriber to weigh in.
	return innerInterest
}

func (l *LayerLayered) pickLevelHint(outerHint, innerHint rs.Option[tracing.LevelFilter], innerIsNone bool) rs.Option[tracing.LevelFilter] {
	if l.innerIsRegistry {
		return outerHint
	}

	if l.hasLayerFilter && l.innerHasLayerFilter {
		outer, ok := outerHint.Get()
		if !ok {
			return rs.None[tracing.LevelFilter]()
		}
		inner, ok := innerHint.Get()
		if !ok {
			return rs.None[tracing.LevelFilter]()
		}
		return rs.Some(max(outer, inner))
	}

	if l.hasLayerFilter && innerHint.IsNone() {
		return rs.None[tracing.LevelFilter]()
	}

	if l.innerHasLayerFilter && outerHint.IsNone() {
		return rs.None[tracing.LevelFilter]()
	}

	// If the layer is `Option::None`, then we want to short-circuit the
	// layer underneath, if it returns `None`, to override the `None` layer
	// returning `Some(OFF)`. An `Option` layer is not ported, so no layer
	// is `None`.

	// Similarly, if the layer on the inside is `None` and it returned an
	// `Off` hint, we want to override that with the outer hint.
	if innerIsNone && innerHint == rs.Some(tracing.LevelFilterOff) {
		return outerHint
	}

	return optionMax(outerHint, innerHint)
}

// optionMax is cmp::max of two Option<LevelFilter>: None is less than Some.
func optionMax(a, b rs.Option[tracing.LevelFilter]) rs.Option[tracing.LevelFilter] {
	x, aOk := a.Get()
	y, bOk := b.Get()
	switch {
	case !aOk:
		return b
	case !bOk:
		return a
	case y >= x:
		return b
	}
	return a
}

// subscriberIsNone is layer::subscriber_is_none: whether the subscriber is
// an `Option` that is None. An `Option` subscriber is not ported.
func subscriberIsNone(subscriber tracing.Subscriber) bool {
	return false
}

// subscriberDowncastRaw is Subscriber::downcast_raw for any subscriber: the
// subscriber itself when it is of the target type, or the part of it that
// is.
func subscriberDowncastRaw(subscriber tracing.Subscriber, target reflect.Type) (any, bool) {
	if reflect.TypeOf(subscriber) == target {
		return subscriber, true
	}
	if parts, ok := subscriber.(tracing.SubscriberDowncaster); ok {
		return parts.DowncastRaw(target)
	}
	return nil, false
}

// subscriberDowncastRegistry is `subscriber.downcast_ref::<Registry>()`.
func subscriberDowncastRegistry(subscriber tracing.Subscriber) (*Registry, bool) {
	raw, ok := subscriberDowncastRaw(subscriber, reflect.TypeFor[*Registry]())
	if !ok {
		return nil, false
	}
	registry, ok := raw.(*Registry)
	return registry, ok
}

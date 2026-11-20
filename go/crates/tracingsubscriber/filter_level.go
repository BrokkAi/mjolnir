// Port of tracing-subscriber 0.3.23 src/filter/level.rs

package tracingsubscriber

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// FilterLevelFilter is `impl Layer for LevelFilter`: a level filter used as
// a layer, which lets through what is at most as verbose as the level. Go
// cannot add methods to tracing.LevelFilter from here, so the layer wraps it;
// it downcasts to the tracing.LevelFilter as the Rust layer does.
type FilterLevelFilter struct {
	Filter tracing.LevelFilter
}

func (l FilterLevelFilter) OnRegisterDispatch(subscriber *tracing.Dispatch) {
	LayerDefaultOnRegisterDispatch(l, subscriber)
}

func (l FilterLevelFilter) OnLayer(subscriber tracing.Subscriber) { LayerDefaultOnLayer(l, subscriber) }

// RegisterCallsite is `impl Layer for LevelFilter`.
func (l FilterLevelFilter) RegisterCallsite(metadata *tracing.Metadata) tracing.SubscriberInterest {
	if l.Filter.Enables(metadata.Level()) {
		return tracing.SubscriberInterestAlways()
	}
	return tracing.SubscriberInterestNever()
}

// Enabled is `impl Layer for LevelFilter`.
func (l FilterLevelFilter) Enabled(metadata *tracing.Metadata, ctx LayerContext) bool {
	return l.Filter.Enables(metadata.Level())
}

func (l FilterLevelFilter) OnNewSpan(attrs *tracing.SpanAttributes, id tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnNewSpan(l, attrs, id, ctx)
}

// MaxLevelHint is `impl Layer for LevelFilter`.
func (l FilterLevelFilter) MaxLevelHint() rs.Option[tracing.LevelFilter] {
	return rs.Some(l.Filter)
}

func (l FilterLevelFilter) OnRecord(span tracing.SpanId, values *tracing.SpanRecord, ctx LayerContext) {
	LayerDefaultOnRecord(l, span, values, ctx)
}

func (l FilterLevelFilter) OnFollowsFrom(span tracing.SpanId, follows tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnFollowsFrom(l, span, follows, ctx)
}

func (l FilterLevelFilter) EventEnabled(event *tracing.Event, ctx LayerContext) bool {
	return LayerDefaultEventEnabled(l, event, ctx)
}

func (l FilterLevelFilter) OnEvent(event *tracing.Event, ctx LayerContext) {
	LayerDefaultOnEvent(l, event, ctx)
}

func (l FilterLevelFilter) OnEnter(id tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnEnter(l, id, ctx)
}

func (l FilterLevelFilter) OnExit(id tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnExit(l, id, ctx)
}

func (l FilterLevelFilter) OnClose(id tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnClose(l, id, ctx)
}

func (l FilterLevelFilter) OnIdChange(old tracing.SpanId, new tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnIdChange(l, old, new, ctx)
}

// DowncastRaw is the default Layer::downcast_raw, the layer being the
// LevelFilter.
func (l FilterLevelFilter) DowncastRaw(target reflect.Type) (any, bool) {
	if target == reflect.TypeFor[tracing.LevelFilter]() {
		return l.Filter, true
	}
	return LayerDefaultDowncastRaw(l, target)
}

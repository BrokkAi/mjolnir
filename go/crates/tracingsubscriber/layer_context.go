// Port of tracing-subscriber 0.3.23 src/layer/context.rs

package tracingsubscriber

import (
	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// LayerContext is tracing_subscriber::layer::Context: what a layer is given
// to look at the subscriber it is layered over. The span lookups need that
// subscriber to keep span data (RegistryLookupSpan); over any other they
// find nothing.
type LayerContext struct {
	// subscriber is nil for Context::none.
	subscriber tracing.Subscriber
}

func layerContextNew(subscriber tracing.Subscriber) LayerContext {
	return LayerContext{subscriber: subscriber}
}

// layerContextNone is Context::none.
func layerContextNone() LayerContext {
	return LayerContext{}
}

func (c LayerContext) lookup() (RegistryLookupSpan, bool) {
	if c.subscriber == nil {
		return nil, false
	}
	lookup, ok := c.subscriber.(RegistryLookupSpan)
	return lookup, ok
}

// CurrentSpan is Context::current_span.
func (c LayerContext) CurrentSpan() tracing.CurrentSpan {
	if c.subscriber == nil {
		return tracing.CurrentSpanNone()
	}
	return c.subscriber.CurrentSpan()
}

// Enabled is Context::enabled.
func (c LayerContext) Enabled(metadata *tracing.Metadata) bool {
	if c.subscriber == nil {
		return true
	}
	return c.subscriber.Enabled(metadata)
}

// Event is Context::event.
func (c LayerContext) Event(event *tracing.Event) {
	if c.subscriber != nil {
		c.subscriber.Event(event)
	}
}

// EventSpan is Context::event_span: the span an event belongs to.
func (c LayerContext) EventSpan(event *tracing.Event) rs.Option[*RegistrySpanRef] {
	if event.IsRoot() {
		return rs.None[*RegistrySpanRef]()
	} else if event.IsContextual() {
		return c.LookupCurrent()
	}
	if id, ok := event.Parent().Get(); ok {
		return c.Span(id)
	}
	return rs.None[*RegistrySpanRef]()
}

// Metadata is Context::metadata.
func (c LayerContext) Metadata(id tracing.SpanId) rs.Option[*tracing.Metadata] {
	span, ok := c.Span(id).Get()
	if !ok {
		return rs.None[*tracing.Metadata]()
	}
	return rs.Some(span.Metadata())
}

// Span is Context::span.
func (c LayerContext) Span(id tracing.SpanId) rs.Option[*RegistrySpanRef] {
	lookup, ok := c.lookup()
	if !ok {
		return rs.None[*RegistrySpanRef]()
	}
	return RegistrySpan(lookup, id)
}

// Exists is Context::exists.
func (c LayerContext) Exists(id tracing.SpanId) bool {
	return c.Span(id).IsSome()
}

// LookupCurrent is Context::lookup_current: the current span's data.
func (c LayerContext) LookupCurrent() rs.Option[*RegistrySpanRef] {
	lookup, ok := c.lookup()
	if !ok {
		return rs.None[*RegistrySpanRef]()
	}
	current := c.subscriber.CurrentSpan()
	id, ok := current.Id().Get()
	if !ok {
		return rs.None[*RegistrySpanRef]()
	}
	span := RegistrySpan(lookup, id)
	rs.DebugAssert(span.IsSome(), "the subscriber should have data for the current span ({:?})!", id)
	return span
}

// SpanScope is Context::span_scope.
func (c LayerContext) SpanScope(id tracing.SpanId) rs.Option[*RegistryScope] {
	span, ok := c.Span(id).Get()
	if !ok {
		return rs.None[*RegistryScope]()
	}
	return rs.Some(span.Scope())
}

// EventScope is Context::event_scope.
func (c LayerContext) EventScope(event *tracing.Event) rs.Option[*RegistryScope] {
	span, ok := c.EventSpan(event).Get()
	if !ok {
		return rs.None[*RegistryScope]()
	}
	return rs.Some(span.Scope())
}

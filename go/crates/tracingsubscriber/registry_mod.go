// Port of tracing-subscriber 0.3.23 src/registry/mod.rs

package tracingsubscriber

import (
	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// RegistryLookupSpan is tracing_subscriber::registry::LookupSpan: a
// subscriber that keeps data about the spans it has made. Registry is the
// one implementation; a Layered over it forwards to it.
type RegistryLookupSpan interface {
	// SpanData returns the data of the span with this ID, if it is open.
	SpanData(id tracing.SpanId) rs.Option[*RegistryData]
}

// RegistrySpan is LookupSpan::span: a reference to the span with this ID,
// if it is open.
func RegistrySpan(lookup RegistryLookupSpan, id tracing.SpanId) rs.Option[*RegistrySpanRef] {
	data, ok := lookup.SpanData(id).Get()
	if !ok {
		return rs.None[*RegistrySpanRef]()
	}
	return rs.Some(&RegistrySpanRef{registry: lookup, data: data})
}

// RegistrySpanRef is tracing_subscriber::registry::SpanRef: a reference to
// a span's data, with the registry it came from.
type RegistrySpanRef struct {
	registry RegistryLookupSpan
	data     *RegistryData
}

// RegistryScope is tracing_subscriber::registry::Scope: an iterator over a
// span and its parents, innermost first.
type RegistryScope struct {
	registry RegistryLookupSpan
	next     rs.Option[tracing.SpanId]
}

// RegistryScopeFromRoot is tracing_subscriber::registry::ScopeFromRoot: the
// same spans, outermost first.
type RegistryScopeFromRoot struct {
	spans []*RegistrySpanRef
}

// FromRoot is Scope::from_root.
func (s *RegistryScope) FromRoot() *RegistryScopeFromRoot {
	var spans []*RegistrySpanRef
	for span, ok := s.Next().Get(); ok; span, ok = s.Next().Get() {
		spans = append(spans, span)
	}
	for i, j := 0, len(spans)-1; i < j; i, j = i+1, j-1 {
		spans[i], spans[j] = spans[j], spans[i]
	}
	return &RegistryScopeFromRoot{spans: spans}
}

// Next is `impl Iterator for Scope`.
func (s *RegistryScope) Next() rs.Option[*RegistrySpanRef] {
	for {
		id, ok := s.next.Take().Get()
		if !ok {
			return rs.None[*RegistrySpanRef]()
		}
		curr, ok := RegistrySpan(s.registry, id).Get()
		if !ok {
			return rs.None[*RegistrySpanRef]()
		}
		s.next = curr.data.Parent()
		return rs.Some(curr)
	}
}

// Next is `impl Iterator for ScopeFromRoot`.
func (s *RegistryScopeFromRoot) Next() rs.Option[*RegistrySpanRef] {
	if len(s.spans) == 0 {
		return rs.None[*RegistrySpanRef]()
	}
	span := s.spans[0]
	s.spans = s.spans[1:]
	return rs.Some(span)
}

// Id is SpanRef::id.
func (s *RegistrySpanRef) Id() tracing.SpanId { return s.data.Id() }

// Metadata is SpanRef::metadata.
func (s *RegistrySpanRef) Metadata() *tracing.Metadata { return s.data.Metadata() }

// Name is SpanRef::name.
func (s *RegistrySpanRef) Name() string { return s.data.Metadata().Name() }

// Fields is SpanRef::fields.
func (s *RegistrySpanRef) Fields() *tracing.FieldFieldSet { return s.data.Metadata().Fields() }

// Parent is SpanRef::parent: the span's parent, if it is still open.
func (s *RegistrySpanRef) Parent() rs.Option[*RegistrySpanRef] {
	id, ok := s.data.Parent().Get()
	if !ok {
		return rs.None[*RegistrySpanRef]()
	}
	data, ok := s.registry.SpanData(id).Get()
	if !ok {
		return rs.None[*RegistrySpanRef]()
	}
	return rs.Some(&RegistrySpanRef{registry: s.registry, data: data})
}

// Scope is SpanRef::scope: this span and its parents, innermost first.
func (s *RegistrySpanRef) Scope() *RegistryScope {
	return &RegistryScope{registry: s.registry, next: rs.Some(s.Id())}
}

// Extensions is SpanRef::extensions: the span's extensions, read-locked
// until the result is dropped.
func (s *RegistrySpanRef) Extensions() *RegistryExtensions { return s.data.Extensions() }

// ExtensionsMut is SpanRef::extensions_mut: the span's extensions,
// write-locked until the result is dropped.
func (s *RegistrySpanRef) ExtensionsMut() *RegistryExtensionsMut { return s.data.ExtensionsMut() }

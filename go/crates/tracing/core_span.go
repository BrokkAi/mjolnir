// Port of tracing-core 0.1.36 src/span.rs

package tracing

import "github.com/BrokkAi/mjolnir/go/rs"

// SpanId is tracing::span::Id: what a subscriber calls a span. It is never
// zero.
type SpanId uint64

// SpanAttributes is tracing::span::Attributes: what a new span is made
// from.
type SpanAttributes struct {
	metadata *Metadata
	values   *FieldValueSet
	parent   parent
}

// SpanRecord is tracing::span::Record: values recorded on a span after it
// was made.
type SpanRecord struct {
	values *FieldValueSet
}

// CurrentSpan is tracing::span::Current: what a subscriber says the current
// span is. (The name Rust's path gives, SpanCurrent, is the function
// Span::current.)
type CurrentSpan struct {
	kind     currentKind
	id       SpanId
	metadata *Metadata
}

type currentKind uint8

const (
	// currentNone: there is no current span.
	currentNone currentKind = iota
	// currentCurrent: this is the current span.
	currentCurrent
	// currentUnknown: the subscriber does not track the current span.
	currentUnknown
)

// ===== impl Span =====

// SpanIdFromU64 is Id::from_u64. It panics for zero.
func SpanIdFromU64(u uint64) SpanId {
	if u == 0 {
		rs.PanicAt(1, "span IDs must be > 0")
	}
	return SpanId(u)
}

// IntoU64 is Id::into_u64.
func (id SpanId) IntoU64() uint64 { return uint64(id) }

// FmtDebug is the derived Debug of Id.
func (id SpanId) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("Id").Field(uint64(id)).Finish()
}

// ===== impl Attributes =====

// SpanAttributesNew is Attributes::new: a span whose parent is the current
// span.
func SpanAttributesNew(metadata *Metadata, values *FieldValueSet) *SpanAttributes {
	return &SpanAttributes{metadata: metadata, values: values, parent: parent{kind: parentCurrent}}
}

// SpanAttributesNewRoot is Attributes::new_root: a span with no parent.
func SpanAttributesNewRoot(metadata *Metadata, values *FieldValueSet) *SpanAttributes {
	return &SpanAttributes{metadata: metadata, values: values, parent: parent{kind: parentRoot}}
}

// SpanAttributesChildOf is Attributes::child_of: a span with the given
// parent.
func SpanAttributesChildOf(parentId SpanId, metadata *Metadata, values *FieldValueSet) *SpanAttributes {
	return &SpanAttributes{metadata: metadata, values: values, parent: parent{kind: parentExplicit, id: parentId}}
}

// Metadata returns the metadata of the span's callsite.
func (a *SpanAttributes) Metadata() *Metadata { return a.metadata }

// Values returns the values the span starts with.
func (a *SpanAttributes) Values() *FieldValueSet { return a.values }

// IsRoot reports whether the span was made a root explicitly.
func (a *SpanAttributes) IsRoot() bool { return a.parent.kind == parentRoot }

// IsContextual reports whether the span's parent is the current span.
func (a *SpanAttributes) IsContextual() bool { return a.parent.kind == parentCurrent }

// Parent returns the span's explicit parent, if it has one.
func (a *SpanAttributes) Parent() rs.Option[SpanId] {
	if a.parent.kind == parentExplicit {
		return rs.Some(a.parent.id)
	}
	return rs.None[SpanId]()
}

// Record visits the values the span starts with.
func (a *SpanAttributes) Record(visitor FieldVisit) { a.values.Record(visitor) }

// Contains reports whether the span starts with a value for field.
func (a *SpanAttributes) Contains(field *FieldField) bool { return a.values.Contains(field) }

// IsEmpty reports whether the span starts with no values.
func (a *SpanAttributes) IsEmpty() bool { return a.values.IsEmpty() }

// Fields returns the fields of the span's callsite.
func (a *SpanAttributes) Fields() *FieldFieldSet { return a.values.FieldSet() }

// ===== impl Record =====

// SpanRecordNew is Record::new.
func SpanRecordNew(values *FieldValueSet) *SpanRecord { return &SpanRecord{values: values} }

// Record visits the recorded values.
func (r *SpanRecord) Record(visitor FieldVisit) { r.values.Record(visitor) }

// Len returns the number of fields the record has entries for.
func (r *SpanRecord) Len() int { return r.values.Len() }

// Contains reports whether the record holds a value for field.
func (r *SpanRecord) Contains(field *FieldField) bool { return r.values.Contains(field) }

// IsEmpty reports whether the record holds no values.
func (r *SpanRecord) IsEmpty() bool { return r.values.IsEmpty() }

// ===== impl Current =====

// CurrentSpanNew is Current::new: the span with this ID and metadata is
// current.
func CurrentSpanNew(id SpanId, metadata *Metadata) CurrentSpan {
	return CurrentSpan{kind: currentCurrent, id: id, metadata: metadata}
}

// CurrentSpanNone is Current::none: no span is current.
func CurrentSpanNone() CurrentSpan { return CurrentSpan{kind: currentNone} }

func currentSpanUnknown() CurrentSpan { return CurrentSpan{kind: currentUnknown} }

// IsKnown is Current::is_known: whether the subscriber tracks a current
// span at all.
func (c CurrentSpan) IsKnown() bool { return c.kind != currentUnknown }

// IntoInner is Current::into_inner: the current span's ID and metadata.
func (c CurrentSpan) IntoInner() rs.Option[rs.Tuple2[SpanId, *Metadata]] {
	if c.kind == currentCurrent {
		return rs.Some(rs.T2(c.id, c.metadata))
	}
	return rs.None[rs.Tuple2[SpanId, *Metadata]]()
}

// Id is Current::id.
func (c CurrentSpan) Id() rs.Option[SpanId] {
	if c.kind == currentCurrent {
		return rs.Some(c.id)
	}
	return rs.None[SpanId]()
}

// Metadata is Current::metadata.
func (c CurrentSpan) Metadata() rs.Option[*Metadata] {
	if c.kind == currentCurrent {
		return rs.Some(c.metadata)
	}
	return rs.None[*Metadata]()
}

// Port of tracing 0.1.44 src/span.rs

package tracing

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Span is tracing::Span: a handle on a span, a period of time with a
// beginning and an end, which a subscriber records.
//
// A Span is used by pointer and owns what Rust's owns: a reference the
// subscriber counts. Clone takes another reference and Drop gives it back;
// when the last reference is dropped the subscriber closes the span. Drop
// every span you make (`defer span.Drop()`), and drop a clone separately.
// Drop is idempotent.
//
// The log records tracing's `log` feature writes about spans are not
// produced: they reach a logger only when no subscriber was ever set, and
// Mjolnir installs none.
type Span struct {
	// inner is Span::inner: nil for a disabled span.
	inner *spanInner
	// meta is Span::meta: nil for Span::none.
	meta    *Metadata
	dropped bool
}

// spanInner is the crate's Inner: the span's ID and the dispatcher that
// knows it.
type spanInner struct {
	id         SpanId
	subscriber *Dispatch
}

// SpanEntered is tracing::span::Entered: the guard Span.Enter returns.
// Dropping it exits the span.
type SpanEntered struct {
	span   *Span
	exited bool
}

// SpanEnteredSpan is tracing::span::EnteredSpan: a span that owns its own
// entering, as Span.Entered returns it. Dropping it exits and drops the span.
type SpanEnteredSpan struct {
	span   *Span
	exited bool
}

// ===== impl Span =====

// SpanNew is Span::new: a span with the current span as its parent, recorded
// by the current default subscriber.
func SpanNew(meta *Metadata, values *FieldValueSet) *Span {
	return DispatcherGetDefault(func(dispatch *Dispatch) *Span {
		return SpanNewWith(meta, values, dispatch)
	})
}

// SpanNewWith is Span::new_with.
func SpanNewWith(meta *Metadata, values *FieldValueSet, dispatch *Dispatch) *Span {
	newSpan := SpanAttributesNew(meta, values)
	return spanMakeWith(meta, newSpan, dispatch)
}

// SpanNewRoot is Span::new_root: a span with no parent.
func SpanNewRoot(meta *Metadata, values *FieldValueSet) *Span {
	return DispatcherGetDefault(func(dispatch *Dispatch) *Span {
		return SpanNewRootWith(meta, values, dispatch)
	})
}

// SpanNewRootWith is Span::new_root_with.
func SpanNewRootWith(meta *Metadata, values *FieldValueSet, dispatch *Dispatch) *Span {
	newSpan := SpanAttributesNewRoot(meta, values)
	return spanMakeWith(meta, newSpan, dispatch)
}

// SpanChildOf is Span::child_of: a span with the given parent, or a root
// span for None.
func SpanChildOf(parent rs.Option[SpanId], meta *Metadata, values *FieldValueSet) *Span {
	return DispatcherGetDefault(func(dispatch *Dispatch) *Span {
		return SpanChildOfWith(parent, meta, values, dispatch)
	})
}

// SpanChildOfWith is Span::child_of_with.
func SpanChildOfWith(parent rs.Option[SpanId], meta *Metadata, values *FieldValueSet, dispatch *Dispatch) *Span {
	var newSpan *SpanAttributes
	if id, ok := parent.Get(); ok {
		newSpan = SpanAttributesChildOf(id, meta, values)
	} else {
		newSpan = SpanAttributesNewRoot(meta, values)
	}
	return spanMakeWith(meta, newSpan, dispatch)
}

// SpanNewDisabled is Span::new_disabled: a span that records nothing but
// knows its metadata.
func SpanNewDisabled(meta *Metadata) *Span {
	return &Span{meta: meta}
}

// SpanNone is Span::none: a span that records nothing and has no metadata.
func SpanNone() *Span {
	return &Span{}
}

// SpanCurrent is Span::current: a new handle on the span the current
// subscriber considers current, or SpanNone.
func SpanCurrent() *Span {
	return DispatcherGetDefault(func(dispatch *Dispatch) *Span {
		if current, ok := dispatch.CurrentSpan().IntoInner().Get(); ok {
			id := dispatch.CloneSpan(current.F0)
			return &Span{
				inner: &spanInner{id: id, subscriber: dispatch.Clone()},
				meta:  current.F1,
			}
		}
		return SpanNone()
	})
}

func spanMakeWith(meta *Metadata, newSpan *SpanAttributes, dispatch *Dispatch) *Span {
	id := dispatch.NewSpan(newSpan)
	return &Span{
		inner: &spanInner{id: id, subscriber: dispatch.Clone()},
		meta:  meta,
	}
}

// Enter is Span::enter: it enters the span until the guard is dropped.
//
//	entered := span.Enter()
//	defer entered.Drop()
func (s *Span) Enter() *SpanEntered {
	s.doEnter()
	return &SpanEntered{span: s}
}

// Entered is Span::entered: it enters the span, and the result owns the
// span. Its Exit gives the span back; dropping it exits and drops the span.
func (s *Span) Entered() *SpanEnteredSpan {
	s.doEnter()
	return &SpanEnteredSpan{span: s}
}

// OrCurrent is Span::or_current: the span, or the current span when this one
// is disabled.
func (s *Span) OrCurrent() *Span {
	if s.IsDisabled() {
		s.Drop()
		return SpanCurrent()
	}
	return s
}

func (s *Span) doEnter() {
	if s.inner != nil {
		s.inner.subscriber.Enter(s.inner.id)
	}
}

// Called from Entered and EnteredSpan drops.
//
// Running this behaviour on drop rather than with an explicit function call
// means that spans may still be exited when unwinding.
func (s *Span) doExit() {
	if s.inner != nil {
		s.inner.subscriber.Exit(s.inner.id)
	}
}

// SpanInScope is Span::in_scope: f called with the span entered.
func SpanInScope[T any](s *Span, f func() T) T {
	entered := s.Enter()
	defer entered.Drop()
	return f()
}

// InScope is Span::in_scope for a closure with no result.
func (s *Span) InScope(f func()) {
	entered := s.Enter()
	defer entered.Drop()
	f()
}

// Field is Span::field: the span's field of that name, if its callsite has
// one.
func (s *Span) Field(name string) rs.Option[FieldField] {
	if s.meta == nil {
		return rs.None[FieldField]()
	}
	return s.meta.Fields().Field(name)
}

// HasField is Span::has_field.
func (s *Span) HasField(name string) bool {
	return s.Field(name).IsSome()
}

// Record is Span::record: it records value for the field named name, which
// the span's callsite must declare (FieldEmpty declares a field to record
// later). A name the callsite does not declare is ignored.
func (s *Span) Record(name string, value any) *Span {
	if s.meta != nil {
		if field, ok := s.meta.Fields().Field(name).Get(); ok {
			s.RecordAll(s.meta.Fields().ValueSet([]FieldField{field}, []any{value}))
		}
	}
	return s
}

// RecordAll is Span::record_all.
func (s *Span) RecordAll(values *FieldValueSet) *Span {
	record := SpanRecordNew(values)
	if s.inner != nil {
		s.inner.subscriber.Record(s.inner.id, record)
	}
	return s
}

// IsDisabled is Span::is_disabled: whether the span records nothing.
func (s *Span) IsDisabled() bool {
	return s.inner == nil
}

// IsNone is Span::is_none: whether the span is SpanNone.
func (s *Span) IsNone() bool {
	return s.IsDisabled() && s.meta == nil
}

// FollowsFrom is Span::follows_from.
func (s *Span) FollowsFrom(from rs.Option[SpanId]) *Span {
	if s.inner != nil {
		if id, ok := from.Get(); ok {
			s.inner.subscriber.RecordFollowsFrom(s.inner.id, id)
		}
	}
	return s
}

// Id is Span::id.
func (s *Span) Id() rs.Option[SpanId] {
	if s.inner == nil {
		return rs.None[SpanId]()
	}
	return rs.Some(s.inner.id)
}

// Metadata is Span::metadata.
func (s *Span) Metadata() rs.Option[*Metadata] {
	if s.meta == nil {
		return rs.None[*Metadata]()
	}
	return rs.Some(s.meta)
}

// SpanWithSubscriber is Span::with_subscriber: f called with the span's ID
// and dispatcher, when it has them.
func SpanWithSubscriber[T any](s *Span, f func(id SpanId, dispatch *Dispatch) T) rs.Option[T] {
	if s.inner == nil {
		return rs.None[T]()
	}
	return rs.Some(f(s.inner.id, s.inner.subscriber))
}

// Equal is `impl PartialEq for Span`: two handles on one span of one
// callsite.
func (s *Span) Equal(other *Span) bool {
	if s.meta == nil || other.meta == nil {
		return false
	}
	if s.meta.Callsite() != other.meta.Callsite() {
		return false
	}
	if s.inner == nil || other.inner == nil {
		return s.inner == nil && other.inner == nil
	}
	return s.inner.id == other.inner.id
}

// FmtDebug is `impl Debug for Span`.
func (s *Span) FmtDebug(f *rs.Formatter) error {
	span := f.DebugStruct("Span")
	if meta := s.meta; meta != nil {
		span.Field("name", meta.Name()).
			Field("level", meta.Level()).
			Field("target", meta.Target())

		if s.inner != nil {
			span.Field("id", s.inner.id)
		} else {
			span.Field("disabled", true)
		}

		if path, ok := meta.ModulePath().Get(); ok {
			span.Field("module_path", path)
		}
		if line, ok := meta.Line().Get(); ok {
			span.Field("line", line)
		}
		if file, ok := meta.File().Get(); ok {
			span.Field("file", file)
		}
	} else {
		span.Field("none", true)
	}

	return span.Finish()
}

// Clone is `impl Clone for Span`: a new handle on the same span, which the
// subscriber counts.
func (s *Span) Clone() *Span {
	clone := &Span{meta: s.meta}
	if s.inner != nil {
		clone.inner = s.inner.clone()
	}
	return clone
}

// Drop is `impl Drop for Span`: it gives the handle's reference back to the
// subscriber, which closes the span when it was the last.
func (s *Span) Drop() {
	if s.dropped {
		return
	}
	s.dropped = true
	if s.inner != nil {
		s.inner.subscriber.TryClose(s.inner.id)
	}
}

// ===== impl Inner =====

func (i *spanInner) clone() *spanInner {
	return &spanInner{
		id:         i.subscriber.CloneSpan(i.id),
		subscriber: i.subscriber.Clone(),
	}
}

// ===== impl Entered =====

// Id is EnteredSpan::id.
func (e *SpanEnteredSpan) Id() rs.Option[SpanId] {
	return e.span.Id()
}

// Deref is `impl Deref for EnteredSpan`: the span.
func (e *SpanEnteredSpan) Deref() *Span {
	return e.span
}

// Exit is EnteredSpan::exit: it exits the span and gives it back.
func (e *SpanEnteredSpan) Exit() *Span {
	span := e.span
	e.span = SpanNone()
	e.exited = true
	span.doExit()
	return span
}

// Drop is `impl Drop for Entered`: it exits the span.
func (e *SpanEntered) Drop() {
	if e.exited {
		return
	}
	e.exited = true
	e.span.doExit()
}

// Drop is `impl Drop for EnteredSpan`: it exits the span, then drops it.
func (e *SpanEnteredSpan) Drop() {
	if e.exited {
		return
	}
	e.exited = true
	e.span.doExit()
	e.span.Drop()
}

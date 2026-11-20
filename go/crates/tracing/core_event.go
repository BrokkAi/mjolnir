// Port of tracing-core 0.1.36 src/event.rs and src/parent.rs

package tracing

import "github.com/BrokkAi/mjolnir/go/rs"

// parent is the crate's Parent: where a span or event hangs in the tree.
type parent struct {
	kind parentKind
	id   SpanId
}

type parentKind uint8

const (
	// parentCurrent: rooted in the current span.
	parentCurrent parentKind = iota
	// parentRoot: a root.
	parentRoot
	// parentExplicit: an explicitly-specified parent.
	parentExplicit
)

// Event is tracing::Event: something that happened at one moment, with
// values for the fields of its callsite.
type Event struct {
	fields   *FieldValueSet
	metadata *Metadata
	parent   parent
}

// EventDispatch is Event::dispatch: it makes an event in the current span
// and sends it to the current default dispatcher.
func EventDispatch(metadata *Metadata, fields *FieldValueSet) {
	event := EventNew(metadata, fields)
	DispatcherGetDefault(func(current *Dispatch) rs.Unit {
		current.Event(event)
		return rs.Unit{}
	})
}

// EventNew is Event::new: an event whose parent is the current span.
func EventNew(metadata *Metadata, fields *FieldValueSet) *Event {
	return &Event{fields: fields, metadata: metadata, parent: parent{kind: parentCurrent}}
}

// EventNewChildOf is Event::new_child_of: an event whose parent is the given
// span, or a root event for None.
func EventNewChildOf(parentId rs.Option[SpanId], metadata *Metadata, fields *FieldValueSet) *Event {
	event := &Event{fields: fields, metadata: metadata, parent: parent{kind: parentRoot}}
	if id, ok := parentId.Get(); ok {
		event.parent = parent{kind: parentExplicit, id: id}
	}
	return event
}

// EventChildOf is Event::child_of: it makes an event with the given parent
// and sends it to the current default dispatcher.
func EventChildOf(parentId rs.Option[SpanId], metadata *Metadata, fields *FieldValueSet) {
	event := EventNewChildOf(parentId, metadata, fields)
	DispatcherGetDefault(func(current *Dispatch) rs.Unit {
		current.Event(event)
		return rs.Unit{}
	})
}

// Record visits the event's values.
func (e *Event) Record(visitor FieldVisit) {
	e.fields.Record(visitor)
}

// Fields returns the fields of the event's callsite.
func (e *Event) Fields() []FieldField {
	return e.fields.FieldSet().Iter()
}

// Metadata returns the metadata of the event's callsite.
func (e *Event) Metadata() *Metadata { return e.metadata }

// IsRoot reports whether the event was made a root explicitly.
func (e *Event) IsRoot() bool { return e.parent.kind == parentRoot }

// IsContextual reports whether the event's parent is the current span.
func (e *Event) IsContextual() bool { return e.parent.kind == parentCurrent }

// Parent returns the event's explicit parent, if it has one.
func (e *Event) Parent() rs.Option[SpanId] {
	if e.parent.kind == parentExplicit {
		return rs.Some(e.parent.id)
	}
	return rs.None[SpanId]()
}

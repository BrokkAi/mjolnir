// Port of tracing-subscriber 0.3.23 src/registry/stack.rs

package tracingsubscriber

import (
	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

type contextId struct {
	id        tracing.SpanId
	duplicate bool
}

// spanStack is SpanStack: the spans a thread has entered, innermost last.
// A span entered again while it is already on the stack is a duplicate,
// which does not count as current again.
type spanStack struct {
	stack []contextId
}

func (s *spanStack) push(id tracing.SpanId) {
	duplicate := false
	for _, i := range s.stack {
		if i.id == id {
			duplicate = true
			break
		}
	}
	s.stack = append(s.stack, contextId{id: id, duplicate: duplicate})
}

func (s *spanStack) pop(expectedId tracing.SpanId) bool {
	for idx := len(s.stack) - 1; idx >= 0; idx-- {
		if s.stack[idx].id == expectedId {
			duplicate := s.stack[idx].duplicate
			s.stack = append(s.stack[:idx], s.stack[idx+1:]...)
			return !duplicate
		}
	}
	return false
}

// iter is SpanStack::iter: the entered spans that are not duplicates,
// innermost first.
func (s *spanStack) iter() []tracing.SpanId {
	var ids []tracing.SpanId
	for idx := len(s.stack) - 1; idx >= 0; idx-- {
		if !s.stack[idx].duplicate {
			ids = append(ids, s.stack[idx].id)
		}
	}
	return ids
}

func (s *spanStack) current() rs.Option[tracing.SpanId] {
	for idx := len(s.stack) - 1; idx >= 0; idx-- {
		if !s.stack[idx].duplicate {
			return rs.Some(s.stack[idx].id)
		}
	}
	return rs.None[tracing.SpanId]()
}

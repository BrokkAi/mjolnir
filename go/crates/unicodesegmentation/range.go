package unicodesegmentation

// This file has no counterpart in the crate. A Rust iterator is driven with
// `for`, `.rev()`, `.collect()` and the other adaptors; the iterators of this
// package have Rust's `Next` and `NextBack`, and the methods here make them
// usable in a Go range loop as well:
//
//	for grapheme := range unicodesegmentation.Graphemes(s, true).All() { .. }
//	for offset, grapheme := range unicodesegmentation.GraphemeIndices(s, true).Rev() { .. }
//
// All takes from the front and Rev, Rust's `.rev()`, from the back. Both
// consume the iterator they are called on: a loop that stops early leaves the
// rest in it, as a Rust `for` over `by_ref()` does.

import (
	"iter"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func rangeFront[T any](next func() rs.Option[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		for {
			item, ok := next().Get()
			if !ok || !yield(item) {
				return
			}
		}
	}
}

func rangeFront2[T any](next func() rs.Option[rs.Tuple2[int, T]]) iter.Seq2[int, T] {
	return func(yield func(int, T) bool) {
		for {
			item, ok := next().Get()
			if !ok || !yield(item.F0, item.F1) {
				return
			}
		}
	}
}

// All yields the remaining grapheme clusters, from the front.
func (s *GraphemeGraphemes) All() iter.Seq[string] { return rangeFront(s.Next) }

// Rev yields the remaining grapheme clusters, from the back.
func (s *GraphemeGraphemes) Rev() iter.Seq[string] { return rangeFront(s.NextBack) }

// All yields the byte offset of each remaining grapheme cluster and the
// cluster, from the front.
func (s *GraphemeGraphemeIndices) All() iter.Seq2[int, string] { return rangeFront2(s.Next) }

// Rev yields the byte offset of each remaining grapheme cluster and the
// cluster, from the back.
func (s *GraphemeGraphemeIndices) Rev() iter.Seq2[int, string] { return rangeFront2(s.NextBack) }

// All yields the remaining pieces between word boundaries, from the front.
func (s *WordUWordBounds) All() iter.Seq[string] { return rangeFront(s.Next) }

// Rev yields the remaining pieces between word boundaries, from the back.
func (s *WordUWordBounds) Rev() iter.Seq[string] { return rangeFront(s.NextBack) }

// All yields the byte offset of each remaining piece between word boundaries
// and the piece, from the front.
func (s *WordUWordBoundIndices) All() iter.Seq2[int, string] { return rangeFront2(s.Next) }

// Rev yields the byte offset of each remaining piece between word boundaries
// and the piece, from the back.
func (s *WordUWordBoundIndices) Rev() iter.Seq2[int, string] { return rangeFront2(s.NextBack) }

// All yields the remaining words, from the front.
func (s *WordUnicodeWords) All() iter.Seq[string] { return rangeFront(s.Next) }

// Rev yields the remaining words, from the back.
func (s *WordUnicodeWords) Rev() iter.Seq[string] { return rangeFront(s.NextBack) }

// All yields the byte offset of each remaining word and the word, from the
// front.
func (s *WordUnicodeWordIndices) All() iter.Seq2[int, string] { return rangeFront2(s.Next) }

// Rev yields the byte offset of each remaining word and the word, from the
// back.
func (s *WordUnicodeWordIndices) Rev() iter.Seq2[int, string] { return rangeFront2(s.NextBack) }

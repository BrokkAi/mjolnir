package unicodesegmentation

// This file has no counterpart in the crate. A Rust `str` is valid UTF-8 and
// a Go string need not be; doc.go says what this package does with a byte
// that is not part of a valid encoding, and this test holds it to that.

import (
	"slices"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// utf8Spans describes pieces of s by the characters they hold, as a Go range
// loop reads s: for each piece, the index of its first character and the
// number of characters in it.
func utf8Spans(s string, pieces []rs.Tuple2[int, string]) [][2]int {
	index := map[int]int{}
	count := 0
	for offset := range s {
		index[offset] = count
		count++
	}
	index[len(s)] = count
	spans := make([][2]int, len(pieces))
	for i, piece := range pieces {
		start := index[piece.F0]
		spans[i] = [2]int{start, index[piece.F0+len(piece.F1)] - start}
	}
	return spans
}

// TestInvalidUtf8ReadAsReplacementCharacters checks that each byte of a
// string that is not part of a valid encoding is read as a U+FFFD of its own:
// the pieces of the string hold the same characters as the pieces of the
// string with those bytes replaced by U+FFFD, in every direction, and each
// piece is a slice of the string given.
func TestInvalidUtf8ReadAsReplacementCharacters(t *testing.T) {
	inputs := []string{
		"\xff", "a\xffb", "\xe2\x80", "\xe2\x80a", "\xe2\x80\x8d", "\xed\xa0\x80", "\xc0\xaf", "\xf4\x90\x80\x80",
		"\U0001F468\xe2\x80\U0001F469", "\U0001F468\u200D\xff\U0001F469", "\xf0\x9f\x87\xa6\xf0\x9f",
		"\U0001F1E6\xff\U0001F1E6", "\r\xff\n", "\xff\u0301", "\u0915\u094D\xff\u0937", "\u0600\xff",
		"\xffabc\xff 12\xff,3\xff", "a\xff\xfe\xfd", "\x80\x80\x80",
	}
	r := &corpusRand{state: 0x7574663820}
	for range 3000 {
		var b strings.Builder
		for _, c := range r.corpusString(8) {
			if r.chance(15) {
				b.WriteByte(byte(0x80 + r.intn(0x80)))
			}
			if r.chance(5) {
				// The first bytes of a character, without the rest.
				var encoded [4]byte
				n := utf8.EncodeRune(encoded[:], c)
				b.Write(encoded[:max(1, n-1-r.intn(n))])
				continue
			}
			b.WriteRune(c)
		}
		inputs = append(inputs, b.String())
	}

	splits := []struct {
		name   string
		pieces func(s string, back bool) []rs.Tuple2[int, string]
	}{
		{"grapheme_indices(true)", func(s string, back bool) []rs.Tuple2[int, string] {
			return utf8Collect(GraphemeIndices(s, true), back)
		}},
		{"grapheme_indices(false)", func(s string, back bool) []rs.Tuple2[int, string] {
			return utf8Collect(GraphemeIndices(s, false), back)
		}},
		{"split_word_bound_indices()", func(s string, back bool) []rs.Tuple2[int, string] {
			return utf8Collect(SplitWordBoundIndices(s), back)
		}},
		{"unicode_word_indices()", func(s string, back bool) []rs.Tuple2[int, string] {
			return utf8Collect(UnicodeWordIndices(s), back)
		}},
	}
	for _, s := range inputs {
		if utf8.ValidString(s) {
			continue
		}
		// One U+FFFD for each byte that is not part of a valid encoding, as a
		// range loop reads them (strings.ToValidUTF8 puts one in for a run).
		var b strings.Builder
		for _, c := range s {
			b.WriteRune(c)
		}
		replaced := b.String()
		for _, split := range splits {
			for _, back := range []bool{false, true} {
				pieces := split.pieces(s, back)
				for _, piece := range pieces {
					if s[piece.F0:piece.F0+len(piece.F1)] != piece.F1 {
						t.Fatalf("%s of %+q: the piece (%d, %+q) is not a slice of the string", split.name, s, piece.F0, piece.F1)
					}
				}
				got, want := utf8Spans(s, pieces), utf8Spans(replaced, split.pieces(replaced, back))
				if !slices.Equal(got, want) {
					t.Fatalf("%s of %+q (backwards: %v): pieces by character %v, with U+FFFD for each bad byte %v",
						split.name, s, back, got, want)
				}
			}
		}
	}
}

// utf8Collect is `iter.collect()` (or `iter.rev().collect()` when back) for
// an iterator with offsets.
func utf8Collect(iter interface {
	Next() rs.Option[rs.Tuple2[int, string]]
	NextBack() rs.Option[rs.Tuple2[int, string]]
}, back bool) []rs.Tuple2[int, string] {
	if back {
		return testCollect(iter.NextBack)
	}
	return testCollect(iter.Next)
}

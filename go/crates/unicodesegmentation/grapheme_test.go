// Port of unicode-segmentation 1.13.3 src/grapheme.rs

// The `#[test]` functions of src/grapheme.rs, then the documentation examples
// of the same file, which Rust runs as tests.

package unicodesegmentation

import (
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// graphemeOutcome writes what a GraphemeCursor query returned as Rust's
// `Debug` of the `Result` it stands for: `Ok(true)`, `Ok(Some(4))`,
// `Ok(None)`, `Err(PreContext(4))`, `Err(NextChunk)`. The tests compare it
// with the `Debug` of the value the Rust test expects.
func graphemeOutcome[T bool | rs.Option[int]](value T, err error) string {
	if err != nil {
		if _, ok := err.(GraphemeIncomplete); !ok {
			panic("the error " + err.Error() + " is not a GraphemeIncomplete")
		}
		return "Err(" + err.Error() + ")"
	}
	switch v := any(value).(type) {
	case bool:
		return "Ok(" + strconv.FormatBool(v) + ")"
	case rs.Option[int]:
		if found, ok := v.Get(); ok {
			return "Ok(Some(" + strconv.Itoa(found) + "))"
		}
		return "Ok(None)"
	default:
		panic(rs.Unreachable(v))
	}
}

// graphemeAssertEq is `assert_eq!(left, right)` on two `Debug` forms.
func graphemeAssertEq(t *testing.T, left, right string) {
	t.Helper()
	if left != right {
		t.Fatalf("assertion `left == right` failed\n  left: %s\n right: %s", left, right)
	}
}

// graphemeAssertEqInt is `assert_eq!(left, right)` on two offsets.
func graphemeAssertEqInt(t *testing.T, left, right int) {
	t.Helper()
	if left != right {
		t.Fatalf("assertion `left == right` failed\n  left: %d\n right: %d", left, right)
	}
}

func TestGrapheme_test_grapheme_cursor_ris_precontext(t *testing.T) {
	s := "\U0001F1FA\U0001F1F8\U0001F1FA\U0001F1F8\U0001F1FA\U0001F1F8"
	c := GraphemeCursorNew(8, len(s), true)
	graphemeAssertEq(t,
		graphemeOutcome(c.IsBoundary(s[4:], 4)),
		"Err(PreContext(4))",
	)
	c.ProvideContext(s[:4], 0)
	graphemeAssertEq(t, graphemeOutcome(c.IsBoundary(s[4:], 4)), "Ok(true)")
}

func TestGrapheme_test_grapheme_cursor_chunk_start_require_precontext(t *testing.T) {
	s := "\r\n"
	c := GraphemeCursorNew(1, len(s), true)
	graphemeAssertEq(t,
		graphemeOutcome(c.IsBoundary(s[1:], 1)),
		"Err(PreContext(1))",
	)
	c.ProvideContext(s[:1], 0)
	graphemeAssertEq(t, graphemeOutcome(c.IsBoundary(s[1:], 1)), "Ok(false)")
}

func TestGrapheme_test_grapheme_cursor_prev_boundary(t *testing.T) {
	s := "abcd"
	c := GraphemeCursorNew(3, len(s), true)
	graphemeAssertEq(t,
		graphemeOutcome(c.PrevBoundary(s[2:], 2)),
		"Err(PrevChunk)",
	)
	graphemeAssertEq(t, graphemeOutcome(c.PrevBoundary(s[:2], 0)), "Ok(Some(2))")
}

func TestGrapheme_test_grapheme_cursor_prev_boundary_chunk_start(t *testing.T) {
	s := "abcd"
	c := GraphemeCursorNew(2, len(s), true)
	graphemeAssertEq(t,
		graphemeOutcome(c.PrevBoundary(s[2:], 2)),
		"Err(PrevChunk)",
	)
	graphemeAssertEq(t, graphemeOutcome(c.PrevBoundary(s[:2], 0)), "Ok(Some(1))")
}

func TestGrapheme_test_grapheme_cursor_boundary_with_zwj_on_chunk_start(t *testing.T) {
	chunk0 := "\U0001F469"       // 4 bytes
	chunk1 := "\u200D\U0001F52C" // 3 bytes + 4 bytes

	fullLen := len(chunk0) + len(chunk1)

	cur := GraphemeCursorNew(0, fullLen, true)
	graphemeAssertEq(t, graphemeOutcome(cur.NextBoundary(chunk0, 0)), "Err(NextChunk)")
	res, err := cur.NextBoundary(chunk1, len(chunk0))
	switch err.(type) {
	case nil:
		graphemeAssertEq(t, graphemeOutcome(res, nil), "Ok(Some(11))")
	case *GraphemeIncomplete_PreContext:
		cur.ProvideContext(chunk0, 0)
		graphemeAssertEq(t, graphemeOutcome(cur.NextBoundary(chunk1, len(chunk0))), "Ok(Some(11))")
	default:
		panic(rs.Unreachable(err))
	}
}

func TestGrapheme_test_grapheme_cursor_emoji_no_zwj(t *testing.T) {
	chunk0 := "\U0001F352" // 4 bytes
	chunk1 := "\U0001F951" // 4 bytes
	fullLen := len(chunk0) + len(chunk1)

	c := GraphemeCursorNew(0, fullLen, true)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk0, 0)), "Err(NextChunk)")
	graphemeAssertEq(t,
		graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))),
		"Err(PreContext("+strconv.Itoa(len(chunk0))+"))",
	)
	c.ProvideContext(chunk0, 0)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(Some(4))")
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(Some(8))")
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(None)")
}

func TestGrapheme_test_grapheme_cursor_emoji_chunk_boundary_before_zwj(t *testing.T) {
	chunk0 := "\U0001F352"               // 4 bytes
	chunk1 := "\u200D\U0001F951"         // 3 + 4 bytes
	fullLen := len(chunk0) + len(chunk1) // 11

	c := GraphemeCursorNew(0, fullLen, true)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk0, 0)), "Err(NextChunk)")
	graphemeAssertEq(t,
		graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))),
		"Err(PreContext("+strconv.Itoa(len(chunk0))+"))",
	)
	c.ProvideContext(chunk0, 0)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(Some(11))")
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(None)")
}

func TestGrapheme_test_grapheme_cursor_emoji_chunk_boundary_after_zwj(t *testing.T) {
	chunk0 := "\U0001F352\u200D"         // 4 + 3 bytes
	chunk1 := "\U0001F951"               // 4 bytes
	fullLen := len(chunk0) + len(chunk1) // 11

	c := GraphemeCursorNew(0, fullLen, true)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk0, 0)), "Err(NextChunk)")
	graphemeAssertEq(t,
		graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))),
		"Err(PreContext("+strconv.Itoa(len(chunk0))+"))",
	)
	c.ProvideContext(chunk0, 0)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(Some(11))")
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Ok(None)")
}

func TestGrapheme_test_grapheme_cursor_emoji_zwj_across_chunks(t *testing.T) {
	chunk0 := "\U0001F352"                             // 4 bytes
	chunk1 := "\u200D"                                 // 3 bytes
	chunk2 := "\U0001F951"                             // 4 bytes
	fullLen := len(chunk0) + len(chunk1) + len(chunk2) // 11
	chunk2Start := len(chunk0) + len(chunk1)

	c := GraphemeCursorNew(0, fullLen, true)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk0, 0)), "Err(NextChunk)")
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk1, len(chunk0))), "Err(NextChunk)")
	graphemeAssertEq(t,
		graphemeOutcome(c.NextBoundary(chunk2, chunk2Start)),
		"Err(PreContext("+strconv.Itoa(chunk2Start)+"))",
	)
	c.ProvideContext(chunk1, len(chunk0))
	graphemeAssertEq(t,
		graphemeOutcome(c.NextBoundary(chunk2, chunk2Start)),
		"Err(PreContext("+strconv.Itoa(len(chunk0))+"))",
	)
	c.ProvideContext(chunk0, 0)
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk2, chunk2Start)), "Ok(Some(11))")
	graphemeAssertEq(t, graphemeOutcome(c.NextBoundary(chunk2, chunk2Start)), "Ok(None)")
}

// The documentation examples.

// GraphemeIndices::as_str
func TestGrapheme_doc_grapheme_indices_as_str(t *testing.T) {
	iter := GraphemeIndices("abc", true)
	graphemeAssertEq(t, iter.AsStr(), "abc")
	iter.Next()
	graphemeAssertEq(t, iter.AsStr(), "bc")
	iter.Next()
	iter.Next()
	graphemeAssertEq(t, iter.AsStr(), "")
}

// Graphemes::as_str
func TestGrapheme_doc_graphemes_as_str(t *testing.T) {
	iter := Graphemes("abc", true)
	graphemeAssertEq(t, iter.AsStr(), "abc")
	iter.Next()
	graphemeAssertEq(t, iter.AsStr(), "bc")
	iter.Next()
	iter.Next()
	graphemeAssertEq(t, iter.AsStr(), "")
}

// GraphemeCursor::new
func TestGrapheme_doc_grapheme_cursor_new(t *testing.T) {
	s := "\u0939\u093F\u0928\u094D\u0926\u0940"
	legacy := GraphemeCursorNew(0, len(s), false)
	graphemeAssertEq(t,
		graphemeOutcome(legacy.NextBoundary(s, 0)),
		"Ok(Some("+strconv.Itoa(len("\u0939"))+"))",
	)
	extended := GraphemeCursorNew(0, len(s), true)
	graphemeAssertEq(t,
		graphemeOutcome(extended.NextBoundary(s, 0)),
		"Ok(Some("+strconv.Itoa(len("\u0939\u093F"))+"))",
	)
}

// GraphemeCursor::set_cursor
func TestGrapheme_doc_set_cursor(t *testing.T) {
	s := "abcd"
	cursor := GraphemeCursorNew(0, len(s), false)
	graphemeAssertEqInt(t, cursor.CurCursor(), 0)
	cursor.SetCursor(2)
	graphemeAssertEqInt(t, cursor.CurCursor(), 2)
}

// GraphemeCursor::cur_cursor
func TestGrapheme_doc_cur_cursor(t *testing.T) {
	// Two flags (🇷🇸🇮🇴), each flag is two RIS codepoints, each RIS is 4 bytes.
	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
	cursor := GraphemeCursorNew(4, len(flags), false)
	graphemeAssertEqInt(t, cursor.CurCursor(), 4)
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(flags, 0)), "Ok(Some(8))")
	graphemeAssertEqInt(t, cursor.CurCursor(), 8)
}

// GraphemeCursor::provide_context
func TestGrapheme_doc_provide_context(t *testing.T) {
	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
	cursor := GraphemeCursorNew(8, len(flags), false)
	// Not enough pre-context to decide if there's a boundary between the two flags.
	graphemeAssertEq(t, graphemeOutcome(cursor.IsBoundary(flags[8:], 8)), "Err(PreContext(8))")
	// Provide one more Regional Indicator Symbol of pre-context
	cursor.ProvideContext(flags[4:8], 4)
	// Still not enough context to decide.
	graphemeAssertEq(t, graphemeOutcome(cursor.IsBoundary(flags[8:], 8)), "Err(PreContext(4))")
	// Provide additional requested context.
	cursor.ProvideContext(flags[0:4], 0)
	// That's enough to decide (it always is when context goes to the start of the string)
	graphemeAssertEq(t, graphemeOutcome(cursor.IsBoundary(flags[8:], 8)), "Ok(true)")
}

// GraphemeCursor::is_boundary
func TestGrapheme_doc_is_boundary(t *testing.T) {
	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
	cursor := GraphemeCursorNew(8, len(flags), false)
	graphemeAssertEq(t, graphemeOutcome(cursor.IsBoundary(flags, 0)), "Ok(true)")
	cursor.SetCursor(12)
	graphemeAssertEq(t, graphemeOutcome(cursor.IsBoundary(flags, 0)), "Ok(false)")
}

// GraphemeCursor::next_boundary
func TestGrapheme_doc_next_boundary(t *testing.T) {
	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
	cursor := GraphemeCursorNew(4, len(flags), false)
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(flags, 0)), "Ok(Some(8))")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(flags, 0)), "Ok(Some(16))")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(flags, 0)), "Ok(None)")
}

// GraphemeCursor::next_boundary: "And an example that uses partial strings"
func TestGrapheme_doc_next_boundary_partial(t *testing.T) {
	s := "abcd"
	cursor := GraphemeCursorNew(0, len(s), false)
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(s[:2], 0)), "Ok(Some(1))")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(s[:2], 0)), "Err(NextChunk)")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(s[2:4], 2)), "Ok(Some(2))")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(s[2:4], 2)), "Ok(Some(3))")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(s[2:4], 2)), "Ok(Some(4))")
	graphemeAssertEq(t, graphemeOutcome(cursor.NextBoundary(s[2:4], 2)), "Ok(None)")
}

// GraphemeCursor::prev_boundary
func TestGrapheme_doc_prev_boundary(t *testing.T) {
	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
	cursor := GraphemeCursorNew(12, len(flags), false)
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(flags, 0)), "Ok(Some(8))")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(flags, 0)), "Ok(Some(0))")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(flags, 0)), "Ok(None)")
}

// GraphemeCursor::prev_boundary: "And an example that uses partial strings
// (note the exact return is not guaranteed, and may be `PrevChunk` or
// `PreContext` arbitrarily)"
func TestGrapheme_doc_prev_boundary_partial(t *testing.T) {
	s := "abcd"
	cursor := GraphemeCursorNew(4, len(s), false)
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(s[2:4], 2)), "Ok(Some(3))")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(s[2:4], 2)), "Err(PrevChunk)")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(s[0:2], 0)), "Ok(Some(2))")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(s[0:2], 0)), "Ok(Some(1))")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(s[0:2], 0)), "Ok(Some(0))")
	graphemeAssertEq(t, graphemeOutcome(cursor.PrevBoundary(s[0:2], 0)), "Ok(None)")
}

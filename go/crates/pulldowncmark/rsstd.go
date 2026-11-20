package pulldowncmark

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// The functions in this file stand in for pieces of Rust's core library that
// go/rs has no function for; each names the Rust method it reproduces. The
// rest of `str`, `char` and `u8` comes from go/rs/strs.

// strCharsNth is `s.chars().nth(n)` for a string whose first n characters are
// known to be single bytes: the character that starts at byte n.
func strCharsNth(s string, n int) (rune, bool) {
	if n >= len(s) {
		return 0, false
	}
	c, _ := utf8.DecodeRuneInString(s[n:])
	return c, true
}

// strCharsLast is `s.chars().last()`.
func strCharsLast(s string) (rune, bool) {
	if len(s) == 0 {
		return 0, false
	}
	c, _ := utf8.DecodeLastRuneInString(s)
	return c, true
}

// strSlice is `&s[start..end]` on a `str`: it panics, with Rust's message
// (strs.Slice), when the range is out of bounds or backwards or when an end
// falls inside a char.
//
// A Rust string is always UTF-8 and a Go string need not be. A byte that is
// not part of a valid encoding counts here as a char of its own, so text that
// is not UTF-8 is cut between any two of its bytes the parser chooses, as it
// was before the parser sliced through this function, and never panics for
// it.
func strSlice(s string, start, end int) string {
	if uint(start) > uint(end) || uint(end) > uint(len(s)) ||
		!strIsCharBoundaryLenient(s, start) || !strIsCharBoundaryLenient(s, end) {
		return strs.Slice(s, start, end)
	}
	return s[start:end]
}

// strIsCharBoundaryLenient is str::is_char_boundary for an index within s,
// where a byte that is not part of a valid encoding is a char of its own:
// only the inside of a validly encoded char is not a boundary.
func strIsCharBoundaryLenient(s string, i int) bool {
	if i == 0 || i >= len(s) || utf8.RuneStart(s[i]) {
		return true
	}
	// s[i] is a continuation byte: it is inside a char when a valid
	// encoding that starts at most three bytes earlier covers it.
	for k := 1; k <= 3 && k <= i; k++ {
		if !utf8.RuneStart(s[i-k]) {
			continue
		}
		_, size := utf8.DecodeRuneInString(s[i-k:])
		return size <= k
	}
	return true
}

// optionUnwrapNone panics as `Option::unwrap()` does on None.
const optionUnwrapNone = "called `Option::unwrap()` on a `None` value"

// debugAssertions is `cfg!(debug_assertions)` for this package. It is a
// variable only so that the comparison with the Rust oracle, which is a
// release build, can switch the assertions off.
var debugAssertions = rs.DebugAssertions

// debugAssert is `debug_assert!(cond)`. message is the panic text Rust
// produces, "assertion failed: " followed by the condition as it is written
// in the Rust source; rs.DebugAssert cannot be given it until the formatter
// is ported.
func debugAssert(cond bool, message string) {
	if debugAssertions && !cond {
		rs.PanicAt(1, message)
	}
}

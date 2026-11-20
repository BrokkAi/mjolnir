package pulldowncmark

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The functions in this file stand in for methods of Rust's standard library
// whose results depend on Rust's exact rules. They belong in go/rs/strs,
// which did not exist when this package was written; each names the Rust
// method it reproduces.

// charIsWhitespace is char::is_whitespace: the Unicode White_Space property
// as the pinned toolchain's table (core::unicode::unicode_data::white_space)
// has it.
func charIsWhitespace(c rune) bool {
	switch {
	case c == ' ' || ('\x09' <= c && c <= '\x0d'):
		return true
	case c <= 0x84:
		return false
	}
	switch c {
	case 0x85, 0xA0, 0x1680, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000:
		return true
	}
	return 0x2000 <= c && c <= 0x200A
}

// u8IsAsciiWhitespace is u8::is_ascii_whitespace: space, tab, line feed, form
// feed and carriage return. Unlike the crate's own is_ascii_whitespace it
// does not include the vertical tab.
func u8IsAsciiWhitespace(c byte) bool {
	return c == '\t' || c == '\n' || c == '\x0C' || c == '\r' || c == ' '
}

// u8IsAsciiAlphabetic is u8::is_ascii_alphabetic.
func u8IsAsciiAlphabetic(c byte) bool {
	return ('A' <= c && c <= 'Z') || ('a' <= c && c <= 'z')
}

// u8IsAsciiDigit is u8::is_ascii_digit.
func u8IsAsciiDigit(c byte) bool {
	return '0' <= c && c <= '9'
}

// u8ToAsciiLowercase is u8::to_ascii_lowercase.
func u8ToAsciiLowercase(c byte) byte {
	if 'A' <= c && c <= 'Z' {
		return c | 0x20
	}
	return c
}

// sliceEqIgnoreAsciiCase is <[u8]>::eq_ignore_ascii_case.
func sliceEqIgnoreAsciiCase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		if u8ToAsciiLowercase(a[i]) != u8ToAsciiLowercase(b[i]) {
			return false
		}
	}
	return true
}

// strIsAscii is str::is_ascii.
func strIsAscii(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] >= 0x80 {
			return false
		}
	}
	return true
}

// strSplitAsciiWhitespace is str::split_ascii_whitespace: the pieces between
// runs of u8::is_ascii_whitespace bytes, never an empty piece.
func strSplitAsciiWhitespace(s string) []string {
	var pieces []string
	start := -1
	for i := 0; i < len(s); i++ {
		if u8IsAsciiWhitespace(s[i]) {
			if start >= 0 {
				pieces = append(pieces, s[start:i])
				start = -1
			}
		} else if start < 0 {
			start = i
		}
	}
	if start >= 0 {
		pieces = append(pieces, s[start:])
	}
	return pieces
}

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

// optionUnwrapNone panics as `Option::unwrap()` does on None.
const optionUnwrapNone = "called `Option::unwrap()` on a `None` value"

// debugAssert is `debug_assert!(cond)`. message is the panic text Rust
// produces, "assertion failed: " followed by the condition as it is written
// in the Rust source; rs.DebugAssert cannot be given it until the formatter
// is ported.
func debugAssert(cond bool, message string) {
	if rs.DebugAssertions && !cond {
		rs.PanicAt(1, message)
	}
}

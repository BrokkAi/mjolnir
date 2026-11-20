// Port of unicode-width 0.2.2 src/lib.rs

// Copyright 2012-2025 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// The crate's own documentation (the rules for determining width) is carried
// in doc.go, which holds the package comment.

package unicodewidth

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// `pub use tables::UNICODE_VERSION;` is UnicodeVersion, in tables.go.

// The crate's `private::Sealed` supertrait has no Go counterpart: with the
// `cjk` feature, which this package reproduces, it is implemented for every
// type, so both traits are open to other crates (ratatui implements
// UnicodeWidthStr for its text types).

// UnicodeWidthChar is the trait `unicode_width::UnicodeWidthChar`:
// methods for determining displayed width of Unicode characters.
//
// The crate implements it for `char`; that implementation is the functions
// [UnicodeWidthCharWidth] and [UnicodeWidthCharWidthCjk].
type UnicodeWidthChar interface {
	// Returns the character's displayed width in columns, or `None` if the
	// character is a control character.
	//
	// This function treats characters in the Ambiguous category according
	// to [Unicode Standard Annex #11](http://www.unicode.org/reports/tr11/)
	// as 1 column wide. This is consistent with the recommendations for non-CJK
	// contexts, or when the context cannot be reliably determined.
	Width() rs.Option[int]

	// Returns the character's displayed width in columns, or `None` if the
	// character is a control character.
	//
	// This function treats characters in the Ambiguous category according
	// to [Unicode Standard Annex #11](http://www.unicode.org/reports/tr11/)
	// as 2 columns wide. This is consistent with the recommendations for
	// CJK contexts.
	//
	// cfg(feature = "cjk")
	WidthCjk() rs.Option[int]
}

// UnicodeWidthCharWidth is `UnicodeWidthChar::width` for `char`: the
// character's displayed width in columns, or None if the character is a
// control character. Ambiguous characters are 1 column wide.
//
// A Rust `char` is always a Unicode scalar value. A rune that is not one (a
// surrogate, a negative number, a number above U+10FFFF) is measured as
// U+FFFD, the character Go itself substitutes for it.
func UnicodeWidthCharWidth(c rune) rs.Option[int] {
	return tablesSingleCharWidth(scalarValue(c))
}

// UnicodeWidthCharWidthCjk is `UnicodeWidthChar::width_cjk` for `char`: as
// [UnicodeWidthCharWidth], with Ambiguous characters 2 columns wide.
func UnicodeWidthCharWidthCjk(c rune) rs.Option[int] {
	return tablesSingleCharWidthCjk(scalarValue(c))
}

// UnicodeWidthStr is the trait `unicode_width::UnicodeWidthStr`:
// methods for determining displayed width of Unicode strings.
//
// The crate implements it for `str`; that implementation is the functions
// [UnicodeWidthStrWidth] and [UnicodeWidthStrWidthCjk].
type UnicodeWidthStr interface {
	// Returns the string's displayed width in columns.
	//
	// This function treats characters in the Ambiguous category according
	// to [Unicode Standard Annex #11](http://www.unicode.org/reports/tr11/)
	// as 1 column wide. This is consistent with the recommendations for
	// non-CJK contexts, or when the context cannot be reliably determined.
	Width() int

	// Returns the string's displayed width in columns.
	//
	// This function treats characters in the Ambiguous category according
	// to [Unicode Standard Annex #11](http://www.unicode.org/reports/tr11/)
	// as 2 column wide. This is consistent with the recommendations for
	// CJK contexts.
	//
	// cfg(feature = "cjk")
	WidthCjk() int
}

// UnicodeWidthStrWidth is `UnicodeWidthStr::width` for `str`: the string's
// displayed width in columns, with Ambiguous characters 1 column wide.
//
// The width of a string is not the sum of the widths of its characters; see
// the rules in the package documentation. A control character counts 1 here,
// although [UnicodeWidthCharWidth] gives None for it.
//
// A Rust `str` is always valid UTF-8. Each byte of s that is not part of a
// valid encoding is measured as U+FFFD, the character Go itself reads there.
func UnicodeWidthStrWidth(s string) int {
	return tablesStrWidth(s)
}

// UnicodeWidthStrWidthCjk is `UnicodeWidthStr::width_cjk` for `str`: as
// [UnicodeWidthStrWidth], with Ambiguous characters 2 columns wide.
func UnicodeWidthStrWidthCjk(s string) int {
	return tablesStrWidthCjk(s)
}

// CharWidth is a short name for [UnicodeWidthCharWidth].
func CharWidth(c rune) rs.Option[int] { return UnicodeWidthCharWidth(c) }

// CharWidthCjk is a short name for [UnicodeWidthCharWidthCjk].
func CharWidthCjk(c rune) rs.Option[int] { return UnicodeWidthCharWidthCjk(c) }

// StrWidth is a short name for [UnicodeWidthStrWidth].
func StrWidth(s string) int { return UnicodeWidthStrWidth(s) }

// StrWidthCjk is a short name for [UnicodeWidthStrWidthCjk].
func StrWidthCjk(s string) int { return UnicodeWidthStrWidthCjk(s) }

// scalarValue returns c, or U+FFFD when c is not a Unicode scalar value and so
// could not be a Rust `char`.
func scalarValue(c rune) rune {
	if !utf8.ValidRune(c) {
		return utf8.RuneError
	}
	return c
}

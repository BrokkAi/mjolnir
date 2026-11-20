package vt100

import (
	"github.com/BrokkAi/mjolnir/go/crates/unicodewidth"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// This file is the only place where vt100 asks for the width of a character.

// unicodeWidthCharWidth is `c.width()` of `unicode_width::UnicodeWidthChar`
// (unicode-width 0.2.2): None for a control character, and otherwise the
// number of columns the character takes, which is 0, 1 or 2 for every
// character but U+17D8, for which it is 3.
//
// The comparison with Rust in oracle_test.go prints every code point through
// both implementations, so a change of the tables behind this function that
// vt100 could see is found there.
func unicodeWidthCharWidth(c rune) rs.Option[int] {
	return unicodewidth.UnicodeWidthCharWidth(c)
}

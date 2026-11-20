// Port of unicode-width 0.2.2 tests/tests.rs

// Copyright 2012-2025 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

package unicodewidth

import (
	"bufio"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// assertWidth is the `assert_width!` macro for a string:
//
//	assert_eq!($s.width(), $nocjk, "{:?} has the wrong width", $s);
//	#[cfg(feature = "cjk")]
//	assert_eq!($s.width_cjk(), $cjk, "{:?} has the wrong width (CJK)", $s);
func assertWidth(t *testing.T, s string, nocjk, cjk int) {
	t.Helper()
	if width := UnicodeWidthStrWidth(s); width != nocjk {
		t.Fatalf("%q has the wrong width\n  left: %d\n right: %d", s, width, nocjk)
	}
	if width := UnicodeWidthStrWidthCjk(s); width != cjk {
		t.Fatalf("%q has the wrong width (CJK)\n  left: %d\n right: %d", s, width, cjk)
	}
}

// assertWidthChar is the `assert_width!` macro for a character.
func assertWidthChar(t *testing.T, c rune, nocjk, cjk rs.Option[int]) {
	t.Helper()
	if width := UnicodeWidthCharWidth(c); width != nocjk {
		t.Fatalf("%q has the wrong width\n  left: %v\n right: %v", c, width, nocjk)
	}
	if width := UnicodeWidthCharWidthCjk(c); width != cjk {
		t.Fatalf("%q has the wrong width (CJK)\n  left: %v\n right: %v", c, width, cjk)
	}
}

// assertEq is `assert_eq!` for two numbers. A character given after them is
// the message `"{c:?}"` of the Rust assertion.
func assertEq(t *testing.T, left, right int, message ...rune) {
	t.Helper()
	if left != right {
		if len(message) > 0 {
			t.Fatalf("assertion `left == right` failed: %q\n  left: %d\n right: %d", message[0], left, right)
		}
		t.Fatalf("assertion `left == right` failed\n  left: %d\n right: %d", left, right)
	}
}

// scalarValues calls f with every Unicode scalar value: Rust's
// `for c in '\0'..=char::MAX`.
func scalarValues(f func(c rune)) {
	for c := rune(0); c <= 0x10FFFF; c++ {
		if c >= 0xD800 && c <= 0xDFFF {
			continue
		}
		f(c)
	}
}

func TestItTests_test_str(t *testing.T) {
	assertWidth(t, "ｈｅｌｌｏ", 10, 10)
	assertWidth(t, "\x00\x00\x00\x01\x01", 5, 5)
	assertWidth(t, "", 0, 0)
	assertWidth(t, "\u2081\u2082\u2083\u2084", 4, 8)
}

func TestItTests_test_emoji(t *testing.T) {
	assertWidth(t, "👩", 2, 2)   // Woman
	assertWidth(t, "🔬", 2, 2)   // Microscope
	assertWidth(t, "👩‍🔬", 2, 2) // Woman scientist
}

// From README
func TestItTests_test_bad_devanagari(t *testing.T) {
	assertEq(t, UnicodeWidthStrWidth("क"), 1)   // Devanagari letter Ka
	assertEq(t, UnicodeWidthStrWidth("ष"), 1)   // Devanagari letter Ssa
	assertEq(t, UnicodeWidthStrWidth("क्ष"), 2) // Ka + Virama + Ssa
}

func TestItTests_test_char(t *testing.T) {
	assertWidthChar(t, 'ｈ', rs.Some(2), rs.Some(2))
	assertWidthChar(t, '\x00', rs.None[int](), rs.None[int]())
	assertWidthChar(t, '\x01', rs.None[int](), rs.None[int]())
	assertWidthChar(t, '\u2081', rs.Some(1), rs.Some(2))
}

func TestItTests_test_char2(t *testing.T) {
	assertWidthChar(t, '\x0A', rs.None[int](), rs.None[int]())
	assertWidthChar(t, 'w', rs.Some(1), rs.Some(1))
	assertWidthChar(t, 'ｈ', rs.Some(2), rs.Some(2))
	assertWidthChar(t, '\u00AD', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\u1160', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\u00A1', rs.Some(1), rs.Some(2))
	assertWidthChar(t, '\u0300', rs.Some(0), rs.Some(0))
}

func TestItTests_unicode_12(t *testing.T) {
	assertWidthChar(t, '\U0001F971', rs.Some(2), rs.Some(2))
}

func TestItTests_test_default_ignorable(t *testing.T) {
	assertWidthChar(t, '\u1160', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\u3164', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\uFFA0', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\U000E0000', rs.Some(0), rs.Some(0))
}

func TestItTests_test_ambiguous(t *testing.T) {
	assertWidth(t, "\u00B7", 1, 2)
	assertWidth(t, "\u0387", 1, 2)
	assertWidth(t, "\u00A8", 1, 1)
	assertWidth(t, "\u02C9", 1, 1)
}

func TestItTests_test_jamo(t *testing.T) {
	assertWidthChar(t, '\u1100', rs.Some(2), rs.Some(2))
	assertWidthChar(t, '\uA97C', rs.Some(2), rs.Some(2))
	// Special case: U+115F HANGUL CHOSEONG FILLER
	assertWidthChar(t, '\u115F', rs.Some(2), rs.Some(2))
	assertWidthChar(t, '\u1160', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\uD7C6', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\u11A8', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\uD7FB', rs.Some(0), rs.Some(0))
}

func TestItTests_test_prepended_concatenation_marks(t *testing.T) {
	for _, c := range []rune{
		'\u0600',
		'\u0601',
		'\u0602',
		'\u0603',
		'\u0604',
		'\u06DD',
		'\U000110BD',
		'\U000110CD',
	} {
		assertWidthChar(t, c, rs.Some(1), rs.Some(1))
	}

	for _, c := range []rune{'\u0605', '\u070F', '\u0890', '\u0891', '\u08E2'} {
		assertWidthChar(t, c, rs.Some(0), rs.Some(0))
	}
}

func TestItTests_test_gcb_prepend(t *testing.T) {
	assertWidth(t, "ൎഉ", 1, 1)
	assertWidth(t, "\U00011A89", 0, 0)
}

func TestItTests_test_interlinear_annotation_chars(t *testing.T) {
	assertWidthChar(t, '\uFFF9', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\uFFFA', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\uFFFB', rs.Some(1), rs.Some(1))
}

func TestItTests_test_hieroglyph_format_controls(t *testing.T) {
	assertWidthChar(t, '\U00013430', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\U00013436', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\U0001343C', rs.Some(1), rs.Some(1))
}

func TestItTests_test_marks(t *testing.T) {
	// Nonspacing marks have 0 width
	assertWidthChar(t, '\u0301', rs.Some(0), rs.Some(0))
	// Enclosing marks have 0 width
	assertWidthChar(t, '\u20DD', rs.Some(0), rs.Some(0))
	// Some spacing marks have width 1
	assertWidthChar(t, '\u09CB', rs.Some(1), rs.Some(1))
	// But others have width 0
	assertWidthChar(t, '\u09BE', rs.Some(0), rs.Some(0))
}

func TestItTests_test_devanagari_caret(t *testing.T) {
	assertWidthChar(t, '\uA8FA', rs.Some(0), rs.Some(0))
}

func TestItTests_test_solidus_overlay(t *testing.T) {
	assertWidth(t, "<\u0338", 1, 2)
	assertWidth(t, "=\u0338", 1, 2)
	assertWidth(t, ">\u0338", 1, 2)
	assertWidth(t, "=\u0301\u0338", 1, 2)
	assertWidth(t, "=\u0338\u0301", 1, 2)
	assertWidth(t, "=\uFE0F\u0338", 1, 2)
	assertWidth(t, "#\uFE0F\u0338", 2, 2)
	assertWidth(t, "#\u0338\uFE0F", 1, 1)

	assertWidth(t, "\u06B8\u0338\u0627", 1, 1)
	assertWidth(t, "\u06B8\u0338\uFE0E\u0627", 1, 1)
	assertWidth(t, "\u06B8\u0338\uFE0F\u0627", 1, 1)
	assertWidth(t, "\u06B8\uFE0E\u0338\u0627", 1, 1)
	assertWidth(t, "\u06B8\uFE0F\u0338\u0627", 1, 1)

	assertWidth(t, "=\u0338\u0627", 2, 3)
}

func TestItTests_test_emoji_presentation(t *testing.T) {
	assertWidthChar(t, '\u0023', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\uFE0F', rs.Some(0), rs.Some(0))
	assertWidth(t, "\u0023\uFE0F", 2, 2)
	assertWidth(t, "a\u0023\uFE0Fa", 4, 4)
	assertWidth(t, "\u0023a\uFE0F", 2, 2)
	assertWidth(t, "a\uFE0F", 1, 1)
	assertWidth(t, "\u0023\u0023\uFE0Fa", 4, 4)
	assertWidth(t, "\u002A\uFE0F", 2, 2)
	assertWidth(t, "\u23F9\uFE0F", 2, 2)
	assertWidth(t, "\u24C2\uFE0F", 2, 2)
	assertWidth(t, "\U0001F6F3\uFE0F", 2, 2)
	assertWidth(t, "\U0001F700\uFE0F", 1, 1)
	assertWidth(t, "\u002A\u0301\uFE0F", 1, 1)
	assertWidth(t, "\u002A\u200D\uFE0F", 1, 1)
	assertWidth(t, "\u002A\uFE0E\uFE0F", 1, 1)
}

func TestItTests_test_text_presentation(t *testing.T) {
	assertWidthChar(t, '\uFE0E', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\u2648', rs.Some(2), rs.Some(2))
	assertWidth(t, "\u2648\uFE0E", 1, 2)
	assertWidth(t, "\u2648\uFE0E\uFE0F", 1, 2)
	assertWidth(t, "\u2648\uFE0F\uFE0E", 2, 2)
	assertWidth(t, "\U0001F21A\uFE0E", 2, 2)
	assertWidth(t, "\u0301\uFE0E", 0, 0)
	assertWidth(t, "a\uFE0E", 1, 1)
	assertWidth(t, "𘀀\uFE0E", 2, 2)
	assertWidth(t, "\u2648\u0301\uFE0E", 2, 2)
	assertWidth(t, "\u2648\u200D\uFE0E", 2, 2)
}

func TestItTests_test_control_line_break(t *testing.T) {
	assertWidthChar(t, '\u2028', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\u2029', rs.Some(1), rs.Some(1))
	assertWidthChar(t, '\r', rs.None[int](), rs.None[int]())
	assertWidthChar(t, '\n', rs.None[int](), rs.None[int]())
	assertWidth(t, "\r", 1, 1)
	assertWidth(t, "\n", 1, 1)
	assertWidth(t, "\r\n", 1, 1)
	assertWidth(t, "\x00", 1, 1)
	assertWidth(t, "1\t2\r\n3\u00854", 7, 7)
	assertWidth(t, "\r\uFE0F\n", 2, 2)
	assertWidth(t, "\r\u200D\n", 2, 2)
}

func TestItTests_char_str_consistent(t *testing.T) {
	scalarValues(func(c rune) {
		s := string(c)
		assertEq(t, UnicodeWidthCharWidth(c).UnwrapOr(1), UnicodeWidthStrWidth(s))
		// cfg(feature = "cjk")
		assertEq(t, UnicodeWidthCharWidthCjk(c).UnwrapOr(1), UnicodeWidthStrWidthCjk(s))
	})
}

func TestItTests_test_lisu_tones(t *testing.T) {
	for c := rune(0xA4F8); c <= 0xA4FD; c++ {
		assertWidthChar(t, c, rs.Some(1), rs.Some(1))
		assertWidth(t, string(c), 1, 1)
	}
	for c1 := rune(0xA4F8); c1 <= 0xA4FD; c1++ {
		for c2 := rune(0xA4F8); c2 <= 0xA4FD; c2++ {
			s := string(c1) + string(c2)
			if c1 >= 0xA4F8 && c1 <= 0xA4FB && c2 >= 0xA4FC && c2 <= 0xA4FD {
				assertWidth(t, s, 1, 1)
			} else {
				assertWidth(t, s, 2, 2)
			}
		}
	}

	assertWidth(t, "ꓪꓹ", 2, 2)
	assertWidth(t, "ꓪꓹꓼ", 2, 2)
	assertWidth(t, "ꓪꓹ\uFE0Fꓼ", 2, 2)
	assertWidth(t, "ꓪꓹ\u200Dꓼ", 2, 2)
	assertWidth(t, "ꓪꓹꓼ\uFE0F", 2, 2)
	assertWidth(t, "ꓪꓹ\u0301ꓼ", 3, 3)
	assertWidth(t, "ꓪꓹꓹ", 3, 3)
	assertWidth(t, "ꓪꓼꓼ", 3, 3)
}

func TestItTests_test_hebrew_alef_lamed(t *testing.T) {
	assertWidth(t, "\u05D0", 1, 1)
	assertWidth(t, "\u05DC", 1, 1)
	assertWidth(t, "\u05D0\u05DC", 2, 2)
	assertWidth(t, "\u05D0\u200D\u05DC", 1, 1)
	assertWidth(t, "\u05D0\u200D\u200D\u200D\u200D\u200D\u200D\u200D\u05DC", 1, 1)
	assertWidth(t, "\u05D0\u05D0\u200D\u05DC", 2, 2)
	assertWidth(t, "\u05D0\u05D0\u200D\u200D\u200D\u200D\u200D\u200D\u05DC", 2, 2)
	assertWidth(t, "\u05D0\uFE0F\u200D\uFE0F\u05DC\uFE0F", 1, 1)
	assertWidth(t, "\u05D0\uFE0E\u200D\uFE0E\u05DC\uFE0E", 1, 1)
}

func TestItTests_test_arabic_lam_alef(t *testing.T) {
	assertWidth(t, "\u0644", 1, 1)
	assertWidth(t, "\u06B8", 1, 1)

	assertWidth(t, "\u0623", 1, 1)
	assertWidth(t, "\u0627", 1, 1)

	assertWidth(t, "\u0644\u0623", 1, 1)
	assertWidth(t, "\u0644\u0627", 1, 1)
	assertWidth(t, "\u06B8\u0623", 1, 1)
	assertWidth(t, "\u06B8\u0627", 1, 1)

	assertWidth(t, "\u0644\u065F\u065E\u0623", 1, 1)
	assertWidth(t, "\u0644\u065F\u065E\u0627", 1, 1)
	assertWidth(t, "\u06B8\u065F\u065E\u0623", 1, 1)
	assertWidth(t, "\u06B8\u065F\u065E\u0627", 1, 1)

	assertWidth(t, "\u06B8\uFE0E\u0627", 1, 1)
	assertWidth(t, "\u06B8\uFE0F\u0627", 1, 1)
	assertWidth(t, "\u06B8\u17B5\u0627", 1, 1)

	assertWidth(t, "\u0644\u0644\u0623", 2, 2)
	assertWidth(t, "\u0644\u0644\u0627", 2, 2)
	assertWidth(t, "\u06B8\u06B8\u0623", 2, 2)
	assertWidth(t, "\u06B8\u06B8\u0627", 2, 2)

	assertWidth(t, "\u0644\u200D\u0623", 2, 2)
	assertWidth(t, "\u0644\u200D\u0627", 2, 2)
	assertWidth(t, "\u06B8\u200D\u0623", 2, 2)
	assertWidth(t, "\u06B8\u200D\u0627", 2, 2)

	assertWidth(t, "\u0644\U0001E94B\u0623", 3, 3)
	assertWidth(t, "\u0644\U0001E94B\u0627", 3, 3)
	assertWidth(t, "\u06B8\U0001E94B\u0623", 3, 3)
	assertWidth(t, "\u06B8\U0001E94B\u0627", 3, 3)
}

func TestItTests_test_buginese_a_i_ya(t *testing.T) {
	assertWidth(t, "\u1A15", 1, 1)
	assertWidth(t, "\u1A17", 0, 0)
	assertWidth(t, "\u1A10", 1, 1)

	assertWidth(t, "\u1A15\u1A17\u200D\u1A10", 1, 1)
	assertWidth(t, "\u1A15\u1A17\u200D\u200D\u200D\u200D\u1A10", 1, 1)
	assertWidth(t, "\u1A15\u1A17\u200D\u0338", 1, 1)
	assertWidth(t, "\u1A15\uFE0E\u1A17\u200D", 1, 1)
	assertWidth(t, "\u1A15\uFE0F\u1A17\u200D", 1, 1)
	assertWidth(t, "\u1A15\u1A17\uFE0E\u200D", 1, 1)
	assertWidth(t, "\u1A15\u1A17\uFE0F\u200D", 1, 1)
	assertWidth(t, "\u1A15\u1A17\u200D\uFE0E", 1, 1)
	assertWidth(t, "\u1A15\u1A17\u200D\uFE0F", 1, 1)
	assertWidth(t, "\u1A15\u17B5\u200D\uFE0E\u1A17\u200D\uFE0F\u200D\uFE0F", 1, 1)

	assertWidth(t, "\u1A15\u1A15\u1A17\u200D\u1A10", 2, 2)
	assertWidth(t, "\u1A15\u1A15\u1A17\u200D\u200D\u200D\u200D\u1A10", 2, 2)

	assertWidth(t, "\u1A15\u1A17\u1A10", 2, 2)
	assertWidth(t, "\u1A15\u200D\u1A10", 2, 2)
	assertWidth(t, "\u1A15\u1A10", 2, 2)
	assertWidth(t, "\u1A15\u1A17\u1A17\u200D\u1A10", 2, 2)
	assertWidth(t, "\u1A15\u1A17\u0338\u200D\u1A10", 2, 2)
}

func TestItTests_test_tifinagh_biconsonants(t *testing.T) {
	assertWidth(t, "\u2D4F", 1, 1)
	assertWidth(t, "\u2D3E", 1, 1)
	assertWidth(t, "\u2D7F", 1, 1)

	assertWidth(t, "\u2D4F\u200D\u2D3E", 1, 1)
	assertWidth(t, "\u2D4F\u2D7F\u2D3E", 1, 1)
	assertWidth(t, "\u2D4F\u200D\u2D3E", 1, 1)
	assertWidth(t, "\u2D4F\uFE0F\u200D\u2D7F\uFE0E\u200D\u17B5\u2D3E", 1, 1)

	assertWidth(t, "\u2D4F\u0301\u2D7F\u2D3E", 3, 3)
	assertWidth(t, "\u2D4F\u0301\u200D\u2D3E", 2, 2)
	assertWidth(t, "\u2D4F\u2D3E", 2, 2)
	assertWidth(t, "\u2D4F\u2D7F\u2D7F\u2D3E", 4, 4)
	assertWidth(t, "\u2D7F\u2D3E", 2, 2)
	assertWidth(t, "\u2D7F\u2D7F\u2D66", 3, 3)
	assertWidth(t, "\u2D66\u2D7F\u2D3E", 3, 3)
}

func TestItTests_test_old_turkic_ligature(t *testing.T) {
	assertWidth(t, "\U00010C32", 1, 1)
	assertWidth(t, "\U00010C03", 1, 1)
	assertWidth(t, "\U00010C32\U00010C03", 2, 2)

	assertWidth(t, "\U00010C32\u200D\U00010C03", 1, 1)
	assertWidth(t, "\U00010C32\uFE0F\u200D\uFE0E\U00010C03", 1, 1)

	assertWidth(t, "\U00010C32\u2D7F\U00010C03", 3, 3)
	assertWidth(t, "\U00010C32\u0301\u200D\U00010C03", 2, 2)
	assertWidth(t, "\U00010C03\u200D\U00010C32", 2, 2)
	assertWidth(t, "\u200D\U00010C32", 1, 1)
}

func TestItTests_test_khmer_coeng(t *testing.T) {
	assertWidth(t, "ល", 1, 1)
	assertWidth(t, "ង", 1, 1)
	assertWidth(t, "លង", 2, 2)
	assertWidth(t, "ល្ង", 1, 1)

	scalarValues(func(c rune) {
		if (c >= 0x1780 && c <= 0x1782) || (c >= 0x1784 && c <= 0x1787) ||
			(c >= 0x1789 && c <= 0x178C) || (c >= 0x178E && c <= 0x1793) ||
			(c >= 0x1795 && c <= 0x1798) || (c >= 0x179B && c <= 0x179D) ||
			c == 0x17A0 || c == 0x17A2 || c == 0x17A7 ||
			(c >= 0x17AB && c <= 0x17AC) || c == 0x17AF {
			assertWidth(t, "\u17D2"+string(c), 0, 0)
			assertWidth(t, "\u17D2\u200D\u200D"+string(c), 0, 0)
		} else {
			assertWidth(t,
				"\u17D2"+string(c),
				UnicodeWidthCharWidth(c).UnwrapOr(1),
				UnicodeWidthCharWidthCjk(c).UnwrapOr(1),
			)
		}
	})
}

func TestItTests_test_khmer_qaa(t *testing.T) {
	assertWidth(t, "\u17A4", 2, 2)
	assertWidth(t, "\u17A2\u17A6", 2, 2)
}

func TestItTests_test_khmer_sign_beyyal(t *testing.T) {
	assertWidth(t, "\u17D8", 3, 3)
	assertWidth(t, "\u17D4\u179B\u17D4", 3, 3)
}

func TestItTests_test_emoji_modifier(t *testing.T) {
	assertWidth(t, "\U0001F46A", 2, 2)
	assertWidth(t, "\U0001F3FB", 2, 2)
	assertWidth(t, "\U0001F46A\U0001F3FB", 2, 2)
	assertWidth(t, "\U0001F46A\u200D\u200D\U0001F3FB", 4, 4)
}

func TestItTests_test_emoji_zwj(t *testing.T) {
	assertWidth(t, "🧑‍🤝‍🧑", 2, 2)

	assertWidth(t, "🇮🇱🕊️🇵🇸", 6, 6)
	assertWidth(t, "🇵🇸\u200D🕊️\u200D🇮🇱", 2, 2)
	assertWidth(t, "🇮🇱\u200D🕊️\u200D\u200D🇵🇸", 4, 4)
	assertWidth(t, "🇵🇸\u200D\u200D🕊️\u200D🇮🇱", 4, 4)

	assertWidth(t, "🇦🇦\u200D🇦🇦", 2, 2)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦", 3, 3)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦", 3, 3)

	assertWidth(t, "🇦🇦\u200D\u200D🇦🇦", 4, 4)
	assertWidth(t, "🇦🇦\u200D🇦\u200D🇦🇦", 5, 5)
	assertWidth(t, "🇦🇦\u200D🇦🇦\u200D🇦🇦", 2, 2)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦\u200D🇦🇦", 5, 5)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦🇦\u200D🇦🇦", 4, 4)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦🇦🇦\u200D🇦🇦", 7, 7)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦🇦🇦🇦\u200D🇦🇦", 6, 6)
	assertWidth(t, "🇦🇦\u200D🇦🇦🇦🇦🇦🇦🇦\u200D🇦🇦", 9, 9)

	assertWidth(t, "🏴󠁧󠁢󠁷󠁬󠁳󠁿", 2, 2)
	assertWidth(t, "🏴󠁧󠁢󠁥󠁮󠁧󠁿\u200D🏴󠁧󠁢󠁳󠁣󠁴󠁿\u200D🏴󠁧󠁢󠁷󠁬󠁳󠁿", 2, 2)

	assertWidth(t, "🇦👪\u200D🏿", 3, 3)
	assertWidth(t, "🇦🏿\u200D🏿", 3, 3)

	assertWidthChar(t, '🏴', rs.Some(2), rs.Some(2))
	assertWidth(t, "\U000E0031", 0, 0)
	assertWidth(t, "\U000E0063", 0, 0)
	assertWidth(t, "\U000E007F", 0, 0)
	assertWidth(t, "🏴\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0031\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0063\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E007F\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0031\U000E007F\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E007F\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E0031\U000E007F\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E0063\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0031\U000E0031\U000E0031\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0063\U000E0063\U000E007F\u200DⓂ️", 4, 4)
	assertWidth(t, "🏴\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0063\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 2, 2)
	assertWidth(t, "🏴\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E0063\U000E007F\u200DⓂ️", 4, 4)

	assertWidth(t, "a\u200D🏴󠁧󠁢󠁷󠁬󠁳󠁿", 3, 3)
	assertWidth(t, "👪\u200Da", 3, 3)
	assertWidth(t, "a\u200Da", 2, 2)

	assertWidth(t, "*\uFE0F", 2, 2)
	assertWidth(t, "*\u20E3", 1, 1)
	assertWidth(t, "*️⃣", 2, 2)
	assertWidth(t, "*\uFE0F", 2, 2)
	assertWidth(t, "*\u20E3\uFE0F", 1, 1)
	assertWidth(t, "*️⃣\u200D👪", 2, 2)
	assertWidth(t, "*\u20E3\uFE0F\u200D👪", 3, 3)
	assertWidth(t, "*\u20E3\u200D👪", 3, 3)
	assertWidth(t, "*\uFE0F\u200D👪", 2, 2)
	assertWidth(t, "*️⃣\u20E3\u200D👪", 4, 4)
	assertWidth(t, "*\uFE0F\uFE0F\u20E3\u200D👪", 4, 4)

	assertWidth(t, "🇦👪\u200D🏿\u200D👪🏻\u200DⓂ️\u200D*\uFE0F\u200D🇦🇦\u200D🏴󠁧󠁢󠁷󠁬󠁳󠁿\u200D👪", 3, 3)
}

func TestItTests_emoji_test_file(t *testing.T) {
	// The crate reads tests/emoji-test.txt; gen.go copies that file to
	// testdata/emoji-test.txt.
	normFile, err := os.Open(filepath.Join("testdata", "emoji-test.txt"))
	if err != nil {
		t.Fatalf("run `go generate` first to copy `emoji-test.txt`: %v", err)
	}
	defer normFile.Close()
	lines := bufio.NewScanner(normFile)
	for lines.Scan() {
		line := lines.Text()
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}

		cps, status, found := strings.Cut(line, ";")
		if !found {
			t.Fatalf("line without `;`: %q", line)
		}
		status = strings.TrimSpace(status)
		if strings.HasPrefix(status, "fully-qualified") || strings.HasPrefix(status, "component") {
			var emoji strings.Builder
			for _, s := range strings.Split(strings.TrimSpace(cps), " ") {
				cp, err := strconv.ParseUint(s, 16, 32)
				if err != nil {
					t.Fatal(err)
				}
				emoji.WriteRune(rune(cp))
			}
			// `dbg!(&emoji)` is left out: the assertion names the string.
			assertWidth(t, emoji.String(), 2, 2)
		}
	}
	if err := lines.Err(); err != nil {
		t.Fatal(err)
	}
}

func TestItTests_ambiguous_line_break(t *testing.T) {
	assertWidth(t, "\u24EA", 1, 2)
	assertWidth(t, "\u2616", 1, 2)
	assertWidth(t, "\u2780", 1, 2)
}

func TestItTests_test_vs1_vs2_vs3(t *testing.T) {
	assertWidthChar(t, '\uFE00', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\uFE01', rs.Some(0), rs.Some(0))
	assertWidthChar(t, '\uFE02', rs.Some(0), rs.Some(0))
	scalarValues(func(c rune) {
		if c == 0x2018 || c == 0x2019 || c == 0x201C || c == 0x201D {
			assertWidthChar(t, c, rs.Some(1), rs.Some(2))
			assertWidth(t, string(c)+"\uFE00", 1, 1)
			assertWidth(t, string(c)+"\uFE00\uFE01", 1, 1)
			assertWidth(t, string(c)+"\uFE01", 2, 2)
			assertWidth(t, string(c)+"\uFE01\uFE00", 2, 2)
			assertWidth(t, string(c)+"\uFE02", 1, 1)
			assertWidth(t, string(c)+"\uFE02\uFE01", 1, 1)
		} else {
			assertEq(t,
				UnicodeWidthStrWidth(string(c)+"\uFE00"),
				UnicodeWidthCharWidth(c).UnwrapOr(1),
				c,
			)
			// cfg(feature = "cjk")
			assertEq(t,
				UnicodeWidthStrWidthCjk(string(c)+"\uFE00"),
				UnicodeWidthCharWidthCjk(c).UnwrapOr(1),
				c,
			)
			assertEq(t,
				UnicodeWidthStrWidth(string(c)+"\uFE01"),
				UnicodeWidthCharWidth(c).UnwrapOr(1),
				c,
			)
			// cfg(feature = "cjk")
			assertEq(t,
				UnicodeWidthStrWidthCjk(string(c)+"\uFE01"),
				UnicodeWidthCharWidthCjk(c).UnwrapOr(1),
				c,
			)
			assertEq(t,
				UnicodeWidthStrWidth(string(c)+"\uFE02"),
				UnicodeWidthCharWidth(c).UnwrapOr(1),
				c,
			)
			// cfg(feature = "cjk")
			assertEq(t,
				UnicodeWidthStrWidthCjk(string(c)+"\uFE02"),
				UnicodeWidthCharWidthCjk(c).UnwrapOr(1),
				c,
			)
		}
	})
}

// Test traits are unsealed

// Rust's `Foo` implements both traits. Their `width` methods have different
// results, which one Go type cannot have, so there is one type for each
// trait.

// cfg(feature = "cjk")
type itTestsFooChar struct{}

// cfg(feature = "cjk")
type itTestsFooStr struct{}

var (
	_ UnicodeWidthChar = itTestsFooChar{}
	_ UnicodeWidthStr  = itTestsFooStr{}
)

// cfg(feature = "cjk")
func (itTestsFooChar) Width() rs.Option[int] {
	return rs.Some(0)
}

func (itTestsFooChar) WidthCjk() rs.Option[int] {
	return rs.Some(0)
}

// cfg(feature = "cjk")
func (itTestsFooStr) Width() int {
	return 0
}

func (itTestsFooStr) WidthCjk() int {
	return 0
}

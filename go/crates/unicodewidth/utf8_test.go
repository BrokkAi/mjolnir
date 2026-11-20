package unicodewidth

// This file has no counterpart in the crate. A Rust `char` is a Unicode scalar
// value and a Rust `str` is valid UTF-8; a Go rune and a Go string need not
// be. doc.go says how this package measures what Rust cannot hold, and these
// tests hold it to that.

import (
	"strings"
	"testing"
	"unicode/utf8"
)

// TestInvalidRunesMeasuredAsReplacementCharacter checks that a rune that is
// not a scalar value is measured as U+FFFD.
func TestInvalidRunesMeasuredAsReplacementCharacter(t *testing.T) {
	for _, c := range []rune{-1, -0x110000, 0xD800, 0xDBFF, 0xDC00, 0xDFFF, 0x110000, 0x7FFFFFFF} {
		if got, want := UnicodeWidthCharWidth(c), UnicodeWidthCharWidth(utf8.RuneError); got != want {
			t.Errorf("width of %#x is %v, the width of U+FFFD is %v", c, got, want)
		}
		if got, want := UnicodeWidthCharWidthCjk(c), UnicodeWidthCharWidthCjk(utf8.RuneError); got != want {
			t.Errorf("CJK width of %#x is %v, the width of U+FFFD is %v", c, got, want)
		}
	}
}

// TestInvalidUtf8MeasuredAsReplacementCharacters checks that each byte of a
// string that is not part of a valid encoding is measured as a U+FFFD of its
// own: the string is as wide as the string with each such byte replaced by
// U+FFFD.
func TestInvalidUtf8MeasuredAsReplacementCharacters(t *testing.T) {
	inputs := []string{
		"\xff", "a\xffb", "\xe2\x80", "\xe2\x80a", "\xed\xa0\x80", "\xc0\xaf", "\xf4\x90\x80\x80",
		"\U0001F468\xe2\x80\U0001F469", "\U0001F468\u200D\xff\U0001F469", "\xf0\x9f\x87\xa6\xf0\x9f",
		"\U0001F1E6\xff\U0001F1E6", "\r\xff\n", "\xff\u0301", "\u0644\xff\u0627", "<\xff\u0338",
		"\u2018\xff\uFE01", "a\xff\xfe\xfd", "\x80\x80\x80",
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
		if got, want := UnicodeWidthStrWidth(s), UnicodeWidthStrWidth(replaced); got != want {
			t.Fatalf("width of %+q is %d, with U+FFFD for each bad byte %d", s, got, want)
		}
		if got, want := UnicodeWidthStrWidthCjk(s), UnicodeWidthStrWidthCjk(replaced); got != want {
			t.Fatalf("CJK width of %+q is %d, with U+FFFD for each bad byte %d", s, got, want)
		}
	}
}

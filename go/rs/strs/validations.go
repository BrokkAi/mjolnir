// Port of core 1.99.1 src/str/validations.rs

package strs

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// utf8IsContByte is utf8_is_cont_byte: whether the byte is a UTF-8
// continuation byte (it starts with the bits `10`).
func utf8IsContByte(b byte) bool {
	return int8(b) < -64
}

// isUtf8CharBoundary is u8::is_utf8_char_boundary.
func isUtf8CharBoundary(b byte) bool {
	// This is bit magic equivalent to: b < 128 || b >= 192
	return int8(b) >= -0x40
}

// nextCodePoint is next_code_point: the first char of s and its length in
// bytes. Rust reads a string it knows to be valid; a Go string may hold
// anything, and a byte that does not start a valid sequence is read as
// U+FFFD of length 1, as Go's own decoding does.
func nextCodePoint(s string) (rune, int) {
	if len(s) > 0 && s[0] < utf8.RuneSelf {
		return rune(s[0]), 1
	}
	return utf8.DecodeRuneInString(s)
}

// nextCodePointReverse is next_code_point_reverse: the last char of s and its
// length in bytes.
func nextCodePointReverse(s string) (rune, int) {
	if n := len(s); n > 0 && s[n-1] < utf8.RuneSelf {
		return rune(s[n-1]), 1
	}
	return utf8.DecodeLastRuneInString(s)
}

// runUtf8Validation is run_utf8_validation: it walks through v checking that
// it is a valid UTF-8 sequence, and reports the first error otherwise.
func runUtf8Validation(v []byte) (Utf8Error, bool) {
	index := 0
	length := len(v)

	for index < length {
		oldOffset := index
		fail := func(errorLen uint8) (Utf8Error, bool) {
			if errorLen == 0 {
				return Utf8Error{validUpTo: oldOffset, errorLen: rs.None[uint8]()}, false
			}
			return Utf8Error{validUpTo: oldOffset, errorLen: rs.Some(errorLen)}, false
		}

		first := v[index]
		if first >= 128 {
			w := utf8CharWidth(first)
			// 2-byte encoding is for codepoints  \u{0080} to  \u{07ff}
			//        first  C2 80        last DF BF
			// 3-byte encoding is for codepoints  \u{0800} to  \u{ffff}
			//        first  E0 A0 80     last EF BF BF
			//   excluding surrogates codepoints  \u{d800} to  \u{dfff}
			//               ED A0 80 to       ED BF BF
			// 4-byte encoding is for codepoints \u{10000} to \u{10ffff}
			//        first  F0 90 80 80  last F4 8F BF BF
			//
			// Use the UTF-8 syntax from the RFC
			//
			// https://tools.ietf.org/html/rfc3629
			// UTF8-1      = %x00-7F
			// UTF8-2      = %xC2-DF UTF8-tail
			// UTF8-3      = %xE0 %xA0-BF UTF8-tail / %xE1-EC 2( UTF8-tail ) /
			//               %xED %x80-9F UTF8-tail / %xEE-EF 2( UTF8-tail )
			// UTF8-4      = %xF0 %x90-BF 2( UTF8-tail ) / %xF1-F3 3( UTF8-tail ) /
			//               %xF4 %x80-8F 2( UTF8-tail )
			switch w {
			case 2:
				index++
				// we needed data, but there was none: error!
				if index >= length {
					return fail(0)
				}
				if int8(v[index]) >= -64 {
					return fail(1)
				}
			case 3:
				index++
				if index >= length {
					return fail(0)
				}
				second := v[index]
				switch {
				case first == 0xE0 && second >= 0xA0 && second <= 0xBF:
				case first >= 0xE1 && first <= 0xEC && second >= 0x80 && second <= 0xBF:
				case first == 0xED && second >= 0x80 && second <= 0x9F:
				case first >= 0xEE && first <= 0xEF && second >= 0x80 && second <= 0xBF:
				default:
					return fail(1)
				}
				index++
				if index >= length {
					return fail(0)
				}
				if int8(v[index]) >= -64 {
					return fail(2)
				}
			case 4:
				index++
				if index >= length {
					return fail(0)
				}
				second := v[index]
				switch {
				case first == 0xF0 && second >= 0x90 && second <= 0xBF:
				case first >= 0xF1 && first <= 0xF3 && second >= 0x80 && second <= 0xBF:
				case first == 0xF4 && second >= 0x80 && second <= 0x8F:
				default:
					return fail(1)
				}
				index++
				if index >= length {
					return fail(0)
				}
				if int8(v[index]) >= -64 {
					return fail(2)
				}
				index++
				if index >= length {
					return fail(0)
				}
				if int8(v[index]) >= -64 {
					return fail(3)
				}
			default:
				return fail(1)
			}
			index++
		} else {
			// Ascii case. (Rust skips ahead two words at a time when the
			// pointer is aligned; the bytes it accepts are the same.)
			index++
		}
	}

	return Utf8Error{}, true
}

// utf8CharWidth is utf8_char_width: given a first byte, how many bytes are in
// this UTF-8 character, and 0 for a byte that cannot start one. It is the
// table UTF8_CHAR_WIDTH of RFC 3629 written as ranges.
func utf8CharWidth(b byte) int {
	switch {
	case b < 0x80:
		return 1
	case b < 0xC2:
		return 0
	case b < 0xE0:
		return 2
	case b < 0xF0:
		return 3
	case b < 0xF5:
		return 4
	default:
		return 0
	}
}

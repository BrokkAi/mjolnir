// Port of core 1.99.1 src/str/validations.rs (run_utf8_validation)

package vte

import "github.com/BrokkAi/mjolnir/go/rs"

// fromUtf8 is `core::str::from_utf8` reduced to what the parser reads from
// its result: whether v is UTF-8 and, when it is not, Utf8Error::valid_up_to
// and Utf8Error::error_len.
//
// It is a copy of Rust's validation because the parser's handling of partial
// and invalid sequences depends on exactly where Rust stops and how many
// bytes it blames: an incomplete sequence at the end of the input has no
// error length, and a sequence cut short by a byte that cannot continue it
// has the length of the part that was read. Go's unicode/utf8 reports
// neither. go/rs/strs.FromUtf8 is the same function and can replace this
// file.
func fromUtf8(v []uint8) (validUpTo int, errorLen rs.Option[int], ok bool) {
	index := 0
	len_ := len(v)

	for index < len_ {
		oldOffset := index

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
				index += 1
				// we needed data, but there was none: error!
				if index >= len_ {
					return oldOffset, rs.None[int](), false
				}
				if int8(v[index]) >= -64 {
					return oldOffset, rs.Some(1), false
				}
			case 3:
				index += 1
				if index >= len_ {
					return oldOffset, rs.None[int](), false
				}
				second := v[index]
				switch {
				case first == 0xE0 && second >= 0xA0 && second <= 0xBF:
				case first >= 0xE1 && first <= 0xEC && second >= 0x80 && second <= 0xBF:
				case first == 0xED && second >= 0x80 && second <= 0x9F:
				case first >= 0xEE && first <= 0xEF && second >= 0x80 && second <= 0xBF:
				default:
					return oldOffset, rs.Some(1), false
				}
				index += 1
				if index >= len_ {
					return oldOffset, rs.None[int](), false
				}
				if int8(v[index]) >= -64 {
					return oldOffset, rs.Some(2), false
				}
			case 4:
				index += 1
				if index >= len_ {
					return oldOffset, rs.None[int](), false
				}
				second := v[index]
				switch {
				case first == 0xF0 && second >= 0x90 && second <= 0xBF:
				case first >= 0xF1 && first <= 0xF3 && second >= 0x80 && second <= 0xBF:
				case first == 0xF4 && second >= 0x80 && second <= 0x8F:
				default:
					return oldOffset, rs.Some(1), false
				}
				index += 1
				if index >= len_ {
					return oldOffset, rs.None[int](), false
				}
				if int8(v[index]) >= -64 {
					return oldOffset, rs.Some(2), false
				}
				index += 1
				if index >= len_ {
					return oldOffset, rs.None[int](), false
				}
				if int8(v[index]) >= -64 {
					return oldOffset, rs.Some(3), false
				}
			default:
				return oldOffset, rs.Some(1), false
			}
			index += 1
		} else {
			// Rust skips ASCII a machine word at a time here; one byte at a
			// time gives the same answer.
			index += 1
		}
	}

	return len_, rs.None[int](), true
}

// Given a first byte, determines how many bytes are in this UTF-8 character.
func utf8CharWidth(b uint8) int {
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

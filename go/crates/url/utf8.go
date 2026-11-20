// Port of core 1.99.1 src/str/lossy.rs, src/str/validations.rs and
// src/str/error.rs, and alloc 1.99.1 src/string.rs (from_utf8_lossy).
//
// These belong to go/rs/strs (strs.FromUtf8, strs.FromUtf8Lossy), which did
// not exist when this package was written. They are private copies; replace
// them with the strs functions when that package lands.

package url

import "strconv"

func itoa(i int) string { return strconv.Itoa(i) }

// utf8CharWidth is core::str::utf8_char_width: given a first byte, how many
// bytes are in this UTF-8 character (0 for a byte that cannot start one).
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
	}
	return 0
}

// utf8Error is core::str::Utf8Error: where a byte sequence stops being
// UTF-8.
type utf8Error struct {
	validUpTo int
	// errorLen is 0 when the end of the input was reached unexpectedly
	// (error_len() == None), and otherwise the length of the invalid
	// sequence.
	errorLen int
}

func (e *utf8Error) Error() string {
	if e.errorLen != 0 {
		return "invalid utf-8 sequence of " + itoa(e.errorLen) + " bytes from index " + itoa(e.validUpTo)
	}
	return "incomplete utf-8 byte sequence from index " + itoa(e.validUpTo)
}

// runUtf8Validation is core::str::validations::run_utf8_validation, which is
// what std::str::from_utf8 reports.
func runUtf8Validation(v []byte) *utf8Error {
	index := 0
	length := len(v)
	for index < length {
		oldOffset := index
		fail := func(errorLen int) *utf8Error {
			return &utf8Error{validUpTo: oldOffset, errorLen: errorLen}
		}
		first := v[index]
		if first >= 128 {
			w := utf8CharWidth(first)
			// next is the `next!()` macro: the following byte, or an
			// "incomplete" error at the end of the input.
			next := func() (byte, bool) {
				index++
				if index >= length {
					return 0, false
				}
				return v[index], true
			}
			switch w {
			case 2:
				b, ok := next()
				if !ok {
					return fail(0)
				}
				if int8(b) >= -64 {
					return fail(1)
				}
			case 3:
				b, ok := next()
				if !ok {
					return fail(0)
				}
				switch {
				case first == 0xE0 && 0xA0 <= b && b <= 0xBF:
				case 0xE1 <= first && first <= 0xEC && 0x80 <= b && b <= 0xBF:
				case first == 0xED && 0x80 <= b && b <= 0x9F:
				case 0xEE <= first && first <= 0xEF && 0x80 <= b && b <= 0xBF:
				default:
					return fail(1)
				}
				b, ok = next()
				if !ok {
					return fail(0)
				}
				if int8(b) >= -64 {
					return fail(2)
				}
			case 4:
				b, ok := next()
				if !ok {
					return fail(0)
				}
				switch {
				case first == 0xF0 && 0x90 <= b && b <= 0xBF:
				case 0xF1 <= first && first <= 0xF3 && 0x80 <= b && b <= 0xBF:
				case first == 0xF4 && 0x80 <= b && b <= 0x8F:
				default:
					return fail(1)
				}
				b, ok = next()
				if !ok {
					return fail(0)
				}
				if int8(b) >= -64 {
					return fail(2)
				}
				b, ok = next()
				if !ok {
					return fail(0)
				}
				if int8(b) >= -64 {
					return fail(3)
				}
			default:
				return fail(1)
			}
			index++
		} else {
			index++
		}
	}
	return nil
}

// utf8ChunksNext is one step of core::str::Utf8Chunks: the length of the
// longest valid prefix of source, and the length of the invalid sequence
// that follows it (0 at the end of the input). An invalid sequence is a
// maximal prefix of a well-formed sequence, or one byte.
func utf8ChunksNext(source []byte) (valid int, invalid int) {
	const tagContU8 = 128
	safeGet := func(i int) byte {
		if i < len(source) {
			return source[i]
		}
		return 0
	}

	i := 0
	validUpTo := 0
loop:
	for i < len(source) {
		b := source[i]
		i++

		if b < 128 {
			// ASCII
		} else {
			w := utf8CharWidth(b)

			switch w {
			case 2:
				if safeGet(i)&192 != tagContU8 {
					break loop
				}
				i++
			case 3:
				second := safeGet(i)
				switch {
				case b == 0xE0 && 0xA0 <= second && second <= 0xBF:
				case 0xE1 <= b && b <= 0xEC && 0x80 <= second && second <= 0xBF:
				case b == 0xED && 0x80 <= second && second <= 0x9F:
				case 0xEE <= b && b <= 0xEF && 0x80 <= second && second <= 0xBF:
				default:
					break loop
				}
				i++
				if safeGet(i)&192 != tagContU8 {
					break loop
				}
				i++
			case 4:
				second := safeGet(i)
				switch {
				case b == 0xF0 && 0x90 <= second && second <= 0xBF:
				case 0xF1 <= b && b <= 0xF3 && 0x80 <= second && second <= 0xBF:
				case b == 0xF4 && 0x80 <= second && second <= 0x8F:
				default:
					break loop
				}
				i++
				if safeGet(i)&192 != tagContU8 {
					break loop
				}
				i++
				if safeGet(i)&192 != tagContU8 {
					break loop
				}
				i++
			default:
				break loop
			}
		}

		validUpTo = i
	}
	return validUpTo, i - validUpTo
}

// stringFromUtf8Lossy is String::from_utf8_lossy: v as a string, with each
// invalid sequence replaced by one U+FFFD. Go's own conversions substitute
// differently (one replacement for every byte, or for a whole run).
func stringFromUtf8Lossy(v []byte) string {
	valid, invalid := utf8ChunksNext(v)
	if invalid == 0 {
		return string(v[:valid])
	}

	const replacement = "�"

	res := make([]byte, 0, len(v))
	res = append(res, v[:valid]...)
	res = append(res, replacement...)
	rest := v[valid+invalid:]
	for len(rest) > 0 {
		valid, invalid = utf8ChunksNext(rest)
		res = append(res, rest[:valid]...)
		if invalid != 0 {
			res = append(res, replacement...)
		}
		rest = rest[valid+invalid:]
	}
	return string(res)
}

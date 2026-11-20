// Port of core 1.99.1 src/str/lossy.rs, src/str/validations.rs (utf8_char_width)
// and alloc 1.99.1 src/string.rs (String::from_utf8_lossy)
//
// go/rs/strs has the same functions for product code (strs.FromUtf8Lossy).
// They are repeated here, unexported, because a path's Display and Debug forms
// are defined by them and this package must not depend on a package that is
// still being written.

package rspath

import "strings"

// utf8Chunk is core::str::Utf8Chunk: a run of valid UTF-8 followed by the
// broken sequence that ended it (at most three bytes; empty at the end of the
// input).
type utf8Chunk struct {
	valid   string
	invalid string
}

// utf8CharWidth is core::str::utf8_char_width: how many bytes a UTF-8
// sequence that starts with b has, or 0 when b cannot start one.
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

// nextUtf8Chunk is Utf8Chunks::next: it splits the next chunk off source.
// ok is false when source is empty.
func nextUtf8Chunk(source string) (chunk utf8Chunk, rest string, ok bool) {
	if len(source) == 0 {
		return utf8Chunk{}, source, false
	}

	const tagContU8 = 128
	safeGet := func(xs string, i int) byte {
		if i < len(xs) {
			return xs[i]
		}
		return 0
	}

	i := 0
	validUpTo := 0
scan:
	for i < len(source) {
		b := source[i]
		i++

		if b < 128 {
			// This could be a `1 => ...` case in the match below, but for
			// the common case of all-ASCII inputs, we bypass loading the
			// sizeable UTF8_CHAR_WIDTH table into cache.
		} else {
			w := utf8CharWidth(b)

			switch w {
			case 2:
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
			case 3:
				next := safeGet(source, i)
				switch {
				case b == 0xE0 && next >= 0xA0 && next <= 0xBF:
				case b >= 0xE1 && b <= 0xEC && next >= 0x80 && next <= 0xBF:
				case b == 0xED && next >= 0x80 && next <= 0x9F:
				case b >= 0xEE && b <= 0xEF && next >= 0x80 && next <= 0xBF:
				default:
					break scan
				}
				i++
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
			case 4:
				next := safeGet(source, i)
				switch {
				case b == 0xF0 && next >= 0x90 && next <= 0xBF:
				case b >= 0xF1 && b <= 0xF3 && next >= 0x80 && next <= 0xBF:
				case b == 0xF4 && next >= 0x80 && next <= 0x8F:
				default:
					break scan
				}
				i++
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
			default:
				break scan
			}
		}

		validUpTo = i
	}

	inspected, remaining := source[:i], source[i:]
	return utf8Chunk{valid: inspected[:validUpTo], invalid: inspected[validUpTo:]}, remaining, true
}

// fromUtf8Lossy is String::from_utf8_lossy: every maximal broken sequence
// becomes one U+FFFD. Go's own conversions substitute differently (one
// replacement for each byte, or one for a whole run).
func fromUtf8Lossy(v string) string {
	chunk, rest, ok := nextUtf8Chunk(v)
	if !ok {
		return ""
	}
	if len(chunk.invalid) == 0 {
		return chunk.valid
	}

	const replacement = "�"

	var res strings.Builder
	res.Grow(len(v))
	res.WriteString(chunk.valid)
	res.WriteString(replacement)

	for {
		chunk, rest, ok = nextUtf8Chunk(rest)
		if !ok {
			break
		}
		res.WriteString(chunk.valid)
		if len(chunk.invalid) != 0 {
			res.WriteString(replacement)
		}
	}

	return res.String()
}

// utf8ChunksDebug is the Debug form of core::str::lossy::Debug, which is how
// a Unix OsStr prints with `{:?}`: the valid parts escaped as a str is, the
// broken bytes as \xNN.
func utf8ChunksDebug(source string) string {
	var f strings.Builder
	f.WriteByte('"')

	rest := source
	for {
		chunk, remaining, ok := nextUtf8Chunk(rest)
		if !ok {
			break
		}
		rest = remaining

		// Valid part.
		writeStrEscaped(&f, chunk.valid)

		// Broken parts of string as hex escape.
		for i := 0; i < len(chunk.invalid); i++ {
			const hexDigits = "0123456789ABCDEF"
			b := chunk.invalid[i]
			f.WriteString(`\x`)
			f.WriteByte(hexDigits[b>>4])
			f.WriteByte(hexDigits[b&15])
		}
	}

	f.WriteByte('"')
	return f.String()
}

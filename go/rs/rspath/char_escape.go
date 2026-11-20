// Port of core 1.99.1 src/char/methods.rs (escape_debug_ext and the
// predicates it reads), src/escape.rs (escape_unicode) and
// src/unicode/unicode_data.rs (skip_search, white_space::lookup)
//
// go/rs/strs and the formatter in go/rs have the same escaping for `{:?}` of
// a str. It is repeated here, unexported, because the Debug form of a path is
// defined by it.

package rspath

import (
	"sort"
	"strings"
	"unicode/utf8"
)

type shortOffsetRunHeader uint32

func newShortOffsetRunHeader(startIndex int, prefixSum uint32) shortOffsetRunHeader {
	return shortOffsetRunHeader(uint32(startIndex)<<21 | prefixSum)
}

func (h shortOffsetRunHeader) startIndex() int {
	return int(uint32(h) >> 21)
}

func (h shortOffsetRunHeader) prefixSum() uint32 {
	return uint32(h) & ((1 << 21) - 1)
}

// skipSearch is unicode_data::skip_search.
//
// The last element of shortOffsetRuns must be greater than char::MAX, and
// the start indices of all its elements must be less than len(offsets).
func skipSearch(needleChar rune, shortOffsetRuns []shortOffsetRunHeader, offsets []uint8) bool {
	needle := uint32(needleChar)

	// binary_search_by_key(&(needle << 11), |header| header.0 << 11), with
	// Ok(idx) => idx + 1 and Err(idx) => idx: the number of headers whose key
	// is not greater than the needle's.
	lastIdx := sort.Search(len(shortOffsetRuns), func(i int) bool {
		return uint32(shortOffsetRuns[i])<<11 > needle<<11
	})

	offsetIdx := shortOffsetRuns[lastIdx].startIndex()
	var length int
	if lastIdx+1 < len(shortOffsetRuns) {
		length = shortOffsetRuns[lastIdx+1].startIndex() - offsetIdx
	} else {
		length = len(offsets) - offsetIdx
	}

	var prev uint32
	if lastIdx > 0 {
		prev = shortOffsetRuns[lastIdx-1].prefixSum()
	}

	total := needle - prev
	var prefixSum uint32
	for i := 0; i < length-1; i++ {
		offset := offsets[offsetIdx]
		prefixSum += uint32(offset)
		if prefixSum > total {
			break
		}
		offsetIdx++
	}
	return offsetIdx%2 == 1
}

// unicodeWhiteSpace is unicode_data::white_space::lookup.
func unicodeWhiteSpace(c rune) bool {
	switch uint32(c) >> 8 {
	case 0:
		return unicodeWhitespaceMap[c&0xff]&1 != 0
	case 22:
		return c == 0x1680
	case 32:
		return unicodeWhitespaceMap[c&0xff]&2 != 0
	case 48:
		return c == 0x3000
	default:
		return false
	}
}

// charIsWhitespace is char::is_whitespace.
func charIsWhitespace(c rune) bool {
	switch {
	case c == ' ' || (c >= 0x09 && c <= 0x0d):
		return true
	case c <= 0x84:
		return false
	default:
		return unicodeWhiteSpace(c)
	}
}

// charIsControl is char::is_control.
func charIsControl(c rune) bool {
	// According to
	// https://www.unicode.org/policies/stability_policy.html#Property_Value,
	// the set of codepoints in `Cc` will never change.
	// So we can just hard-code the patterns to match against instead of using a table.
	return c <= 0x1f || (c >= 0x7f && c <= 0x9f)
}

// charIsPrivateUse is char::is_private_use.
func charIsPrivateUse(c rune) bool {
	// According to
	// https://www.unicode.org/policies/stability_policy.html#Property_Value,
	// the set of codepoints in `Co` will never change.
	// So we can just hard-code the patterns to match against instead of using a table.
	return (c >= 0xE000 && c <= 0xF8FF) || (c >= 0xF0000 && c <= 0xFFFFD) || (c >= 0x100000 && c <= 0x10FFFD)
}

// charIsFormatControl is char::is_format_control.
func charIsFormatControl(c rune) bool {
	return c > 0xAC && unicodeCf(c)
}

// charIsAssigned is char::is_assigned.
func charIsAssigned(c rune) bool {
	switch {
	case c <= 0x377:
		return true
	case c >= 0x378 && c <= 0x3FFFD:
		return !unicodeCnPlanes03(c)
	// Assigned character ranges in planes 4 and above.
	// `src/tools/unicode-table-generator/src/main.rs` asserts that this is correct
	case c == 0xE0001,
		c >= 0xE0020 && c <= 0xE007F,
		c >= 0xE0100 && c <= 0xE01EF,
		c >= 0xF0000 && c <= 0xFFFFD,
		c >= 0x100000 && c <= 0x10FFFD:
		return true
	default:
		return false
	}
}

// charIsDefaultIgnorable is char::is_default_ignorable.
func charIsDefaultIgnorable(c rune) bool {
	return c > 0xAC && unicodeDefaultIgnorableCodePoint(c)
}

// charIsGraphemeExtender is char::is_grapheme_extender.
func charIsGraphemeExtender(c rune) bool {
	return c > 0x02FF && unicodeGraphemeExtend(c)
}

// writeEscapeDebug writes c as char::escape_debug_ext does with the arguments
// every string Debug impl passes: escape_grapheme_extender true,
// escape_single_quote false, escape_double_quote true.
func writeEscapeDebug(f *strings.Builder, c rune) {
	switch {
	// Special escapes
	case c == '"':
		f.WriteString(`\"`)
	case c == '\\':
		f.WriteString(`\\`)
	case c == '\n':
		f.WriteString(`\n`)
	case c == '\t':
		f.WriteString(`\t`)
	case c == '\r':
		f.WriteString(`\r`)
	case c == 0:
		f.WriteString(`\0`)

	// ASCII fast path,
	// plus U+FF9E HALFWIDTH KATAKANA VOICED SOUND MARK
	// and U+FF9F HALFWIDTH KATAKANA SEMI-VOICED SOUND MARK
	// which should not be escaped despite being grapheme extenders.
	case (c >= 0x20 && c <= 0x7E) || c == 0xFF9E || c == 0xFF9F:
		f.WriteRune(c)

	case charIsControl(c) ||
		charIsPrivateUse(c) ||
		charIsWhitespace(c) ||
		charIsGraphemeExtender(c) ||
		charIsDefaultIgnorable(c) ||
		charIsFormatControl(c) ||
		!charIsAssigned(c):
		writeEscapeUnicode(f, c)

	default:
		f.WriteRune(c)
	}
}

// writeEscapeUnicode writes c as \u{NNNN}: lower-case hexadecimal with no
// leading zeros.
func writeEscapeUnicode(f *strings.Builder, c rune) {
	const hexDigits = "0123456789abcdef"
	f.WriteString(`\u{`)
	started := false
	for shift := 20; shift >= 0; shift -= 4 {
		digit := (uint32(c) >> uint(shift)) & 15
		if digit != 0 || started || shift == 0 {
			started = true
			f.WriteByte(hexDigits[digit])
		}
	}
	f.WriteByte('}')
}

// writeStrEscaped writes the characters of s, which must be valid UTF-8,
// escaped as `{:?}` of a str escapes them, without the surrounding quotes.
func writeStrEscaped(f *strings.Builder, s string) {
	for len(s) > 0 {
		c, size := utf8.DecodeRuneInString(s)
		writeEscapeDebug(f, c)
		s = s[size:]
	}
}

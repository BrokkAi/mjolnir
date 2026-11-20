// Port of core 1.99.1 src/str/mod.rs

package strs

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// usizeText prints a `usize` held in a Go int.
func usizeText(n int) string {
	return strconv.FormatUint(uint64(uint(n)), 10)
}

// charDebug is `format!("{c:?}")`: the char in single quotes, escaped as
// Debug escapes it.
func charDebug(c rune) string {
	escaped, _ := charEscapeDebugExt(c, escapeDebugExtArgs{
		escapeGraphemeExtender: true,
		escapeSingleQuote:      true,
		escapeDoubleQuote:      false,
	})
	return "'" + escaped + "'"
}

// sliceErrorFail is slice_error_fail_rt: the message of the panic for a range
// that cannot slice s.
func sliceErrorFail(s string, begin, end int) string {
	length := len(s)

	// 1. begin is OOB.
	if uint(begin) > uint(length) {
		return "start byte index " + usizeText(begin) + " is out of bounds for string of length " + usizeText(length)
	}

	// 2. end is OOB.
	if uint(end) > uint(length) {
		return "end byte index " + usizeText(end) + " is out of bounds for string of length " + usizeText(length)
	}

	// 3. range is backwards.
	if begin > end {
		return "byte range starts at " + usizeText(begin) + " but ends at " + usizeText(end)
	}

	// 4. begin is inside a character.
	if !IsCharBoundary(s, begin) {
		floor := FloorCharBoundary(s, begin)
		ceil := CeilCharBoundary(s, begin)
		ch, _ := nextCodePoint(s[floor:ceil])
		return "start byte index " + usizeText(begin) + " is not a char boundary; it is inside " + charDebug(ch) +
			" (bytes " + usizeText(floor) + ".." + usizeText(ceil) + " of string)"
	}

	// 5. end is inside a character.
	if !IsCharBoundary(s, end) {
		floor := FloorCharBoundary(s, end)
		ceil := CeilCharBoundary(s, end)
		ch, _ := nextCodePoint(s[floor:ceil])
		return "end byte index " + usizeText(end) + " is not a char boundary; it is inside " + charDebug(ch) +
			" (bytes " + usizeText(floor) + ".." + usizeText(ceil) + " of string)"
	}

	// 6. end is OOB and range is inclusive (end == len).
	// This test cannot be combined with 2. above because for cases like
	// `"abcαβγ"[4..9]` the error is that 4 is inside 'α', not that 9 is OOB.
	return "end byte index " + usizeText(end) + " is out of bounds for string of length " + usizeText(length)
}

// IsCharBoundary is str::is_char_boundary: whether the byte at index starts a
// char, or index is the end of s. An index beyond the end is not a boundary.
func IsCharBoundary(s string, index int) bool {
	// 0 is always ok.
	if index == 0 {
		return true
	}

	if uint(index) >= uint(len(s)) {
		// For `true` we have two options:
		//
		// - index == len(s)
		//   Empty strings are valid, so return true
		// - index > len(s)
		//   In this case return false
		return index == len(s)
	}
	return isUtf8CharBoundary(s[index])
}

// FloorCharBoundary is str::floor_char_boundary: the closest char boundary at
// or before index, and the length of s for an index beyond it.
func FloorCharBoundary(s string, index int) int {
	if uint(index) >= uint(len(s)) {
		return len(s)
	}
	for index > 0 && !isUtf8CharBoundary(s[index]) {
		index--
	}
	return index
}

// CeilCharBoundary is str::ceil_char_boundary: the closest char boundary at
// or after index, and the length of s for an index beyond it.
func CeilCharBoundary(s string, index int) int {
	if uint(index) >= uint(len(s)) {
		return len(s)
	}
	i := index
	for !isUtf8CharBoundary(s[i]) {
		i++
		if i >= len(s) {
			break
		}
	}
	return i
}

// SplitAt is str::split_at: s cut at byte mid, which must be on a char
// boundary.
func SplitAt(s string, mid int) (string, string) {
	if !IsCharBoundary(s, mid) {
		rs.PanicAt(1, sliceErrorFail(s, 0, mid))
	}
	return s[:mid], s[mid:]
}

// SplitAtChecked is str::split_at_checked: s cut at byte mid, or None when
// mid is beyond the end or inside a char.
func SplitAtChecked(s string, mid int) rs.Option[rs.Tuple2[string, string]] {
	if !IsCharBoundary(s, mid) {
		return rs.None[rs.Tuple2[string, string]]()
	}
	return rs.Some(rs.T2(s[:mid], s[mid:]))
}

// Trim is str::trim: s without leading and trailing whitespace
// (char::is_whitespace: the Unicode White_Space property).
func Trim(s string) string {
	return TrimEnd(TrimStart(s))
}

// TrimStart is str::trim_start.
func TrimStart(s string) string {
	for len(s) > 0 {
		c, size := nextCodePoint(s)
		if !IsWhitespace(c) {
			break
		}
		s = s[size:]
	}
	return s
}

// TrimEnd is str::trim_end.
func TrimEnd(s string) string {
	for len(s) > 0 {
		c, size := nextCodePointReverse(s)
		if !IsWhitespace(c) {
			break
		}
		s = s[:len(s)-size]
	}
	return s
}

// trimMatchesWith is str::trim_matches for any double-ended searcher.
func trimMatchesWith(s string, matcher searcher) string {
	i := 0
	j := 0
	if a, b, ok := matcher.nextReject(); ok {
		i = a
		j = b // Remember earliest known match, correct it below if
		// last match is different
	}
	if _, b, ok := matcher.nextRejectBack(); ok {
		j = b
	}
	return s[i:j]
}

// trimStartMatchesWith is str::trim_start_matches for any searcher.
func trimStartMatchesWith(s string, matcher searcher) string {
	i := len(s)
	if a, _, ok := matcher.nextReject(); ok {
		i = a
	}
	return s[i:]
}

// trimEndMatchesWith is str::trim_end_matches for any searcher.
func trimEndMatchesWith(s string, matcher searcher) string {
	j := 0
	if _, b, ok := matcher.nextRejectBack(); ok {
		j = b
	}
	return s[:j]
}

// IsAscii is str::is_ascii: whether every char of s is ASCII.
func IsAscii(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] >= 0x80 {
			return false
		}
	}
	return true
}

// EqIgnoreAsciiCase is str::eq_ignore_ascii_case: whether a and b are equal
// once the ASCII letters of both are lowercased. Other chars must match
// exactly.
func EqIgnoreAsciiCase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		if CharToAsciiLowercase(a[i]) != CharToAsciiLowercase(b[i]) {
			return false
		}
	}
	return true
}

// MakeAsciiUppercase is str::make_ascii_uppercase: it uppercases the ASCII
// letters of *s in place.
func MakeAsciiUppercase(s *string) {
	*s = ToAsciiUppercase(*s)
}

// MakeAsciiLowercase is str::make_ascii_lowercase: it lowercases the ASCII
// letters of *s in place.
func MakeAsciiLowercase(s *string) {
	*s = ToAsciiLowercase(*s)
}

// TrimAsciiStart is str::trim_ascii_start: s without leading ASCII
// whitespace (u8::is_ascii_whitespace).
func TrimAsciiStart(s string) string {
	for len(s) > 0 && IsAsciiWhitespace(s[0]) {
		s = s[1:]
	}
	return s
}

// TrimAsciiEnd is str::trim_ascii_end.
func TrimAsciiEnd(s string) string {
	for len(s) > 0 && IsAsciiWhitespace(s[len(s)-1]) {
		s = s[:len(s)-1]
	}
	return s
}

// TrimAscii is str::trim_ascii.
func TrimAscii(s string) string {
	return TrimAsciiEnd(TrimAsciiStart(s))
}

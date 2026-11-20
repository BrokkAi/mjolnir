// Port of core 1.99.1 src/str/iter.rs

package strs

import "github.com/BrokkAi/mjolnir/go/rs"

// Rust's string iterators are lazy and most of them run from both ends. The
// functions of this package return what the iterator would yield, as a
// slice: a slice can be ranged over, indexed, measured and walked backwards,
// which covers `next`, `nth`, `last`, `count`, `rev`, `peekable` and
// `collect`. The cost is that the whole input is always walked, and that a
// predicate is called for every char even when the Rust code would have
// stopped early.

// Chars is str::chars: the chars of s. For the valid UTF-8 a Rust string
// always is, it never yields U+FFFD unless the string holds that char.
//
// `for c in s.chars()` is also `for _, c := range s` in Go, which decodes the
// same way; use Chars when the chars are needed as a value.
func Chars(s string) []rune {
	chars := make([]rune, 0, CharsCount(s))
	for len(s) > 0 {
		c, size := nextCodePoint(s)
		chars = append(chars, c)
		s = s[size:]
	}
	return chars
}

// CharsRev is `s.chars().rev()`: the chars of s, last first.
func CharsRev(s string) []rune {
	chars := make([]rune, 0, CharsCount(s))
	for len(s) > 0 {
		c, size := nextCodePointReverse(s)
		chars = append(chars, c)
		s = s[:len(s)-size]
	}
	return chars
}

// CharsCount is `s.chars().count()`: the number of chars in s. (Rust counts
// the bytes that are not continuation bytes, and so does this.)
func CharsCount(s string) int {
	count := 0
	for i := 0; i < len(s); i++ {
		if !utf8IsContByte(s[i]) {
			count++
		}
	}
	return count
}

// CharsNext is `s.chars().next()`: the first char of s, or None when s is
// empty. Unlike Chars(s)[0] it does not walk the whole string.
func CharsNext(s string) rs.Option[rune] {
	if s == "" {
		return rs.None[rune]()
	}
	c, _ := nextCodePoint(s)
	return rs.Some(c)
}

// CharsNextBack is `s.chars().next_back()`, `s.chars().last()` and
// `s.chars().rev().next()`: the last char of s, or None when s is empty.
func CharsNextBack(s string) rs.Option[rune] {
	if s == "" {
		return rs.None[rune]()
	}
	c, _ := nextCodePointReverse(s)
	return rs.Some(c)
}

// CharIndices is str::char_indices: each char of s with the byte index it
// starts at.
//
// `for (i, c) in s.char_indices()` is also `for i, c := range s` in Go; use
// CharIndices when the pairs are needed as a value.
func CharIndices(s string) []rs.Tuple2[int, rune] {
	indices := make([]rs.Tuple2[int, rune], 0, CharsCount(s))
	for offset := 0; offset < len(s); {
		c, size := nextCodePoint(s[offset:])
		indices = append(indices, rs.T2(offset, c))
		offset += size
	}
	return indices
}

// CharIndicesRev is `s.char_indices().rev()`: each char of s with the byte
// index it starts at, last first.
func CharIndicesRev(s string) []rs.Tuple2[int, rune] {
	indices := make([]rs.Tuple2[int, rune], 0, CharsCount(s))
	for end := len(s); end > 0; {
		c, size := nextCodePointReverse(s[:end])
		end -= size
		indices = append(indices, rs.T2(end, c))
	}
	return indices
}

// splitInternal is iter::SplitInternal: the state behind split, rsplit,
// split_terminator, split_inclusive and, inside splitNInternal, splitn.
type splitInternal struct {
	start              int
	end                int
	haystack           string
	matcher            searcher
	allowTrailingEmpty bool
	finished           bool
}

func newSplitInternal(haystack string, matcher searcher, allowTrailingEmpty bool) splitInternal {
	return splitInternal{
		start:              0,
		end:                len(haystack),
		haystack:           haystack,
		matcher:            matcher,
		allowTrailingEmpty: allowTrailingEmpty,
		finished:           false,
	}
}

func (s *splitInternal) getEnd() (string, bool) {
	if !s.finished {
		s.finished = true

		if s.allowTrailingEmpty || s.end-s.start > 0 {
			return s.haystack[s.start:s.end], true
		}
	}

	return "", false
}

func (s *splitInternal) next() (string, bool) {
	if s.finished {
		return "", false
	}

	if a, b, ok := s.matcher.nextMatch(); ok {
		elt := s.haystack[s.start:a]
		s.start = b
		return elt, true
	}
	return s.getEnd()
}

func (s *splitInternal) nextInclusive() (string, bool) {
	if s.finished {
		return "", false
	}

	if _, b, ok := s.matcher.nextMatch(); ok {
		elt := s.haystack[s.start:b]
		s.start = b
		return elt, true
	}
	return s.getEnd()
}

func (s *splitInternal) nextBack() (string, bool) {
	if s.finished {
		return "", false
	}

	if !s.allowTrailingEmpty {
		s.allowTrailingEmpty = true
		if elt, ok := s.nextBack(); ok && elt != "" {
			return elt, true
		}
		if s.finished {
			return "", false
		}
	}

	if a, b, ok := s.matcher.nextMatchBack(); ok {
		elt := s.haystack[b:s.end]
		s.end = a
		return elt, true
	}
	s.finished = true
	return s.haystack[s.start:s.end], true
}

// splitNInternal is iter::SplitNInternal.
type splitNInternal struct {
	iter  splitInternal
	count int
}

func (s *splitNInternal) next() (string, bool) {
	switch s.count {
	case 0:
		return "", false
	case 1:
		s.count = 0
		return s.iter.getEnd()
	default:
		s.count--
		return s.iter.next()
	}
}

func (s *splitNInternal) nextBack() (string, bool) {
	switch s.count {
	case 0:
		return "", false
	case 1:
		s.count = 0
		return s.iter.getEnd()
	default:
		s.count--
		return s.iter.nextBack()
	}
}

// collect drains an iterator's next function into a slice.
func collect(next func() (string, bool)) []string {
	var parts []string
	for {
		part, ok := next()
		if !ok {
			return parts
		}
		parts = append(parts, part)
	}
}

// The functions below are the bodies of the pattern methods of `str`, once
// for any searcher; str_pattern.go gives each its four public spellings.

func splitWith(s string, m searcher) []string {
	iter := newSplitInternal(s, m, true)
	return collect(iter.next)
}

func splitInclusiveWith(s string, m searcher) []string {
	iter := newSplitInternal(s, m, false)
	return collect(iter.nextInclusive)
}

func rsplitWith(s string, m searcher) []string {
	iter := newSplitInternal(s, m, true)
	return collect(iter.nextBack)
}

func splitTerminatorWith(s string, m searcher) []string {
	iter := newSplitInternal(s, m, false)
	return collect(iter.next)
}

func rsplitTerminatorWith(s string, m searcher) []string {
	iter := newSplitInternal(s, m, false)
	return collect(iter.nextBack)
}

// splitCount is a `usize` count held in a Go int: a negative value is a count
// larger than any string has parts.
func splitCount(n int) int {
	if n < 0 {
		return rs.UsizeMax
	}
	return n
}

func splitnWith(s string, n int, m searcher) []string {
	iter := splitNInternal{iter: newSplitInternal(s, m, true), count: splitCount(n)}
	return collect(iter.next)
}

func rsplitnWith(s string, n int, m searcher) []string {
	iter := splitNInternal{iter: newSplitInternal(s, m, true), count: splitCount(n)}
	return collect(iter.nextBack)
}

func matchesWith(s string, m searcher) []string {
	var found []string
	for {
		a, b, ok := m.nextMatch()
		if !ok {
			return found
		}
		found = append(found, s[a:b])
	}
}

func rmatchesWith(s string, m searcher) []string {
	var found []string
	for {
		a, b, ok := m.nextMatchBack()
		if !ok {
			return found
		}
		found = append(found, s[a:b])
	}
}

func matchIndicesWith(s string, m searcher) []rs.Tuple2[int, string] {
	var found []rs.Tuple2[int, string]
	for {
		start, end, ok := m.nextMatch()
		if !ok {
			return found
		}
		found = append(found, rs.T2(start, s[start:end]))
	}
}

func rmatchIndicesWith(s string, m searcher) []rs.Tuple2[int, string] {
	var found []rs.Tuple2[int, string]
	for {
		start, end, ok := m.nextMatchBack()
		if !ok {
			return found
		}
		found = append(found, rs.T2(start, s[start:end]))
	}
}

// Lines is str::lines: the lines of s. A line ends at a line feed, or at a
// carriage return directly followed by a line feed; the ending is not part of
// the line. A carriage return on its own is ordinary text, the last line
// needs no ending, and a final ending does not start another, empty line.
func Lines(s string) []string {
	iter := newSplitInternal(s, newCharSearcher(s, '\n'), false)
	var lines []string
	for {
		line, ok := iter.nextInclusive()
		if !ok {
			return lines
		}
		lines = append(lines, linesMap(line))
	}
}

// linesMap is LinesMap.
func linesMap(line string) string {
	if n := len(line); n > 0 && line[n-1] == '\n' {
		line = line[:n-1]
	} else {
		return line
	}
	if n := len(line); n > 0 && line[n-1] == '\r' {
		line = line[:n-1]
	}
	return line
}

// SplitWhitespace is str::split_whitespace: the parts of s between runs of
// whitespace (char::is_whitespace), never an empty part.
func SplitWhitespace(s string) []string {
	iter := newSplitInternal(s, newFnSearcher(s, IsWhitespace), true)
	var parts []string
	for {
		part, ok := iter.next()
		if !ok {
			return parts
		}
		if part != "" {
			parts = append(parts, part)
		}
	}
}

// SplitAsciiWhitespace is str::split_ascii_whitespace: the parts of s between
// runs of ASCII whitespace (u8::is_ascii_whitespace), never an empty part.
func SplitAsciiWhitespace(s string) []string {
	var parts []string
	start := 0
	for i := 0; i <= len(s); i++ {
		if i == len(s) || IsAsciiWhitespace(s[i]) {
			if i > start {
				parts = append(parts, s[start:i])
			}
			start = i + 1
		}
	}
	return parts
}

// EncodeUtf16 is str::encode_utf16: s as UTF-16 code units.
func EncodeUtf16(s string) []uint16 {
	units := make([]uint16, 0, len(s))
	for len(s) > 0 {
		c, size := nextCodePoint(s)
		s = s[size:]
		if CharLenUtf16(c) == 1 {
			units = append(units, uint16(c))
		} else {
			code := uint32(c) - 0x1_0000
			units = append(units, uint16(code>>10)|0xD800, uint16(code&0x3FF)|0xDC00)
		}
	}
	return units
}

// EscapeDebug is str::escape_debug: each char of s escaped with
// char::escape_debug, except that a grapheme extender is escaped only at the
// start of the string.
func EscapeDebug(s string) string {
	escaped := make([]byte, 0, len(s))
	args := escapeDebugExtArgsEscapeAll
	for len(s) > 0 {
		c, size := nextCodePoint(s)
		s = s[size:]
		text, _ := charEscapeDebugExt(c, args)
		escaped = append(escaped, text...)
		// CharEscapeDebugContinue
		args = escapeDebugExtArgs{escapeGraphemeExtender: false, escapeSingleQuote: true, escapeDoubleQuote: true}
	}
	return string(escaped)
}

// EscapeDefault is str::escape_default: each char of s escaped with
// char::escape_default.
func EscapeDefault(s string) string {
	escaped := make([]byte, 0, len(s))
	for len(s) > 0 {
		c, size := nextCodePoint(s)
		s = s[size:]
		if c >= 0x20 && c <= 0x7e && c != '\\' && c != '\'' && c != '"' {
			escaped = append(escaped, byte(c))
		} else {
			escaped = append(escaped, CharEscapeDefault(c)...)
		}
	}
	return string(escaped)
}

// EscapeUnicode is str::escape_unicode: each char of s escaped with
// char::escape_unicode.
func EscapeUnicode(s string) string {
	escaped := make([]byte, 0, len(s)*6)
	for len(s) > 0 {
		c, size := nextCodePoint(s)
		s = s[size:]
		escaped = append(escaped, CharEscapeUnicode(c)...)
	}
	return string(escaped)
}

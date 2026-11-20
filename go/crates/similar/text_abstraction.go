// Port of similar 2.7.0 src/text/abstraction.rs

package similar

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Reference to a [`DiffableStr`].
//
// This type exists because while the library only really provides ways to
// work with `&str` and `&[u8]` there are types that deref into those string
// slices such as `String` and `Vec<u8>`.
//
// `DiffableStrRef` needs no Go counterpart: `&str`, `String` and `Cow<str>`
// are all `string`.

// All supported diffable strings.
//
// The text module can work with different types of strings depending
// on how the crate is compiled.  Out of the box `&str` is always supported
// but with the `bytes` feature one can also work with `[u8]` slices for
// as long as they are ASCII compatible.
//
// Mjolnir builds the crate without `bytes`, so `str` is the only
// implementation, and the trait's methods are the functions below, each
// named DiffableStr followed by the method.

// Splits the value into newlines with newlines attached.
func DiffableStrTokenizeLines(self string) []string {
	// `\r` and `\n` are single bytes that never occur inside another
	// character's encoding, so walking bytes finds what Rust's walk over
	// characters finds.
	lastPos := 0
	lines := []string{}

	for idx := 0; idx < len(self); idx++ {
		c := self[idx]
		if c == '\r' {
			if idx+1 < len(self) && self[idx+1] == '\n' {
				lines = append(lines, self[lastPos:idx+2])
				idx++
				lastPos = idx + 1
			} else {
				lines = append(lines, self[lastPos:idx+1])
				lastPos = idx + 1
			}
		} else if c == '\n' {
			lines = append(lines, self[lastPos:idx+1])
			lastPos = idx + 1
		}
	}

	if lastPos < len(self) {
		lines = append(lines, self[lastPos:])
	}

	return lines
}

// Splits the value into newlines with newlines separated.
func DiffableStrTokenizeLinesAndNewlines(self string) []string {
	rv := []string{}

	for idx := 0; idx < len(self); {
		c, size := utf8.DecodeRuneInString(self[idx:])
		isNewline := c == '\r' || c == '\n'
		start := idx
		end := idx + size
		for end < len(self) {
			nextChar, nextSize := utf8.DecodeRuneInString(self[end:])
			if (nextChar == '\r' || nextChar == '\n') != isNewline {
				break
			}
			end += nextSize
		}
		rv = append(rv, self[start:end])
		idx = end
	}

	return rv
}

// Tokenizes into words.
func DiffableStrTokenizeWords(self string) []string {
	rv := []string{}

	for idx := 0; idx < len(self); {
		c, size := utf8.DecodeRuneInString(self[idx:])
		isWhitespace := charIsWhitespace(c)
		start := idx
		end := idx + size
		for end < len(self) {
			nextChar, nextSize := utf8.DecodeRuneInString(self[end:])
			if charIsWhitespace(nextChar) != isWhitespace {
				break
			}
			end += nextSize
		}
		rv = append(rv, self[start:end])
		idx = end
	}

	return rv
}

// Tokenizes the input into characters.
func DiffableStrTokenizeChars(self string) []string {
	rv := []string{}
	for i := 0; i < len(self); {
		_, size := utf8.DecodeRuneInString(self[i:])
		rv = append(rv, self[i:i+size])
		i += size
	}
	return rv
}

// Decodes the string (potentially) lossy.
func DiffableStrAsStr(self string) rs.Option[string] {
	return rs.Some(self)
}

// Decodes the string (potentially) lossy.
func DiffableStrToStringLossy(self string) string {
	return self
}

// Checks if the string ends in a newline.
func DiffableStrEndsWithNewline(self string) bool {
	if len(self) == 0 {
		return false
	}
	last := self[len(self)-1]
	return last == '\r' || last == '\n'
}

// The length of the string.
func DiffableStrLen(self string) int {
	return len(self)
}

// Slices the string.
func DiffableStrSlice(self string, rng Range) string {
	return self[rng.Start:rng.End]
}

// Returns the string as slice of raw bytes.
func DiffableStrAsBytes(self string) []byte {
	return []byte(self)
}

// Checks if the string is empty.
func DiffableStrIsEmpty(self string) bool {
	return DiffableStrLen(self) == 0
}

// charIsWhitespace is `char::is_whitespace`: the Unicode `White_Space`
// property, written out (it is the same set in every Unicode version since
// 6.3) so that the answer does not depend on the Unicode tables of the Go
// toolchain.
func charIsWhitespace(c rune) bool {
	switch {
	case c == ' ' || ('\x09' <= c && c <= '\x0d'):
		return true
	case c <= 0x84:
		return false
	}
	switch c {
	case 0x0085, 0x00A0, 0x1680, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000:
		return true
	}
	return 0x2000 <= c && c <= 0x200A
}

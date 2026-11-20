// Port of alloc 1.99.1 src/str.rs

package strs

import (
	"math"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// mapUppercaseSigma is map_uppercase_sigma: the lowercase of the Σ at byte i
// of from, which is ς at the end of a word and σ elsewhere.
func mapUppercaseSigma(from string, i int) rune {
	// See https://www.unicode.org/versions/latest/core-spec/chapter-3/#G54277
	// for the definition of `Final_Sigma`.
	before := from[:i]
	precededByCased := false
	for len(before) > 0 {
		c, size := nextCodePointReverse(before)
		before = before[:len(before)-size]
		if !charIsCaseIgnorable(c) {
			precededByCased = charIsCased(c)
			break
		}
	}
	after := from[i+len("Σ"):]
	followedByCased := false
	for len(after) > 0 {
		c, size := nextCodePoint(after)
		after = after[size:]
		if !charIsCaseIgnorable(c) {
			followedByCased = charIsCased(c)
			break
		}
	}
	isWordFinal := precededByCased && !followedByCased
	if isWordFinal {
		return 'ς'
	}
	return 'σ'
}

// replaceWith is str::replace for any searcher.
func replaceWith(s string, matcher searcher, to string) string {
	var result strings.Builder
	lastEnd := 0
	matched := false
	for {
		start, end, ok := matcher.nextMatch()
		if !ok {
			break
		}
		matched = true
		result.WriteString(s[lastEnd:start])
		result.WriteString(to)
		lastEnd = end
	}
	if !matched {
		return s
	}
	result.WriteString(s[lastEnd:])
	return result.String()
}

// replacenWith is str::replacen for any searcher.
func replacenWith(s string, matcher searcher, to string, count int) string {
	count = splitCount(count)
	var result strings.Builder
	lastEnd := 0
	for ; count > 0; count-- {
		start, end, ok := matcher.nextMatch()
		if !ok {
			break
		}
		result.WriteString(s[lastEnd:start])
		result.WriteString(to)
		lastEnd = end
	}
	result.WriteString(s[lastEnd:])
	return result.String()
}

// appendCaseMapping appends the chars of a case mapping, without its padding.
func appendCaseMapping(dst []byte, mapped [3]rune) []byte {
	var buffer [CharMaxLenUtf8]byte
	for _, c := range caseMapping(mapped) {
		dst = append(dst, buffer[:encodeUtf8Raw(c, buffer[:])]...)
	}
	return dst
}

// ToLowercase is str::to_lowercase: the lowercase of s by the Unicode
// Lowercase mapping of the pinned toolchain, with the full mappings (İ becomes
// two chars) and with Σ becoming ς at the end of a word and σ elsewhere.
func ToLowercase(s string) string {
	// convert_while_ascii: the ASCII prefix needs no table.
	prefixLen := 0
	needsWork := false
	for prefixLen < len(s) && s[prefixLen] < 0x80 {
		if IsAsciiUppercase(s[prefixLen]) {
			needsWork = true
		}
		prefixLen++
	}
	if prefixLen == len(s) && !needsWork {
		return s
	}
	out := make([]byte, 0, len(s))
	for i := 0; i < prefixLen; i++ {
		out = append(out, CharToAsciiLowercase(s[i]))
	}

	var buffer [CharMaxLenUtf8]byte
	for i := prefixLen; i < len(s); {
		c, size := nextCodePoint(s[i:])
		if c == 'Σ' {
			// Σ maps to σ, except at the end of a word where it maps to ς.
			// This is the only conditional (contextual) but language-independent mapping
			// in `SpecialCasing.txt`,
			// so hard-code it rather than have a generic "condition" mechanism.
			// See https://github.com/rust-lang/rust/issues/26035
			sigmaLowercase := mapUppercaseSigma(s, i)
			out = append(out, buffer[:encodeUtf8Raw(sigmaLowercase, buffer[:])]...)
		} else {
			out = appendCaseMapping(out, conversionsToLower(c))
		}
		i += size
	}
	return string(out)
}

// ToUppercase is str::to_uppercase: the uppercase of s by the Unicode
// Uppercase mapping of the pinned toolchain, with the full mappings (ß
// becomes SS).
func ToUppercase(s string) string {
	prefixLen := 0
	needsWork := false
	for prefixLen < len(s) && s[prefixLen] < 0x80 {
		if IsAsciiLowercase(s[prefixLen]) {
			needsWork = true
		}
		prefixLen++
	}
	if prefixLen == len(s) && !needsWork {
		return s
	}
	out := make([]byte, 0, len(s))
	for i := 0; i < prefixLen; i++ {
		out = append(out, CharToAsciiUppercase(s[i]))
	}

	for i := prefixLen; i < len(s); {
		c, size := nextCodePoint(s[i:])
		out = appendCaseMapping(out, conversionsToUpper(c))
		i += size
	}
	return string(out)
}

// Repeat is str::repeat: s repeated n times. It panics as Rust does when the
// length of the result does not fit a usize.
func Repeat(s string, n int) string {
	if len(s) == 0 || n == 0 {
		return ""
	}
	if n < 0 || n > math.MaxInt/len(s) {
		rs.PanicAt(1, "capacity overflow")
	}
	return strings.Repeat(s, n)
}

// ToAsciiUppercase is str::to_ascii_uppercase: s with its ASCII letters
// uppercased and every other char unchanged.
func ToAsciiUppercase(s string) string {
	for i := 0; i < len(s); i++ {
		if IsAsciiLowercase(s[i]) {
			out := []byte(s)
			for ; i < len(out); i++ {
				out[i] = CharToAsciiUppercase(out[i])
			}
			return string(out)
		}
	}
	return s
}

// ToAsciiLowercase is str::to_ascii_lowercase: s with its ASCII letters
// lowercased and every other char unchanged.
func ToAsciiLowercase(s string) string {
	for i := 0; i < len(s); i++ {
		if IsAsciiUppercase(s[i]) {
			out := []byte(s)
			for ; i < len(out); i++ {
				out[i] = CharToAsciiLowercase(out[i])
			}
			return string(out)
		}
	}
	return s
}

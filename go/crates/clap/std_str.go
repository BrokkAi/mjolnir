// Port of core 1.99.1 src/str/mod.rs, src/fmt/mod.rs (`impl Debug for str`)
// and src/num/mod.rs (`from_str_radix`), and of core 1.99.1
// src/num/dec2flt (the grammar `f64::from_str` accepts)
//
// go/rs/strs has these for product code. The parts clap's output depends on
// are repeated here, unexported, because this package must not depend on a
// package that is still being written.

package clap

import (
	"math"
	"strconv"
	"strings"
	"unicode/utf8"
)

// toStringLossy is OsStr::to_string_lossy: the text, with every maximal
// broken UTF-8 sequence replaced by U+FFFD.
func toStringLossy(s string) string {
	if utf8.ValidString(s) {
		return s
	}
	return fromUtf8Lossy(s)
}

// utf8ValidUpTo is Utf8Error::valid_up_to: the length of the longest prefix
// of s that is valid UTF-8 (the whole length when s is valid).
func utf8ValidUpTo(s string) int {
	i := 0
	for i < len(s) {
		if s[i] < utf8.RuneSelf {
			i++
			continue
		}
		r, size := utf8.DecodeRuneInString(s[i:])
		if r == utf8.RuneError && size == 1 {
			return i
		}
		i += size
	}
	return i
}

// strDebug is `format!("{:?}", s)` for a str.
func strDebug(s string) string {
	var f strings.Builder
	f.Grow(len(s) + 2)
	f.WriteByte('"')
	writeStrEscaped(&f, s)
	f.WriteByte('"')
	return f.String()
}

// strContainsWhitespace is `s.contains(char::is_whitespace)`.
func strContainsWhitespace(s string) bool {
	for _, c := range s {
		if charIsWhitespace(c) {
			return true
		}
	}
	return false
}

// strTrimEnd is str::trim_end: without trailing White_Space characters.
func strTrimEnd(s string) string {
	for len(s) > 0 {
		c, size := utf8.DecodeLastRuneInString(s)
		if !charIsWhitespace(c) {
			break
		}
		s = s[:len(s)-size]
	}
	return s
}

// strTrimStart is str::trim_start.
func strTrimStart(s string) string {
	for len(s) > 0 {
		c, size := utf8.DecodeRuneInString(s)
		if !charIsWhitespace(c) {
			break
		}
		s = s[size:]
	}
	return s
}

// strTrim is str::trim.
func strTrim(s string) string {
	return strTrimEnd(strTrimStart(s))
}

// strCharsCount is `s.chars().count()` for a str.
func strCharsCount(s string) int {
	return utf8.RuneCountInString(s)
}

// asciiLower is u8::to_ascii_lowercase.
func asciiLower(b byte) byte {
	if b >= 'A' && b <= 'Z' {
		return b + ('a' - 'A')
	}
	return b
}

// strToAsciiLowercase is str::to_ascii_lowercase.
func strToAsciiLowercase(s string) string {
	out := []byte(s)
	for i := range out {
		out[i] = asciiLower(out[i])
	}
	return string(out)
}

// strEqIgnoreAsciiCase is str::eq_ignore_ascii_case.
func strEqIgnoreAsciiCase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		if asciiLower(a[i]) != asciiLower(b[i]) {
			return false
		}
	}
	return true
}

// parseIntError is core::num::ParseIntError: its Display text.
type parseIntError struct {
	message string
}

func (e *parseIntError) Error() string { return e.message }

var (
	errIntEmpty        = &parseIntError{message: "cannot parse integer from empty string"}
	errIntInvalidDigit = &parseIntError{message: "invalid digit found in string"}
	errIntPosOverflow  = &parseIntError{message: "number too large to fit in target type"}
	errIntNegOverflow  = &parseIntError{message: "number too small to fit in target type"}
)

// parseInt is `<iN>::from_str` for a signed type of the given width: the
// value is checked digit by digit, so the first problem met from the left is
// the one reported.
func parseInt(src string, bits uint) (int64, error) {
	if len(src) == 0 {
		return 0, errIntEmpty
	}
	if src == "+" || src == "-" {
		return 0, errIntInvalidDigit
	}
	isPositive := true
	digits := src
	switch src[0] {
	case '+':
		digits = src[1:]
	case '-':
		isPositive = false
		digits = src[1:]
	}
	maxValue := int64(1)<<(bits-1) - 1
	minValue := -maxValue - 1
	var result int64
	for i := 0; i < len(digits); i++ {
		c := digits[i]
		if c < '0' || c > '9' {
			return 0, errIntInvalidDigit
		}
		d := int64(c - '0')
		if isPositive {
			if result > (maxValue-d)/10 {
				return 0, errIntPosOverflow
			}
			result = result*10 + d
		} else {
			if result < (minValue+d)/10 {
				return 0, errIntNegOverflow
			}
			result = result*10 - d
		}
	}
	return result, nil
}

// parseUint is `<uN>::from_str` for an unsigned type of the given width. A
// leading `-` is not a sign for an unsigned type: it is an invalid digit.
func parseUint(src string, bits uint) (uint64, error) {
	if len(src) == 0 {
		return 0, errIntEmpty
	}
	if src == "+" || src == "-" {
		return 0, errIntInvalidDigit
	}
	digits := src
	if src[0] == '+' {
		digits = src[1:]
	}
	maxValue := ^uint64(0)
	if bits < 64 {
		maxValue = uint64(1)<<bits - 1
	}
	var result uint64
	for i := 0; i < len(digits); i++ {
		c := digits[i]
		if c < '0' || c > '9' {
			return 0, errIntInvalidDigit
		}
		d := uint64(c - '0')
		if result > (maxValue-d)/10 {
			return 0, errIntPosOverflow
		}
		result = result*10 + d
	}
	return result, nil
}

// parseFloatError is core::num::ParseFloatError: its Display text.
type parseFloatError struct {
	message string
}

func (e *parseFloatError) Error() string { return e.message }

var (
	errFloatEmpty   = &parseFloatError{message: "cannot parse float from empty string"}
	errFloatInvalid = &parseFloatError{message: "invalid float literal"}
)

// parseFloat is `f64::from_str` (bits 64) and `f32::from_str` (bits 32).
//
// Rust's grammar is narrower than strconv's: no hexadecimal form, no
// underscores, and `inf`, `infinity` and `nan` only as whole words. The value
// of an accepted number is the correctly rounded one in both.
func parseFloat(src string, bits int) (float64, error) {
	if len(src) == 0 {
		return 0, errFloatEmpty
	}
	s := src
	negative := s[0] == '-'
	if s[0] == '-' || s[0] == '+' {
		s = s[1:]
	}
	if len(s) == 0 {
		return 0, errFloatInvalid
	}
	if floatIsNumber(s) {
		value, err := strconv.ParseFloat(src, bits)
		if err != nil {
			// The only failure left is a range error, for which strconv
			// returns the infinity Rust returns.
			if numError, ok := err.(*strconv.NumError); !ok || numError.Err != strconv.ErrRange {
				return 0, errFloatInvalid
			}
		}
		return value, nil
	}
	lower := strToAsciiLowercase(s)
	switch lower {
	case "inf", "infinity":
		if negative {
			return math.Inf(-1), nil
		}
		return math.Inf(1), nil
	case "nan":
		nan := uint64(0x7ff8000000000000)
		if negative {
			nan |= 1 << 63
		}
		return math.Float64frombits(nan), nil
	}
	return 0, errFloatInvalid
}

// floatIsNumber is dec2flt's parse_number succeeding on the whole of s:
// digits with an optional fraction, at least one digit in all, and an
// optional exponent that has digits.
func floatIsNumber(s string) bool {
	i := 0
	digits := 0
	for i < len(s) && s[i] >= '0' && s[i] <= '9' {
		i++
		digits++
	}
	if i < len(s) && s[i] == '.' {
		i++
		for i < len(s) && s[i] >= '0' && s[i] <= '9' {
			i++
			digits++
		}
	}
	if digits == 0 {
		return false
	}
	if i < len(s) && (s[i] == 'e' || s[i] == 'E') {
		i++
		if i < len(s) && (s[i] == '-' || s[i] == '+') {
			i++
		}
		if i >= len(s) || s[i] < '0' || s[i] > '9' {
			return false
		}
		for i < len(s) && s[i] >= '0' && s[i] <= '9' {
			i++
		}
	}
	return i == len(s)
}

// usizeToString is `n.to_string()` for a usize held as an int, where
// rs.UsizeMax stands for usize::MAX.
func usizeToString(n int) string {
	return strconv.Itoa(n)
}

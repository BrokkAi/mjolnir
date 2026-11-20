// Port of chrono 0.4.44 src/format/scan.rs

package chrono

// Various scanning routines for the parser.

import (
	"strings"
	"unicode/utf8"
)

// isWhitespace is Rust's `char::is_whitespace`: the Unicode White_Space
// property.
func isWhitespace(c rune) bool {
	switch {
	case c == ' ' || ('\x09' <= c && c <= '\x0d'):
		return true
	case c <= 0x84:
		return false
	case c == 0x85 || c == 0xA0 || c == 0x1680 || c == 0x3000:
		return true
	case 0x2000 <= c && c <= 0x200A:
		return true
	case c == 0x2028 || c == 0x2029 || c == 0x202F || c == 0x205F:
		return true
	}
	return false
}

// trimStart is Rust's `str::trim_start`: s without its leading white space.
func trimStart(s string) string {
	return strings.TrimLeftFunc(s, isWhitespace)
}

func isAsciiDigit(c byte) bool { return '0' <= c && c <= '9' }

func isAsciiAlphabetic(c byte) bool {
	return ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z')
}

// equalIgnoreAsciiCase is `<[u8]>::eq_ignore_ascii_case`.
func equalIgnoreAsciiCase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		x, y := a[i], b[i]
		if 'A' <= x && x <= 'Z' {
			x += 'a' - 'A'
		}
		if 'A' <= y && y <= 'Z' {
			y += 'a' - 'A'
		}
		if x != y {
			return false
		}
	}
	return true
}

// scanNumber tries to parse the non-negative number from `min` to `max`
// digits.
//
// The absence of digits at all is an unconditional error.
// More than `max` digits are consumed up to the first `max` digits.
// Any number that does not fit in `i64` is an error.
func scanNumber(s string, min int, max int) (string, int64, error) {
	// We are only interested in ascii numbers, so we can work with the `str` as bytes. We stop on
	// the first non-numeric byte, which may be another ascii character or beginning of multi-byte
	// UTF-8 character.
	if len(s) < min {
		return "", 0, errTooShort
	}

	var n int64
	for i := 0; i < len(s) && i < max; i++ {
		c := s[i]
		if !isAsciiDigit(c) {
			if i < min {
				return "", 0, errInvalid
			}
			return s[i:], n, nil
		}

		var ok bool
		if n, ok = checkedMulI64(n, 10); ok {
			n, ok = checkedAddI64(n, int64(c-'0'))
		}
		if !ok {
			return "", 0, errOutOfRange
		}
	}

	if max < len(s) {
		return s[max:], n, nil
	}
	return s[len(s):], n, nil
}

// scanNanosecond tries to consume at least one digits as a fractional
// second. Returns the number of whole nanoseconds (0--999,999,999).
func scanNanosecond(s string) (string, uint32, error) {
	// record the number of digits consumed for later scaling.
	origlen := len(s)
	s, v64, err := scanNumber(s, 1, 9)
	if err != nil {
		return "", 0, err
	}
	v := uint32(v64) // 999,999,999 should fit u32
	consumed := origlen - len(s)

	// scale the number accordingly.
	scale := [10]uint32{0, 100_000_000, 10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1}
	v, ok := checkedMulU32(v, scale[consumed])
	if !ok {
		return "", 0, errOutOfRange
	}

	// if there are more than 9 digits, skip next digits.
	s = strings.TrimLeftFunc(s, func(c rune) bool { return '0' <= c && c <= '9' })

	return s, v, nil
}

// scanNanosecondFixed tries to consume a fixed number of digits as a
// fractional second. Returns the number of whole nanoseconds
// (0--999,999,999).
func scanNanosecondFixed(s string, digits int) (string, int64, error) {
	// record the number of digits consumed for later scaling.
	s, v, err := scanNumber(s, digits, digits)
	if err != nil {
		return "", 0, err
	}

	// scale the number accordingly.
	scale := [10]int64{0, 100_000_000, 10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1}
	v, ok := checkedMulI64(v, scale[digits])
	if !ok {
		return "", 0, errOutOfRange
	}

	return s, v, nil
}

// scanShortMonth0 tries to parse the month index (0 through 11) with the
// first three ASCII letters.
func scanShortMonth0(s string) (string, uint8, error) {
	if len(s) < 3 {
		return "", 0, errTooShort
	}
	var month0 uint8
	switch string([]byte{s[0] | 32, s[1] | 32, s[2] | 32}) {
	case "jan":
		month0 = 0
	case "feb":
		month0 = 1
	case "mar":
		month0 = 2
	case "apr":
		month0 = 3
	case "may":
		month0 = 4
	case "jun":
		month0 = 5
	case "jul":
		month0 = 6
	case "aug":
		month0 = 7
	case "sep":
		month0 = 8
	case "oct":
		month0 = 9
	case "nov":
		month0 = 10
	case "dec":
		month0 = 11
	default:
		return "", 0, errInvalid
	}
	return s[3:], month0, nil
}

// scanShortWeekday tries to parse the weekday with the first three ASCII
// letters.
func scanShortWeekday(s string) (string, Weekday, error) {
	if len(s) < 3 {
		return "", 0, errTooShort
	}
	var weekday Weekday
	switch string([]byte{s[0] | 32, s[1] | 32, s[2] | 32}) {
	case "mon":
		weekday = Weekday_Mon
	case "tue":
		weekday = Weekday_Tue
	case "wed":
		weekday = Weekday_Wed
	case "thu":
		weekday = Weekday_Thu
	case "fri":
		weekday = Weekday_Fri
	case "sat":
		weekday = Weekday_Sat
	case "sun":
		weekday = Weekday_Sun
	default:
		return "", 0, errInvalid
	}
	return s[3:], weekday, nil
}

// scanShortOrLongMonth0 tries to parse the month index (0 through 11) with
// short or long month names. It prefers long month names to short month
// names when both are possible.
func scanShortOrLongMonth0(s string) (string, uint8, error) {
	// lowercased month names, minus first three chars
	longMonthSuffixes := [12]string{
		"uary", "ruary", "ch", "il", "", "e", "y", "ust", "tember", "ober", "ember", "ember",
	}

	s, month0, err := scanShortMonth0(s)
	if err != nil {
		return "", 0, err
	}

	// tries to consume the suffix if possible
	suffix := longMonthSuffixes[month0]
	if len(s) >= len(suffix) && equalIgnoreAsciiCase(s[:len(suffix)], suffix) {
		s = s[len(suffix):]
	}

	return s, month0, nil
}

// scanShortOrLongWeekday tries to parse the weekday with short or long
// weekday names. It prefers long weekday names to short weekday names when
// both are possible.
func scanShortOrLongWeekday(s string) (string, Weekday, error) {
	// lowercased weekday names, minus first three chars
	longWeekdaySuffixes := [7]string{"day", "sday", "nesday", "rsday", "day", "urday", "day"}

	s, weekday, err := scanShortWeekday(s)
	if err != nil {
		return "", 0, err
	}

	// tries to consume the suffix if possible
	suffix := longWeekdaySuffixes[weekday.NumDaysFromMonday()]
	if len(s) >= len(suffix) && equalIgnoreAsciiCase(s[:len(suffix)], suffix) {
		s = s[len(suffix):]
	}

	return s, weekday, nil
}

// scanChar tries to consume exactly one given character.
func scanChar(s string, c1 byte) (string, error) {
	if len(s) == 0 {
		return "", errTooShort
	}
	if s[0] == c1 {
		return s[1:], nil
	}
	return "", errInvalid
}

// scanSpace tries to consume one or more whitespace.
func scanSpace(s string) (string, error) {
	trimmed := trimStart(s)
	if len(trimmed) < len(s) {
		return trimmed, nil
	} else if len(s) == 0 {
		return "", errTooShort
	}
	return "", errInvalid
}

// scanColonOrSpace consumes any number (including zero) of colon or spaces.
func scanColonOrSpace(s string) (string, error) {
	return strings.TrimLeftFunc(s, func(c rune) bool { return c == ':' || isWhitespace(c) }), nil
}

// scanTimezoneOffset parses a timezone from `s` and returns the offset in
// seconds.
//
// The `consumeColon` function is used to parse a mandatory or optional `:`
// separator between hours offset and minutes offset.
//
// The `allowMissingMinutes` flag allows the timezone minutes offset to be
// missing from `s`.
//
// The `allowTzMinusSign` flag allows the timezone offset negative character
// to also be `−` MINUS SIGN (U+2212) in addition to the typical
// ASCII-compatible `-` HYPHEN-MINUS (U+2D).
// This is part of [RFC 3339 & ISO 8601].
//
// [RFC 3339 & ISO 8601]: https://en.wikipedia.org/w/index.php?title=ISO_8601&oldid=1114309368#Time_offsets_from_UTC
func scanTimezoneOffset(
	s string,
	consumeColon func(string) (string, error),
	allowZulu bool,
	allowMissingMinutes bool,
	allowTzMinusSign bool,
) (string, int32, error) {
	if allowZulu {
		if len(s) > 0 && (s[0] == 'Z' || s[0] == 'z') {
			return s[1:], 0, nil
		}
	}

	if len(s) == 0 {
		return "", 0, errTooShort
	}
	var negative bool
	first, size := utf8.DecodeRuneInString(s)
	switch {
	case first == '+':
		// PLUS SIGN (U+2B)
		s = s[1:]

		negative = false
	case first == '-':
		// HYPHEN-MINUS (U+2D)
		s = s[1:]

		negative = true
	case first == '−' && size == 3:
		// MINUS SIGN (U+2212)
		if !allowTzMinusSign {
			return "", 0, errInvalid
		}
		s = s[size:]

		negative = true
	default:
		return "", 0, errInvalid
	}

	// hours (00--99)
	if len(s) < 2 {
		return "", 0, errTooShort
	}
	if !isAsciiDigit(s[0]) || !isAsciiDigit(s[1]) {
		return "", 0, errInvalid
	}
	hours := int32((s[0]-'0')*10 + (s[1] - '0'))
	s = s[2:]

	// colons (and possibly other separators)
	s, err := consumeColon(s)
	if err != nil {
		return "", 0, err
	}

	// minutes (00--59)
	// if the next two items are digits then we have to add minutes
	var minutes int32
	if len(s) >= 2 {
		m1, m2 := s[0], s[1]
		switch {
		case '0' <= m1 && m1 <= '5' && isAsciiDigit(m2):
			minutes = int32((m1-'0')*10 + (m2 - '0'))
		case '6' <= m1 && m1 <= '9' && isAsciiDigit(m2):
			return "", 0, errOutOfRange
		default:
			return "", 0, errInvalid
		}
	} else if allowMissingMinutes {
		minutes = 0
	} else {
		return "", 0, errTooShort
	}
	switch {
	case len(s) >= 2:
		s = s[2:]
	case len(s) == 0:
	default:
		return "", 0, errTooShort
	}

	seconds := hours*3600 + minutes*60
	if negative {
		return s, -seconds, nil
	}
	return s, seconds, nil
}

// scanTimezoneOffset2822 is the same as scanTimezoneOffset but also allows
// for RFC 2822 legacy timezones. May return 0 for an insufficient offset
// data (i.e. `-0000`).
// See [RFC 2822 Section 4.3].
//
// [RFC 2822 Section 4.3]: https://tools.ietf.org/html/rfc2822#section-4.3
func scanTimezoneOffset2822(s string) (string, int32, error) {
	// tries to parse legacy time zone names
	upto := len(s)
	for i := 0; i < len(s); i++ {
		if !isAsciiAlphabetic(s[i]) {
			upto = i
			break
		}
	}
	if upto > 0 {
		name := s[:upto]
		s := s[upto:]
		offsetHours := func(o int32) (string, int32, error) { return s, o * 3600, nil }
		// RFC 2822 requires support for some named North America timezones, a small subset of all
		// named timezones.
		if equalIgnoreAsciiCase(name, "gmt") ||
			equalIgnoreAsciiCase(name, "ut") ||
			equalIgnoreAsciiCase(name, "z") {
			return offsetHours(0)
		} else if equalIgnoreAsciiCase(name, "edt") {
			return offsetHours(-4)
		} else if equalIgnoreAsciiCase(name, "est") || equalIgnoreAsciiCase(name, "cdt") {
			return offsetHours(-5)
		} else if equalIgnoreAsciiCase(name, "cst") || equalIgnoreAsciiCase(name, "mdt") {
			return offsetHours(-6)
		} else if equalIgnoreAsciiCase(name, "mst") || equalIgnoreAsciiCase(name, "pdt") {
			return offsetHours(-7)
		} else if equalIgnoreAsciiCase(name, "pst") {
			return offsetHours(-8)
		} else if len(name) == 1 {
			c := name[0]
			if ('a' <= c && c <= 'i') || ('k' <= c && c <= 'y') || ('A' <= c && c <= 'I') || ('K' <= c && c <= 'Y') {
				// recommended by RFC 2822: consume but treat it as -0000
				return s, 0, nil
			}
		}
		return "", 0, errInvalid
	}
	return scanTimezoneOffset(s, func(s string) (string, error) { return s, nil }, false, false, false)
}

// scanComment2822 tries to consume an RFC2822 comment including preceding
// white space.
//
// Returns the remaining string after the closing parenthesis.
func scanComment2822(s string) (string, error) {
	const (
		start = iota
		next
		escape
	)

	s = trimStart(s)

	state := start
	depth := 0
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case state == start && c == '(':
			state, depth = next, 1
		case state == next && depth == 1 && c == ')':
			return s[i+1:], nil
		case state == next && c == '\\':
			state = escape
		case state == next && c == '(':
			depth++
		case state == next && c == ')':
			depth--
		case state == next || state == escape:
			state = next
		default:
			return "", errInvalid
		}
	}

	return "", errTooShort
}

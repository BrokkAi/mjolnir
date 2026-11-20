// Port of core 1.99.1 src/char/methods.rs, src/char/convert.rs and
// src/char/mod.rs

package strs

import (
	"strconv"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// A Rust `char` is a Go rune. Every function here expects a Unicode scalar
// value, as a `char` always is; a rune that is not one (a surrogate, a
// negative value, anything above CharMax) has no class and no case mapping.

const (
	// CharMin is char::MIN.
	CharMin rune = 0
	// CharMax is char::MAX.
	CharMax rune = '\U0010FFFF'
	// CharMaxLenUtf8 is char::MAX_LEN_UTF8.
	CharMaxLenUtf8 = 4
	// CharMaxLenUtf16 is char::MAX_LEN_UTF16.
	CharMaxLenUtf16 = 2
	// CharReplacementCharacter is char::REPLACEMENT_CHARACTER.
	CharReplacementCharacter rune = '�'
)

// CharUnicodeVersion is char::UNICODE_VERSION: the version of Unicode the
// classes and case mappings in this package follow.
var CharUnicodeVersion = rs.T3(unicodeVersion[0], unicodeVersion[1], unicodeVersion[2])

// CharFromU32 is char::from_u32: None for a surrogate or a value above
// char::MAX.
func CharFromU32(i uint32) rs.Option[rune] {
	if (i^0xD800)-0x800 >= 0x110000-0x800 {
		return rs.None[rune]()
	}
	return rs.Some(rune(i))
}

// CharFromDigit is char::from_digit.
func CharFromDigit(num uint32, radix uint32) rs.Option[rune] {
	if radix > 36 {
		rs.PanicAt(1, "from_digit: radix is too high (maximum 36)")
	}
	if num < radix {
		if num < 10 {
			return rs.Some(rune('0' + num))
		}
		return rs.Some(rune('a' + num - 10))
	}
	return rs.None[rune]()
}

// CharIsDigit is char::is_digit.
func CharIsDigit(c rune, radix uint32) bool {
	_, ok := charToDigit(c, radix)
	return ok
}

// CharToDigit is char::to_digit.
func CharToDigit(c rune, radix uint32) rs.Option[uint32] {
	return rs.OptionOf(charToDigit(c, radix))
}

func charToDigit(c rune, radix uint32) (uint32, bool) {
	if radix < 2 || radix > 36 {
		rs.PanicAt(2, "to_digit: invalid radix -- radix must be in the range 2 to 36 inclusive")
	}
	var value uint32
	if c > '9' && radix > 10 {
		// mask to convert ASCII letters to uppercase
		const toUppercaseMask uint32 = ^uint32(0b0010_0000)
		// Converts an ASCII letter to its corresponding integer value:
		// A-Z => 10-35, a-z => 10-35. Other characters produce values >= 36.
		value = ((uint32(c) - 'A') & toUppercaseMask) + 10
	} else {
		// convert digit to value, non-digits wrap to values > 36
		value = uint32(c) - '0'
	}
	return value, value < radix
}

// CharEscapeUnicode is char::escape_unicode: `\u{NNNNNN}` with the fewest hex
// digits.
func CharEscapeUnicode(c rune) string {
	return `\u{` + strconv.FormatUint(uint64(uint32(c)), 16) + `}`
}

// escapeDebugExtArgs is EscapeDebugExtArgs.
type escapeDebugExtArgs struct {
	// Escape Extended Grapheme codepoints?
	escapeGraphemeExtender bool
	// Escape single quotes?
	escapeSingleQuote bool
	// Escape double quotes?
	escapeDoubleQuote bool
}

var escapeDebugExtArgsEscapeAll = escapeDebugExtArgs{
	escapeGraphemeExtender: true,
	escapeSingleQuote:      true,
	escapeDoubleQuote:      true,
}

// charEscapeDebugExt is char::escape_debug_ext. It returns the escaped form,
// and whether it is the char itself (EscapeDebug::printable).
func charEscapeDebugExt(c rune, args escapeDebugExtArgs) (escaped string, printable bool) {
	switch {
	// Special escapes
	case c == '"' && args.escapeDoubleQuote:
		return `\"`, false
	case c == '\'' && args.escapeSingleQuote:
		return `\'`, false
	case c == '\\':
		return `\\`, false
	case c == '\n':
		return `\n`, false
	case c == '\t':
		return `\t`, false
	case c == '\r':
		return `\r`, false
	case c == 0:
		return `\0`, false

	// ASCII fast path,
	// plus U+FF9E HALFWIDTH KATAKANA VOICED SOUND MARK
	// and U+FF9F HALFWIDTH KATAKANA SEMI-VOICED SOUND MARK
	// which should not be escaped despite being grapheme extenders.
	case (c >= 0x20 && c <= 0x7E) || c == 0xFF9E || c == 0xFF9F:
		return string(c), true

	case IsControl(c) ||
		charIsPrivateUse(c) ||
		IsWhitespace(c) ||
		args.escapeGraphemeExtender && charIsGraphemeExtender(c) ||
		charIsDefaultIgnorable(c) ||
		charIsFormatControl(c) ||
		!charIsAssigned(c):
		return CharEscapeUnicode(c), false

	default:
		return string(c), true
	}
}

// CharEscapeDebug is char::escape_debug.
func CharEscapeDebug(c rune) string {
	escaped, _ := charEscapeDebugExt(c, escapeDebugExtArgsEscapeAll)
	return escaped
}

// CharEscapeDefault is char::escape_default.
func CharEscapeDefault(c rune) string {
	switch {
	case c == '\t':
		return `\t`
	case c == '\r':
		return `\r`
	case c == '\n':
		return `\n`
	case c == '\\' || c == '\'' || c == '"':
		return `\` + string(c)
	case c >= 0x20 && c <= 0x7e:
		return string(c)
	default:
		return CharEscapeUnicode(c)
	}
}

// CharLenUtf8 is char::len_utf8.
func CharLenUtf8(c rune) int {
	code := uint32(c)
	switch {
	case code < 0x80:
		return 1
	case code < 0x800:
		return 2
	case code < 0x10000:
		return 3
	default:
		return 4
	}
}

// CharLenUtf16 is char::len_utf16.
func CharLenUtf16(c rune) int {
	if uint32(c)&0xFFFF == uint32(c) {
		return 1
	}
	return 2
}

// CharEncodeUtf8 is char::encode_utf8: the char as a string. Rust writes into
// a buffer the caller supplies; here the result is returned.
func CharEncodeUtf8(c rune) string {
	var buffer [utf8.UTFMax]byte
	return string(buffer[:encodeUtf8Raw(c, buffer[:])])
}

// encodeUtf8Raw is encode_utf8_raw_unchecked: it writes the UTF-8 form of c
// to dst and returns its length.
func encodeUtf8Raw(c rune, dst []byte) int {
	code := uint32(c)
	length := CharLenUtf8(c)
	switch length {
	case 1:
		dst[0] = byte(code)
	case 2:
		dst[0] = byte(code>>6&0x3F) | 0b1100_0000
		dst[1] = byte(code&0x3F) | 0b1000_0000
	case 3:
		dst[0] = byte(code>>12&0x3F) | 0b1110_0000
		dst[1] = byte(code>>6&0x3F) | 0b1000_0000
		dst[2] = byte(code&0x3F) | 0b1000_0000
	default:
		dst[0] = byte(code>>18&0x3F) | 0b1111_0000
		dst[1] = byte(code>>12&0x3F) | 0b1000_0000
		dst[2] = byte(code>>6&0x3F) | 0b1000_0000
		dst[3] = byte(code&0x3F) | 0b1000_0000
	}
	return length
}

// CharEncodeUtf16 is char::encode_utf16: one or two UTF-16 code units.
func CharEncodeUtf16(c rune) []uint16 {
	code := uint32(c)
	if CharLenUtf16(c) == 1 {
		return []uint16{uint16(code)}
	}
	code -= 0x1_0000
	return []uint16{uint16(code>>10) | 0xD800, uint16(code&0x3FF) | 0xDC00}
}

// IsAlphabetic is char::is_alphabetic.
func IsAlphabetic(c rune) bool {
	switch {
	case c >= 'a' && c <= 'z', c >= 'A' && c <= 'Z':
		return true
	case c >= 0 && c <= 0xA9:
		return false
	default:
		return unicodeAlphabetic(c)
	}
}

// charIsCased is char::is_cased.
func charIsCased(c rune) bool {
	switch {
	case c >= 'a' && c <= 'z', c >= 'A' && c <= 'Z':
		return true
	case c >= 0 && c <= 0xA9:
		return false
	default:
		return unicodeLowercase(c) || unicodeUppercase(c) || unicodeLt(c)
	}
}

// IsLowercase is char::is_lowercase.
func IsLowercase(c rune) bool {
	switch {
	case c >= 'a' && c <= 'z':
		return true
	case c >= 0 && c <= 0xA9:
		return false
	default:
		return unicodeLowercase(c)
	}
}

// IsUppercase is char::is_uppercase.
func IsUppercase(c rune) bool {
	switch {
	case c >= 'A' && c <= 'Z':
		return true
	case c >= 0 && c <= 0xBF:
		return false
	default:
		return unicodeUppercase(c)
	}
}

// IsNumeric is char::is_numeric.
func IsNumeric(c rune) bool {
	switch {
	case c >= '0' && c <= '9':
		return true
	case c >= 0 && c <= 0xB1:
		return false
	default:
		return unicodeN(c)
	}
}

// IsAlphanumeric is char::is_alphanumeric.
func IsAlphanumeric(c rune) bool {
	switch {
	case c >= 'a' && c <= 'z', c >= 'A' && c <= 'Z', c >= '0' && c <= '9':
		return true
	case c >= 0 && c <= 0xA9:
		return false
	default:
		return unicodeAlphabetic(c) || unicodeN(c)
	}
}

// IsWhitespace is char::is_whitespace.
func IsWhitespace(c rune) bool {
	switch {
	case c == ' ', c >= 0x09 && c <= 0x0d:
		return true
	case c >= 0 && c <= 0x84:
		return false
	default:
		return unicodeWhiteSpace(c)
	}
}

// IsControl is char::is_control: the general category Cc, which never
// changes.
func IsControl(c rune) bool {
	return (c >= 0 && c <= 0x1f) || (c >= 0x7f && c <= 0x9f)
}

// charIsPrivateUse is char::is_private_use: the general category Co, which
// never changes.
func charIsPrivateUse(c rune) bool {
	return (c >= 0xE000 && c <= 0xF8FF) || (c >= 0xF0000 && c <= 0xFFFFD) || (c >= 0x100000 && c <= 0x10FFFD)
}

// charIsFormatControl is char::is_format_control.
func charIsFormatControl(c rune) bool {
	return c > 0xAC && unicodeCf(c)
}

// charIsAssigned is char::is_assigned.
func charIsAssigned(c rune) bool {
	switch {
	case c >= 0 && c <= 0x377:
		return true
	case c >= 0x378 && c <= 0x3FFFD:
		return !unicodeCnPlanes03(c)
	// Assigned character ranges in planes 4 and above.
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

// charIsCaseIgnorable is char::is_case_ignorable.
func charIsCaseIgnorable(c rune) bool {
	if CharIsAscii(c) {
		return c == '\'' || c == '.' || c == ':' || c == '^' || c == '`'
	}
	return unicodeCaseIgnorable(c)
}

// caseMapping is CaseMappingIter::new: the mapped chars without the padding.
func caseMapping(chars [3]rune) []rune {
	switch {
	case chars[2] != 0:
		return chars[:]
	case chars[1] != 0:
		return chars[:2]
	default:
		// Deliberately don't check `chars[0]`,
		// as '\0' lowercases to itself
		return chars[:1]
	}
}

// CharToLowercase is char::to_lowercase: the one to three chars the lowercase
// mapping of c is made of. `c.to_lowercase().next().unwrap()` is its first
// element and `.to_string()` its conversion to a string.
func CharToLowercase(c rune) []rune {
	return caseMapping(conversionsToLower(c))
}

// CharToUppercase is char::to_uppercase: the one to three chars the uppercase
// mapping of c is made of.
func CharToUppercase(c rune) []rune {
	return caseMapping(conversionsToUpper(c))
}

// AsciiChar is the two types Rust gives its ASCII methods to: `char` (a Go
// rune) and `u8` (a Go byte). The ASCII predicates below take either, so
// `b.is_ascii_digit()` on a byte and `c.is_ascii_digit()` on a char are the
// same call.
type AsciiChar interface{ ~int32 | ~uint8 }

// CharIsAscii is char::is_ascii and u8::is_ascii.
func CharIsAscii[C AsciiChar](c C) bool {
	return uint32(c) <= 0x7F
}

// CharToAsciiUppercase is char::to_ascii_uppercase and u8::to_ascii_uppercase.
func CharToAsciiUppercase[C AsciiChar](c C) C {
	if IsAsciiLowercase(c) {
		return c ^ 0b0010_0000
	}
	return c
}

// CharToAsciiLowercase is char::to_ascii_lowercase and u8::to_ascii_lowercase.
func CharToAsciiLowercase[C AsciiChar](c C) C {
	if IsAsciiUppercase(c) {
		return c ^ 0b0010_0000
	}
	return c
}

// CharEqIgnoreAsciiCase is char::eq_ignore_ascii_case and
// u8::eq_ignore_ascii_case.
func CharEqIgnoreAsciiCase[C AsciiChar](a, b C) bool {
	return CharToAsciiLowercase(a) == CharToAsciiLowercase(b)
}

// IsAsciiAlphabetic is char::is_ascii_alphabetic and u8::is_ascii_alphabetic.
func IsAsciiAlphabetic[C AsciiChar](c C) bool {
	u := uint32(c)
	return (u >= 'a' && u <= 'z') || (u >= 'A' && u <= 'Z')
}

// IsAsciiUppercase is char::is_ascii_uppercase and u8::is_ascii_uppercase.
func IsAsciiUppercase[C AsciiChar](c C) bool {
	u := uint32(c)
	return u >= 'A' && u <= 'Z'
}

// IsAsciiLowercase is char::is_ascii_lowercase and u8::is_ascii_lowercase.
func IsAsciiLowercase[C AsciiChar](c C) bool {
	u := uint32(c)
	return u >= 'a' && u <= 'z'
}

// IsAsciiAlphanumeric is char::is_ascii_alphanumeric and
// u8::is_ascii_alphanumeric.
func IsAsciiAlphanumeric[C AsciiChar](c C) bool {
	u := uint32(c)
	return (u >= '0' && u <= '9') || (u >= 'A' && u <= 'Z') || (u >= 'a' && u <= 'z')
}

// IsAsciiDigit is char::is_ascii_digit and u8::is_ascii_digit.
func IsAsciiDigit[C AsciiChar](c C) bool {
	u := uint32(c)
	return u >= '0' && u <= '9'
}

// IsAsciiHexdigit is char::is_ascii_hexdigit and u8::is_ascii_hexdigit.
func IsAsciiHexdigit[C AsciiChar](c C) bool {
	u := uint32(c)
	return (u >= '0' && u <= '9') || (u >= 'A' && u <= 'F') || (u >= 'a' && u <= 'f')
}

// IsAsciiPunctuation is char::is_ascii_punctuation and
// u8::is_ascii_punctuation.
func IsAsciiPunctuation[C AsciiChar](c C) bool {
	u := uint32(c)
	return (u >= '!' && u <= '/') || (u >= ':' && u <= '@') || (u >= '[' && u <= '`') || (u >= '{' && u <= '~')
}

// IsAsciiGraphic is char::is_ascii_graphic and u8::is_ascii_graphic.
func IsAsciiGraphic[C AsciiChar](c C) bool {
	u := uint32(c)
	return u >= '!' && u <= '~'
}

// IsAsciiWhitespace is char::is_ascii_whitespace and u8::is_ascii_whitespace:
// space, tab, line feed, form feed and carriage return, and not the vertical
// tab that char::is_whitespace also accepts.
func IsAsciiWhitespace[C AsciiChar](c C) bool {
	u := uint32(c)
	return u == '\t' || u == '\n' || u == '\x0C' || u == '\r' || u == ' '
}

// IsAsciiControl is char::is_ascii_control and u8::is_ascii_control.
func IsAsciiControl[C AsciiChar](c C) bool {
	u := uint32(c)
	return u <= 0x1F || u == 0x7F
}

// ParseCharError is char::ParseCharError: what parsing a string as a char
// returns when the string does not hold exactly one.
type ParseCharError struct {
	kind charErrorKind
}

type charErrorKind uint8

const (
	charErrorKind_EmptyString charErrorKind = iota
	charErrorKind_TooManyChars
)

var _ = rs.RegisterEnum[charErrorKind]("CharErrorKind",
	rs.Const(charErrorKind_EmptyString, "EmptyString"),
	rs.Const(charErrorKind_TooManyChars, "TooManyChars"),
)

// String is the error's Display text.
func (e *ParseCharError) String() string {
	switch e.kind {
	case charErrorKind_EmptyString:
		return "cannot parse char from empty string"
	default:
		return "too many characters in string"
	}
}

// Error makes a ParseCharError a Go error.
func (e *ParseCharError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseCharError`.
func (e *ParseCharError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived `Debug`: `ParseCharError { kind: EmptyString }`.
func (e *ParseCharError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ParseCharError").Field("kind", e.kind).Finish()
}

func (e *ParseCharError) debugString() string {
	if e.kind == charErrorKind_EmptyString {
		return "ParseCharError { kind: EmptyString }"
	}
	return "ParseCharError { kind: TooManyChars }"
}

// ParseChar is `s.parse::<char>()` and char::from_str: the single char s
// holds.
func ParseChar(s string) (rune, error) {
	if s == "" {
		return 0, &ParseCharError{kind: charErrorKind_EmptyString}
	}
	c, size := utf8.DecodeRuneInString(s)
	if size != len(s) {
		return 0, &ParseCharError{kind: charErrorKind_TooManyChars}
	}
	return c, nil
}

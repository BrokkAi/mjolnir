// Port of core 1.99.1 src/fmt/mod.rs (`impl Debug for str`) and
// src/char/methods.rs (`char::escape_debug_ext`)

package rsio

import (
	"reflect"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

// debugStr is `format!("{:?}", s)` for a string: the text an io::Error's
// Debug form shows for a static message and for a string payload.
//
// It is here, and not a call into the formatter, so that an error can print
// itself without the formatter. The two agree: Rust escapes a character that
// is not printable or that extends a grapheme, and both properties are taken
// from the Unicode tables of the Go toolchain, which can differ from Rust's
// only for characters assigned in a newer Unicode version than the older of
// the two knows.
func debugStr(s string) string {
	var out strings.Builder
	out.Grow(len(s) + 2)
	out.WriteByte('"')
	for i := 0; i < len(s); {
		b := s[i]
		// Runs of printable ASCII are written as they are.
		if b >= 0x20 && b <= 0x7E && b != '\\' && b != '"' {
			out.WriteByte(b)
			i++
			continue
		}
		r, size := utf8.DecodeRuneInString(s[i:])
		out.WriteString(escapeDebugChar(r))
		i += size
	}
	out.WriteByte('"')
	return out.String()
}

// escapeDebugChar is char::escape_debug_ext with the arguments `impl Debug
// for str` passes: grapheme extenders and double quotes are escaped, single
// quotes are not.
func escapeDebugChar(r rune) string {
	switch r {
	case 0:
		return `\0`
	case '\t':
		return `\t`
	case '\r':
		return `\r`
	case '\n':
		return `\n`
	case '\\':
		return `\\`
	case '"':
		return `\"`
	}
	if isGraphemeExtended(r) || !isPrintable(r) {
		return `\u{` + strconv.FormatInt(int64(r), 16) + `}`
	}
	return string(r)
}

// isGraphemeExtended is char::is_grapheme_extended: the Unicode property
// Grapheme_Extend, which is the categories Me and Mn and the property
// Other_Grapheme_Extend.
func isGraphemeExtended(r rune) bool {
	if r < 0x300 {
		return false
	}
	return unicode.In(r, unicode.Mn, unicode.Me, unicode.Other_Grapheme_Extend)
}

// isPrintable is core::unicode::printable::is_printable: everything but the
// categories Cc, Cf, Cs, Co, Cn, Zl, Zp and Zs, with the space printable.
func isPrintable(r rune) bool {
	switch {
	case r < 32:
		return false
	case r < 127:
		return true
	}
	return unicode.IsPrint(r)
}

func typeName(value any) string {
	if value == nil {
		return "nil"
	}
	return reflect.TypeOf(value).String()
}

// Port of core 1.99.1 src/fmt/mod.rs (`impl Debug for str`) and
// src/char/methods.rs (`char::escape_debug_ext`).

package serde

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

// DebugStr is `format!("{:?}", s)` for a str: s in double quotes with Rust's
// escapes. serde prints the string of `Unexpected::Str` this way, and
// serde_json the code of its own errors.
func DebugStr(s string) string {
	var out strings.Builder
	out.Grow(len(s) + 2)
	out.WriteByte('"')
	for index := 0; index < len(s); {
		c := s[index]
		if c >= 0x20 && c <= 0x7E && c != '\\' && c != '"' {
			out.WriteByte(c)
			index++
			continue
		}
		r, size := utf8.DecodeRuneInString(s[index:])
		if r == utf8.RuneError && size == 1 {
			// Not UTF-8, which a Rust str never is. Keep the byte.
			out.WriteByte(c)
			index++
			continue
		}
		writeEscapeDebug(&out, r)
		index += size
	}
	out.WriteByte('"')
	return out.String()
}

// writeEscapeDebug writes r as `char::escape_debug_ext` does with
// escape_grapheme_extender and escape_double_quote set, as the Debug of a str
// asks.
func writeEscapeDebug(out *strings.Builder, r rune) {
	switch r {
	case '"':
		out.WriteString(`\"`)
	case '\\':
		out.WriteString(`\\`)
	case '\n':
		out.WriteString(`\n`)
	case '\t':
		out.WriteString(`\t`)
	case '\r':
		out.WriteString(`\r`)
	case 0:
		out.WriteString(`\0`)
	default:
		if (r >= 0x20 && r <= 0x7E) || r == 0xFF9E || r == 0xFF9F || !escapedInDebug(r) {
			out.WriteRune(r)
			return
		}
		out.WriteString(`\u{`)
		out.WriteString(strconv.FormatInt(int64(r), 16))
		out.WriteByte('}')
	}
}

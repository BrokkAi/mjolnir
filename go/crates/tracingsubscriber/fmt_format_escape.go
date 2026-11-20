// Port of tracing-subscriber 0.3.23 src/fmt/format/escape.rs

package tracingsubscriber

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

// ANSI escape sequence sanitization to prevent terminal injection attacks.

// escapeGuard is EscapeGuard over text already formatted: the text with
// the characters that start terminal escape sequences escaped, when
// sanitize is on, or the text as it is.
func escapeGuard(text string, sanitize bool) string {
	if !sanitize {
		return text
	}
	return escapingWrite(text)
}

// escapingWrite is EscapingWriter::write_str: it streams the string
// character by character, escaping ANSI and C1 control sequences.
//
// A Go string may hold bytes that are not UTF-8, which a Rust string cannot;
// they are copied as they are.
func escapingWrite(s string) string {
	var out strings.Builder
	for len(s) > 0 {
		ch, size := utf8.DecodeRuneInString(s)
		switch {
		case ch == utf8.RuneError && size == 1:
			out.WriteByte(s[0])

		// C0 control characters that can be used in terminal escape sequences
		case ch == '\x1b':
			out.WriteString("\\x1b") // ESC
		case ch == '\x07':
			out.WriteString("\\x07") // BEL
		case ch == '\x08':
			out.WriteString("\\x08") // BS
		case ch == '\x0c':
			out.WriteString("\\x0c") // FF
		case ch == '\x7f':
			out.WriteString("\\x7f") // DEL

		// C1 control characters (\x80-\x9f) - 8-bit control codes
		// These can be used as alternative escape sequences in some terminals
		case ch >= 0x80 && ch <= 0x9f:
			out.WriteString("\\u{" + strconv.FormatUint(uint64(ch), 16) + "}")

		default:
			out.WriteString(s[:size])
		}
		s = s[size:]
	}
	return out.String()
}

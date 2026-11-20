// Port of core 1.99.1 src/char/methods.rs (escape_debug_ext), src/escape.rs
// and the char impls of src/fmt/mod.rs.

package rs

import "unicode/utf8"

// fmtEscapeDebugExtArgs is char::EscapeDebugExtArgs.
type fmtEscapeDebugExtArgs struct {
	// Escape Grapheme Extender codepoints?
	//
	// Note that this excludes
	// U+FF9E HALFWIDTH KATAKANA VOICED SOUND MARK
	// and U+FF9F HALFWIDTH KATAKANA SEMI-VOICED SOUND MARK,
	// which are never escaped, as graphically
	// they are not combining. See <https://github.com/microsoft/terminal/issues/18087>
	// for background on these characters.
	escapeGraphemeExtender bool

	// Escape single quotes?
	escapeSingleQuote bool

	// Escape double quotes?
	escapeDoubleQuote bool
}

// fmtEscapeDebugExt is char::escape_debug_ext. It returns the escape sequence
// for c, or "" when c prints as itself (EscapeDebug::printable, the only
// result whose length is 1).
func fmtEscapeDebugExt(c rune, args fmtEscapeDebugExtArgs) string {
	switch {
	// Special escapes
	case c == '"' && args.escapeDoubleQuote:
		return `\"`
	case c == '\'' && args.escapeSingleQuote:
		return `\'`
	case c == '\\':
		return `\\`
	case c == '\n':
		return `\n`
	case c == '\t':
		return `\t`
	case c == '\r':
		return `\r`
	case c == 0:
		return `\0`

	// ASCII fast path,
	// plus U+FF9E HALFWIDTH KATAKANA VOICED SOUND MARK
	// and U+FF9F HALFWIDTH KATAKANA SEMI-VOICED SOUND MARK
	// which should not be escaped despite being grapheme extenders.
	case (c >= 0x20 && c <= 0x7E) || c == 0xFF9E || c == 0xFF9F:
		return ""

	case fmtCharIsControl(c) ||
		fmtCharIsPrivateUse(c) ||
		fmtCharIsWhitespace(c) ||
		args.escapeGraphemeExtender && fmtCharIsGraphemeExtender(c) ||
		fmtCharIsDefaultIgnorable(c) ||
		fmtCharIsFormatControl(c) ||
		!fmtCharIsAssigned(c):
		return fmtEscapeUnicode(c)

	default:
		return ""
	}
}

// fmtEscapeUnicode is escape::escape_unicode: `\u{NNNN}` with the fewest
// lower-case hexadecimal digits, and at least one.
func fmtEscapeUnicode(c rune) string {
	const hexDigits = "0123456789abcdef"
	var buf [10]byte
	end := len(buf) - 1
	buf[end] = '}'
	value := uint32(c)
	for {
		end--
		buf[end] = hexDigits[value&15]
		value >>= 4
		if value == 0 {
			break
		}
	}
	end--
	buf[end] = '{'
	end--
	buf[end] = 'u'
	end--
	buf[end] = '\\'
	return string(buf[end:])
}

// fmtDebugStr is `impl Debug for str`.
//
// A Go string may hold bytes that are not UTF-8, which a Rust `str` cannot.
// Such a byte is written as `\xNN`, which is what Rust prints for the same
// bytes in a Path or an OsStr on Unix (core::str::lossy::Debug), the other
// Rust types a Go string stands for.
func fmtDebugStr(s string, f *Formatter) error {
	if err := f.buf.writeChar('"'); err != nil {
		return err
	}

	// substring we know is printable
	printableStart, printableEnd := 0, 0

	// the loop here first skips over runs of printable ASCII as a fast path.
	// other chars (unicode, or ASCII that needs escaping) are then handled per-`char`.
	rest := 0
	for rest < len(s) {
		nonPrintableStart := -1
		for i := rest; i < len(s); i++ {
			if b := s[i]; b > 0x7E || b < 0x20 || b == '\\' || b == '"' {
				nonPrintableStart = i - rest
				break
			}
		}
		if nonPrintableStart < 0 {
			printableEnd += len(s) - rest
			break
		}

		printableEnd += nonPrintableStart
		rest += nonPrintableStart

		c, size := utf8.DecodeRuneInString(s[rest:])
		var esc string
		if c == utf8.RuneError && size == 1 {
			// Broken parts of string as hex escape.
			const upperHex = "0123456789ABCDEF"
			b := s[rest]
			esc = string([]byte{'\\', 'x', upperHex[b>>4], upperHex[b&15]})
		} else {
			esc = fmtEscapeDebugExt(c, fmtEscapeDebugExtArgs{
				escapeGraphemeExtender: true,
				escapeSingleQuote:      false,
				escapeDoubleQuote:      true,
			})
		}
		if esc != "" {
			if err := f.buf.writeStr(s[printableStart:printableEnd]); err != nil {
				return err
			}
			if err := f.buf.writeStr(esc); err != nil {
				return err
			}
			printableStart = printableEnd + size
		}
		printableEnd += size
		rest += size
	}

	if err := f.buf.writeStr(s[printableStart:printableEnd]); err != nil {
		return err
	}

	return f.buf.writeChar('"')
}

// fmtDebugChar is `impl Debug for char`.
func fmtDebugChar(c rune, f *Formatter) error {
	if err := f.buf.writeChar('\''); err != nil {
		return err
	}
	esc := fmtEscapeDebugExt(c, fmtEscapeDebugExtArgs{
		escapeGraphemeExtender: true,
		escapeSingleQuote:      true,
		escapeDoubleQuote:      false,
	})
	var err error
	if esc == "" {
		err = f.buf.writeChar(c)
	} else {
		err = f.buf.writeStr(esc)
	}
	if err != nil {
		return err
	}
	return f.buf.writeChar('\'')
}

// fmtDisplayChar is `impl Display for char`.
func fmtDisplayChar(c rune, f *Formatter) error {
	if f.options.flags&(fmtFlagWidth|fmtFlagPrecision) == 0 {
		return f.buf.writeChar(c)
	}
	return f.Pad(string(c))
}

// FmtDisplay prints the character, as `{}` prints a Rust char.
func (c Char) FmtDisplay(f *Formatter) error { return fmtDisplayChar(rune(c), f) }

// FmtDebug prints the character quoted and escaped, as `{:?}` prints a Rust
// char.
func (c Char) FmtDebug(f *Formatter) error { return fmtDebugChar(rune(c), f) }

// String is `char::to_string`.
func (c Char) String() string { return string(rune(c)) }

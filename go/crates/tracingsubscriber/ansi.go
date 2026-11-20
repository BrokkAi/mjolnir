// Port of nu-ansi-term 0.50.3 src/ansi.rs and src/style.rs, the part
// tracing-subscriber 0.3.23 uses: plain, bold, dimmed and italic styles, and
// the foreground colours of the levels.

package tracingsubscriber

import "strings"

// ansiColor is nu_ansi_term::Color for the colours used here: its value is
// the foreground SGR code, 0 for none.
type ansiColor uint8

const (
	ansiRed    ansiColor = 31
	ansiGreen  ansiColor = 32
	ansiYellow ansiColor = 33
	ansiBlue   ansiColor = 34
	ansiPurple ansiColor = 35
)

// ansiStyle is nu_ansi_term::Style.
type ansiStyle struct {
	foreground ansiColor
	isBold     bool
	isDimmed   bool
	isItalic   bool
}

// styleNew is Style::new: a style that changes nothing.
func styleNew() ansiStyle { return ansiStyle{} }

func (s ansiStyle) bold() ansiStyle {
	s.isBold = true
	return s
}

func (s ansiStyle) dimmed() ansiStyle {
	s.isDimmed = true
	return s
}

func (s ansiStyle) italic() ansiStyle {
	s.isItalic = true
	return s
}

// colorPaint is Color::paint: text in the colour.
func colorPaint(color ansiColor, text string) string {
	return ansiStyle{foreground: color}.paint(text)
}

func (s ansiStyle) isPlain() bool { return s == ansiStyle{} }

// prefix is Style::prefix: the escape that turns the style on, nothing for a
// plain style.
func (s ansiStyle) prefix() string {
	// If there are actually no styles here, then don't write *any* codes
	// as the prefix. An empty ANSI code may not affect the terminal
	// output at all, but a user may just want a code-free string.
	if s.isPlain() {
		return ""
	}
	var codes []string
	if s.isBold {
		codes = append(codes, "1")
	}
	if s.isDimmed {
		codes = append(codes, "2")
	}
	if s.isItalic {
		codes = append(codes, "3")
	}
	if s.foreground != 0 {
		codes = append(codes, string('0'+rune(s.foreground/10))+string('0'+rune(s.foreground%10)))
	}
	return "\x1b[" + strings.Join(codes, ";") + "m"
}

// suffix is Style::suffix: the reset, nothing for a plain style.
func (s ansiStyle) suffix() string {
	if s.isPlain() {
		return ""
	}
	return "\x1b[0m"
}

// paint is Style::paint written with `{}`: the text between the prefix and
// the suffix. The formatter's width and fill do not apply to it.
func (s ansiStyle) paint(text string) string {
	return s.prefix() + text + s.suffix()
}

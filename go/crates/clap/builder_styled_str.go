// Port of clap_builder 4.6.0 src/builder/styled_str.rs

package clap

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// BuilderStyledStr is clap::builder::StyledStr: terminal-styling container.
//
// Styling may be encoded as ANSI Escape Code. The value is the text with its
// escape codes: converting a Go string to a BuilderStyledStr is Rust's
// `StyledStr::from(&str)`, String gives the text without styling (Rust's
// Display), and Ansi gives it with styling.
type BuilderStyledStr string

// BuilderStyledStrNew is StyledStr::new: create an empty buffer.
func BuilderStyledStrNew() BuilderStyledStr { return "" }

// BuilderStyledStrFromString is `impl From<String> for StyledStr`.
func BuilderStyledStrFromString(name string) BuilderStyledStr { return BuilderStyledStr(name) }

// Ansi is StyledStr::ansi: display using ANSI Escape Code styling.
func (s BuilderStyledStr) Ansi() string { return string(s) }

func (s *BuilderStyledStr) pushString(msg string) {
	*s += BuilderStyledStr(msg)
}

// PushStr is StyledStr::push_str: appends a given string slice onto the end
// of this StyledStr.
func (s *BuilderStyledStr) PushStr(msg string) {
	*s += BuilderStyledStr(msg)
}

func (s *BuilderStyledStr) trimStartLines() {
	if pos := strings.IndexByte(string(*s), '\n'); pos >= 0 {
		leading, help := string(*s)[:pos+1], string(*s)[pos+1:]
		if strTrim(leading) == "" {
			*s = BuilderStyledStr(help)
		}
	}
}

func (s *BuilderStyledStr) trimEnd() {
	*s = BuilderStyledStr(strTrimEnd(string(*s)))
}

func (s *BuilderStyledStr) replaceNewlineVar() {
	*s = BuilderStyledStr(strings.ReplaceAll(string(*s), "{n}", "\n"))
}

func (s *BuilderStyledStr) indent(initial string, trailing string) {
	text := initial + string(*s)

	lineSep := "\n" + trailing
	*s = BuilderStyledStr(strings.ReplaceAll(text, "\n", lineSep))
}

// wrap is StyledStr::wrap. Without the crate's `wrap_help` feature, which is
// how Mjolnir builds clap, it does nothing: help is never wrapped.
func (s *BuilderStyledStr) wrap(hardWidth int) {
	if !featureWrapHelp {
		return
	}
	text := string(*s)
	var out strings.Builder
	out.Grow(len(text))

	last := 0
	wrapper := textwrapLineWrapperNew(hardWidth)
	for _, content := range stripStrSpans(text) {
		// Preserve styling
		current := content.start
		if last != current {
			out.WriteString(text[last:current])
		}
		last = content.end

		for i, line := range strSplitInclusive(text[content.start:content.end], '\n') {
			if 0 < i {
				// reset char count on newline, skipping the start as we might have carried
				// over from a prior block of styled text
				wrapper.reset()
			}
			words := textwrapFindWordsAsciiSpace(line)
			for _, word := range wrapper.wrap(words) {
				out.WriteString(word)
			}
		}
	}
	if last != len(text) {
		out.WriteString(text[last:])
	}
	*s = BuilderStyledStr(strTrimEnd(out.String()))
}

func (s BuilderStyledStr) displayWidth() int {
	width := 0
	for _, c := range s.iterText() {
		width += textwrapDisplayWidth(c)
	}
	return width
}

func (s BuilderStyledStr) isEmpty() bool { return len(s) == 0 }

func (s BuilderStyledStr) asStyledStr() string { return string(s) }

// iterText is StyledStr::iter_text: the pieces of text between the escape
// codes.
func (s BuilderStyledStr) iterText() []string {
	return stripStr(string(s))
}

func (s *BuilderStyledStr) pushStyled(other BuilderStyledStr) {
	*s += other
}

// String is `impl Display for StyledStr`: color-unaware printing. Never uses
// coloring.
func (s BuilderStyledStr) String() string {
	return strings.Join(s.iterText(), "")
}

// FmtDisplay is `impl Display for StyledStr`.
func (s BuilderStyledStr) FmtDisplay(f *rs.Formatter) error {
	for _, part := range s.iterText() {
		if err := f.Pad(part); err != nil {
			return err
		}
	}
	return nil
}

// strSplitInclusive is str::split_inclusive on one byte: the pieces of s,
// each with the separator that ended it.
func strSplitInclusive(s string, sep byte) []string {
	var parts []string
	for len(s) > 0 {
		index := strings.IndexByte(s, sep)
		if index < 0 {
			parts = append(parts, s)
			break
		}
		parts = append(parts, s[:index+1])
		s = s[index+1:]
	}
	return parts
}

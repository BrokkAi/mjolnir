// Port of winnow 0.5.40 src/error.rs, src/stream/mod.rs, src/token/mod.rs and
// src/combinator/*.rs: the parts toml_edit's parser is built from.
//
// toml_edit's parser is a set of winnow combinators, and what a user sees of
// a syntax error (where it points, and which "invalid ..." and "expected ..."
// lines it carries) falls out of how those combinators treat a failure: which
// of them rewind the input, which turn a recoverable failure into a final
// one, and which add context on the way out. The parser files therefore
// follow the Rust parsers combinator by combinator, and this file holds the
// rules they rely on:
//
//   - A failure is either a backtrack (ErrMode::Backtrack: another
//     alternative may still match) or a cut (ErrMode::Cut: the parse fails).
//   - `opt`, `repeat`, `separated` and every `alt` branch but the last
//     rewind the input when the parser they run backtracks. The last branch
//     of an `alt`, a sequence, and `cut_err` do not: the input stays where
//     the failing parser left it, and that position is where the error is
//     reported.
//   - `try_map`, `verify` and `and_then` rewind to where their parser
//     started when the mapping function rejects the output.
//   - `context` adds its label or expected value to any failure passing
//     through it, backtrack or cut, innermost first.
//   - `ContextError::or` keeps the later error, so an `alt` reports its last
//     branch.

package tomledit

import (
	"strings"
)

// input is winnow's `Located<&BStr>`: the document and how far the parser
// has got.
type input struct {
	data string
	pos  int
}

func newInput(s string) *input {
	return &input{data: s}
}

// eofOffset is Stream::eof_offset: how many bytes are left.
func (in *input) eofOffset() int { return len(in.data) - in.pos }

// peek returns the next byte without taking it; ok is false at the end.
func (in *input) peek() (byte, bool) {
	if in.pos < len(in.data) {
		return in.data[in.pos], true
	}
	return 0, false
}

// oneOf is winnow's `one_of`: it takes the next byte when the set contains
// it. A mismatch and the end of input both leave the input where it was.
func (in *input) oneOf(set func(byte) bool) bool {
	if in.pos < len(in.data) && set(in.data[in.pos]) {
		in.pos++
		return true
	}
	return false
}

// eat is `one_of` for a single byte, which is what a `u8` is as a parser.
func (in *input) eat(b byte) bool {
	if in.pos < len(in.data) && in.data[in.pos] == b {
		in.pos++
		return true
	}
	return false
}

// tag is winnow's `tag` (`literal`), which is what a byte string is as a
// parser: it takes the bytes when the input starts with them.
func (in *input) tag(literal string) bool {
	if strings.HasPrefix(in.data[in.pos:], literal) {
		in.pos += len(literal)
		return true
	}
	return false
}

// takeWhile0 is `take_while(0.., set)`.
func (in *input) takeWhile0(set func(byte) bool) string {
	start := in.pos
	for in.pos < len(in.data) && set(in.data[in.pos]) {
		in.pos++
	}
	return in.data[start:in.pos]
}

// takeWhile1 is `take_while(1.., set)`: it fails, taking nothing, when the
// first byte is not in the set.
func (in *input) takeWhile1(set func(byte) bool) (string, bool) {
	start := in.pos
	for in.pos < len(in.data) && set(in.data[in.pos]) {
		in.pos++
	}
	return in.data[start:in.pos], in.pos > start
}

// takeWhileMN is `take_while(m..=n, set)` (winnow's take_till_m_n): at least
// m and at most n bytes of the set. It fails, taking nothing, when fewer than
// m are there.
func (in *input) takeWhileMN(m, n int, set func(byte) bool) (string, bool) {
	count := 0
	for in.pos+count < len(in.data) && count < n && set(in.data[in.pos+count]) {
		count++
	}
	if count < m {
		return "", false
	}
	text := in.data[in.pos : in.pos+count]
	in.pos += count
	return text, true
}

// errMode is which variant of winnow's ErrMode a failure is.
type errMode uint8

const (
	// errNone is success.
	errNone errMode = iota
	// errBacktrack is ErrMode::Backtrack: a recoverable error.
	errBacktrack
	// errCut is ErrMode::Cut: an unrecoverable error.
	errCut
)

type strContextKind uint8

const (
	// contextLabel is StrContext::Label: description of what is currently
	// being parsed.
	contextLabel strContextKind = iota
	// contextExpectedChar is StrContext::Expected(StrContextValue::CharLiteral).
	contextExpectedChar
	// contextExpectedString is StrContext::Expected(StrContextValue::StringLiteral).
	contextExpectedString
	// contextExpectedDescription is StrContext::Expected(StrContextValue::Description).
	contextExpectedDescription
)

// strContext is winnow's StrContext: additional error context.
type strContext struct {
	kind strContextKind
	text string
	char rune
}

func label(name string) strContext { return strContext{kind: contextLabel, text: name} }

func expectedChar(c rune) strContext { return strContext{kind: contextExpectedChar, char: c} }

func expectedString(s string) strContext { return strContext{kind: contextExpectedString, text: s} }

func expectedDescription(s string) strContext {
	return strContext{kind: contextExpectedDescription, text: s}
}

// parseErr is winnow's `ErrMode<ContextError>`, with errNone for a parser
// that succeeded: the result of every parser besides its output.
type parseErr struct {
	mode    errMode
	context []strContext
	cause   error
}

func (e parseErr) failed() bool { return e.mode != errNone }

// backtrack is `ErrMode::from_error_kind`: a recoverable error with nothing
// attached, which is what every token parser fails with.
func backtrack() parseErr { return parseErr{mode: errBacktrack} }

// cutEmpty is `cut_err` over a token parser that failed.
func cutEmpty() parseErr { return parseErr{mode: errCut} }

// external is `ErrMode::from_external_error`: a recoverable error caused by
// the error a mapping function returned.
func external(cause error) parseErr { return parseErr{mode: errBacktrack, cause: cause} }

// cut is ErrMode::cut, which `cut_err` applies: a backtrack becomes a cut.
func (e parseErr) cut() parseErr {
	if e.mode == errBacktrack {
		e.mode = errCut
	}
	return e
}

// add is `Parser::context`: the context is added to a failure of either
// mode.
func (e parseErr) add(context ...strContext) parseErr {
	if e.mode != errNone {
		e.context = append(e.context, context...)
	}
	return e
}

// String is `impl Display for StrContextValue` for an expected value.
func (c strContext) String() string {
	switch c.kind {
	case contextExpectedChar:
		switch {
		case c.char == '\n':
			return "newline"
		case c.char == '`':
			return "'`'"
		case c.char < 0x20 || c.char == 0x7f:
			// c.is_ascii_control() => write!(f, "`{}`", c.escape_debug())
			return "`" + escapeDebugControl(c.char) + "`"
		default:
			return "`" + string(c.char) + "`"
		}
	case contextExpectedString:
		return "`" + c.text + "`"
	default:
		return c.text
	}
}

// escapeDebugControl is char::escape_debug for an ASCII control character.
func escapeDebugControl(c rune) string {
	switch c {
	case '\t':
		return "\\t"
	case '\r':
		return "\\r"
	case '\n':
		return "\\n"
	case 0:
		return "\\0"
	default:
		const hex = "0123456789abcdef"
		if c < 0x10 {
			return "\\u{" + string(hex[c]) + "}"
		}
		return "\\u{" + string(hex[c>>4]) + string(hex[c&0xf]) + "}"
	}
}

// message is `impl Display for ContextError<StrContext>`.
func (e parseErr) message() string {
	var out strings.Builder
	newline := false

	for _, c := range e.context {
		if c.kind == contextLabel {
			newline = true
			out.WriteString("invalid ")
			out.WriteString(c.text)
			break
		}
	}

	first := true
	for _, c := range e.context {
		if c.kind == contextLabel {
			continue
		}
		if first {
			if newline {
				out.WriteString("\n")
			}
			newline = true
			out.WriteString("expected ")
			first = false
		} else {
			out.WriteString(", ")
		}
		out.WriteString(c.String())
	}

	if e.cause != nil {
		if newline {
			out.WriteString("\n")
		}
		out.WriteString(e.cause.Error())
	}
	return out.String()
}

// Port of rustc 1.99.1 compiler/rustc_parse_format/src/lib.rs (the grammar of
// format strings) and of the parts of
// compiler/rustc_builtin_macros/src/format.rs and
// compiler/rustc_ast_lowering/src/format.rs that decide which argument each
// placeholder reads and which flags it carries.
//
// In Rust this work happens while compiling a `format!` call, and a mistake
// is a compile error. Here it happens the first time a format string is
// used, and a mistake panics.

package rs

import (
	"strconv"
	"strings"
	"sync"
	"unicode"
)

// fmtTrait is the formatting trait a placeholder asks for: the type at the
// end of its spec.
type fmtTrait uint8

const (
	// `{}`
	fmtTraitDisplay fmtTrait = iota
	// `{:?}`
	fmtTraitDebug
	// `{:e}`
	fmtTraitLowerExp
	// `{:E}`
	fmtTraitUpperExp
	// `{:o}`
	fmtTraitOctal
	// `{:p}`
	fmtTraitPointer
	// `{:b}`
	fmtTraitBinary
	// `{:x}`
	fmtTraitLowerHex
	// `{:X}`
	fmtTraitUpperHex
)

// fmtCountKind is rustc_parse_format::Count: where a width or a precision
// comes from.
type fmtCountKind uint8

const (
	// The count is implied and cannot be explicitly specified.
	fmtCountImplied fmtCountKind = iota
	// The count is specified explicitly.
	fmtCountIs
	// The count is specified by the argument at the given index.
	fmtCountIsParam
	// The count is specified by the argument with the given name.
	fmtCountIsName
	// The count is specified by a star (like in `{:.*}`) that refers to the argument at the given index.
	fmtCountIsStar
)

type fmtCount struct {
	kind fmtCountKind
	// value is the count itself (fmtCountIs) or an argument index.
	value int
	name  string
}

// fmtPositionKind is rustc_parse_format::Position: which argument a
// placeholder formats.
type fmtPositionKind uint8

const (
	// The argument is implied to be located at an index
	fmtArgumentImplicitlyIs fmtPositionKind = iota
	// The argument is located at a specific index given in the format,
	fmtArgumentIs
	// The argument has a name.
	fmtArgumentNamed
)

// fmtPlaceholder is one `{...}` of a format string.
type fmtPlaceholder struct {
	position fmtPositionKind
	index    int
	name     string

	// flags is FormattingOptions.flags as rustc_ast_lowering computes it:
	// the fill character, the alignment and the flags, with the width and
	// precision bits set when the spec has them.
	flags     uint32
	width     fmtCount
	precision fmtCount

	trait fmtTrait
}

// fmtPiece is a stretch of literal text or a placeholder.
type fmtPiece struct {
	literal     string
	placeholder *fmtPlaceholder
}

// fmtTemplate is a parsed format string: what fmt::Arguments holds apart from
// the argument values.
type fmtTemplate struct {
	source string
	pieces []fmtPiece
	// err is why the string is not a format string.
	err string
	// literalLen is the combined length of the literal pieces and
	// startsWithPlaceholder whether a placeholder comes before any text;
	// both feed Arguments::estimated_capacity.
	literalLen            int
	startsWithPlaceholder bool
}

// asStr is Arguments::as_str: the text, when there is nothing to fill in.
func (t *fmtTemplate) asStr() (string, bool) {
	switch len(t.pieces) {
	case 0:
		return "", true
	case 1:
		if t.pieces[0].placeholder == nil {
			return t.pieces[0].literal, true
		}
	}
	return "", false
}

// estimatedCapacity is Arguments::estimated_capacity.
func (t *fmtTemplate) estimatedCapacity() int {
	if s, ok := t.asStr(); ok {
		return len(s)
	}
	if t.startsWithPlaceholder && t.literalLen < 16 {
		// If the format string starts with a placeholder,
		// don't preallocate anything, unless length
		// of literal pieces is significant.
		return 0
	}
	// There are some placeholders, so any additional push
	// will reallocate the string. To avoid that,
	// we're "pre-doubling" the capacity here.
	return t.literalLen * 2
}

// Parsed format strings, by their text. Rust takes a format string only as a
// literal, so a ported program has a fixed set of them and the cache holds
// one entry for each.
var fmtTemplateCache sync.Map // string -> *fmtTemplate

// fmtParse returns the parsed form of a format string. It panics when the
// string is malformed.
func fmtParse(format string) *fmtTemplate {
	if cached, ok := fmtTemplateCache.Load(format); ok {
		return fmtTemplateChecked(cached.(*fmtTemplate))
	}
	parser := fmtParser{input: []rune(format)}
	template := parser.parse()
	template.source = strings.Clone(format)
	cached, _ := fmtTemplateCache.LoadOrStore(template.source, template)
	return fmtTemplateChecked(cached.(*fmtTemplate))
}

func fmtTemplateChecked(template *fmtTemplate) *fmtTemplate {
	if template.err != "" {
		panic(fmtPanicInfo("invalid format string: " + template.err + ": " + strconv.Quote(template.source)))
	}
	return template
}

// fmtParser is rustc_parse_format::Parser.
type fmtParser struct {
	input []rune
	pos   int
	// The next implicit position: `{}` and `.*` take arguments in turn.
	curarg int
	err    string
}

func (p *fmtParser) fail(description string) {
	if p.err == "" {
		p.err = description
	}
}

func (p *fmtParser) peek() (rune, bool) {
	if p.pos < len(p.input) {
		return p.input[p.pos], true
	}
	return 0, false
}

func (p *fmtParser) peekAhead() (rune, bool) {
	if p.pos+1 < len(p.input) {
		return p.input[p.pos+1], true
	}
	return 0, false
}

// consume optionally consumes the specified character. If the character is
// not at the current position, then the current iterator isn't moved and
// `false` is returned, otherwise the character is consumed and `true` is
// returned.
func (p *fmtParser) consume(c rune) bool {
	if next, ok := p.peek(); ok && next == c {
		p.pos++
		return true
	}
	return false
}

// ws consumes all whitespace characters until the first non-whitespace
// character.
func (p *fmtParser) ws() {
	for p.pos < len(p.input) && fmtCharIsWhitespace(p.input[p.pos]) {
		p.pos++
	}
}

// parse is the Parser's Iterator::next, run to the end of the string.
func (p *fmtParser) parse() *fmtTemplate {
	template := &fmtTemplate{}
	var literal []rune
	flush := func() {
		if len(literal) > 0 {
			text := string(literal)
			template.pieces = append(template.pieces, fmtPiece{literal: text})
			template.literalLen += len(text)
			literal = literal[:0]
		}
	}
	for p.err == "" {
		c, ok := p.peek()
		if !ok {
			break
		}
		switch c {
		case '{':
			p.pos++
			if p.consume('{') {
				// double open brace escape: "{{"
				literal = append(literal, '{')
				continue
			}
			// single open brace
			flush()
			placeholder := p.argument()
			p.ws()
			if !p.consume('}') {
				p.missingClosingBrace()
			}
			if len(template.pieces) == 0 {
				template.startsWithPlaceholder = true
			}
			template.pieces = append(template.pieces, fmtPiece{placeholder: placeholder})
		case '}':
			p.pos++
			if p.consume('}') {
				// double close brace escape: "}}"
				literal = append(literal, '}')
			} else {
				// error: single close brace without corresponding open brace
				p.fail("unmatched `}` found (if you intended to print `}`, you can escape it using `}}`)")
			}
		default:
			literal = append(literal, c)
			p.pos++
		}
	}
	flush()
	if p.err != "" {
		return &fmtTemplate{err: p.err}
	}
	return template
}

func (p *fmtParser) missingClosingBrace() {
	if c, ok := p.peek(); ok {
		p.fail("expected `}`, found `" + fmtEscapeDebugChar(c) + "`")
	} else {
		p.fail("expected `}` but string was terminated (if you intended to print `{`, you can escape it using `{{`)")
	}
}

// fmtEscapeDebugChar is char::escape_debug as a string.
func fmtEscapeDebugChar(c rune) string {
	if esc := fmtEscapeDebugExt(c, fmtEscapeDebugExtArgs{escapeGraphemeExtender: true, escapeSingleQuote: true, escapeDoubleQuote: true}); esc != "" {
		return esc
	}
	return string(c)
}

// argument parses an `Argument` structure, or what's contained within braces
// inside the format string.
func (p *fmtParser) argument() *fmtPlaceholder {
	placeholder := &fmtPlaceholder{}

	hasPosition := p.position(placeholder)
	p.ws()

	p.format(placeholder)

	// Resolve position after parsing format spec.
	if !hasPosition {
		placeholder.position = fmtArgumentImplicitlyIs
		placeholder.index = p.curarg
		p.curarg++
	}

	return placeholder
}

// position parses a positional argument for a format. This could either be an
// integer index of an argument, a named argument, or a blank string.
// It returns true if the position is not implicitly consuming a macro
// argument, false if it's the case.
func (p *fmtParser) position(placeholder *fmtPlaceholder) bool {
	if i, ok := p.integer(); ok {
		placeholder.position = fmtArgumentIs
		placeholder.index = i
		return true
	}
	if c, ok := p.peek(); ok && fmtIsIdStart(c) {
		word := p.word()

		// Recover from `r#ident` in format strings.
		if word == "r" {
			if next, ok := p.peek(); ok && next == '#' {
				if ahead, ok := p.peekAhead(); ok && fmtIsIdStart(ahead) {
					p.fail("raw identifiers are not supported")
				}
			}
		}

		placeholder.position = fmtArgumentNamed
		placeholder.name = word
		return true
	}
	// This is an `ArgumentNext`.
	// Record the fact and do the resolution after parsing the
	// format spec, to make things like `{:.*}` work.
	return false
}

// format parses a format specifier at the current position, storing all of
// the relevant information in the placeholder.
func (p *fmtParser) format(placeholder *fmtPlaceholder) {
	fill := ' '
	align := fmtFlagAlignUnknown
	var flags uint32
	finish := func() {
		placeholder.flags = uint32(fill) | align | flags
	}

	if !p.consume(':') {
		finish()
		return
	}

	// fill character
	if c, ok := p.peek(); ok {
		if ahead, ok := p.peekAhead(); ok && (ahead == '>' || ahead == '<' || ahead == '^') {
			p.pos++
			fill = c
		}
	}
	// Alignment
	if p.consume('<') {
		align = fmtFlagAlignLeft
	} else if p.consume('>') {
		align = fmtFlagAlignRight
	} else if p.consume('^') {
		align = fmtFlagAlignCenter
	}
	// Sign flags
	if p.consume('+') {
		flags |= fmtFlagSignPlus
	} else if p.consume('-') {
		flags |= fmtFlagSignMinus
	}
	// Alternate marker
	if p.consume('#') {
		flags |= fmtFlagAlternate
	}
	// Width and precision
	havewidth := false

	if p.consume('0') {
		// small ambiguity with '0$' as a format string. In theory this is a
		// '0' flag and then an ill-formatted format string with just a '$'
		// and no count, but this is better if we instead interpret this as
		// no '0' flag and '0$' as the width instead.
		if p.consume('$') {
			placeholder.width = fmtCount{kind: fmtCountIsParam, value: 0}
			havewidth = true
		} else {
			flags |= fmtFlagSignAwareZeroPad
		}
	}

	if !havewidth {
		placeholder.width = p.count()
	}

	if p.consume('.') {
		if p.consume('*') {
			// Resolve `*` by retrieving the next positional argument, which is the precision
			i := p.curarg
			p.curarg++
			placeholder.precision = fmtCount{kind: fmtCountIsStar, value: i}
		} else {
			placeholder.precision = p.count()
		}
	}

	// Optional radix followed by the actual format specifier
	var ty string
	if p.consume('x') {
		if p.consume('?') {
			flags |= fmtFlagDebugLowerHex
			ty = "?"
		} else {
			ty = "x"
		}
	} else if p.consume('X') {
		if p.consume('?') {
			flags |= fmtFlagDebugUpperHex
			ty = "?"
		} else {
			ty = "X"
		}
	} else if p.consume('?') {
		ty = "?"
		if c, ok := p.peek(); ok && (c == '#' || c == 'x' || c == 'X') {
			p.fail("expected `}`, found `" + string(c) + "` (write `" + string(c) + "?`)")
		}
	} else {
		ty = p.word()
	}

	if placeholder.width.kind != fmtCountImplied {
		flags |= fmtFlagWidth
	}
	if placeholder.precision.kind != fmtCountImplied {
		flags |= fmtFlagPrecision
	}
	finish()

	switch ty {
	case "":
		placeholder.trait = fmtTraitDisplay
	case "?":
		placeholder.trait = fmtTraitDebug
	case "e":
		placeholder.trait = fmtTraitLowerExp
	case "E":
		placeholder.trait = fmtTraitUpperExp
	case "o":
		placeholder.trait = fmtTraitOctal
	case "p":
		placeholder.trait = fmtTraitPointer
	case "b":
		placeholder.trait = fmtTraitBinary
	case "x":
		placeholder.trait = fmtTraitLowerHex
	case "X":
		placeholder.trait = fmtTraitUpperHex
	default:
		// Only report the type when the spec was otherwise read to its end;
		// a stray character is the better description of `{:10q5}`.
		if c, ok := p.peek(); !ok || c == '}' || fmtCharIsWhitespace(c) {
			p.fail("unknown format trait `" + ty + "`")
		}
	}
}

// count parses a `Count` parameter at the current position. This does not
// check for 'CountIsNextParam' because that is only used in precision, not
// width.
func (p *fmtParser) count() fmtCount {
	if i, ok := p.integer(); ok {
		if p.consume('$') {
			return fmtCount{kind: fmtCountIsParam, value: i}
		}
		return fmtCount{kind: fmtCountIs, value: i}
	}
	tmp := p.pos
	savedErr := p.err
	word := p.word()
	if word == "" {
		p.pos = tmp
		return fmtCount{kind: fmtCountImplied}
	} else if p.consume('$') {
		return fmtCount{kind: fmtCountIsName, name: word}
	}
	// Not a count after all: the word is the type, and is read again.
	p.pos = tmp
	p.err = savedErr
	return fmtCount{kind: fmtCountImplied}
}

// word parses a word starting at the current position. A word is the same as
// a Rust identifier, except that it can't start with `_` character.
func (p *fmtParser) word() string {
	start := p.pos
	if c, ok := p.peek(); ok && fmtIsIdStart(c) {
		p.pos++
	} else {
		return ""
	}
	for {
		if c, ok := p.peek(); ok && fmtIsIdContinue(c) {
			p.pos++
		} else {
			break
		}
	}

	word := string(p.input[start:p.pos])
	if word == "_" {
		p.fail("invalid argument name `_` (argument name cannot be a single underscore)")
	}
	return word
}

// integer parses an integer at the current position, as far as it goes.
func (p *fmtParser) integer() (int, bool) {
	cur := 0
	found := false
	overflow := false
	start := p.pos
	for {
		c, ok := p.peek()
		if !ok || c < '0' || c > '9' {
			break
		}
		p.pos++
		cur = cur*10 + int(c-'0')
		if cur > 0xFFFF {
			overflow = true
			cur = 0xFFFF
		}
		found = true
	}

	if overflow {
		overflowedInt := string(p.input[start:p.pos])
		p.fail("integer `" + overflowedInt + "` does not fit into the type `u16` whose range is `0..=65535`")
	}

	return cur, found
}

// fmtIsIdStart is rustc_lexer::is_id_start: `_` or a character with the
// XID_Start property. Outside ASCII the property is approximated with Go's
// own Unicode tables (letters and letter numbers), which is as close as the
// names in a format string need.
func fmtIsIdStart(c rune) bool {
	switch {
	case c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'):
		return true
	case c < 0x80:
		return false
	default:
		return unicode.IsLetter(c) || unicode.Is(unicode.Nl, c)
	}
}

// fmtIsIdContinue is rustc_lexer::is_id_continue: a character with the
// XID_Continue property, approximated outside ASCII as fmtIsIdStart is.
func fmtIsIdContinue(c rune) bool {
	switch {
	case c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9'):
		return true
	case c < 0x80:
		return false
	default:
		return unicode.IsLetter(c) || unicode.In(c, unicode.Nl, unicode.Mn, unicode.Mc, unicode.Nd, unicode.Pc)
	}
}

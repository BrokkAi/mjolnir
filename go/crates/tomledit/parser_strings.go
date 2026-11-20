// Port of toml_edit 0.20.2 src/parser/strings.rs

package tomledit

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

// ;; String

// string = ml-basic-string / basic-string / ml-literal-string / literal-string
func parseString(in *input) (string, parseErr) {
	// alt((
	//     ml_basic_string,
	//     basic_string,
	//     ml_literal_string,
	//     literal_string.map(Cow::Borrowed),
	// ))
	start := in.pos
	s, e := mlBasicString(in)
	if e.mode != errBacktrack {
		return s, e
	}
	in.pos = start
	s, e = basicString(in)
	if e.mode != errBacktrack {
		return s, e
	}
	in.pos = start
	s, e = mlLiteralString(in)
	if e.mode != errBacktrack {
		return s, e
	}
	in.pos = start
	return literalString(in)
}

// ;; Basic String

// basic-string = quotation-mark *basic-char quotation-mark
func basicString(in *input) (string, parseErr) {
	if !in.eat(quotationMark) {
		return "", backtrack()
	}

	var c stringBuilder
	for {
		// opt(basic_chars)
		start := in.pos
		ci, e := basicChars(in)
		if e.mode == errBacktrack {
			in.pos = start
			break
		}
		if e.mode == errCut {
			return "", e
		}
		c.push(ci)
	}

	if !in.eat(quotationMark) {
		return "", cutEmpty().add(label("basic string"))
	}

	return c.finish(), parseErr{}
}

// stringBuilder is the `Cow<str>` the string parsers build: the first piece
// is borrowed, and only a second one allocates.
type stringBuilder struct {
	first   string
	builder *strings.Builder
}

func (b *stringBuilder) push(piece string) {
	switch {
	case b.builder != nil:
		b.builder.WriteString(piece)
	case b.first == "":
		b.first = piece
	default:
		b.builder = &strings.Builder{}
		b.builder.WriteString(b.first)
		b.builder.WriteString(piece)
	}
}

func (b *stringBuilder) finish() string {
	if b.builder != nil {
		return b.builder.String()
	}
	return b.first
}

// quotation-mark = %x22            ; "
const quotationMark = '"'

// basic-char = basic-unescaped / escaped
func basicChars(in *input) (string, parseErr) {
	// alt((
	//     // Deviate from the official grammar by batching the unescaped chars so we build a string a
	//     // chunk at a time, rather than a `char` at a time.
	//     take_while(1.., BASIC_UNESCAPED)
	//         .try_map(std::str::from_utf8)
	//         .map(Cow::Borrowed),
	//     escaped.map(|c| Cow::Owned(String::from(c))),
	// ))
	if chunk, ok := in.takeWhile1(isBasicUnescaped); ok {
		return chunk, parseErr{}
	}
	c, e := escaped(in)
	if e.failed() {
		return "", e
	}
	return string(c), parseErr{}
}

// basic-unescaped = wschar / %x21 / %x23-5B / %x5D-7E / non-ascii
func isBasicUnescaped(b byte) bool {
	return isWschar(b) || b == 0x21 || (b >= 0x23 && b <= 0x5B) || (b >= 0x5D && b <= 0x7E) || isNonASCII(b)
}

// escaped = escape escape-seq-char
func escaped(in *input) (rune, parseErr) {
	// preceded(ESCAPE, escape_seq_char)
	if !in.eat(escape) {
		return 0, backtrack()
	}
	return escapeSeqChar(in)
}

// escape = %x5C                    ; \
const escape = '\\'

// escape-seq-char =  %x22         ; "    quotation mark  U+0022
// escape-seq-char =/ %x5C         ; \    reverse solidus U+005C
// escape-seq-char =/ %x62         ; b    backspace       U+0008
// escape-seq-char =/ %x66         ; f    form feed       U+000C
// escape-seq-char =/ %x6E         ; n    line feed       U+000A
// escape-seq-char =/ %x72         ; r    carriage return U+000D
// escape-seq-char =/ %x74         ; t    tab             U+0009
// escape-seq-char =/ %x75 4HEXDIG ; uXXXX                U+XXXX
// escape-seq-char =/ %x55 8HEXDIG ; UXXXXXXXX            U+XXXXXXXX
func escapeSeqChar(in *input) (rune, parseErr) {
	// dispatch! {any; ...}
	b, ok := in.peek()
	if !ok {
		return 0, backtrack()
	}
	in.pos++
	switch b {
	case 'b':
		return 0x8, parseErr{}
	case 'f':
		return 0xc, parseErr{}
	case 'n':
		return '\n', parseErr{}
	case 'r':
		return '\r', parseErr{}
	case 't':
		return '\t', parseErr{}
	case 'u':
		c, e := hexescape(in, 4)
		if e.failed() {
			return 0, e.cut().add(label("unicode 4-digit hex code"))
		}
		return c, parseErr{}
	case 'U':
		c, e := hexescape(in, 8)
		if e.failed() {
			return 0, e.cut().add(label("unicode 8-digit hex code"))
		}
		return c, parseErr{}
	case '\\':
		return '\\', parseErr{}
	case '"':
		return '"', parseErr{}
	default:
		return 0, cutEmpty().add(
			label("escape sequence"),
			expectedChar('b'),
			expectedChar('f'),
			expectedChar('n'),
			expectedChar('r'),
			expectedChar('t'),
			expectedChar('u'),
			expectedChar('U'),
			expectedChar('\\'),
			expectedChar('"'),
		)
	}
}

func hexescape(in *input, n int) (rune, parseErr) {
	// take_while(0..=N, HEXDIG)
	//     .verify(|b: &[u8]| b.len() == N)
	//     .map(..)
	//     .verify_map(|s| u32::from_str_radix(s, 16).ok())
	//     .try_map(|h| char::from_u32(h).ok_or(CustomError::OutOfRange))
	start := in.pos
	digits, _ := in.takeWhileMN(0, n, isHexdig)
	if len(digits) != n {
		in.pos = start
		return 0, backtrack()
	}
	h, err := strconv.ParseUint(digits, 16, 32)
	if err != nil {
		in.pos = start
		return 0, backtrack()
	}
	// char::from_u32
	if h > utf8.MaxRune || (h >= 0xD800 && h <= 0xDFFF) {
		in.pos = start
		return 0, external(&customError_OutOfRange{})
	}
	return rune(h), parseErr{}
}

// ;; Multiline Basic String

// ml-basic-string = ml-basic-string-delim [ newline ] ml-basic-body
//
//	ml-basic-string-delim
func mlBasicString(in *input) (string, parseErr) {
	// delimited(
	//     ML_BASIC_STRING_DELIM,
	//     preceded(opt(newline), cut_err(ml_basic_body)),
	//     cut_err(ML_BASIC_STRING_DELIM),
	// )
	// .context(StrContext::Label("multiline basic string"))
	s, e := func() (string, parseErr) {
		if !in.tag(mlBasicStringDelim) {
			return "", backtrack()
		}
		beforeNewline := in.pos
		if newline(in).failed() {
			in.pos = beforeNewline
		}
		body, e := mlBasicBody(in)
		if e.failed() {
			return "", e.cut()
		}
		if !in.tag(mlBasicStringDelim) {
			return "", cutEmpty()
		}
		return body, parseErr{}
	}()
	if e.failed() {
		return "", e.add(label("multiline basic string"))
	}
	return s, parseErr{}
}

// ml-basic-string-delim = 3quotation-mark
const mlBasicStringDelim = "\"\"\""

// ml-basic-body = *mlb-content *( mlb-quotes 1*mlb-content ) [ mlb-quotes ]
func mlBasicBody(in *input) (string, parseErr) {
	var c stringBuilder
	// optMlbContent is `opt(mlb_content)`.
	optMlbContent := func() (string, bool, parseErr) {
		start := in.pos
		ci, e := mlbContent(in)
		switch e.mode {
		case errBacktrack:
			in.pos = start
			return "", false, parseErr{}
		case errCut:
			return "", false, e
		}
		return ci, true, parseErr{}
	}
	for {
		ci, ok, e := optMlbContent()
		if e.failed() {
			return "", e
		}
		if !ok {
			break
		}
		c.push(ci)
	}

	for {
		// opt(mlb_quotes(none_of(b'\"').value(())))
		start := in.pos
		qi, e := mlbQuotes(in, func(in *input) bool {
			// none_of(b'\"')
			b, ok := in.peek()
			return ok && b != '"'
		})
		if e.mode == errBacktrack {
			in.pos = start
			break
		}
		if e.mode == errCut {
			return "", e
		}
		ci, ok, e := optMlbContent()
		if e.failed() {
			return "", e
		}
		if !ok {
			// The quotes stay consumed: the Rust loop breaks without
			// rewinding, and the closing delimiter is looked for after them.
			break
		}
		c.push(qi)
		c.push(ci)
		for {
			ci, ok, e := optMlbContent()
			if e.failed() {
				return "", e
			}
			if !ok {
				break
			}
			c.push(ci)
		}
	}

	// opt(mlb_quotes(tag(ML_BASIC_STRING_DELIM).value(())))
	start := in.pos
	qi, e := mlbQuotes(in, func(in *input) bool {
		return strings.HasPrefix(in.data[in.pos:], mlBasicStringDelim)
	})
	switch e.mode {
	case errBacktrack:
		in.pos = start
	case errCut:
		return "", e
	default:
		c.push(qi)
	}

	return c.finish(), parseErr{}
}

// mlb-content = mlb-char / newline / mlb-escaped-nl
// mlb-char = mlb-unescaped / escaped
func mlbContent(in *input) (string, parseErr) {
	// alt((
	//     // Deviate from the official grammar by batching the unescaped chars so we build a string a
	//     // chunk at a time, rather than a `char` at a time.
	//     take_while(1.., MLB_UNESCAPED)
	//         .try_map(std::str::from_utf8)
	//         .map(Cow::Borrowed),
	//     // Order changed fromg grammar so `escaped` can more easily `cut_err` on bad escape sequences
	//     mlb_escaped_nl.map(|_| Cow::Borrowed("")),
	//     escaped.map(|c| Cow::Owned(String::from(c))),
	//     newline.map(|_| Cow::Borrowed("\n")),
	// ))
	if chunk, ok := in.takeWhile1(isMlbUnescaped); ok {
		return chunk, parseErr{}
	}
	start := in.pos
	if e := mlbEscapedNl(in); e.mode != errBacktrack {
		return "", e
	}
	in.pos = start
	c, e := escaped(in)
	if e.mode != errBacktrack {
		if e.failed() {
			return "", e
		}
		return string(c), parseErr{}
	}
	in.pos = start
	if e := newline(in); e.failed() {
		return "", e
	}
	return "\n", parseErr{}
}

// mlb-quotes = 1*2quotation-mark
//
// term is the parser that must match, without being consumed, after the
// quotes.
func mlbQuotes(in *input, term func(*input) bool) (string, parseErr) {
	return quotesBefore(in, '"', term)
}

// quotesBefore is the body mlb_quotes and mll_quotes share: two quotes
// followed by term, or else one quote followed by term.
func quotesBefore(in *input, quote byte, term func(*input) bool) (string, parseErr) {
	start := in.pos
	// terminated(b"\"\"", peek(term.by_ref()))
	if in.eat(quote) && in.eat(quote) && term(in) {
		return in.data[start:in.pos], parseErr{}
	}
	in.pos = start
	// terminated(b"\"", peek(term.by_ref()))
	if !in.eat(quote) {
		return "", backtrack()
	}
	if !term(in) {
		return "", backtrack()
	}
	return in.data[start:in.pos], parseErr{}
}

// mlb-unescaped = wschar / %x21 / %x23-5B / %x5D-7E / non-ascii
func isMlbUnescaped(b byte) bool {
	return isWschar(b) || b == 0x21 || (b >= 0x23 && b <= 0x5B) || (b >= 0x5D && b <= 0x7E) || isNonASCII(b)
}

// mlb-escaped-nl = escape ws newline *( wschar / newline
// When the last non-whitespace character on a line is a \,
// it will be trimmed along with all whitespace
// (including newlines) up to the next non-whitespace
// character or closing delimiter.
func mlbEscapedNl(in *input) parseErr {
	// repeat(1.., (ESCAPE, ws, ws_newlines))
	one := func() parseErr {
		if !in.eat(escape) {
			return backtrack()
		}
		ws(in)
		return wsNewlines(in)
	}
	if e := one(); e.failed() {
		return e
	}
	for {
		start := in.pos
		if e := one(); e.failed() {
			in.pos = start
			return parseErr{}
		}
	}
}

// ;; Literal String

// literal-string = apostrophe *literal-char apostrophe
func literalString(in *input) (string, parseErr) {
	// delimited(
	//     APOSTROPHE,
	//     cut_err(take_while(0.., LITERAL_CHAR)),
	//     cut_err(APOSTROPHE),
	// )
	// .try_map(std::str::from_utf8)
	// .context(StrContext::Label("literal string"))
	if !in.eat(apostrophe) {
		return "", backtrack().add(label("literal string"))
	}
	s := in.takeWhile0(isLiteralChar)
	if !in.eat(apostrophe) {
		return "", cutEmpty().add(label("literal string"))
	}
	return s, parseErr{}
}

// apostrophe = %x27 ; ' apostrophe
const apostrophe = '\''

// literal-char = %x09 / %x20-26 / %x28-7E / non-ascii
func isLiteralChar(b byte) bool {
	return b == 0x9 || (b >= 0x20 && b <= 0x26) || (b >= 0x28 && b <= 0x7E) || isNonASCII(b)
}

// ;; Multiline Literal String

// ml-literal-string = ml-literal-string-delim [ newline ] ml-literal-body
//
//	ml-literal-string-delim
func mlLiteralString(in *input) (string, parseErr) {
	// delimited(
	//     (ML_LITERAL_STRING_DELIM, opt(newline)),
	//     cut_err(ml_literal_body.map(|t| {
	//         if t.contains("\r\n") {
	//             Cow::Owned(t.replace("\r\n", "\n"))
	//         } else {
	//             Cow::Borrowed(t)
	//         }
	//     })),
	//     cut_err(ML_LITERAL_STRING_DELIM),
	// )
	// .context(StrContext::Label("multiline literal string"))
	s, e := func() (string, parseErr) {
		if !in.tag(mlLiteralStringDelim) {
			return "", backtrack()
		}
		beforeNewline := in.pos
		if newline(in).failed() {
			in.pos = beforeNewline
		}
		t := mlLiteralBody(in)
		if strings.Contains(t, "\r\n") {
			t = strings.ReplaceAll(t, "\r\n", "\n")
		}
		if !in.tag(mlLiteralStringDelim) {
			return "", cutEmpty()
		}
		return t, parseErr{}
	}()
	if e.failed() {
		return "", e.add(label("multiline literal string"))
	}
	return s, parseErr{}
}

// ml-literal-string-delim = 3apostrophe
const mlLiteralStringDelim = "'''"

// ml-literal-body = *mll-content *( mll-quotes 1*mll-content ) [ mll-quotes ]
func mlLiteralBody(in *input) string {
	// (
	//     repeat(0.., mll_content).map(|()| ()),
	//     repeat(
	//         0..,
	//         (
	//             mll_quotes(none_of(APOSTROPHE).value(())),
	//             repeat(1.., mll_content).map(|()| ()),
	//         ),
	//     )
	//     .map(|()| ()),
	//     opt(mll_quotes(tag(ML_LITERAL_STRING_DELIM).value(()))),
	// )
	//     .recognize()
	//     .try_map(std::str::from_utf8)
	start := in.pos
	// repeatMllContent is `repeat(0.., mll_content)`; it returns how many
	// were taken.
	repeatMllContent := func() int {
		count := 0
		for {
			before := in.pos
			if !mllContent(in) {
				in.pos = before
				return count
			}
			count++
		}
	}
	repeatMllContent()
	for {
		before := in.pos
		_, e := mllQuotes(in, func(in *input) bool {
			// none_of(APOSTROPHE)
			b, ok := in.peek()
			return ok && b != apostrophe
		})
		if e.failed() || repeatMllContent() == 0 {
			in.pos = before
			break
		}
	}
	before := in.pos
	_, e := mllQuotes(in, func(in *input) bool {
		return strings.HasPrefix(in.data[in.pos:], mlLiteralStringDelim)
	})
	if e.failed() {
		in.pos = before
	}
	return in.data[start:in.pos]
}

// mll-content = mll-char / newline
func mllContent(in *input) bool {
	// alt((one_of(MLL_CHAR), newline))
	if in.oneOf(isMllChar) {
		return true
	}
	return !newline(in).failed()
}

// mll-char = %x09 / %x20-26 / %x28-7E / non-ascii
func isMllChar(b byte) bool {
	return b == 0x9 || (b >= 0x20 && b <= 0x26) || (b >= 0x28 && b <= 0x7E) || isNonASCII(b)
}

// mll-quotes = 1*2apostrophe
//
// term is the parser that must match, without being consumed, after the
// quotes.
func mllQuotes(in *input, term func(*input) bool) (string, parseErr) {
	return quotesBefore(in, apostrophe, term)
}

// Port of toml_edit 0.20.2 src/parser/trivia.rs

package tomledit

// wschar = ( %x20 /              ; Space
//
//	%x09 )              ; Horizontal tab
func isWschar(b byte) bool {
	return b == ' ' || b == '\t'
}

// ws = *wschar
func ws(in *input) string {
	return in.takeWhile0(isWschar)
}

// non-ascii = %x80-D7FF / %xE000-10FFFF
// - ASCII is 0xxxxxxx
// - First byte for UTF-8 is 11xxxxxx
// - Subsequent UTF-8 bytes are 10xxxxxx
func isNonASCII(b byte) bool {
	return b >= 0x80
}

// non-eol = %x09 / %x20-7E / non-ascii
func isNonEol(b byte) bool {
	return b == 0x09 || (b >= 0x20 && b <= 0x7E) || isNonASCII(b)
}

// comment-start-symbol = %x23 ; #
const commentStartSymbol = '#'

// comment = comment-start-symbol *non-eol
func comment(in *input) parseErr {
	if !in.eat(commentStartSymbol) {
		return backtrack()
	}
	in.takeWhile0(isNonEol)
	return parseErr{}
}

// newline = ( %x0A /              ; LF
//
//	%x0D.0A )           ; CRLF
func newline(in *input) parseErr {
	// alt((one_of(LF).value(b'\n'), (one_of(CR), one_of(LF)).value(b'\n')))
	if in.eat(lf) {
		return parseErr{}
	}
	if !in.eat(cr) {
		return backtrack()
	}
	if !in.eat(lf) {
		// The second branch is the alt's last, so its failure is returned
		// with the carriage return consumed.
		return backtrack()
	}
	return parseErr{}
}

const (
	lf = '\n'
	cr = '\r'
)

// ws-newline       = *( wschar / newline )
func wsNewline(in *input) {
	// repeat(0.., alt((newline.value(&b"\n"[..]), take_while(1.., WSCHAR))))
	for {
		start := in.pos
		if !newline(in).failed() {
			continue
		}
		in.pos = start
		if _, ok := in.takeWhile1(isWschar); ok {
			continue
		}
		return
	}
}

// ws-newlines      = newline *( wschar / newline )
func wsNewlines(in *input) parseErr {
	if e := newline(in); e.failed() {
		return e
	}
	wsNewline(in)
	return parseErr{}
}

// note: this rule is not present in the original grammar
// ws-comment-newline = *( ws-newline-nonempty / comment )
func wsCommentNewline(in *input) {
	// repeat(0.., alt((
	//     repeat(1.., alt((take_while(1.., WSCHAR), newline.value(&b"\n"[..])))),
	//     comment.value(()),
	// )))
	for {
		start := in.pos
		count := 0
		for {
			inner := in.pos
			if _, ok := in.takeWhile1(isWschar); ok {
				count++
				continue
			}
			if !newline(in).failed() {
				count++
				continue
			}
			in.pos = inner
			break
		}
		if count > 0 {
			continue
		}
		in.pos = start
		if !comment(in).failed() {
			continue
		}
		in.pos = start
		return
	}
}

// note: this rule is not present in the original grammar
// line-ending = newline / eof
func lineEnding(in *input) parseErr {
	// alt((newline.value("\n"), eof.value("")))
	start := in.pos
	if !newline(in).failed() {
		return parseErr{}
	}
	in.pos = start
	if in.eofOffset() == 0 {
		return parseErr{}
	}
	return backtrack()
}

// note: this rule is not present in the original grammar
// line-trailing = ws [comment] skip-line-ending
func lineTrailing(in *input) (Range, parseErr) {
	// terminated((ws, opt(comment)).span(), line_ending)
	start := in.pos
	ws(in)
	comment(in) // opt: a failed comment has consumed nothing
	span := Range{Start: start, End: in.pos}
	if e := lineEnding(in); e.failed() {
		return Range{}, e
	}
	return span, parseErr{}
}

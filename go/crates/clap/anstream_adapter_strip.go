// Port of anstream 1.0.0 src/adapter/strip.rs, anstyle-parse 1.0.0
// src/state/mod.rs and src/state/definitions.rs, and utf8parse 0.2.2
// src/lib.rs and src/types.rs

package clap

// anstyleParseState is anstyle_parse::state::State.
type anstyleParseState uint8

const (
	anstyleParseState_Anywhere anstyleParseState = iota
	anstyleParseState_CsiEntry
	anstyleParseState_CsiIgnore
	anstyleParseState_CsiIntermediate
	anstyleParseState_CsiParam
	anstyleParseState_DcsEntry
	anstyleParseState_DcsIgnore
	anstyleParseState_DcsIntermediate
	anstyleParseState_DcsParam
	anstyleParseState_DcsPassthrough
	anstyleParseState_Escape
	anstyleParseState_EscapeIntermediate
	anstyleParseState_Ground
	anstyleParseState_OscString
	anstyleParseState_SosPmApcString
	anstyleParseState_Utf8
)

// anstyleParseAction is anstyle_parse::state::Action.
type anstyleParseAction uint8

const (
	anstyleParseAction_Nop anstyleParseAction = iota
	anstyleParseAction_Clear
	anstyleParseAction_Collect
	anstyleParseAction_CsiDispatch
	anstyleParseAction_EscDispatch
	anstyleParseAction_Execute
	anstyleParseAction_Hook
	anstyleParseAction_Ignore
	anstyleParseAction_OscEnd
	anstyleParseAction_OscPut
	anstyleParseAction_OscStart
	anstyleParseAction_Param
	anstyleParseAction_Print
	anstyleParseAction_Put
	anstyleParseAction_Unhook
	anstyleParseAction_BeginUtf8
)

// anstyleParseStateChange is anstyle_parse::state::state_change: transition
// to next State.
//
// Note: This does not directly support UTF-8.
//   - If the data is validated as UTF-8 (e.g. str) or single-byte C1 control
//     codes are unsupported, then treat Action::BeginUtf8 and
//     Action::Execute for UTF-8 continuations as Action::Print.
//   - If the data is not validated, then a UTF-8 state machine will need to
//     be implemented on top, starting with Action::BeginUtf8.
//
// Note: When State::Anywhere is returned, revert back to the prior state.
func anstyleParseStateChange(state anstyleParseState, b byte) (anstyleParseState, anstyleParseAction) {
	// Handle state changes in the anywhere state before evaluating changes
	// for current state.
	change := anstyleParseStateChanges[anstyleParseState_Anywhere][b]
	if change == 0 {
		change = anstyleParseStateChanges[state][b]
	}

	// Unpack into a state and action
	// State is stored in bottom 4 bits
	// Action is stored in top 4 bits
	return anstyleParseState(change & 0x0f), anstyleParseAction(change >> 4)
}

// stripSpan is one printable piece of a string: its byte offsets.
type stripSpan struct {
	start int
	end   int
}

// stripStrSpans is anstream::adapter::strip_str, as the offsets of the
// pieces: strip the bytes of ANSI escapes from a str.
func stripStrSpans(data string) []stripSpan {
	var spans []stripSpan
	state := anstyleParseState_Ground
	pos := 0
	for {
		// next_str
		for pos < len(data) {
			b := data[pos]
			nextState, action := anstyleParseStateChange(state, b)
			if nextState != anstyleParseState_Anywhere {
				state = nextState
			}
			if stripIsPrintableBytes(action, b) {
				break
			}
			pos++
		}
		state = anstyleParseState_Ground

		start := pos
		for pos < len(data) {
			b := data[pos]
			_, action := anstyleParseStateChange(anstyleParseState_Ground, b)
			if !(stripIsPrintableBytes(action, b) || stripIsUtf8Continuation(b)) {
				break
			}
			pos++
		}
		if start == pos {
			return spans
		}
		spans = append(spans, stripSpan{start: start, end: pos})
	}
}

// stripStr is anstream::adapter::strip_str: the printable pieces of a str.
func stripStr(data string) []string {
	spans := stripStrSpans(data)
	pieces := make([]string, len(spans))
	for i, span := range spans {
		pieces[i] = data[span.start:span.end]
	}
	return pieces
}

func stripIsUtf8Continuation(b byte) bool {
	return b >= 0x80 && b <= 0xbf
}

// stripBytes is anstream::adapter::StripBytes: incrementally strip
// non-contiguous data.
type stripBytes struct {
	state      anstyleParseState
	utf8parser utf8parseParser
}

func stripBytesNew() stripBytes {
	return stripBytes{state: anstyleParseState_Ground}
}

// stripNext is StripBytes::strip_next: strip the next segment of data,
// returning its printable pieces.
func (s *stripBytes) stripNext(data []byte) [][]byte {
	var pieces [][]byte
	bytes := data
	for {
		// next_bytes
		offset := len(bytes)
		for i, b := range bytes {
			if s.state == anstyleParseState_Utf8 {
				offset = i
				break
			}
			nextState, action := anstyleParseStateChange(s.state, b)
			if nextState != anstyleParseState_Anywhere {
				s.state = nextState
			}
			if stripIsPrintableBytes(action, b) {
				offset = i
				break
			}
		}
		bytes = bytes[offset:]

		offset = len(bytes)
		for i, b := range bytes {
			if s.state == anstyleParseState_Utf8 {
				if s.utf8parser.add(b) {
					s.state = anstyleParseState_Ground
				}
				continue
			}
			nextState, action := anstyleParseStateChange(anstyleParseState_Ground, b)
			if nextState != anstyleParseState_Anywhere {
				s.state = nextState
			}
			if s.state == anstyleParseState_Utf8 {
				s.utf8parser.add(b)
				continue
			}
			if !stripIsPrintableBytes(action, b) {
				offset = i
				break
			}
		}
		printable := bytes[:offset]
		bytes = bytes[offset:]
		if len(printable) == 0 {
			return pieces
		}
		pieces = append(pieces, printable)
	}
}

func stripIsPrintableBytes(action anstyleParseAction, b byte) bool {
	// VT320 considered 0x7f to be `Print`able but we expect to be working in UTF-8 systems and not
	// ISO Latin-1, making it DEL and non-printable
	const del = 0x7f

	// Continuations aren't included as they may also be control codes, requiring more context
	return (action == anstyleParseAction_Print && b != del) ||
		action == anstyleParseAction_BeginUtf8 ||
		(action == anstyleParseAction_Execute && byteIsAsciiWhitespace(b))
}

// byteIsAsciiWhitespace is u8::is_ascii_whitespace: space, horizontal tab,
// line feed, form feed, or carriage return.
func byteIsAsciiWhitespace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\x0C' || b == '\r'
}

// utf8parseState is utf8parse's State.
type utf8parseState uint8

const (
	utf8parseState_Ground utf8parseState = iota
	utf8parseState_Tail3
	utf8parseState_Tail2
	utf8parseState_Tail1
	utf8parseState_U3_2_e0
	utf8parseState_U3_2_ed
	utf8parseState_Utf8_4_3_f0
	utf8parseState_Utf8_4_3_f4
)

// utf8parseParser is utf8parse::Parser, reduced to what the strip adapter
// asks of it: whether a byte completed a code point or broke a sequence.
type utf8parseParser struct {
	state utf8parseState
}

// add is anstream's Utf8Parser::add: advance the parser by one byte and
// report whether a code point or an invalid sequence was produced.
func (p *utf8parseParser) add(b byte) bool {
	// State::advance, with `true` for the actions InvalidSequence, EmitByte
	// and SetByte1, which are the ones that call the receiver.
	next, emitted := utf8parseState_Ground, true
	switch p.state {
	case utf8parseState_Ground:
		switch {
		case b <= 0x7f:
			next, emitted = utf8parseState_Ground, true
		case b >= 0xc2 && b <= 0xdf:
			next, emitted = utf8parseState_Tail1, false
		case b == 0xe0:
			next, emitted = utf8parseState_U3_2_e0, false
		case b >= 0xe1 && b <= 0xec:
			next, emitted = utf8parseState_Tail2, false
		case b == 0xed:
			next, emitted = utf8parseState_U3_2_ed, false
		case b >= 0xee && b <= 0xef:
			next, emitted = utf8parseState_Tail2, false
		case b == 0xf0:
			next, emitted = utf8parseState_Utf8_4_3_f0, false
		case b >= 0xf1 && b <= 0xf3:
			next, emitted = utf8parseState_Tail3, false
		case b == 0xf4:
			next, emitted = utf8parseState_Utf8_4_3_f4, false
		}
	case utf8parseState_U3_2_e0:
		if b >= 0xa0 && b <= 0xbf {
			next, emitted = utf8parseState_Tail1, false
		}
	case utf8parseState_U3_2_ed:
		if b >= 0x80 && b <= 0x9f {
			next, emitted = utf8parseState_Tail1, false
		}
	case utf8parseState_Utf8_4_3_f0:
		if b >= 0x90 && b <= 0xbf {
			next, emitted = utf8parseState_Tail2, false
		}
	case utf8parseState_Utf8_4_3_f4:
		if b >= 0x80 && b <= 0x8f {
			next, emitted = utf8parseState_Tail2, false
		}
	case utf8parseState_Tail3:
		if b >= 0x80 && b <= 0xbf {
			next, emitted = utf8parseState_Tail2, false
		}
	case utf8parseState_Tail2:
		if b >= 0x80 && b <= 0xbf {
			next, emitted = utf8parseState_Tail1, false
		}
	case utf8parseState_Tail1:
		if b >= 0x80 && b <= 0xbf {
			next, emitted = utf8parseState_Ground, true
		}
	}
	p.state = next
	return emitted
}

// Port of toml_edit 0.20.2 src/parser/document.rs

package tomledit

// ;; TOML

// toml = expression *( newline expression )

// expression = ( ( ws comment ) /
//
//	( ws keyval ws [ comment ] ) /
//	( ws table ws [ comment ] ) /
//	  ws )
func document(in *input) (Document, parseErr) {
	state := parseStateDefault()

	// (
	//     // Remove BOM if present
	//     opt(b"\xEF\xBB\xBF"),
	//     parse_ws(state_ref),
	//     repeat(0.., (
	//         dispatch! {peek(any);
	//             crate::parser::trivia::COMMENT_START_SYMBOL => cut_err(parse_comment(state_ref)),
	//             crate::parser::table::STD_TABLE_OPEN => cut_err(table(state_ref)),
	//             crate::parser::trivia::LF |
	//             crate::parser::trivia::CR => parse_newline(state_ref),
	//             _ => cut_err(keyval(state_ref)),
	//         },
	//         parse_ws(state_ref),
	//     ))
	//     .map(|()| ()),
	//     eof,
	// )
	in.tag("\xEF\xBB\xBF")
	parseWs(in, state)
	for {
		start := in.pos
		e := func() parseErr {
			b, ok := in.peek()
			if !ok {
				return backtrack()
			}
			var e parseErr
			switch b {
			case commentStartSymbol:
				e = parseComment(in, state).cut()
			case stdTableOpen:
				e = table(in, state).cut()
			case lf, cr:
				e = parseNewline(in, state)
			default:
				e = keyvalInto(in, state).cut()
			}
			if e.failed() {
				return e
			}
			parseWs(in, state)
			return parseErr{}
		}()
		if e.mode == errBacktrack {
			in.pos = start
			break
		}
		if e.mode == errCut {
			return Document{}, e
		}
	}
	if in.eofOffset() != 0 {
		return Document{}, backtrack()
	}
	doc, err := state.intoDocument()
	if err != nil {
		return Document{}, external(err)
	}
	return doc, parseErr{}
}

func parseComment(in *input, state *parseState) parseErr {
	// (comment, line_ending).span()
	start := in.pos
	if e := comment(in); e.failed() {
		return e
	}
	if e := lineEnding(in); e.failed() {
		return e
	}
	state.onComment(Range{Start: start, End: in.pos})
	return parseErr{}
}

func parseWs(in *input, state *parseState) {
	start := in.pos
	ws(in)
	state.onWs(Range{Start: start, End: in.pos})
}

func parseNewline(in *input, state *parseState) parseErr {
	start := in.pos
	if e := newline(in); e.failed() {
		return e
	}
	state.onWs(Range{Start: start, End: in.pos})
	return parseErr{}
}

// keyvalInto is the Rust `keyval` of this module: it parses a key/value pair
// and adds it to the current table.
func keyvalInto(in *input, state *parseState) parseErr {
	// parse_keyval.try_map(|(p, kv)| state.borrow_mut().on_keyval(p, kv))
	start := in.pos
	pair, e := parseKeyval(in)
	if e.failed() {
		return e
	}
	if err := state.onKeyval(pair.path, pair.kv); err != nil {
		in.pos = start
		return external(err)
	}
	return parseErr{}
}

// keyval = key keyval-sep val
func parseKeyval(in *input) (keyvalPair, parseErr) {
	// (
	//     key,
	//     cut_err((
	//         one_of(KEYVAL_SEP)
	//             .context(StrContext::Expected(StrContextValue::CharLiteral('.')))
	//             .context(StrContext::Expected(StrContextValue::CharLiteral('='))),
	//         (
	//             ws.span(),
	//             value(RecursionCheck::default()),
	//             line_trailing
	//                 .context(StrContext::Expected(StrContextValue::CharLiteral('\n')))
	//                 .context(StrContext::Expected(StrContextValue::CharLiteral('#'))),
	//         ),
	//     )),
	// )
	path, e := key(in)
	if e.failed() {
		return keyvalPair{}, e
	}
	if !in.eat(keyvalSep) {
		return keyvalPair{}, cutEmpty().add(expectedChar('.'), expectedChar('='))
	}
	preStart := in.pos
	ws(in)
	pre := Range{Start: preStart, End: in.pos}
	v, e := value(in, recursionCheck{})
	if e.failed() {
		return keyvalPair{}, e.cut()
	}
	suf, e := lineTrailing(in)
	if e.failed() {
		return keyvalPair{}, e.cut().add(expectedChar('\n'), expectedChar('#'))
	}

	k := path[len(path)-1]
	path = path[:len(path)-1]

	v.decorate(rawStringWithSpan(pre), rawStringWithSpan(suf))
	return keyvalPair{
		path: path,
		kv: tableKeyValue{
			key:   k,
			value: &Item_Value{F0: v},
		},
	}, parseErr{}
}

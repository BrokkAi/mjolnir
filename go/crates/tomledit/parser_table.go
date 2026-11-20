// Port of toml_edit 0.20.2 src/parser/table.rs

package tomledit

// std-table-open  = %x5B ws     ; [ Left square bracket
const stdTableOpen = '['

// std-table-close = ws %x5D     ; ] Right square bracket
const stdTableClose = ']'

// array-table-open  = %x5B.5B ws  ; [[ Double left square bracket
const arrayTableOpen = "[["

// array-table-close = ws %x5D.5D  ; ]] Double right quare bracket
const arrayTableClose = "]]"

// ;; Standard Table

// std-table = std-table-open key *( table-key-sep key) std-table-close
func stdTable(in *input, state *parseState) parseErr {
	// (
	//     delimited(
	//         STD_TABLE_OPEN,
	//         cut_err(key),
	//         cut_err(STD_TABLE_CLOSE)
	//             .context(StrContext::Expected(StrContextValue::CharLiteral('.')))
	//             .context(StrContext::Expected(StrContextValue::StringLiteral("]"))),
	//     )
	//     .with_span(),
	//     cut_err(line_trailing)
	//         .context(StrContext::Expected(StrContextValue::CharLiteral('\n')))
	//         .context(StrContext::Expected(StrContextValue::CharLiteral('#'))),
	// )
	//     .try_map(|((h, span), t)| state.borrow_mut().deref_mut().on_std_header(h, t, span))
	start := in.pos
	if !in.eat(stdTableOpen) {
		return backtrack()
	}
	h, e := key(in)
	if e.failed() {
		return e.cut()
	}
	if !in.eat(stdTableClose) {
		return cutEmpty().add(expectedChar('.'), expectedString("]"))
	}
	span := Range{Start: start, End: in.pos}
	t, e := lineTrailing(in)
	if e.failed() {
		return e.cut().add(expectedChar('\n'), expectedChar('#'))
	}
	if err := state.onStdHeader(h, t, span); err != nil {
		in.pos = start
		return external(err)
	}
	return parseErr{}
}

// ;; Array Table

// array-table = array-table-open key *( table-key-sep key) array-table-close
func arrayTable(in *input, state *parseState) parseErr {
	// (
	//     delimited(
	//         ARRAY_TABLE_OPEN,
	//         cut_err(key),
	//         cut_err(ARRAY_TABLE_CLOSE)
	//             .context(StrContext::Expected(StrContextValue::CharLiteral('.')))
	//             .context(StrContext::Expected(StrContextValue::StringLiteral("]]"))),
	//     )
	//     .with_span(),
	//     cut_err(line_trailing)
	//         .context(StrContext::Expected(StrContextValue::CharLiteral('\n')))
	//         .context(StrContext::Expected(StrContextValue::CharLiteral('#'))),
	// )
	//     .try_map(|((h, span), t)| state.borrow_mut().deref_mut().on_array_header(h, t, span))
	start := in.pos
	if !in.tag(arrayTableOpen) {
		return backtrack()
	}
	h, e := key(in)
	if e.failed() {
		return e.cut()
	}
	if !in.tag(arrayTableClose) {
		return cutEmpty().add(expectedChar('.'), expectedString("]]"))
	}
	span := Range{Start: start, End: in.pos}
	t, e := lineTrailing(in)
	if e.failed() {
		return e.cut().add(expectedChar('\n'), expectedChar('#'))
	}
	if err := state.onArrayHeader(h, t, span); err != nil {
		in.pos = start
		return external(err)
	}
	return parseErr{}
}

// ;; Table

// table = std-table / array-table
func table(in *input, state *parseState) parseErr {
	// dispatch!(peek::<_, &[u8],_,_>(take(2usize));
	//     b"[[" => array_table(state),
	//     _ => std_table(state),
	// )
	// .context(StrContext::Label("table header"))
	var e parseErr
	switch {
	case in.eofOffset() < 2:
		e = backtrack()
	case in.data[in.pos:in.pos+2] == arrayTableOpen:
		e = arrayTable(in, state)
	default:
		e = stdTable(in, state)
	}
	return e.add(label("table header"))
}

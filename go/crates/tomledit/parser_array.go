// Port of toml_edit 0.20.2 src/parser/array.rs

package tomledit

// ;; Array

// array = array-open array-values array-close
func array(in *input, check recursionCheck) (Array, parseErr) {
	// delimited(
	//     ARRAY_OPEN,
	//     cut_err(array_values(check)),
	//     cut_err(ARRAY_CLOSE)
	//         .context(StrContext::Label("array"))
	//         .context(StrContext::Expected(StrContextValue::CharLiteral(']'))),
	// )
	if !in.eat(arrayOpen) {
		return Array{}, backtrack()
	}
	values, e := arrayValues(in, check)
	if e.failed() {
		return Array{}, e.cut()
	}
	if !in.eat(arrayClose) {
		return Array{}, cutEmpty().add(label("array"), expectedChar(']'))
	}
	return values, parseErr{}
}

// note: we're omitting ws and newlines here, because
// they should be part of the formatted values
// array-open  = %x5B ws-newline  ; [
const arrayOpen = '['

// array-close = ws-newline %x5D  ; ]
const arrayClose = ']'

// array-sep = ws %x2C ws  ; , Comma
const arraySep = ','

// note: this rule is modified
// array-values = [ ( array-value array-sep array-values ) /
//
//	array-value / ws-comment-newline ]
func arrayValues(in *input, check recursionCheck) (Array, parseErr) {
	check, e := check.recursing()
	if e.failed() {
		return Array{}, e
	}
	// (
	//     opt(
	//         (separated1(array_value(check), ARRAY_SEP), opt(ARRAY_SEP)).map(..),
	//     ),
	//     ws_comment_newline.span(),
	// )
	start := in.pos
	var values []Item
	comma := false
	e = func() parseErr {
		// Parse the first element
		first, e := arrayValue(in, check)
		if e.failed() {
			return e
		}
		values = append(values, &Item_Value{F0: first})
		for {
			beforeSeparator := in.pos
			if !in.eat(arraySep) {
				break
			}
			next, e := arrayValue(in, check)
			if e.mode == errBacktrack {
				in.pos = beforeSeparator
				break
			}
			if e.mode == errCut {
				return e
			}
			values = append(values, &Item_Value{F0: next})
		}
		comma = in.eat(arraySep)
		return parseErr{}
	}()
	switch e.mode {
	case errBacktrack:
		in.pos = start
		values = nil
		comma = false
	case errCut:
		return Array{}, e
	}
	trailingStart := in.pos
	wsCommentNewline(in)
	trailing := Range{Start: trailingStart, End: in.pos}

	array := arrayWithVec(values)
	array.SetTrailingComma(comma)
	array.trailing = rawStringWithSpan(trailing)
	return array, parseErr{}
}

func arrayValue(in *input, check recursionCheck) (Value, parseErr) {
	// (
	//     ws_comment_newline.span(),
	//     value(check),
	//     ws_comment_newline.span(),
	// )
	ws1Start := in.pos
	wsCommentNewline(in)
	ws1 := Range{Start: ws1Start, End: in.pos}
	v, e := value(in, check)
	if e.failed() {
		return nil, e
	}
	ws2Start := in.pos
	wsCommentNewline(in)
	ws2 := Range{Start: ws2Start, End: in.pos}
	v.decorate(rawStringWithSpan(ws1), rawStringWithSpan(ws2))
	return v, parseErr{}
}

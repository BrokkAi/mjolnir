// Port of toml_edit 0.20.2 src/parser/key.rs

package tomledit

// key = simple-key / dotted-key
// dotted-key = simple-key 1*( dot-sep simple-key )
func key(in *input) ([]Key, parseErr) {
	// separated1(
	//     (ws.span(), simple_key, ws.span()).map(..),
	//     DOT_SEP,
	// )
	// .context(StrContext::Label("key"))
	// .try_map(|k: Vec<_>| {
	//     // Inserting the key will require recursion down the line
	//     RecursionCheck::check_depth(k.len())?;
	//     Ok::<_, CustomError>(k)
	// })
	start := in.pos
	one := func() (Key, parseErr) {
		preStart := in.pos
		ws(in)
		pre := Range{Start: preStart, End: in.pos}
		raw, k, e := simpleKey(in)
		if e.failed() {
			return Key{}, e
		}
		suffixStart := in.pos
		ws(in)
		suffix := Range{Start: suffixStart, End: in.pos}
		return KeyNew(k).
			withReprUnchecked(reprNewUnchecked(raw)).
			WithDecor(decorWithSpans(pre, suffix)), parseErr{}
	}

	var keys []Key
	// Parse the first element
	first, e := one()
	if e.failed() {
		return nil, e.add(label("key"))
	}
	keys = append(keys, first)

	for {
		beforeSeparator := in.pos
		if !in.eat(dotSep) {
			break
		}
		next, e := one()
		if e.mode == errBacktrack {
			in.pos = beforeSeparator
			break
		}
		if e.mode == errCut {
			return nil, e.add(label("key"))
		}
		keys = append(keys, next)
	}

	if err := recursionCheckDepth(len(keys)); err != nil {
		in.pos = start
		return nil, external(err)
	}
	return keys, parseErr{}
}

// simple-key = quoted-key / unquoted-key
// quoted-key = basic-string / literal-string
func simpleKey(in *input) (RawString, InternalString, parseErr) {
	// dispatch! {peek(any);
	//     crate::parser::strings::QUOTATION_MARK => basic_string
	//         .map(|s: std::borrow::Cow<'_, str>| s.as_ref().into()),
	//     crate::parser::strings::APOSTROPHE => literal_string.map(|s: &str| s.into()),
	//     _ => unquoted_key.map(|s: &str| s.into()),
	// }
	// .with_span()
	b, ok := in.peek()
	if !ok {
		return RawString{}, "", backtrack()
	}
	start := in.pos
	var k string
	var e parseErr
	switch b {
	case quotationMark:
		k, e = basicString(in)
	case apostrophe:
		k, e = literalString(in)
	default:
		k, e = unquotedKey(in)
	}
	if e.failed() {
		return RawString{}, "", e
	}
	raw := rawStringWithSpan(Range{Start: start, End: in.pos})
	return raw, k, parseErr{}
}

// unquoted-key = 1*( ALPHA / DIGIT / %x2D / %x5F ) ; A-Z / a-z / 0-9 / - / _
func unquotedKey(in *input) (string, parseErr) {
	s, ok := in.takeWhile1(isUnquotedChar)
	if !ok {
		return "", backtrack()
	}
	return s, parseErr{}
}

func isUnquotedChar(c byte) bool {
	return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_'
}

// dot-sep   = ws %x2E ws  ; . Period
const dotSep = '.'

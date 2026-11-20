// Port of toml_edit 0.20.2 src/parser/inline_table.rs

package tomledit

// ;; Inline Table

// inline-table = inline-table-open inline-table-keyvals inline-table-close
func inlineTable(in *input, check recursionCheck) (InlineTable, parseErr) {
	// delimited(
	//     INLINE_TABLE_OPEN,
	//     cut_err(inline_table_keyvals(check).try_map(|(kv, p)| table_from_pairs(kv, p))),
	//     cut_err(INLINE_TABLE_CLOSE)
	//         .context(StrContext::Label("inline table"))
	//         .context(StrContext::Expected(StrContextValue::CharLiteral('}'))),
	// )
	if !in.eat(inlineTableOpen) {
		return InlineTable{}, backtrack()
	}
	start := in.pos
	pairs, preamble, e := inlineTableKeyvals(in, check)
	if e.failed() {
		return InlineTable{}, e.cut()
	}
	table, err := tableFromPairs(pairs, preamble)
	if err != nil {
		in.pos = start
		return InlineTable{}, external(err).cut()
	}
	if !in.eat(inlineTableClose) {
		return InlineTable{}, cutEmpty().add(label("inline table"), expectedChar('}'))
	}
	return table, parseErr{}
}

// keyvalPair is the `(Vec<Key>, TableKeyValue)` a keyval parses to: the
// dotted path leading to the key, and the key with its value.
type keyvalPair struct {
	path []Key
	kv   tableKeyValue
}

func tableFromPairs(v []keyvalPair, preamble RawString) (InlineTable, error) {
	root := InlineTableNew()
	root.preamble = preamble
	// Assuming almost all pairs will be directly in `root`

	for _, pair := range v {
		path, kv := pair.path, pair.kv
		table, err := inlineDescendPath(&root, path)
		if err != nil {
			return InlineTable{}, err
		}

		// "Likewise, using dotted keys to redefine tables already defined in [table] form is not allowed"
		mixedTableTypes := table.IsDotted() == (len(path) == 0)
		if mixedTableTypes {
			return InlineTable{}, &customError_DuplicateKey{key: kv.key.Get()}
		}

		key := kv.key.getInternal()
		if existing := table.items.get(key); existing != nil {
			return InlineTable{}, &customError_DuplicateKey{key: existing.mapKey}
		}
		table.items.insert(key, kv)
	}
	return root, nil
}

func inlineDescendPath(table *InlineTable, path []Key) (*InlineTable, error) {
	dotted := len(path) != 0
	for i := range path {
		key := &path[i]
		entry := table.EntryFormat(key).OrInsertWith(func() Value {
			newTable := InlineTableNew()
			newTable.setImplicit(dotted)
			newTable.SetDotted(dotted)

			return &Value_InlineTable{F0: newTable}
		})
		if inline, ok := (*entry).(*Value_InlineTable); ok {
			sweetChildOfMine := &inline.F0
			// Since tables cannot be defined more than once, redefining such tables using a
			// [table] header is not allowed. Likewise, using dotted keys to redefine tables
			// already defined in [table] form is not allowed.
			if dotted && !sweetChildOfMine.isImplicit() {
				return nil, &customError_DuplicateKey{key: key.Get()}
			}
			table = sweetChildOfMine
		} else {
			return nil, customErrorExtendWrongType(path, i, (*entry).TypeName())
		}
	}
	return table, nil
}

// inline-table-open  = %x7B ws     ; {
const inlineTableOpen = '{'

// inline-table-close = ws %x7D     ; }
const inlineTableClose = '}'

// inline-table-sep   = ws %x2C ws  ; , Comma
const inlineTableSep = ','

// keyval-sep = ws %x3D ws ; =
const keyvalSep = '='

// inline-table-keyvals = [ inline-table-keyvals-non-empty ]
// inline-table-keyvals-non-empty =
// ( key keyval-sep val inline-table-sep inline-table-keyvals-non-empty ) /
// ( key keyval-sep val )

func inlineTableKeyvals(in *input, check recursionCheck) ([]keyvalPair, RawString, parseErr) {
	check, e := check.recursing()
	if e.failed() {
		return nil, RawString{}, e
	}
	// (
	//     separated0(keyval(check), INLINE_TABLE_SEP),
	//     ws.span().map(RawString::with_span),
	// )
	var pairs []keyvalPair
	start := in.pos
	first, e := inlineKeyval(in, check)
	switch e.mode {
	case errBacktrack:
		in.pos = start
	case errCut:
		return nil, RawString{}, e
	default:
		pairs = append(pairs, first)
		for {
			beforeSeparator := in.pos
			if !in.eat(inlineTableSep) {
				break
			}
			next, e := inlineKeyval(in, check)
			if e.mode == errBacktrack {
				in.pos = beforeSeparator
				break
			}
			if e.mode == errCut {
				return nil, RawString{}, e
			}
			pairs = append(pairs, next)
		}
	}
	preambleStart := in.pos
	ws(in)
	return pairs, rawStringWithSpan(Range{Start: preambleStart, End: in.pos}), parseErr{}
}

// inlineKeyval is the Rust `keyval` of this module.
func inlineKeyval(in *input, check recursionCheck) (keyvalPair, parseErr) {
	// (
	//     key,
	//     cut_err((
	//         one_of(KEYVAL_SEP)
	//             .context(StrContext::Expected(StrContextValue::CharLiteral('.')))
	//             .context(StrContext::Expected(StrContextValue::CharLiteral('='))),
	//         (ws.span(), value(check), ws.span()),
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
	v, e := value(in, check)
	if e.failed() {
		return keyvalPair{}, e.cut()
	}
	sufStart := in.pos
	ws(in)
	suf := Range{Start: sufStart, End: in.pos}

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

// Port of toml_edit 0.20.2 src/parser/state.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

type parseState struct {
	document             Document
	trailing             rs.Option[Range]
	currentTablePosition int
	currentTable         Table
	currentIsArray       bool
	currentTablePath     []Key
}

func (s *parseState) intoDocument() (Document, error) {
	if err := s.finalizeTable(); err != nil {
		return Document{}, err
	}
	if trailing, ok := s.trailing.Get(); ok {
		s.document.trailing = rawStringWithSpan(trailing)
	} else {
		s.document.trailing = RawStringDefault()
	}
	return s.document, nil
}

func (s *parseState) onWs(span Range) {
	if old, ok := s.trailing.Take().Get(); ok {
		s.trailing = rs.Some(Range{Start: old.Start, End: span.End})
	} else {
		s.trailing = rs.Some(span)
	}
}

func (s *parseState) onComment(span Range) {
	if old, ok := s.trailing.Take().Get(); ok {
		s.trailing = rs.Some(Range{Start: old.Start, End: span.End})
	} else {
		s.trailing = rs.Some(span)
	}
}

func (s *parseState) onKeyval(path []Key, kv tableKeyValue) error {
	{
		prefix := s.trailing.Take()
		firstKey := &kv.key
		if len(path) != 0 {
			firstKey = &path[0]
		}
		p, hasP := prefix.Get()
		var k Range
		hasK := false
		if keyPrefix, ok := firstKey.decor.Prefix().Get(); ok {
			k, hasK = keyPrefix.span().Get()
		}
		var merged rs.Option[Range]
		switch {
		case hasP && hasK:
			merged = rs.Some(Range{Start: p.Start, End: k.End})
		case hasP:
			merged = rs.Some(p)
		case hasK:
			merged = rs.Some(k)
		default:
			merged = rs.None[Range]()
		}
		if span, ok := merged.Get(); ok {
			firstKey.decor.prefix = rs.Some(rawStringWithSpan(span))
		} else {
			firstKey.decor.prefix = rs.Some(RawStringDefault())
		}
	}

	if existing, ok := s.currentTable.span.Get(); ok {
		if value, ok := kv.value.span().Get(); ok {
			s.currentTable.span = rs.Some(Range{Start: existing.Start, End: value.End})
		}
	}
	table := &s.currentTable
	table, err := descendPath(table, path, true)
	if err != nil {
		return err
	}

	// "Likewise, using dotted keys to redefine tables already defined in [table] form is not allowed"
	mixedTableTypes := table.IsDotted() == (len(path) == 0)
	if mixedTableTypes {
		return &customError_DuplicateKey{key: kv.key.Get()}
	}

	key := kv.key.getInternal()
	if existing := table.items.get(key); existing != nil {
		// "Since tables cannot be defined more than once, redefining such tables using a [table] header is not allowed"
		return &customError_DuplicateKey{
			key:   existing.mapKey,
			table: rs.Some(cloneKeys(s.currentTablePath)),
		}
	}
	table.items.insert(key, kv)

	return nil
}

func (s *parseState) startArrayTable(path []Key, decor Decor, span Range) error {
	rs.DebugAssert(len(path) != 0)
	rs.DebugAssert(s.currentTable.IsEmpty())
	rs.DebugAssert(len(s.currentTablePath) == 0)

	// Look up the table on start to ensure the duplicate_key error points to the right line
	root := s.document.AsTableMut()
	parentTable, err := descendPath(root, path[:len(path)-1], false)
	if err != nil {
		return err
	}
	key := &path[len(path)-1]
	entry := parentTable.EntryFormat(key).OrInsert(&Item_ArrayOfTables{F0: ArrayOfTablesNew()})
	if !(*entry).IsArrayOfTables() {
		return customErrorDuplicateKey(path, len(path)-1)
	}

	s.currentTablePosition++
	s.currentTable.decor = decor
	s.currentTable.SetImplicit(false)
	s.currentTable.SetDotted(false)
	s.currentTable.SetPosition(s.currentTablePosition)
	s.currentTable.span = rs.Some(span)
	s.currentIsArray = true
	s.currentTablePath = path

	return nil
}

func (s *parseState) startTable(path []Key, decor Decor, span Range) error {
	rs.DebugAssert(len(path) != 0)
	rs.DebugAssert(s.currentTable.IsEmpty())
	rs.DebugAssert(len(s.currentTablePath) == 0)

	// 1. Look up the table on start to ensure the duplicate_key error points to the right line
	// 2. Ensure any child tables from an implicit table are preserved
	root := s.document.AsTableMut()
	parentTable, err := descendPath(root, path[:len(path)-1], false)
	if err != nil {
		return err
	}
	key := &path[len(path)-1]
	if entry, ok := parentTable.Remove(key.Get()).Get(); ok {
		if t, isTable := entry.(*Item_Table); isTable && t.F0.implicit && !t.F0.IsDotted() {
			s.currentTable = t.F0
		} else {
			// Since tables cannot be defined more than once, redefining such tables using a [table] header is not allowed. Likewise, using dotted keys to redefine tables already defined in [table] form is not allowed.
			return customErrorDuplicateKey(path, len(path)-1)
		}
	}

	s.currentTablePosition++
	s.currentTable.decor = decor
	s.currentTable.SetImplicit(false)
	s.currentTable.SetDotted(false)
	s.currentTable.SetPosition(s.currentTablePosition)
	s.currentTable.span = rs.Some(span)
	s.currentIsArray = false
	s.currentTablePath = path

	return nil
}

func (s *parseState) finalizeTable() error {
	table := s.currentTable
	s.currentTable = TableDefault()
	path := s.currentTablePath
	s.currentTablePath = nil

	root := s.document.AsTableMut()
	if len(path) == 0 {
		rs.Assert(root.IsEmpty())
		*root = table
	} else if s.currentIsArray {
		parentTable, err := descendPath(root, path[:len(path)-1], false)
		if err != nil {
			return err
		}
		key := &path[len(path)-1]

		entry := parentTable.EntryFormat(key).OrInsert(&Item_ArrayOfTables{F0: ArrayOfTablesNew()})
		array, ok := (*entry).AsArrayOfTablesMut().Get()
		if !ok {
			return customErrorDuplicateKey(path, len(path)-1)
		}
		array.Push(table)
		span := rs.None[Range]()
		if len(array.values) > 0 {
			first, hasFirst := array.values[0].span().Get()
			last, hasLast := array.values[len(array.values)-1].span().Get()
			if hasFirst && hasLast {
				span = rs.Some(Range{Start: first.Start, End: last.End})
			}
		}
		array.span = span
	} else {
		parentTable, err := descendPath(root, path[:len(path)-1], false)
		if err != nil {
			return err
		}
		key := &path[len(path)-1]

		entry := parentTable.EntryFormat(key)
		switch entry := entry.(type) {
		case *Entry_Occupied:
			// if [a.b.c] header preceded [a.b]
			if t, isTable := (*entry.F0.IntoMut()).(*Item_Table); isTable && t.F0.implicit {
				t.F0 = table
			} else {
				return customErrorDuplicateKey(path, len(path)-1)
			}
		case *Entry_Vacant:
			item := &Item_Table{F0: table}
			entry.F0.Insert(item)
		}
	}

	return nil
}

func descendPath(table *Table, path []Key, dotted bool) (*Table, error) {
	for i := range path {
		key := &path[i]
		entry := table.EntryFormat(key).OrInsertWith(func() Item {
			newTable := TableNew()
			newTable.SetImplicit(true)
			newTable.SetDotted(dotted)

			return &Item_Table{F0: newTable}
		})
		switch item := (*entry).(type) {
		case *Item_Value:
			return nil, customErrorExtendWrongType(path, i, item.F0.TypeName())
		case *Item_ArrayOfTables:
			array := &item.F0
			rs.DebugAssert(!array.IsEmpty())

			index := array.Len() - 1
			lastChild := array.GetMut(index).Unwrap()

			table = lastChild
		case *Item_Table:
			sweetChildOfMine := &item.F0
			// Since tables cannot be defined more than once, redefining such tables using a
			// [table] header is not allowed. Likewise, using dotted keys to redefine tables
			// already defined in [table] form is not allowed.
			if dotted && !sweetChildOfMine.IsImplicit() {
				return nil, &customError_DuplicateKey{key: key.Get()}
			}
			table = sweetChildOfMine
		default:
			panic(rs.Unreachable())
		}
	}
	return table, nil
}

func (s *parseState) onStdHeader(path []Key, trailing Range, span Range) error {
	rs.DebugAssert(len(path) != 0)

	if err := s.finalizeTable(); err != nil {
		return err
	}
	leading := RawStringDefault()
	if taken, ok := s.trailing.Take().Get(); ok {
		leading = rawStringWithSpan(taken)
	}
	return s.startTable(
		path,
		Decor{prefix: rs.Some(leading), suffix: rs.Some(rawStringWithSpan(trailing))},
		span,
	)
}

func (s *parseState) onArrayHeader(path []Key, trailing Range, span Range) error {
	rs.DebugAssert(len(path) != 0)

	if err := s.finalizeTable(); err != nil {
		return err
	}
	leading := RawStringDefault()
	if taken, ok := s.trailing.Take().Get(); ok {
		leading = rawStringWithSpan(taken)
	}
	return s.startArrayTable(
		path,
		Decor{prefix: rs.Some(leading), suffix: rs.Some(rawStringWithSpan(trailing))},
		span,
	)
}

func parseStateDefault() *parseState {
	root := TableNew()
	root.span = rs.Some(Range{Start: 0, End: 0})
	return &parseState{
		document:             DocumentNew(),
		trailing:             rs.None[Range](),
		currentTablePosition: 0,
		currentTable:         root,
		currentIsArray:       false,
		currentTablePath:     nil,
	}
}

// Port of toml_edit 0.20.2 src/encode.rs

package tomledit

import (
	"math"
	"sort"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The Rust trait `Encode` is implemented for Key, key paths, Formatted<T>,
// Array, InlineTable and Value. Each implementation is a function here; the
// default decor is passed as its prefix and its suffix.

func encodeKey(k *Key, buf *strings.Builder, input rs.Option[string], defaultPrefix, defaultSuffix string) {
	decor := k.Decor()
	decor.prefixEncode(buf, input, defaultPrefix)

	if text, ok := input.Get(); ok {
		if repr, ok := k.AsRepr().Get(); ok {
			repr.encode(buf, text)
		} else {
			repr := k.DefaultRepr()
			repr.encode(buf, text)
		}
	} else {
		repr := k.DisplayRepr()
		buf.WriteString(repr)
	}

	decor.suffixEncode(buf, input, defaultSuffix)
}

// encodeKeyPath is `impl Encode for &[Key]` and `impl Encode for &[&Key]`.
func encodeKeyPath(keys []*Key, buf *strings.Builder, input rs.Option[string], defaultPrefix, defaultSuffix string) {
	for i, key := range keys {
		first := i == 0
		last := i+1 == len(keys)

		prefix := defaultKeyPathDecorPrefix
		if first {
			prefix = defaultPrefix
		}
		suffix := defaultKeyPathDecorSuffix
		if last {
			suffix = defaultSuffix
		}

		if !first {
			buf.WriteString(".")
		}
		encodeKey(key, buf, input, prefix, suffix)
	}
}

func encodeFormatted[T any](f *Formatted[T], buf *strings.Builder, input rs.Option[string], defaultPrefix, defaultSuffix string) {
	decor := f.Decor()
	decor.prefixEncode(buf, input, defaultPrefix)

	if text, ok := input.Get(); ok {
		if repr, ok := f.AsRepr().Get(); ok {
			repr.encode(buf, text)
		} else {
			repr := f.DefaultRepr()
			repr.encode(buf, text)
		}
	} else {
		repr := f.DisplayRepr()
		buf.WriteString(repr)
	}

	decor.suffixEncode(buf, input, defaultSuffix)
}

func encodeArray(a *Array, buf *strings.Builder, input rs.Option[string], defaultPrefix, defaultSuffix string) {
	decor := a.Decor()
	decor.prefixEncode(buf, input, defaultPrefix)
	buf.WriteString("[")

	i := 0
	for elem := range a.Iter() {
		var innerPrefix, innerSuffix string
		if i == 0 {
			innerPrefix, innerSuffix = defaultLeadingValueDecorPrefix, defaultLeadingValueDecorSuffix
		} else {
			innerPrefix, innerSuffix = defaultValueDecorPrefix, defaultValueDecorSuffix
			buf.WriteString(",")
		}
		encodeValue(elem, buf, input, innerPrefix, innerSuffix)
		i++
	}
	if a.TrailingComma() && !a.IsEmpty() {
		buf.WriteString(",")
	}

	a.Trailing().encodeWithDefault(buf, input, "")
	buf.WriteString("]")
	decor.suffixEncode(buf, input, defaultSuffix)
}

func encodeInlineTable(t *InlineTable, buf *strings.Builder, input rs.Option[string], defaultPrefix, defaultSuffix string) {
	decor := t.Decor()
	decor.prefixEncode(buf, input, defaultPrefix)
	buf.WriteString("{")
	t.Preamble().encodeWithDefault(buf, input, "")

	children := t.GetValues()
	length := len(children)
	for i, child := range children {
		if i != 0 {
			buf.WriteString(",")
		}
		innerPrefix, innerSuffix := defaultValueDecorPrefix, defaultValueDecorSuffix
		if i == length-1 {
			innerPrefix, innerSuffix = defaultTrailingValueDecorPrefix, defaultTrailingValueDecorSuffix
		}
		encodeKeyPath(child.F0, buf, input, defaultInlineKeyDecorPrefix, defaultInlineKeyDecorSuffix)
		buf.WriteString("=")
		encodeValue(child.F1, buf, input, innerPrefix, innerSuffix)
	}

	buf.WriteString("}")
	decor.suffixEncode(buf, input, defaultSuffix)
}

func encodeValue(v Value, buf *strings.Builder, input rs.Option[string], defaultPrefix, defaultSuffix string) {
	switch value := v.(type) {
	case *Value_String:
		encodeFormatted(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	case *Value_Integer:
		encodeFormatted(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	case *Value_Float:
		encodeFormatted(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	case *Value_Boolean:
		encodeFormatted(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	case *Value_Datetime:
		encodeFormatted(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	case *Value_Array:
		encodeArray(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	case *Value_InlineTable:
		encodeInlineTable(&value.F0, buf, input, defaultPrefix, defaultSuffix)
	default:
		panic(rs.Unreachable(v))
	}
}

// String is `impl Display for Document`.
func (d *Document) String() string {
	var f strings.Builder
	var path []Key
	lastPosition := 0
	type positioned struct {
		position int
		table    *Table
		path     []Key
		isArray  bool
	}
	var tables []positioned
	visitNestedTables(d.AsTable(), &path, false, func(t *Table, p []Key, isArray bool) {
		if pos, ok := t.Position().Get(); ok {
			lastPosition = pos
		}
		tables = append(tables, positioned{position: lastPosition, table: t, path: append([]Key(nil), p...), isArray: isArray})
	})

	sort.SliceStable(tables, func(i, j int) bool { return tables[i].position < tables[j].position })
	firstTable := true
	for _, entry := range tables {
		visitTable(&f, d.original, entry.table, entry.path, entry.isArray, &firstTable)
	}
	d.Trailing().encodeWithDefault(&f, d.original, "")
	return f.String()
}

// FmtDisplay is `impl Display for Document`.
func (d *Document) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(d.String()) }

func visitNestedTables(table *Table, path *[]Key, isArrayOfTables bool, callback func(t *Table, path []Key, isArray bool)) {
	if !table.IsDotted() {
		callback(table, *path, isArrayOfTables)
	}

	for _, kv := range table.items.entries {
		switch item := kv.value.(type) {
		case *Item_Table:
			t := &item.F0
			key := kv.key.Clone()
			if t.IsDotted() {
				// May have newlines and generally isn't written for standard tables
				key.DecorMut().Clear()
			}
			*path = append(*path, key)
			visitNestedTables(t, path, false, callback)
			*path = (*path)[:len(*path)-1]
		case *Item_ArrayOfTables:
			for t := range item.F0.Iter() {
				key := kv.key.Clone()
				*path = append(*path, key)
				visitNestedTables(t, path, true, callback)
				*path = (*path)[:len(*path)-1]
			}
		}
	}
}

func visitTable(buf *strings.Builder, input rs.Option[string], table *Table, path []Key, isArrayOfTables bool, firstTable *bool) {
	children := table.GetValues()
	// We are intentionally hiding implicit tables without any tables nested under them (ie
	// `table.is_empty()` which is in contrast to `table.get_values().is_empty()`).  We are
	// trusting the user that an empty implicit table is not semantically meaningful
	//
	// This allows a user to delete all tables under this implicit table and the implicit table
	// will disappear.
	//
	// However, this means that users need to take care in deciding what tables get marked as
	// implicit.
	isVisibleStdTable := !(table.implicit && len(children) == 0)

	keys := make([]*Key, len(path))
	for index := range path {
		keys[index] = &path[index]
	}

	if len(path) == 0 {
		// don't print header for the root node
		if len(children) != 0 {
			*firstTable = false
		}
	} else if isArrayOfTables {
		defaultPrefix, defaultSuffix := defaultTableDecorPrefix, defaultTableDecorSuffix
		if *firstTable {
			*firstTable = false
			defaultPrefix = ""
		}
		table.decor.prefixEncode(buf, input, defaultPrefix)
		buf.WriteString("[[")
		encodeKeyPath(keys, buf, input, defaultKeyPathDecorPrefix, defaultKeyPathDecorSuffix)
		buf.WriteString("]]")
		table.decor.suffixEncode(buf, input, defaultSuffix)
		buf.WriteString("\n")
	} else if isVisibleStdTable {
		defaultPrefix, defaultSuffix := defaultTableDecorPrefix, defaultTableDecorSuffix
		if *firstTable {
			*firstTable = false
			defaultPrefix = ""
		}
		table.decor.prefixEncode(buf, input, defaultPrefix)
		buf.WriteString("[")
		encodeKeyPath(keys, buf, input, defaultKeyPathDecorPrefix, defaultKeyPathDecorSuffix)
		buf.WriteString("]")
		table.decor.suffixEncode(buf, input, defaultSuffix)
		buf.WriteString("\n")
	}
	// print table body
	for _, child := range children {
		encodeKeyPath(child.F0, buf, input, defaultKeyDecorPrefix, defaultKeyDecorSuffix)
		buf.WriteString("=")
		encodeValue(child.F1, buf, input, defaultValueDecorPrefix, defaultValueDecorSuffix)
		buf.WriteString("\n")
	}
}

// toStringRepr is the body of `impl ValueRepr for String`, with the style
// and the literal choice left to infer_style when they are None.
func toStringRepr(value string, style rs.Option[stringStyle], literal rs.Option[bool]) Repr {
	var chosenStyle stringStyle
	var chosenLiteral bool
	styleValue, hasStyle := style.Get()
	literalValue, hasLiteral := literal.Get()
	switch {
	case hasStyle && hasLiteral:
		chosenStyle, chosenLiteral = styleValue, literalValue
	case hasLiteral:
		inferred, _ := inferStyle(value)
		chosenStyle, chosenLiteral = inferred, literalValue
	case hasStyle:
		_, inferred := inferStyle(value)
		chosenStyle, chosenLiteral = styleValue, inferred
	default:
		chosenStyle, chosenLiteral = inferStyle(value)
	}

	var output strings.Builder
	output.Grow(len(value) * 2)
	if chosenLiteral {
		output.WriteString(chosenStyle.literalStart())
		output.WriteString(value)
		output.WriteString(chosenStyle.literalEnd())
	} else {
		output.WriteString(chosenStyle.standardStart())
		for _, ch := range value {
			switch {
			case ch == '\u0008':
				output.WriteString("\\b")
			case ch == '\u0009':
				output.WriteString("\\t")
			case ch == '\u000a':
				switch chosenStyle {
				case stringStyleNewlineTriple:
					output.WriteByte('\n')
				case stringStyleOnelineSingle:
					output.WriteString("\\n")
				default:
					panic(rs.Unreachable())
				}
			case ch == '\u000c':
				output.WriteString("\\f")
			case ch == '\u000d':
				output.WriteString("\\r")
			case ch == '"':
				output.WriteString("\\\"")
			case ch == '\\':
				output.WriteString("\\\\")
			case ch <= '\u001f' || ch == '\u007f':
				output.WriteString("\\u")
				hex := strings.ToUpper(strconv.FormatUint(uint64(ch), 16))
				for len(hex) < 4 {
					hex = "0" + hex
				}
				output.WriteString(hex)
			default:
				output.WriteRune(ch)
			}
		}
		output.WriteString(chosenStyle.standardEnd())
	}

	return reprNewUnchecked(RawStringFromString(output.String()))
}

type stringStyle uint8

const (
	stringStyleNewlineTriple stringStyle = iota
	stringStyleOnelineTriple
	stringStyleOnelineSingle
)

func (s stringStyle) literalStart() string {
	switch s {
	case stringStyleNewlineTriple:
		return "'''\n"
	case stringStyleOnelineTriple:
		return "'''"
	default:
		return "'"
	}
}

func (s stringStyle) literalEnd() string {
	switch s {
	case stringStyleNewlineTriple:
		return "'''"
	case stringStyleOnelineTriple:
		return "'''"
	default:
		return "'"
	}
}

func (s stringStyle) standardStart() string {
	switch s {
	case stringStyleNewlineTriple:
		return "\"\"\"\n"
	// note: OnelineTriple can happen if do_pretty wants to do
	// '''it's one line'''
	// but literal == false
	default:
		return "\""
	}
}

func (s stringStyle) standardEnd() string {
	switch s {
	case stringStyleNewlineTriple:
		return "\"\"\""
	// note: OnelineTriple can happen if do_pretty wants to do
	// '''it's one line'''
	// but literal == false
	default:
		return "\""
	}
}

func inferStyle(value string) (stringStyle, bool) {
	// For doing pretty prints we store in a new String
	// because there are too many cases where pretty cannot
	// work. We need to determine:
	// - if we are a "multi-line" pretty (if there are \n)
	// - if ['''] appears if multi or ['] if single
	// - if there are any invalid control characters
	//
	// Doing it any other way would require multiple passes
	// to determine if a pretty string works or not.
	ty := stringStyleOnelineSingle
	// found consecutive single quotes
	maxFoundSingles := 0
	foundSingles := 0
	preferLiteral := false
	canBePretty := true

	for _, ch := range value {
		if canBePretty {
			if ch == '\'' {
				foundSingles++
				if foundSingles >= 3 {
					canBePretty = false
				}
			} else {
				if foundSingles > maxFoundSingles {
					maxFoundSingles = foundSingles
				}
				foundSingles = 0
			}
			switch {
			case ch == '\t':
			case ch == '\\':
				preferLiteral = true
			case ch == '\n':
				ty = stringStyleNewlineTriple
			// Escape codes are needed if any ascii control
			// characters are present, including \b \f \r.
			case ch <= '\u001f' || ch == '\u007f':
				canBePretty = false
			}
		} else {
			// the string cannot be represented as pretty,
			// still check if it should be multiline
			if ch == '\n' {
				ty = stringStyleNewlineTriple
			}
		}
	}
	if foundSingles > 0 && strings.HasSuffix(value, "'") {
		// We cannot escape the ending quote so we must use """
		canBePretty = false
	}
	if !preferLiteral {
		canBePretty = false
	}
	if !canBePretty {
		rs.DebugAssert(ty != stringStyleOnelineTriple)
		return ty, false
	}
	if foundSingles > maxFoundSingles {
		maxFoundSingles = foundSingles
	}
	rs.DebugAssert(maxFoundSingles < 3)
	if ty == stringStyleOnelineSingle && maxFoundSingles >= 1 {
		// no newlines, but must use ''' because it has ' in it
		ty = stringStyleOnelineTriple
	}
	return ty, true
}

// int64ToRepr is `impl ValueRepr for i64`.
func int64ToRepr(value int64) Repr {
	return reprNewUnchecked(RawStringFromString(strconv.FormatInt(value, 10)))
}

func toF64Repr(f float64) Repr {
	var repr string
	isNegative := math.Signbit(f)
	switch {
	case isNegative && math.IsNaN(f):
		repr = "-nan"
	case !isNegative && math.IsNaN(f):
		repr = "nan"
	case isNegative && f == 0.0:
		repr = "-0.0"
	case !isNegative && f == 0.0:
		repr = "0.0"
	default:
		if math.Mod(f, 1.0) == 0.0 {
			repr = formatF64(f) + ".0"
		} else {
			repr = formatF64(f)
		}
	}
	return reprNewUnchecked(RawStringFromString(repr))
}

// formatF64 is `format!("{}", f)` for a float that is not NaN: the shortest
// decimal text that reads back as f, never in exponent form.
func formatF64(f float64) string {
	if math.IsInf(f, 1) {
		return "inf"
	}
	if math.IsInf(f, -1) {
		return "-inf"
	}
	return strconv.FormatFloat(f, 'f', -1, 64)
}

// boolToRepr is `impl ValueRepr for bool`.
func boolToRepr(value bool) Repr {
	return reprNewUnchecked(RawStringFromString(strconv.FormatBool(value)))
}

// datetimeToRepr is `impl ValueRepr for Datetime`.
func datetimeToRepr(value Datetime) Repr {
	return reprNewUnchecked(RawStringFromString(value.String()))
}

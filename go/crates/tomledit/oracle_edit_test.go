package tomledit

import (
	"bytes"
	"encoding/json"
	"math"
	"math/rand"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// An edit script is a document and a list of steps; the oracle applies each
// step with toml_edit and replies with the text of the document afterwards
// and what each step returned. A step is a JSON array: its name, the path of
// the item it works on (keys and array indexes), then its own arguments. The
// names and their meaning are those of `edit_step` in the oracle module.

// editValue builds the value an edit step names: `{"s": ..}`, `{"i": ..}`,
// `{"f": ..}`, `{"b": ..}`, `{"d": ..}`, `{"a": [..]}` or `{"parse": text}`.
func editValue(t testing.TB, raw json.RawMessage) Value {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil || len(object) != 1 {
		t.Fatalf("not an edit value: %s", raw)
	}
	for kind, inner := range object {
		var text string
		json.Unmarshal(inner, &text)
		switch kind {
		case "s":
			return ValueFromString(text)
		case "i":
			i, err := strconv.ParseInt(text, 10, 64)
			if err != nil {
				t.Fatal(err)
			}
			return ValueFromInt64(i)
		case "f":
			bits, err := strconv.ParseUint(text, 16, 64)
			if err != nil {
				t.Fatal(err)
			}
			return ValueFromFloat64(math.Float64frombits(bits))
		case "b":
			var b bool
			json.Unmarshal(inner, &b)
			return ValueFromBool(b)
		case "d":
			datetime, err := DatetimeFromStr(text)
			if err != nil {
				t.Fatal(err)
			}
			return ValueFromDatetime(datetime)
		case "a":
			var elements []json.RawMessage
			json.Unmarshal(inner, &elements)
			array := ArrayNew()
			for _, element := range elements {
				array.Push(editValue(t, element))
			}
			return &Value_Array{F0: array}
		case "parse":
			value, err := ValueFromStr(text)
			if err != nil {
				t.Fatal(err)
			}
			return value
		}
	}
	t.Fatalf("not an edit value: %s", raw)
	return nil
}

// pathOf decodes a path: strings are keys, numbers are indexes.
func pathOf(raw json.RawMessage) []any {
	var steps []json.RawMessage
	json.Unmarshal(raw, &steps)
	path := make([]any, 0, len(steps))
	for _, step := range steps {
		var key string
		if json.Unmarshal(step, &key) == nil {
			path = append(path, key)
			continue
		}
		var index int
		json.Unmarshal(step, &index)
		path = append(path, index)
	}
	return path
}

// itemAt walks the path with Item::get_mut, as the oracle does.
func itemAt(document *Document, path []any) (*Item, bool) {
	item := document.AsItemMut()
	for _, step := range path {
		next, ok := ItemGetMut(item, step).Get()
		if !ok {
			return nil, false
		}
		item = next
	}
	return item, true
}

func optionalString(raw json.RawMessage) (string, bool) {
	var text string
	if len(raw) == 0 || json.Unmarshal(raw, &text) != nil || string(raw) == "null" {
		return "", false
	}
	return text, true
}

func optionalBool(raw json.RawMessage) (bool, bool) {
	var value bool
	if len(raw) == 0 || string(raw) == "null" || json.Unmarshal(raw, &value) != nil {
		return false, false
	}
	return value, true
}

func optionalInt(raw json.RawMessage) (int, bool) {
	var value int
	if len(raw) == 0 || string(raw) == "null" || json.Unmarshal(raw, &value) != nil {
		return 0, false
	}
	return value, true
}

func applyDecor(decor *Decor, prefixRaw, suffixRaw json.RawMessage) {
	prefix, hasPrefix := optionalString(prefixRaw)
	suffix, hasSuffix := optionalString(suffixRaw)
	switch {
	case hasPrefix && hasSuffix:
		*decor = DecorNew(prefix, suffix)
	case hasPrefix:
		decor.SetPrefix(prefix)
	case hasSuffix:
		decor.SetSuffix(suffix)
	default:
		decor.Clear()
	}
}

// place is the `place` of mj-core/src/config/document.rs.
func place(item *Item, nextPosition *int) {
	switch i := (*item).(type) {
	case *Item_Table:
		table := &i.F0
		table.SetPosition(*nextPosition)
		*nextPosition++
		for _, child := range table.IterMut() {
			place(child, nextPosition)
		}
	case *Item_ArrayOfTables:
		for table := range i.F0.IterMut() {
			table.SetPosition(*nextPosition)
			*nextPosition++
			for _, child := range table.IterMut() {
				place(child, nextPosition)
			}
		}
	}
}

func editStep(t testing.TB, document *Document, step []json.RawMessage) any {
	var op string
	json.Unmarshal(step[0], &op)
	path := pathOf(step[1])
	arg := func(index int) json.RawMessage {
		if index < len(step) {
			return step[index]
		}
		return nil
	}
	argString := func(index int) string {
		text, _ := optionalString(arg(index))
		return text
	}
	argIndex := func(index int) int {
		value, _ := optionalInt(arg(index))
		return value
	}
	switch op {
	case "index_set":
		value := editValue(t, arg(2))
		item := document.AsItemMut()
		for _, part := range path {
			if ItemGetMut(item, part).IsNone() {
				return "index not found"
			}
			if _, isKey := part.(string); isKey {
				item = ItemIndexMut(item, part)
			} else {
				item = ItemGetMut(item, part).Unwrap()
			}
		}
		*item = ItemValue(value)
		return "set"
	case "replace_keep_decor":
		value := editValue(t, arg(2))
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		existing, ok := (*item).AsValueMut().Get()
		if !ok {
			return "not a value"
		}
		decor := (*existing).Decor().Clone()
		*value.DecorMut() = decor
		*existing = value
		return "replaced"
	case "replace":
		value := editValue(t, arg(2))
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		*item = &Item_Value{F0: value}
		return "replaced"
	case "insert_from":
		key := argString(2)
		source, err := DocumentFromStr(argString(3))
		if err != nil {
			t.Fatal(err)
		}
		sourceItem := source.AsItem()
		for _, part := range pathOf(arg(4)) {
			next, ok := sourceItem.Get(part).Get()
			if !ok {
				t.Fatalf("no such source item")
			}
			sourceItem = next
		}
		inserted := sourceItem.Clone()
		if position, ok := optionalInt(arg(5)); ok {
			next := position
			place(&inserted, &next)
		}
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLikeMut().Get()
		if !ok {
			return "not a table"
		}
		previous := table.Insert(key, inserted)
		return previous.IsSome()
	case "insert":
		key := argString(2)
		value := editValue(t, arg(3))
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLikeMut().Get()
		if !ok {
			return "not a table"
		}
		previous := table.Insert(key, ItemValue(value))
		return previous.IsSome()
	case "insert_table":
		key := argString(2)
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableMut().Get()
		if !ok {
			return "not a table"
		}
		newTable := TableNew()
		if position, ok := optionalInt(arg(3)); ok {
			newTable.SetPosition(position)
		}
		if implicit, ok := optionalBool(arg(4)); ok {
			newTable.SetImplicit(implicit)
		}
		previous := table.Insert(key, &Item_Table{F0: newTable})
		return previous.IsSome()
	case "remove":
		key := argString(2)
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLikeMut().Get()
		if !ok {
			return "not a table"
		}
		if removed, ok := table.Remove(key).Get(); ok {
			return removed.TypeName()
		}
		return nil
	case "keys":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLike().Get()
		if !ok {
			return "not a table"
		}
		keys := []any{}
		for key, child := range table.Iter() {
			keys = append(keys, []any{key, child.TypeName()})
		}
		return keys
	case "get":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		var integer, boolean any
		if i, ok := (*item).AsInteger().Get(); ok {
			integer = i
		}
		if b, ok := (*item).AsBool().Get(); ok {
			boolean = b
		}
		return map[string]any{
			"type":    (*item).TypeName(),
			"str":     optionString((*item).AsStr()),
			"integer": integer,
			"bool":    boolean,
			"display": (*item).String(),
		}
	case "set_position":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableMut().Get()
		if !ok {
			return "not a table"
		}
		table.SetPosition(argIndex(2))
		return "positioned"
	case "set_implicit":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableMut().Get()
		if !ok {
			return "not a table"
		}
		implicit, _ := optionalBool(arg(2))
		table.SetImplicit(implicit)
		return "done"
	case "set_dotted":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLikeMut().Get()
		if !ok {
			return "not a table"
		}
		dotted, _ := optionalBool(arg(2))
		table.SetDotted(dotted)
		return "done"
	case "fmt":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		switch i := (*item).(type) {
		case *Item_Table:
			i.F0.Fmt()
		case *Item_Value:
			switch v := i.F0.(type) {
			case *Value_InlineTable:
				v.F0.Fmt()
			case *Value_Array:
				v.F0.Fmt()
			default:
				return "not formattable"
			}
		default:
			return "not formattable"
		}
		return "formatted"
	case "sort_values":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLikeMut().Get()
		if !ok {
			return "not a table"
		}
		table.SortValues()
		return "sorted"
	case "decor":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		var decor *Decor
		switch i := (*item).(type) {
		case *Item_Value:
			decor = i.F0.DecorMut()
		case *Item_Table:
			decor = i.F0.DecorMut()
		default:
			return "no decor"
		}
		applyDecor(decor, arg(2), arg(3))
		return "decorated"
	case "key_decor":
		key := argString(2)
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		table, ok := (*item).AsTableLikeMut().Get()
		if !ok {
			return "not a table"
		}
		decor, ok := table.KeyDecorMut(key).Get()
		if !ok {
			return "no key"
		}
		applyDecor(decor, arg(3), arg(4))
		return "decorated"
	case "array_push", "array_push_formatted":
		value := editValue(t, arg(2))
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayMut().Get()
		if !ok {
			return "not an array"
		}
		if op == "array_push" {
			array.Push(value)
		} else {
			array.PushFormatted(value)
		}
		return array.Len()
	case "array_insert":
		index := argIndex(2)
		value := editValue(t, arg(3))
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayMut().Get()
		if !ok {
			return "not an array"
		}
		if index > array.Len() {
			return "out of bounds"
		}
		array.Insert(index, value)
		return array.Len()
	case "array_replace":
		index := argIndex(2)
		value := editValue(t, arg(3))
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayMut().Get()
		if !ok {
			return "not an array"
		}
		if index >= array.Len() {
			return "out of bounds"
		}
		old := array.Replace(index, value)
		return old.String()
	case "array_remove":
		index := argIndex(2)
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayMut().Get()
		if !ok {
			return "not an array"
		}
		if index >= array.Len() {
			return "out of bounds"
		}
		old := array.Remove(index)
		return old.String()
	case "array_trailing":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayMut().Get()
		if !ok {
			return "not an array"
		}
		if trailing, ok := optionalString(arg(2)); ok {
			array.SetTrailing(trailing)
		}
		if comma, ok := optionalBool(arg(3)); ok {
			array.SetTrailingComma(comma)
		}
		return "done"
	case "aot_push":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayOfTablesMut().Get()
		if !ok {
			return "not an array of tables"
		}
		table := TableNew()
		for _, pair := range orderedObject(t, arg(2)) {
			table.Insert(pair.key, ItemValue(editValue(t, pair.value)))
		}
		array.Push(table)
		return array.Len()
	case "aot_remove":
		index := argIndex(2)
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		array, ok := (*item).AsArrayOfTablesMut().Get()
		if !ok {
			return "not an array of tables"
		}
		if index >= array.Len() {
			return "out of bounds"
		}
		array.Remove(index)
		return array.Len()
	case "make_value":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		ItemMakeValue(item)
		return (*item).TypeName()
	case "into_table":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		taken := *item
		*item = ItemDefault()
		if table, err := taken.IntoTable(); err == nil {
			*item = &Item_Table{F0: table}
		} else {
			*item = err.(*ItemError).Item
		}
		return (*item).TypeName()
	case "into_array_of_tables":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		taken := *item
		*item = ItemDefault()
		if array, err := taken.IntoArrayOfTables(); err == nil {
			*item = &Item_ArrayOfTables{F0: array}
		} else {
			*item = err.(*ItemError).Item
		}
		return (*item).TypeName()
	case "set_trailing":
		document.SetTrailing(argString(2))
		return "done"
	case "display":
		item, ok := itemAt(document, path)
		if !ok {
			return "missing"
		}
		return (*item).String()
	}
	t.Fatalf("unknown step %s", op)
	return nil
}

type orderedPair struct {
	key   string
	value json.RawMessage
}

// orderedObject decodes a JSON object keeping the order of its members,
// which is the order serde_json (with preserve_order) gives the oracle.
func orderedObject(t testing.TB, raw json.RawMessage) []orderedPair {
	if len(raw) == 0 || string(raw) == "null" {
		return nil
	}
	decoder := json.NewDecoder(bytes.NewReader(raw))
	token, err := decoder.Token()
	if err != nil || token != json.Delim('{') {
		t.Fatalf("not an object: %s", raw)
	}
	var pairs []orderedPair
	for decoder.More() {
		keyToken, err := decoder.Token()
		if err != nil {
			t.Fatal(err)
		}
		var value json.RawMessage
		if err := decoder.Decode(&value); err != nil {
			t.Fatal(err)
		}
		pairs = append(pairs, orderedPair{key: keyToken.(string), value: value})
	}
	return pairs
}

func answerEditScript(t testing.TB, request json.RawMessage) any {
	var decoded struct {
		Text  string              `json:"text"`
		Steps [][]json.RawMessage `json:"steps"`
	}
	if err := json.Unmarshal(request, &decoded); err != nil {
		t.Fatal(err)
	}
	document, err := DocumentFromStr(decoded.Text)
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	log := []any{}
	for _, step := range decoded.Steps {
		log = append(log, editStep(t, &document, step))
	}
	return map[string]any{"ok": document.String(), "log": log}
}

// editDocuments are the documents the scripts edit, each with the paths of
// its tables, arrays and values, so that most steps name something that is
// there. They share a small vocabulary of keys, so that a path made up at
// random names something now and then too.
var editDocuments = []editDocument{
	{"", nil},
	{"a = 1\n", [][]any{{"a"}}},
	{"# top\na = 1 # one\nb = \"two\"   # two\n\n# about c\nc = [1, 2, 3]\n",
		[][]any{{"a"}, {"b"}, {"c"}, {"c", 0}, {"c", 2}}},
	{"a = 1\n\n[t]\nb = 2\nc = 3\n\n[u]\nd = 4\n",
		[][]any{{"a"}, {"t"}, {"t", "b"}, {"t", "c"}, {"u"}, {"u", "d"}}},
	{"# header\n\n[t] # t\n# about a\na = 1\n\n  [t.u]  # nested\n  b = { c = 1, d = [1, 2] } # inline\n\n[v]\n",
		[][]any{{"t"}, {"t", "a"}, {"t", "u"}, {"t", "u", "b"}, {"t", "u", "b", "c"}, {"t", "u", "b", "d"}, {"t", "u", "b", "d", 1}, {"v"}}},
	{"a = 1\n[[aot]]\nb = 1\n[[aot]]\nb = 2\n# between\n[[aot]]\nb = 3\n[t]\nc = 1\n",
		[][]any{{"a"}, {"aot"}, {"aot", 0}, {"aot", 1}, {"aot", 2}, {"aot", 1, "b"}, {"t"}, {"t", "c"}}},
	{"a.b.c = 1\na.b.d = 2\nt.u = { v = 1 }\n",
		[][]any{{"a"}, {"a", "b"}, {"a", "b", "c"}, {"t"}, {"t", "u"}, {"t", "u", "v"}}},
	{"arr = [\n  1, # one\n  2,\n  3, # three\n]\ninl = { a = 1, b = \"x\" }\nt = {}\n",
		[][]any{{"arr"}, {"arr", 0}, {"arr", 1}, {"inl"}, {"inl", "a"}, {"inl", "b"}, {"t"}}},
	{"a = 1\r\nb = 2\r\n[t]\r\nc = 3\r\n",
		[][]any{{"a"}, {"b"}, {"t"}, {"t", "c"}}},
	{"[t.u.v]\na = 1\n[t]\nb = 2\n[w]\n[t.u]\nc = 3\n",
		[][]any{{"t"}, {"t", "u"}, {"t", "u", "v"}, {"t", "u", "v", "a"}, {"t", "b"}, {"t", "u", "c"}}},
	{"a = \"str\"\nb = 1.5\nc = true\nd = 1979-05-27T07:32:00Z\narr = [\"x\", \"y\"]\n\n[t]\n\n[u]\na = 1\n",
		[][]any{{"a"}, {"b"}, {"c"}, {"d"}, {"arr"}, {"arr", 1}, {"t"}, {"u"}, {"u", "a"}}},
	{"   a   =   1   \n\tb\t=\t2\t# tab\n\n\n\n[ t ]\n  c = 3\n",
		[][]any{{"a"}, {"b"}, {"t"}, {"t", "c"}}},
	{"arr = [ [1, 2], [\"a\"], { k = [true] } ]\nempty = []\nnested = { a = { b = { c = [] } } }\n[[aot]]\n[[aot.inner]]\nx = 1\n[[aot.inner]]\nx = 2\n[[aot]]\n",
		[][]any{{"arr"}, {"arr", 0}, {"arr", 2}, {"arr", 2, "k"}, {"empty"}, {"nested"}, {"nested", "a"}, {"nested", "a", "b"},
			{"nested", "a", "b", "c"}, {"aot"}, {"aot", 0}, {"aot", 0, "inner"}, {"aot", 0, "inner", 1}, {"aot", 1}}},
}

type editDocument struct {
	text  string
	paths [][]any
}

// arrays returns the known paths that name an array or an array of tables:
// the ones another known path extends with an index.
func (d *editDocument) arrays() [][]any {
	var arrays [][]any
	seen := map[string]bool{}
	for _, path := range d.paths {
		if len(path) == 0 {
			continue
		}
		if _, isIndex := path[len(path)-1].(int); !isIndex {
			continue
		}
		parent := path[:len(path)-1]
		encoded, _ := json.Marshal(parent)
		if !seen[string(encoded)] {
			seen[string(encoded)] = true
			arrays = append(arrays, parent)
		}
	}
	return arrays
}

// editSources are the documents items are copied from by insert_from, with
// the paths of the items worth copying.
var editSources = []struct {
	text  string
	paths [][]any
}{
	{"x = 1\ny = \"s\" # c\n[t]\na = 1\nb = [1, 2]\n[t.n]\nc = 2\n[[aot]]\nd = 1\n[[aot]]\nd = 2\n",
		[][]any{{"x"}, {"y"}, {"t"}, {"t", "a"}, {"t", "b"}, {"t", "n"}, {"aot"}, {"aot", 0}}},
	{"inl = { a = 1, b = { c = 2 } }\narr = [1, [2, 3], { k = 1 }]\ndot.ted.key = 1\n",
		[][]any{{"inl"}, {"inl", "b"}, {"arr"}, {"arr", 1}, {"arr", 2}, {"dot"}, {"dot", "ted"}}},
}

var editKeys = []string{"a", "b", "c", "d", "t", "u", "v", "arr", "aot", "inl", "new", "new key", "é", ""}

func randomEditValue(random *rand.Rand, depth int) any {
	switch random.Intn(9) {
	case 0:
		return map[string]any{"s": reprStrings[random.Intn(len(reprStrings))]}
	case 1:
		return map[string]any{"i": strconv.FormatInt(random.Int63n(2000)-1000, 10)}
	case 2:
		floats := []float64{0, 1.5, -2.25, 1e10, 0.1, math.Inf(1), math.NaN()}
		return map[string]any{"f": floatBits(floats[random.Intn(len(floats))])}
	case 3:
		return map[string]any{"b": random.Intn(2) == 0}
	case 4:
		datetimes := []string{"1979-05-27T07:32:00Z", "1979-05-27", "07:32:00.5", "2000-01-01T00:00:00+05:30"}
		return map[string]any{"d": datetimes[random.Intn(len(datetimes))]}
	case 5:
		if depth > 1 {
			return map[string]any{"s": "deep"}
		}
		elements := []any{}
		for range random.Intn(4) {
			elements = append(elements, randomEditValue(random, depth+1))
		}
		return map[string]any{"a": elements}
	case 6:
		parsed := []string{"1", "2", "\"s\"", "[1, 2]", "[ 1 , 2 , ]", "{ a = 1 }", "{a=1,b=2}", "'lit'", "[\n  1,\n  2,\n]", "{}", "[]", "0x10", "1.0e3"}
		return map[string]any{"parse": parsed[random.Intn(len(parsed))]}
	default:
		return map[string]any{"s": editKeys[random.Intn(len(editKeys))]}
	}
}

// randomEditPath returns a path into a document: mostly one of the known
// ones, sometimes with a step added or dropped, sometimes made up.
func randomEditPath(random *rand.Rand, known [][]any) []any {
	if len(known) > 0 && random.Intn(10) < 7 {
		path := append([]any{}, known[random.Intn(len(known))]...)
		switch random.Intn(8) {
		case 0:
			path = append(path, editKeys[random.Intn(10)])
		case 1:
			path = append(path, random.Intn(3))
		case 2:
			if len(path) > 0 {
				path = path[:len(path)-1]
			}
		}
		return path
	}
	path := []any{}
	for range random.Intn(4) {
		if random.Intn(6) == 0 {
			path = append(path, random.Intn(3))
		} else {
			path = append(path, editKeys[random.Intn(10)])
		}
	}
	return path
}

func randomDecorText(random *rand.Rand) any {
	texts := []any{nil, "", " ", "  ", "\n", "# comment\n", "\n# comment\n", " # trailing", "\t"}
	return texts[random.Intn(len(texts))]
}

func randomEditStep(random *rand.Rand, document *editDocument) []any {
	path := randomEditPath(random, document.paths)
	key := editKeys[random.Intn(len(editKeys))]
	choice := random.Intn(34)
	if arrays := document.arrays(); choice >= 23 && choice <= 26 || choice >= 30 {
		// The array steps are tried on the arrays, mostly.
		if choice >= 30 {
			choice = 23 + choice%4
		}
		if len(arrays) > 0 && random.Intn(5) != 0 {
			path = arrays[random.Intn(len(arrays))]
		}
	}
	switch choice {
	case 0, 1, 2:
		if len(path) == 0 {
			path = []any{key}
		}
		return []any{"index_set", path, randomEditValue(random, 0)}
	case 3, 4:
		return []any{"replace_keep_decor", path, randomEditValue(random, 0)}
	case 5:
		return []any{"replace", path, randomEditValue(random, 0)}
	case 6, 7, 8:
		source := editSources[random.Intn(len(editSources))]
		sourcePath := source.paths[random.Intn(len(source.paths))]
		var position any
		if random.Intn(2) == 0 {
			position = random.Intn(40)
		}
		return []any{"insert_from", path, key, source.text, sourcePath, position}
	case 9, 10, 11:
		return []any{"insert", path, key, randomEditValue(random, 0)}
	case 12:
		var position, implicit any
		if random.Intn(2) == 0 {
			position = random.Intn(40)
		}
		if random.Intn(3) == 0 {
			implicit = random.Intn(2) == 0
		}
		return []any{"insert_table", path, key, position, implicit}
	case 13, 14, 15:
		return []any{"remove", path, key}
	case 16:
		return []any{"keys", path}
	case 17:
		return []any{"get", path}
	case 18:
		return []any{"set_position", path, random.Intn(40)}
	case 19:
		if random.Intn(2) == 0 {
			return []any{"set_implicit", path, random.Intn(2) == 0}
		}
		return []any{"set_dotted", path, random.Intn(2) == 0}
	case 20:
		if random.Intn(2) == 0 {
			return []any{"fmt", path}
		}
		return []any{"sort_values", path}
	case 21:
		return []any{"decor", path, randomDecorText(random), randomDecorText(random)}
	case 22:
		return []any{"key_decor", path, key, randomDecorText(random), randomDecorText(random)}
	case 23:
		if random.Intn(2) == 0 {
			return []any{"array_push", path, randomEditValue(random, 0)}
		}
		return []any{"array_push_formatted", path, randomEditValue(random, 0)}
	case 24:
		if random.Intn(2) == 0 {
			return []any{"array_insert", path, random.Intn(4), randomEditValue(random, 0)}
		}
		return []any{"array_replace", path, random.Intn(4), randomEditValue(random, 0)}
	case 25:
		if random.Intn(2) == 0 {
			return []any{"array_remove", path, random.Intn(4)}
		}
		var trailing, comma any
		if random.Intn(2) == 0 {
			trailing = []string{"", " ", "\n", " # c\n"}[random.Intn(4)]
		}
		if random.Intn(2) == 0 {
			comma = random.Intn(2) == 0
		}
		return []any{"array_trailing", path, trailing, comma}
	case 26:
		if random.Intn(2) == 0 {
			return []any{"aot_push", path, map[string]any{"k": randomEditValue(random, 0)}}
		}
		return []any{"aot_remove", path, random.Intn(3)}
	case 27:
		switch random.Intn(3) {
		case 0:
			return []any{"make_value", path}
		case 1:
			return []any{"into_table", path}
		default:
			return []any{"into_array_of_tables", path}
		}
	case 28:
		return []any{"set_trailing", []any{}, []string{"", "\n", "# end\n", "  "}[random.Intn(4)]}
	default:
		return []any{"display", path}
	}
}

func TestDiffAgainstRust_edit_script(t *testing.T) {
	oracle.Check(t, "toml", "edit_script", func(yield func(any)) {
		// What Mjolnir itself does (mj-worker-staging catalog.rs): replace a
		// top-level value keeping its decor, or add the key.
		for _, text := range []string{
			"",
			"model = \"x\"\n",
			"# mine\nmodel_catalog_json = \"own.json\" # keep\nmodel = \"x\"\n",
			"model = \"x\"\n\n[profiles.a]\nmodel = \"y\"\n",
			"model_catalog_json = { not = \"a string\" }\n",
			"[model_catalog_json]\nx = 1\n",
		} {
			yield(map[string]any{"op": "edit_script", "text": text, "steps": []any{
				[]any{"replace_keep_decor", []any{"model_catalog_json"}, map[string]any{"s": "catalog.json"}},
			}})
			yield(map[string]any{"op": "edit_script", "text": text, "steps": []any{
				[]any{"index_set", []any{"model_catalog_json"}, map[string]any{"s": "catalog.json"}},
			}})
		}
		random := rand.New(rand.NewSource(20231003))
		for range 1500 * stressRounds() {
			document := editDocuments[random.Intn(len(editDocuments))]
			steps := []any{}
			for range 1 + random.Intn(6) {
				steps = append(steps, randomEditStep(random, &document))
			}
			yield(map[string]any{"op": "edit_script", "text": document.text, "steps": steps})
		}
	}, func(request json.RawMessage) any {
		return oracle.Catch(func() any { return answerEditScript(t, request) })
	})
}

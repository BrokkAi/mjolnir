package tomledit

import (
	"encoding/json"
	"errors"
	"math"
	"math/rand"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests ask the oracle module `toml` (go/port/oracle/src/
// toml_oracle.rs) what toml_edit 0.20.2 does with an input and compare.

type oracleRequest struct {
	Op    string          `json:"op"`
	Text  string          `json:"text"`
	Key   string          `json:"key"`
	Value json.RawMessage `json:"value"`
}

func decodeRequest(t testing.TB, request json.RawMessage) oracleRequest {
	var decoded oracleRequest
	if err := json.Unmarshal(request, &decoded); err != nil {
		t.Fatalf("request does not decode: %v", err)
	}
	return decoded
}

// optionString encodes an `Option<&str>` as serde_json does: null or the
// string.
func optionString(option rs.Option[string]) any {
	if text, ok := option.Get(); ok {
		return text
	}
	return nil
}

func dumpRawString(raw rs.Option[*RawString]) any {
	if value, ok := raw.Get(); ok {
		return optionString(value.AsStr())
	}
	return nil
}

func dumpDecor(decor *Decor) any {
	return []any{dumpRawString(decor.Prefix()), dumpRawString(decor.Suffix())}
}

func dumpRepr(repr rs.Option[*Repr]) any {
	if value, ok := repr.Get(); ok {
		return optionString(value.AsRaw().AsStr())
	}
	return nil
}

func dumpKey(key *Key) any {
	return map[string]any{
		"key":   key.Get(),
		"repr":  dumpRepr(key.AsRepr()),
		"decor": dumpDecor(key.Decor()),
	}
}

func floatBits(f float64) string {
	text := strconv.FormatUint(math.Float64bits(f), 16)
	for len(text) < 16 {
		text = "0" + text
	}
	return text
}

func dumpValue(value Value) any {
	switch v := value.(type) {
	case *Value_String:
		return map[string]any{"kind": "string", "value": v.F0.Value(), "repr": dumpRepr(v.F0.AsRepr()), "decor": dumpDecor(v.F0.Decor())}
	case *Value_Integer:
		return map[string]any{"kind": "integer", "value": strconv.FormatInt(v.F0.Value(), 10), "repr": dumpRepr(v.F0.AsRepr()), "decor": dumpDecor(v.F0.Decor())}
	case *Value_Float:
		return map[string]any{"kind": "float", "value": floatBits(v.F0.Value()), "repr": dumpRepr(v.F0.AsRepr()), "decor": dumpDecor(v.F0.Decor())}
	case *Value_Boolean:
		return map[string]any{"kind": "boolean", "value": v.F0.Value(), "repr": dumpRepr(v.F0.AsRepr()), "decor": dumpDecor(v.F0.Decor())}
	case *Value_Datetime:
		return map[string]any{"kind": "datetime", "value": v.F0.Value().String(), "repr": dumpRepr(v.F0.AsRepr()), "decor": dumpDecor(v.F0.Decor())}
	case *Value_Array:
		values := []any{}
		for element := range v.F0.Iter() {
			values = append(values, dumpValue(element))
		}
		return map[string]any{
			"kind":           "array",
			"decor":          dumpDecor(v.F0.Decor()),
			"trailing":       optionString(v.F0.Trailing().AsStr()),
			"trailing_comma": v.F0.TrailingComma(),
			"values":         values,
		}
	case *Value_InlineTable:
		items := []any{}
		for key, element := range v.F0.Iter() {
			full := v.F0.GetKeyValue(key).Expect("key from iter").F0
			items = append(items, []any{dumpKey(full), dumpValue(element)})
		}
		return map[string]any{
			"kind":     "inline table",
			"decor":    dumpDecor(v.F0.Decor()),
			"preamble": optionString(v.F0.Preamble().AsStr()),
			"dotted":   v.F0.IsDotted(),
			"items":    items,
		}
	default:
		panic(rs.Unreachable(value))
	}
}

func dumpItem(item Item) any {
	switch i := item.(type) {
	case *Item_None:
		return map[string]any{"kind": "none"}
	case *Item_Value:
		return dumpValue(i.F0)
	case *Item_Table:
		return dumpTable(&i.F0)
	case *Item_ArrayOfTables:
		tables := []any{}
		for table := range i.F0.Iter() {
			tables = append(tables, dumpTable(table))
		}
		return map[string]any{"kind": "array of tables", "tables": tables}
	default:
		panic(rs.Unreachable(item))
	}
}

func dumpTable(table *Table) any {
	items := []any{}
	for key, item := range table.Iter() {
		full := table.GetKeyValue(key).Expect("key from iter").F0
		items = append(items, []any{dumpKey(full), dumpItem(item)})
	}
	var position any
	if p, ok := table.Position().Get(); ok {
		position = p
	}
	return map[string]any{
		"kind":     "table",
		"decor":    dumpDecor(table.Decor()),
		"implicit": table.IsImplicit(),
		"dotted":   table.IsDotted(),
		"position": position,
		"items":    items,
	}
}

func tomlErrorReply(err error) any {
	var tomlError *TomlError
	if !errors.As(err, &tomlError) {
		return map[string]any{"err": err.Error()}
	}
	var span any
	if s, ok := tomlError.Span().Get(); ok {
		span = []int{s.Start, s.End}
	}
	return map[string]any{"err": tomlError.Error(), "message": tomlError.Message(), "span": span}
}

func answerEditParse(text string, dump bool) any {
	document, err := DocumentFromStr(text)
	if err != nil {
		return tomlErrorReply(err)
	}
	if !dump {
		return map[string]any{"ok": document.String()}
	}
	return map[string]any{
		"ok":       document.String(),
		"dump":     dumpTable(document.AsTable()),
		"trailing": optionString(document.Trailing().AsStr()),
	}
}

func TestDiffAgainstRust_edit_parse(t *testing.T) {
	documents := corpusDocuments(t)
	oracle.Check(t, "toml", "edit_parse", func(yield func(any)) {
		for _, text := range documents {
			yield(map[string]any{"op": "edit_parse", "text": text})
		}
	}, func(request json.RawMessage) any {
		return oracle.Catch(func() any { return answerEditParse(decodeRequest(t, request).Text, false) })
	})
}

func TestDiffAgainstRust_edit_dump(t *testing.T) {
	oracle.Check(t, "toml", "edit_dump", func(yield func(any)) {
		for _, c := range loadTomlTest(t) {
			if c.valid() && len(c.Toml) < 3000 {
				yield(map[string]any{"op": "edit_dump", "text": c.Toml})
			}
		}
		for _, text := range handwrittenDocuments {
			yield(map[string]any{"op": "edit_dump", "text": text})
		}
	}, func(request json.RawMessage) any {
		return oracle.Catch(func() any { return answerEditParse(decodeRequest(t, request).Text, true) })
	})
}

// snippets are keys, dotted keys and values on their own, valid and not.
var snippets = []string{
	"", " ", "a", "a ", " a", "a.b", "a . b", "a.b.c", "a.", ".a", "a..b", "\"a\"", "'a'", "\"a\".b",
	"\"a b\"", "'a.b'", "\"\"", "''", "\"\\n\"", "\"\\q\"", "\"a", "'a", "a b", "a=b", "é", "\"é\"", "a.é",
	"1", "-1", "1.5", "0x1F", "true", "false", "inf", "nan", "\"s\"", "'s'", "\"\"\"m\"\"\"", "'''m'''",
	"[1, 2]", "[ 1 , 2 , ]", "[]", "{ a = 1 }", "{}", "{ a = 1, b.c = 2 }", "1979-05-27", "07:32:00",
	"1979-05-27T07:32:00Z", " 1", "1 ", " 1 ", "1 # c", "# c\n1", "1\n", "[1,\n 2]", "[ # c\n 1 ]",
	"x", "1 2", "[", "{", "\"", "'", "1_", "0x", "1979-13-01", "{ a = 1, a = 2 }", "tru", "+", "++1",
	"a\tb", "\t1\t", "\"a\\tb\"", "\"\\u00e9\"", "'''\na\n'''", "\"\"\"\na\\\n  b\"\"\"",
}

func TestDiffAgainstRust_edit_snippets(t *testing.T) {
	oracle.Check(t, "toml", "edit_snippets", func(yield func(any)) {
		for _, text := range snippets {
			yield(map[string]any{"op": "edit_key", "text": text})
			yield(map[string]any{"op": "edit_key_path", "text": text})
			yield(map[string]any{"op": "edit_value", "text": text})
		}
		for _, depth := range []int{126, 127, 128, 129} {
			yield(map[string]any{"op": "edit_key_path", "text": strings.Repeat("k.", depth-1) + "k"})
			yield(map[string]any{"op": "edit_value", "text": strings.Repeat("[", depth) + strings.Repeat("]", depth)})
		}
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		return oracle.Catch(func() any {
			switch r.Op {
			case "edit_key":
				key, err := KeyFromStr(r.Text)
				if err != nil {
					return map[string]any{"err": err.Error()}
				}
				return map[string]any{"ok": map[string]any{"key": key.Get(), "display": key.String()}}
			case "edit_key_path":
				keys, err := KeyParse(r.Text)
				if err != nil {
					return map[string]any{"err": err.Error()}
				}
				dumped := []any{}
				for index := range keys {
					dumped = append(dumped, dumpKey(&keys[index]))
				}
				return map[string]any{"ok": dumped}
			default:
				value, err := ValueFromStr(r.Text)
				if err != nil {
					return map[string]any{"err": err.Error()}
				}
				return map[string]any{"ok": map[string]any{"display": value.String(), "dump": dumpValue(value)}}
			}
		})
	})
}

// reprStrings are strings whose default representation exercises each
// choice toml_edit makes: basic or literal, one line or several, and what is
// escaped.
var reprStrings = []string{
	"", "a", "a b", "é", "😀", "tab\there", "new\nline", "cr\rhere", "crlf\r\nhere", "back\\slash",
	"C:\\Users\\x", "C:\\Users\\x\nnext", "quote\"d", "apostrophe's", "it's a \\path", "'''", "''", "'",
	"a''b\\", "a'''b\\", "ends with '", "ends with \\'", "\\", "\\\\", "\"\"\"", "\x00", "\x01", "\x08",
	"\x0c", "\x1f", "\x7f", "\u0080", "\u2028", "bell\x07\\", "path\\with\ttab", "multi\nline\\path",
	"multi\nline's\\path", "'leading", "trailing\n", "\n", "\n\n", " leading space", "#", "a=b", "a.b",
	"\\'", "'\\", "x\\'y'", "x\\''y", "x\\'''y",
}

// reprKeys are keys whose default representation is bare, or quoted.
var reprKeys = []string{
	"", "a", "A-Z_0-9", "a b", "a.b", "é", "a\"b", "a'b", "a\\b", "a\nb", "a\tb", "1", "-", "_", " ", "😀", "a\x00b",
}

func reprFloats() []float64 {
	floats := []float64{
		0, math.Copysign(0, -1), 1, -1, 0.5, 1.5, -2.5, 0.1, 1e15, 1e16, 1e17, 1e21, 1e22, 1e100, 1e300,
		1e-5, 1e-7, 1e-100, 1.7976931348623157e308, 5e-324, 2.2250738585072014e-308, 123456789.123456789,
		0.30000000000000004, 1.0000000000000002, 9007199254740993, 3.141592653589793,
		math.Inf(1), math.Inf(-1), math.NaN(), math.Float64frombits(0xfff8000000000000),
		float64(float32(0.1)), 100, 1e2, 12345678901234567890,
	}
	random := rand.New(rand.NewSource(7))
	for range 150 {
		floats = append(floats, math.Float64frombits(random.Uint64()))
	}
	for range 100 {
		floats = append(floats, (random.Float64()-0.5)*math.Pow(10, float64(random.Intn(40)-20)))
	}
	return floats
}

func TestDiffAgainstRust_edit_repr(t *testing.T) {
	oracle.Check(t, "toml", "edit_repr", func(yield func(any)) {
		for index, s := range reprStrings {
			yield(map[string]any{"op": "edit_repr", "value": map[string]any{"s": s}, "key": reprKeys[index%len(reprKeys)]})
		}
		for index, s := range reprKeys {
			yield(map[string]any{"op": "edit_repr", "value": map[string]any{"s": s}, "key": reprStrings[index%len(reprStrings)]})
		}
		for _, f := range reprFloats() {
			yield(map[string]any{"op": "edit_repr", "value": map[string]any{"f": floatBits(f)}, "key": "f"})
		}
		for _, i := range []int64{0, 1, -1, 42, math.MaxInt64, math.MinInt64} {
			yield(map[string]any{"op": "edit_repr", "value": map[string]any{"i": strconv.FormatInt(i, 10)}, "key": "i"})
		}
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		var inner map[string]string
		if err := json.Unmarshal(r.Value, &inner); err != nil {
			t.Fatal(err)
		}
		return oracle.Catch(func() any {
			var value Value
			switch {
			case hasKey(inner, "s"):
				value = ValueFromString(inner["s"])
			case hasKey(inner, "f"):
				bits, _ := strconv.ParseUint(inner["f"], 16, 64)
				value = ValueFromFloat64(math.Float64frombits(bits))
			default:
				i, _ := strconv.ParseInt(inner["i"], 10, 64)
				value = ValueFromInt64(i)
			}
			key := KeyNew(r.Key)
			return map[string]any{"value": value.String(), "key": key.String()}
		})
	})
}

func hasKey(m map[string]string, key string) bool {
	_, ok := m[key]
	return ok
}

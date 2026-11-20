package toml

import (
	"bufio"
	"compress/gzip"
	"encoding/base64"
	"encoding/json"
	"errors"
	"math"
	"math/rand"
	"os"
	"sort"
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests ask the oracle module `toml` (go/port/oracle/src/
// toml_oracle.rs) what toml 0.8.2 does with an input and compare.
//
// A toml::Value crosses the boundary in a form that keeps its type: one
// object with a single member, `{"s": string}`, `{"i": "decimal"}`,
// `{"f": "bits in hex"}`, `{"b": bool}`, `{"d": "datetime"}`, `{"a": [..]}`
// or `{"t": {..}}`.

type oracleRequest struct {
	Op       string          `json:"op"`
	Text     string          `json:"text"`
	Value    json.RawMessage `json:"value"`
	Existing string          `json:"existing"`
	Loaded   string          `json:"loaded"`
	Updated  string          `json:"updated"`
}

func decodeRequest(t testing.TB, request json.RawMessage) oracleRequest {
	t.Helper()
	var decoded oracleRequest
	if err := json.Unmarshal(request, &decoded); err != nil {
		t.Fatalf("request does not decode: %v", err)
	}
	return decoded
}

func floatBits(f float64) string {
	text := strconv.FormatUint(math.Float64bits(f), 16)
	for len(text) < 16 {
		text = "0" + text
	}
	return text
}

// toCanonical is the oracle's `to_canonical`.
func toCanonical(value Value) any {
	switch v := value.(type) {
	case *Value_String:
		return map[string]any{"s": v.F0}
	case *Value_Integer:
		return map[string]any{"i": strconv.FormatInt(v.F0, 10)}
	case *Value_Float:
		return map[string]any{"f": floatBits(v.F0)}
	case *Value_Boolean:
		return map[string]any{"b": v.F0}
	case *Value_Datetime:
		return map[string]any{"d": v.F0.String()}
	case *Value_Array:
		elements := []any{}
		for _, element := range v.F0 {
			elements = append(elements, toCanonical(element))
		}
		return map[string]any{"a": elements}
	case *Value_Table:
		entries := map[string]any{}
		for key, element := range v.F0.Iter() {
			entries[key] = toCanonical(element)
		}
		return map[string]any{"t": entries}
	default:
		panic(rs.Unreachable(value))
	}
}

// fromCanonical is the oracle's `from_canonical`.
func fromCanonical(t testing.TB, raw json.RawMessage) Value {
	t.Helper()
	var outer map[string]json.RawMessage
	if err := json.Unmarshal(raw, &outer); err != nil || len(outer) != 1 {
		t.Fatalf("not a canonical value: %s", raw)
	}
	for kind, inner := range outer {
		text := func() string {
			var s string
			if err := json.Unmarshal(inner, &s); err != nil {
				t.Fatalf("not a canonical value: %s", raw)
			}
			return s
		}
		switch kind {
		case "s":
			return &Value_String{F0: text()}
		case "i":
			i, err := strconv.ParseInt(text(), 10, 64)
			if err != nil {
				t.Fatalf("bad integer in %s", raw)
			}
			return &Value_Integer{F0: i}
		case "f":
			bits, err := strconv.ParseUint(text(), 16, 64)
			if err != nil {
				t.Fatalf("bad float in %s", raw)
			}
			return &Value_Float{F0: math.Float64frombits(bits)}
		case "b":
			var b bool
			if err := json.Unmarshal(inner, &b); err != nil {
				t.Fatalf("bad bool in %s", raw)
			}
			return &Value_Boolean{F0: b}
		case "d":
			d, err := tomledit.DatetimeFromStr(text())
			if err != nil {
				t.Fatalf("bad datetime in %s", raw)
			}
			return &Value_Datetime{F0: d}
		case "a":
			var elements []json.RawMessage
			if err := json.Unmarshal(inner, &elements); err != nil {
				t.Fatalf("bad array in %s", raw)
			}
			array := []Value{}
			for _, element := range elements {
				array = append(array, fromCanonical(t, element))
			}
			return &Value_Array{F0: array}
		case "t":
			var entries map[string]json.RawMessage
			if err := json.Unmarshal(inner, &entries); err != nil {
				t.Fatalf("bad table in %s", raw)
			}
			table := TableNew()
			for key, element := range entries {
				table.Insert(key, fromCanonical(t, element))
			}
			return &Value_Table{F0: table}
		}
	}
	t.Fatalf("not a canonical value: %s", raw)
	return nil
}

// deErrorReply is the oracle's `de_error`.
func deErrorReply(err error) any {
	var e *DeError
	if !errors.As(err, &e) {
		return map[string]any{"err": err.Error(), "message": nil, "span": nil}
	}
	var span any
	if s, ok := e.Span().Get(); ok {
		span = []int{s.Start, s.End}
	}
	return map[string]any{"err": e.Error(), "message": e.Message(), "span": span}
}

func serResult(text string, err error) any {
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	return map[string]any{"ok": text}
}

// tomlTestDocuments returns the documents of the toml-test corpus that
// go/crates/tomledit keeps in its testdata, valid and invalid, as text.
func tomlTestDocuments(t testing.TB) (valid []string, invalid []string) {
	t.Helper()
	file, err := os.Open("../tomledit/testdata/toml_test.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	scanner := bufio.NewScanner(file)
	scanner.Buffer(nil, 1<<22)
	for scanner.Scan() {
		var c struct {
			Name       string `json:"name"`
			Toml       string `json:"toml"`
			TomlBase64 string `json:"toml_base64"`
		}
		if err := json.Unmarshal(scanner.Bytes(), &c); err != nil {
			t.Fatal(err)
		}
		text := c.Toml
		if c.TomlBase64 != "" {
			raw, err := base64.StdEncoding.DecodeString(c.TomlBase64)
			if err != nil {
				t.Fatal(err)
			}
			text = string(raw)
		}
		if !utf8.ValidString(text) {
			continue
		}
		if strings.HasPrefix(c.Name, "valid/") {
			valid = append(valid, text)
		} else {
			invalid = append(invalid, text)
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	return valid, invalid
}

// editParseDocuments returns the documents go/crates/tomledit's recorded
// edit_parse corpus parses: the toml-test corpus, handwritten documents,
// invalid ones and seeded mutations of all of them.
func editParseDocuments(t testing.TB) []string {
	t.Helper()
	file, err := os.Open("../tomledit/testdata/oracle/edit_parse.jsonl.gz")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	unpacked, err := gzip.NewReader(file)
	if err != nil {
		t.Fatal(err)
	}
	defer unpacked.Close()
	var documents []string
	scanner := bufio.NewScanner(unpacked)
	scanner.Buffer(nil, 1<<24)
	for scanner.Scan() {
		var c struct {
			Request struct {
				Text string `json:"text"`
			} `json:"request"`
		}
		if err := json.Unmarshal(scanner.Bytes(), &c); err != nil {
			t.Fatal(err)
		}
		documents = append(documents, c.Request.Text)
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	return documents
}

// valueDocuments are documents aimed at what toml adds to toml_edit's
// parser: the private datetime field, and the conversion into a Value.
var valueDocuments = []string{
	"\"$__toml_private_datetime\" = \"1979-05-27T07:32:00Z\"\n",
	"\"$__toml_private_datetime\" = \"not a date\"\n",
	"\"$__toml_private_datetime\" = 5\n",
	"\"$__toml_private_datetime\" = \"1979-05-27\"\nother = 1\n",
	"other = 1\n\"$__toml_private_datetime\" = \"1979-05-27\"\n",
	"[t]\n\"$__toml_private_datetime\" = \"07:32:00\"\n",
	"a = { \"$__toml_private_datetime\" = \"1979-05-27T07:32:00Z\" }\n",
	"a = [{ \"$__toml_private_datetime\" = 1 }]\n",
	"a = 1979-05-27T07:32:00.123456789Z\n",
	"a = 1979-05-27T07:32:00.1234567891Z\n",
	"a = 1987-07-05T17:45:56.600Z\n",
	"a = 9223372036854775807\nb = -9223372036854775808\nc = 0x7FFFFFFFFFFFFFFF\n",
	"a = nan\nb = -nan\nc = +inf\nd = -inf\ne = -0.0\n",
	"[[a]]\n[[a]]\nb = 1\n[[a.c]]\n",
	"a = []\nb = [[]]\nc = [{}]\nd = [[], {}]\n",
	"z = 1\ny = 2\nx = 3\n[b]\n[a]\n",
	"a = 1979-05-27T23:59:60Z\n",
	"a = 23:59:60\n",
	"a = 24:00:00\n",
}

func TestDiffAgainstRust_parse(t *testing.T) {
	oracle.Check(t, "toml", "parse", func(yield func(any)) {
		seen := map[string]bool{}
		add := func(text string) {
			if !seen[text] {
				seen[text] = true
				yield(map[string]any{"op": "parse", "text": text})
			}
		}
		for _, text := range valueDocuments {
			add(text)
		}
		for _, text := range editParseDocuments(t) {
			add(text)
		}
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		return oracle.Catch(func() any {
			value, err := FromStr[Value](r.Text)
			if err != nil {
				return deErrorReply(err)
			}
			return map[string]any{"ok": toCanonical(value)}
		})
	})
}

func TestDiffAgainstRust_parse_table(t *testing.T) {
	oracle.Check(t, "toml", "parse_table", func(yield func(any)) {
		valid, invalid := tomlTestDocuments(t)
		for _, text := range valueDocuments {
			yield(map[string]any{"op": "parse_table", "text": text})
		}
		for _, text := range valid {
			yield(map[string]any{"op": "parse_table", "text": text})
		}
		for _, text := range invalid {
			yield(map[string]any{"op": "parse_table", "text": text})
		}
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		return oracle.Catch(func() any {
			table, err := TableFromStr(r.Text)
			if err != nil {
				return deErrorReply(err)
			}
			return map[string]any{"ok": toCanonical(&Value_Table{F0: table})}
		})
	})
}

// valueKeys are keys whose spelling a serializer has to decide on: bare,
// quoted, escaped.
var valueKeys = []string{
	"a", "b", "key", "x_y", "A-Z_0-9", "", " ", "a b", "a.b", "é", "😀", "1", "-", "_", "\"", "'", "\\",
	"a\nb", "a\tb", "a\x00b", "\x7f", "true", "inf", "nan", "$__toml_private_datetime", "[x]", "#",
}

// valueStrings are strings whose representation exercises each choice the
// serializer makes: basic or literal, one line or several, what is escaped.
var valueStrings = []string{
	"", "a", "a b", "é", "😀", "tab\there", "new\nline", "cr\rhere", "crlf\r\nhere", "back\\slash",
	"C:\\Users\\x", "C:\\Users\\x\nnext", "quote\"d", "apostrophe's", "it's a \\path", "'''", "''", "'",
	"a''b\\", "a'''b\\", "\"\"\"", "\x00", "\x01", "\x08", "\x0c", "\x1f", "\x7f", "\u0080", "\u2028",
	"multi\nline's\\path", "trailing\n", "\n", "\n\n", " leading", "#", "a=b", "x\\'y'",
}

var valueDatetimes = []string{
	"1979-05-27T07:32:00Z", "1979-05-27T00:32:00.999999-07:00", "1979-05-27T07:32:00",
	"1979-05-27", "07:32:00", "00:32:00.5", "1979-05-27T07:32:00+05:30", "2000-02-29T23:59:59Z",
	"1979-05-27T07:32:00.000000001Z", "0000-01-01T00:00:00Z",
}

var valueFloats = []float64{
	0, math.Copysign(0, -1), 1, -1, 0.5, 1.5, -2.5, 0.1, 1e15, 1e16, 1e17, 1e21, 1e100, 1e300,
	1e-5, 1e-7, 1e-100, math.MaxFloat64, 5e-324, 123456789.123456789, 3.141592653589793,
	math.Inf(1), math.Inf(-1), math.NaN(), 100, 12345678901234567890,
}

var valueInts = []int64{0, 1, -1, 42, -17, math.MaxInt64, math.MinInt64, 1 << 53}

// randomValue returns a value of at most depth levels of arrays and
// tables, drawn from the vocabularies above.
func randomValue(random *rand.Rand, depth int) Value {
	kinds := 7
	if depth <= 0 {
		kinds = 5
	}
	switch random.Intn(kinds) {
	case 0:
		return &Value_String{F0: valueStrings[random.Intn(len(valueStrings))]}
	case 1:
		return &Value_Integer{F0: valueInts[random.Intn(len(valueInts))]}
	case 2:
		return &Value_Float{F0: valueFloats[random.Intn(len(valueFloats))]}
	case 3:
		return &Value_Boolean{F0: random.Intn(2) == 0}
	case 4:
		d, err := tomledit.DatetimeFromStr(valueDatetimes[random.Intn(len(valueDatetimes))])
		if err != nil {
			panic(err)
		}
		return &Value_Datetime{F0: d}
	case 5:
		return &Value_Array{F0: randomArray(random, depth-1)}
	default:
		return &Value_Table{F0: randomTable(random, depth-1)}
	}
}

func randomArray(random *rand.Rand, depth int) []Value {
	n := random.Intn(4)
	array := make([]Value, 0, n)
	switch random.Intn(4) {
	case 0: // tables only: an array of tables when it is not empty
		for range n {
			array = append(array, &Value_Table{F0: randomTable(random, depth)})
		}
	case 1: // one kind of scalar
		element := randomValue(random, -1)
		for range n {
			array = append(array, rs.Clone(element))
		}
	default:
		for range n {
			array = append(array, randomValue(random, depth))
		}
	}
	return array
}

func randomTable(random *rand.Rand, depth int) Table {
	table := TableNew()
	for range random.Intn(5) {
		table.Insert(valueKeys[random.Intn(len(valueKeys))], randomValue(random, depth))
	}
	return table
}

// serializedValues returns the values the serialization tests write: fixed
// shapes first, then the toml-test corpus as Go reads it, then seeded random
// trees.
func serializedValues(t testing.TB) []Value {
	t.Helper()
	parse := func(text string) Value {
		value, err := FromStr[Value](text)
		if err != nil {
			t.Fatalf("%q does not parse: %v", text, err)
		}
		return value
	}
	values := []Value{
		&Value_Table{F0: TableNew()},
		&Value_Integer{F0: 1},
		&Value_String{F0: "top"},
		&Value_Array{F0: []Value{}},
		&Value_Array{F0: []Value{&Value_Table{F0: TableNew()}}},
		parse("a = 1\n"),
		parse("a = [1, 2, 3]\nb = [[1], ['x']]\n"),
		parse("a = []\n[b]\n[c.d]\n[[e]]\n[[e]]\nx = 1\n"),
		parse("a = [{ x = 1 }, 2]\nb = [{ y = 1 }, { z = [{ w = 1 }] }]\n"),
		parse("t = { a = { b = { c = 1 } } }\nu = [[{ v = 1 }]]\n"),
		parse("x = 1\n[y]\nz = 2\n[y.w]\nq = [1, { r = 2 }]\n"),
		parse("long = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]\n"),
	}
	valid, _ := tomlTestDocuments(t)
	for _, text := range valid {
		if value, err := FromStr[Value](text); err == nil {
			values = append(values, value)
		}
	}
	random := rand.New(rand.NewSource(8))
	for range 400 {
		values = append(values, &Value_Table{F0: randomTable(random, 1+random.Intn(3))})
	}
	for range 60 {
		values = append(values, randomValue(random, 2))
	}
	return values
}

func TestDiffAgainstRust_serialize(t *testing.T) {
	oracle.Check(t, "toml", "serialize", func(yield func(any)) {
		for index, value := range serializedValues(t) {
			canonical := toCanonical(value)
			yield(map[string]any{"op": "to_string", "value": canonical})
			yield(map[string]any{"op": "to_string_pretty", "value": canonical})
			if index%3 == 0 {
				yield(map[string]any{"op": "display", "value": canonical})
				yield(map[string]any{"op": "try_from", "value": canonical})
			}
			if _, isTable := value.(*Value_Table); isTable && index%3 == 1 {
				yield(map[string]any{"op": "table_display", "value": canonical})
			}
		}
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		value := fromCanonical(t, r.Value)
		return oracle.Catch(func() any {
			switch r.Op {
			case "to_string":
				return serResult(ToString(value))
			case "to_string_pretty":
				return serResult(ToStringPretty(value))
			case "display":
				return map[string]any{"ok": value.String()}
			case "table_display":
				table := value.AsTable().Unwrap()
				return map[string]any{"ok": table.String()}
			default:
				converted, err := ValueTryFrom(value)
				if err != nil {
					return map[string]any{"err": err.Error()}
				}
				return map[string]any{"ok": toCanonical(converted)}
			}
		})
	})
}

// datetimeTexts are datetimes valid and not, for `Datetime::from_str`.
var datetimeTexts = []string{
	"", "1", "12", "123", "1979-05-27", "1979-05-27T07:32:00Z", "1979-05-27t07:32:00z", "1979-05-27 07:32:00Z",
	"1979-05-27T07:32:00", "1979-05-27T07:32:00.999999", "1979-05-27T07:32:00.999999-07:00",
	"1979-05-27T07:32:00+00:00", "1979-05-27T07:32:00-00:00", "1979-05-27T07:32:00+23:59",
	"1979-05-27T07:32:00+24:00", "1979-05-27T07:32:00.", "1979-05-27T07:32:00.1234567891",
	"07:32:00", "07:32:00.5", "07:32:00Z", "07:32", "7:32:00", "24:00:00", "23:60:00", "23:59:60", "23:59:61",
	"1979-13-27", "1979-00-27", "1979-02-30", "1979-02-29", "2000-02-29", "1900-02-29", "1979-05-32",
	"1979-05-27T", "1979-05-27T07", "1979-05-27T07:32", "1979-05-27T07:32:00ZZ", "1979-05-27X07:32:00",
	"19790-05-27", "979-05-27", "1979-5-27", "1979-05-7", "1979-05-27T07:32:00+0700", "1979-05-27T07:32:00+07",
	"1979-05-27T07:32:00 Z", " 1979-05-27", "1979-05-27 ", "é979-05-27", "1979-05-27T07:32:00.é",
	"0000-00-00", "0000-01-01", "9999-12-31T23:59:59.999999999Z",
}

func TestDiffAgainstRust_datetime(t *testing.T) {
	oracle.Check(t, "toml", "datetime", func(yield func(any)) {
		for _, text := range datetimeTexts {
			yield(map[string]any{"op": "datetime", "text": text})
		}
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		return oracle.Catch(func() any {
			datetime, err := tomledit.DatetimeFromStr(r.Text)
			if err != nil {
				return map[string]any{"err": err.Error()}
			}
			return map[string]any{"ok": datetime.String()}
		})
	})
}

// sortedKeys returns the keys of a table in a fixed order, for the edit
// generator.
func sortedKeys(table *Table) []string {
	var keys []string
	for key := range table.Keys() {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}

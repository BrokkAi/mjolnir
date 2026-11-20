package tomledit_test

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// The compliance tests of toml_edit run the toml-test corpus through the
// crates toml-test-data 1.3.1 and toml-test-harness 0.4.5, which are not in
// the registry here. testdata/toml_test.jsonl holds the corpus (see
// corpus_test.go); this file holds what the harness contributes: the decoded
// form both sides of a comparison are brought to (`toml_test::decoded`),
// and its equality. A string, a boolean and an integer compare as text (the
// decoder writes an integer with `to_string`, as the corpus does); a float
// compares by value, with every NaN equal and `inf` equal to `+inf`; a
// datetime compares by value: one with an offset by the instant it names,
// the local kinds field by field. A table compares without regard to order,
// as the harness's HashMap does.

// tomlTestCase is one case of testdata/toml_test.jsonl.
type tomlTestCase struct {
	Name       string          `json:"name"`
	Toml       string          `json:"toml"`
	TomlBase64 string          `json:"toml_base64"`
	Toml10     bool            `json:"toml_1_0"`
	JSON       json.RawMessage `json:"json"`
	Stderr     *string         `json:"stderr"`
}

func (c *tomlTestCase) fixture(t *testing.T) []byte {
	t.Helper()
	if c.TomlBase64 != "" {
		raw, err := base64.StdEncoding.DecodeString(c.TomlBase64)
		if err != nil {
			t.Fatalf("%s: %v", c.Name, err)
		}
		return raw
	}
	return []byte(c.Toml)
}

func loadTomlTestCases(t *testing.T) []tomlTestCase {
	t.Helper()
	file, err := os.Open("testdata/toml_test.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	var cases []tomlTestCase
	scanner := bufio.NewScanner(file)
	scanner.Buffer(nil, 1<<22)
	for scanner.Scan() {
		var c tomlTestCase
		if err := json.Unmarshal(scanner.Bytes(), &c); err != nil {
			t.Fatal(err)
		}
		cases = append(cases, c)
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	if len(cases) == 0 {
		t.Fatal("testdata/toml_test.jsonl holds no cases")
	}
	return cases
}

// decoded is toml_test::decoded::Decoded: a value, a table or an array.
// Exactly one field is set: value for a value, table (non-nil) for a table,
// array (non-nil) for an array.
type decoded struct {
	value *decodedValue
	table map[string]*decoded
	array []*decoded
}

// decodedValue is toml_test::decoded::DecodedValue: a scalar with its
// toml-test type ("string", "integer", "float", "bool", "datetime",
// "datetime-local", "date-local", "time-local") and its text.
type decodedValue struct {
	kind  string
	value string
}

var decodedKinds = map[string]bool{
	"string": true, "integer": true, "float": true, "bool": true,
	"datetime": true, "datetime-local": true, "date-local": true, "time-local": true,
}

// decodedFromJSON is `Decoded::from_slice`: the untagged enum tries a value
// (an object with a known "type" and a string "value") before a table.
func decodedFromJSON(t *testing.T, raw json.RawMessage) *decoded {
	t.Helper()
	trimmed := strings.TrimSpace(string(raw))
	if strings.HasPrefix(trimmed, "[") {
		var elements []json.RawMessage
		if err := json.Unmarshal(raw, &elements); err != nil {
			t.Fatal(err)
		}
		array := make([]*decoded, 0, len(elements))
		for _, element := range elements {
			array = append(array, decodedFromJSON(t, element))
		}
		return &decoded{array: array}
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil {
		t.Fatalf("not a decoded value: %s", raw)
	}
	if len(object) == 2 {
		var kind, value string
		if json.Unmarshal(object["type"], &kind) == nil && json.Unmarshal(object["value"], &value) == nil && decodedKinds[kind] {
			return &decoded{value: &decodedValue{kind: kind, value: value}}
		}
	}
	table := make(map[string]*decoded, len(object))
	for key, element := range object {
		table[key] = decodedFromJSON(t, element)
	}
	return &decoded{table: table}
}

// decodedFloat is `impl From<f64> for DecodedValue`: NaN as "nan", the
// infinities with their sign, any other value as its shortest text.
func decodedFloat(f float64) *decodedValue {
	switch {
	case f != f:
		return &decodedValue{kind: "float", value: "nan"}
	case f > 0 && f*2 == f:
		return &decodedValue{kind: "float", value: "+inf"}
	case f < 0 && f*2 == f:
		return &decodedValue{kind: "float", value: "-inf"}
	}
	return &decodedValue{kind: "float", value: strconv.FormatFloat(f, 'g', -1, 64)}
}

// equal is `impl PartialEq for Decoded`.
func (d *decoded) equal(other *decoded) bool {
	switch {
	case d.value != nil:
		return other.value != nil && d.value.equal(other.value)
	case d.table != nil:
		if other.table == nil || len(d.table) != len(other.table) {
			return false
		}
		for key, value := range d.table {
			if found, ok := other.table[key]; !ok || !value.equal(found) {
				return false
			}
		}
		return true
	default:
		if other.array == nil || len(d.array) != len(other.array) {
			return false
		}
		for index := range d.array {
			if !d.array[index].equal(other.array[index]) {
				return false
			}
		}
		return true
	}
}

func parseHarnessFloat(text string) (float64, bool) {
	f, err := strs.ParseF64(text)
	return f, err == nil
}

// equal is `impl PartialEq for DecodedValue`.
func (v *decodedValue) equal(other *decodedValue) bool {
	if v.kind != other.kind {
		return false
	}
	switch v.kind {
	case "float":
		a, aok := parseHarnessFloat(v.value)
		b, bok := parseHarnessFloat(other.value)
		if !aok || !bok {
			return v.value == other.value
		}
		if a != a && b != b {
			return true
		}
		return a == b
	case "datetime", "datetime-local", "date-local", "time-local":
		a, aerr := tomledit.DatetimeFromStr(v.value)
		b, berr := tomledit.DatetimeFromStr(other.value)
		if aerr != nil || berr != nil {
			return v.value == other.value
		}
		if v.kind == "datetime" {
			return datetimeInstant(a).Equal(datetimeInstant(b))
		}
		return a.Equal(b)
	default:
		return v.value == other.value
	}
}

// datetimeInstant is the instant an offset datetime names.
func datetimeInstant(d tomledit.Datetime) time.Time {
	date := d.Date.Unwrap()
	clock := d.Time.Unwrap()
	minutes := 0
	if custom, ok := d.Offset.Unwrap().(*tomledit.Offset_Custom); ok {
		minutes = int(custom.Minutes)
	}
	return time.Date(int(date.Year), time.Month(date.Month), int(date.Day), int(clock.Hour), int(clock.Minute),
		int(clock.Second), int(clock.Nanosecond), time.FixedZone("", minutes*60))
}

// String renders a decoded value for a failure message.
func (d *decoded) String() string {
	switch {
	case d.value != nil:
		return "{" + d.value.kind + ": " + strconv.Quote(d.value.value) + "}"
	case d.table != nil:
		parts := make([]string, 0, len(d.table))
		for key, value := range d.table {
			parts = append(parts, strconv.Quote(key)+": "+value.String())
		}
		return "{" + strings.Join(parts, ", ") + "}"
	default:
		parts := make([]string, 0, len(d.array))
		for _, value := range d.array {
			parts = append(parts, value.String())
		}
		return "[" + strings.Join(parts, ", ") + "]"
	}
}

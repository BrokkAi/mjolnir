package serdejson

// Differential tests against serde_json 1.0.150 (preserve_order,
// float_roundtrip, raw_value), through the oracle module
// go/port/oracle/src/serde_json_oracle.rs (and serde_json_value_ops.rs for
// the value_* operations), which documents each operation.

import (
	"bytes"
	"encoding/json"
	"math"
	"math/big"
	"math/rand"
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/collections"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

type jsonRequest struct {
	Op      string `json:"op"`
	JSON    string `json:"json,omitempty"`
	Bytes   []int  `json:"bytes,omitempty"`
	Mode    string `json:"mode,omitempty"`
	Type    string `json:"type,omitempty"`
	Bits    string `json:"bits,omitempty"`
	Pointer string `json:"pointer,omitempty"`
	Key     string `json:"key,omitempty"`
	Index   *int   `json:"index,omitempty"`
}

func (r jsonRequest) input() []byte {
	if r.Bytes != nil {
		out := make([]byte, len(r.Bytes))
		for i, b := range r.Bytes {
			out[i] = byte(b)
		}
		return out
	}
	return []byte(r.JSON)
}

// withInput sets the request's input: as text when it is UTF-8 and the
// mode reads a str, otherwise as bytes.
func withInput(r jsonRequest, input string) jsonRequest {
	if utf8.ValidString(input) && (r.Mode == "" || r.Mode == "str") {
		r.JSON = input
		return r
	}
	if r.Mode == "" || r.Mode == "str" {
		r.Mode = "slice"
	}
	r.Bytes = make([]int, len(input))
	for i := 0; i < len(input); i++ {
		r.Bytes[i] = int(input[i])
	}
	return r
}

func jsonErrorReply(err error) map[string]any {
	e, ok := err.(*Error)
	if !ok {
		return map[string]any{"go_error": err.Error()}
	}
	class := map[Category]string{CategoryIo: "Io", CategorySyntax: "Syntax", CategoryData: "Data", CategoryEof: "Eof"}[e.Classify()]
	return map[string]any{"err": e.Error(), "class": class, "line": e.Line(), "column": e.Column()}
}

func fromInput[T any](r jsonRequest) (T, any) {
	input := r.input()
	var value T
	var err error
	switch r.Mode {
	case "slice":
		value, err = FromSlice[T](input)
	case "reader":
		value, err = FromReader[T](bytes.NewReader(input))
	default:
		value, err = FromStr[T](string(input))
	}
	if err != nil {
		return value, jsonErrorReply(err)
	}
	return value, nil
}

func writtenReply(value any) any {
	result := map[string]any{}
	if text, err := ToString(value); err != nil {
		result["compact"] = map[string]any{"ser_err": err.Error()}
	} else {
		result["compact"] = text
	}
	if text, err := ToStringPretty(value); err != nil {
		result["pretty"] = map[string]any{"ser_err": err.Error()}
	} else {
		result["pretty"] = text
	}
	return map[string]any{"ok": result}
}

func primOf[T any](r jsonRequest) any {
	value, failed := fromInput[T](r)
	if failed != nil {
		return failed
	}
	return writtenReply(&value)
}

func answerJSON(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		var r jsonRequest
		if err := json.Unmarshal(raw, &r); err != nil {
			t.Fatal(err)
		}
		return oracle.Catch(func() any { return answer(t, r) })
	}
}

func answer(t *testing.T, r jsonRequest) any {
	switch r.Op {
	case "parse":
		value, failed := fromInput[Value](r)
		if failed != nil {
			return failed
		}
		return writtenReply(valueRef{value})
	case "prim":
		switch r.Type {
		case "bool":
			return primOf[bool](r)
		case "i8":
			return primOf[int8](r)
		case "i16":
			return primOf[int16](r)
		case "i32":
			return primOf[int32](r)
		case "i64":
			return primOf[int64](r)
		case "u8":
			return primOf[uint8](r)
		case "u16":
			return primOf[uint16](r)
		case "u32":
			return primOf[uint32](r)
		case "u64":
			return primOf[uint64](r)
		case "f32":
			return primOf[float32](r)
		case "f64":
			return primOf[float64](r)
		case "String":
			return primOf[string](r)
		case "unit":
			return primOf[rs.Unit](r)
		case "OptionU8":
			return primOf[rs.Option[uint8]](r)
		case "VecString":
			return primOf[[]string](r)
		case "Map":
			return primOf[Map](r)
		}
	case "f64_format":
		bits, _ := strconv.ParseUint(r.Bits, 10, 64)
		value := math.Float64frombits(bits)
		text, _ := ToString(value)
		asValue, _ := ToValue(value)
		var key any
		keyed := collections.BTreeMapFrom(rs.T2(value, 0))
		if keyText, err := ToString(&keyed); err != nil {
			key = map[string]any{"err": err.Error()}
		} else {
			key = keyText
		}
		display := ""
		if n, ok := NumberFromF64(value).Get(); ok {
			display = n.String()
		}
		return map[string]any{
			"text":       text,
			"value":      ValueToString(asValue),
			"unexpected": toError(serde.DeErrorInvalidType(&serde.DeUnexpectedFloat{F0: value}, serde.DeExpectedStr("a boolean"))).Error(),
			"key":        key,
			"display":    display,
		}
	case "f32_format":
		bits, _ := strconv.ParseUint(r.Bits, 10, 64)
		value := math.Float32frombits(uint32(bits))
		text, _ := ToString(value)
		asValue, _ := ToValue(value)
		return map[string]any{"text": text, "value": ValueToString(asValue)}
	case "string":
		text := string(r.input())
		compact, _ := ToString(text)
		pretty, _ := ToStringPretty([]string{text})
		back, _ := FromStr[string](compact)
		return map[string]any{"compact": compact, "pretty": pretty, "back": back == text}
	case "stream":
		var items []any
		var d *Deserializer
		if r.Mode == "reader" {
			d = DeserializerFromReader(bytes.NewReader(r.input()))
		} else {
			d = DeserializerFromSlice(r.input())
		}
		stream := DeserializerIntoIter[Value](d)
		for range 10 {
			item := stream.Next()
			result, ok := item.Get()
			if !ok {
				items = append(items, map[string]any{"end": stream.ByteOffset()})
				break
			}
			if result.Err != nil {
				reply := jsonErrorReply(result.Err)
				reply["offset"] = stream.ByteOffset()
				items = append(items, reply)
				continue
			}
			items = append(items, map[string]any{"value": ValueToString(result.Value), "offset": stream.ByteOffset()})
		}
		return items
	case "raw":
		raw, err := RawValueFromString(string(r.input()))
		if err != nil {
			return jsonErrorReply(err)
		}
		holder := rawHolder{Raw: raw}
		compact, _ := ToString(&holder)
		pretty, _ := ToStringPretty(&holder)
		var asValue any
		if v, err := ToValue(&holder); err != nil {
			asValue = map[string]any{"err": err.Error()}
		} else {
			asValue = ValueToString(v)
		}
		return map[string]any{"get": raw.Get(), "compact": compact, "pretty": pretty, "value": asValue}
	case "number":
		n, err := NumberFromStr(string(r.input()))
		if err != nil {
			return jsonErrorReply(err)
		}
		return map[string]any{"display": n.String(), "is_i64": n.IsI64(), "is_u64": n.IsU64(), "is_f64": n.IsF64()}
	case "value_probe":
		return valueProbe(r)
	case "value_from_value":
		return valueFromValue(r)
	case "value_index_mut":
		value, err := FromStr[Value](r.JSON)
		if err != nil {
			t.Fatal(err)
		}
		if r.Index != nil {
			*ValueIndexMut(&value, *r.Index) = int64(1)
		} else {
			*ValueIndexMut(&value, r.Key) = int64(1)
		}
		return map[string]any{"after": ValueToString(value)}
	}
	t.Fatalf("no Go answer for %+v", r)
	return nil
}

type rawHolder struct {
	Raw *RawValue `rs:"raw"`
}

var _ = rs.RegisterStruct[rawHolder]("RawHolder")

func optionString[T any](o rs.Option[T], f func(T) any) any {
	if v, ok := o.Get(); ok {
		return f(v)
	}
	return nil
}

func valueProbe(r jsonRequest) any {
	value, err := FromStr[Value](r.JSON)
	if err != nil {
		panic(err)
	}
	index := 0
	if r.Index != nil {
		index = *r.Index
	}
	found := ValuePointer(value, r.Pointer)
	str := func(v Value) any { return ValueToString(v) }
	onFound := func(f func(Value) any) any {
		if v, ok := found.Get(); ok {
			return f(v)
		}
		return nil
	}
	return map[string]any{
		"pointer":     optionString(found, str),
		"get_key":     optionString(ValueGet(value, r.Key), str),
		"get_index":   optionString(ValueGet(value, index), str),
		"index_key":   ValueToString(ValueIndex(value, r.Key)),
		"index_index": ValueToString(ValueIndex(value, index)),
		"as_str": onFound(func(v Value) any {
			return optionString(ValueAsStr(v), func(s string) any { return s })
		}),
		"as_bool": onFound(func(v Value) any {
			return optionString(ValueAsBool(v), func(b bool) any { return b })
		}),
		"as_i64": onFound(func(v Value) any {
			return optionString(ValueAsI64(v), func(i int64) any { return i })
		}),
		"as_u64": onFound(func(v Value) any {
			return optionString(ValueAsU64(v), func(u uint64) any { return u })
		}),
		"as_f64": onFound(func(v Value) any {
			return optionString(ValueAsF64(v), func(f float64) any { return strconv.FormatUint(math.Float64bits(f), 10) })
		}),
		"is_null":           onFound(func(v Value) any { return ValueIsNull(v) }),
		"is_number":         onFound(func(v Value) any { return ValueIsNumber(v) }),
		"is_i64":            onFound(func(v Value) any { return ValueIsI64(v) }),
		"is_u64":            onFound(func(v Value) any { return ValueIsU64(v) }),
		"is_f64":            onFound(func(v Value) any { return ValueIsF64(v) }),
		"is_array":          onFound(func(v Value) any { return ValueIsArray(v) }),
		"is_object":         onFound(func(v Value) any { return ValueIsObject(v) }),
		"display":           ValueToString(value),
		"alternate":         ValueToStringPretty(value),
		"eq_self_reordered": ValueEq(reordered(value), value),
	}
}

func reordered(v Value) Value {
	switch x := v.(type) {
	case Map:
		out := Map{}
		for i := len(x) - 1; i >= 0; i-- {
			MapInsert(&out, x[i].Key, reordered(x[i].Value))
		}
		return out
	case []any:
		out := make([]any, len(x))
		for i := range x {
			out[i] = reordered(x[i])
		}
		return out
	}
	return v
}

func fromValueOf[T any](value Value) any {
	result, err := FromValue[T](value)
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	converted, err := ToValue(&result)
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	return map[string]any{"ok": ValueToString(converted)}
}

func valueFromValue(r jsonRequest) any {
	value, err := FromStr[Value](r.JSON)
	if err != nil {
		panic(err)
	}
	switch r.Type {
	case "bool":
		return fromValueOf[bool](value)
	case "u8":
		return fromValueOf[uint8](value)
	case "i64":
		return fromValueOf[int64](value)
	case "u64":
		return fromValueOf[uint64](value)
	case "f32":
		return fromValueOf[float32](value)
	case "f64":
		return fromValueOf[float64](value)
	case "String":
		return fromValueOf[string](value)
	case "unit":
		return fromValueOf[rs.Unit](value)
	case "VecU8":
		return fromValueOf[[]uint8](value)
	case "OptionString":
		return fromValueOf[rs.Option[string]](value)
	case "MapStringU8":
		return fromValueOf[collections.BTreeMap[string, uint8]](value)
	case "MapU8Bool":
		return fromValueOf[collections.BTreeMap[uint8, bool]](value)
	case "Tuple2":
		return fromValueOf[rs.Tuple2[uint8, string]](value)
	}
	panic("unknown type " + r.Type)
}

// documents are JSON texts of every shape, valid and not.
var documents = []string{
	``, ` `, `null`, `true`, `false`, `0`, `-0`, `1`, `-1`, `0.5`, `-0.0`, `1e3`, `1E+3`, `1e-3`,
	`"s"`, `""`, `[]`, `{}`, `[[]]`, `{"a":{}}`, `[1,2,3]`, `{"a":1,"b":[true,null]}`,
	" \t\r\n[ 1 , 2 ]\n", "{\n  \"a\": 1,\n  \"b\": 2\n}", "[\r\n1,\r\n2\r\n]",
	`nul`, `nulll`, `tru`, `truex`, `fals`, `falsee`, `nan`, `NaN`, `Infinity`, `-Infinity`, `+1`,
	`01`, `-01`, `00`, `1.`, `1.e5`, `.5`, `-`, `-a`, `1e`, `1e+`, `1e-`, `1ea`, `1.5e`, `2.`, `0x10`,
	`[1,]`, `[1,,2]`, `[,1]`, `[1 2]`, `{"a":1,}`, `{"a" 1}`, `{"a":}`, `{a:1}`, `{"a":1 "b":2}`,
	`{1:2}`, `{"a":1}}`, `[1]]`, `[1}`, `{"a":1]`, `"abc`, `"\`, `"\u12`, `"\u12g4"`, `"\x"`,
	"\"a\tb\"", "\"a\nb\"", "\"\x00\"", "\"\x1f\"", "\"\x7f\"", `"\ud800"`, `"\udc00"`, `"𐀀"`,
	`"\ud800A"`, `"\ud800x"`, `"\ud800\"`, `"😀"`, `"\/\b\f\n\r\t\"\\"`,
	`[1] x`, `[1] [2]`, `1 2`, `{} {}`, "\"\xff\"", "\"\xc3\x28\"", "[\"\xe2\x82\"]", "\xef\xbb\xbf{}",
	`18446744073709551615`, `18446744073709551616`, `-9223372036854775808`, `-9223372036854775809`,
	`9223372036854775807`, `9223372036854775808`, `123456789012345678901234567890`,
	`1e308`, `1e309`, `-1e309`, `1e-324`, `1e-400`, `4.9e-324`, `2.4703282292062327e-324`,
	`2.4703282292062328e-324`, `1.7976931348623157e308`, `1.7976931348623158e308`,
	`1.7976931348623159e308`, `0.1e999999999999999999`, `0e999999999999`, `1e-999999999999`,
	`1e2147483647`, `1e2147483648`, `1e-2147483648`, `1e-2147483649`, `123.456e-2147483648`,
	`0.000000000000000000000000000000000000001`, `100000000000000000000000000000000000000000.0`,
	`{"a":1,"a":2}`, `{"a":[1],"b":{"c":"d"},"a":3}`, `[1, 2.5, -3, "four", [5], {"six": 6}]`,
	"{\"\xe9\":1}", `{"é":"ü"}`, `"😀"`, `["\u0000"]`,
}

// numbers are the texts of number tokens that float parsing must get right.
func floatTexts(rng *rand.Rand) []string {
	out := []string{
		"0", "-0", "0.0", "1", "0.1", "0.2", "0.3", "1.1", "2.5", "1e23", "8.98846567431158e307",
		"9007199254740993", "9007199254740992.5", "1.00000000000000011102230246251565404236316680908203125",
		"1.00000000000000011102230246251565404236316680908203124", "1.00000000000000011102230246251565404236316680908203126",
		"2.2250738585072011e-308", "2.2250738585072012e-308", "4.94065645841246544e-324", "7.41098468761869816e-324",
		"179769313486231570000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000.0",
		"0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000049406564584124654",
		"3.4028235e38", "3.4028236e38", "3.40282356779733661637539395458142568448e38", "3.40282356779733661637539395458142568447e38",
		"1.17549435e-38", "1.4e-45", "7.0e-46", "7.1e-46", "16777217", "16777216.5", "0.1e1", "1234567890123456789012",
		"12345678901234567890.12345678901234567890", "0.00000000000000000000000000001234567890123456789",
	}
	for range 400 {
		// Random digits with a point and an exponent somewhere.
		n := 1 + rng.Intn(30)
		digits := make([]byte, n)
		for i := range digits {
			digits[i] = byte('0' + rng.Intn(10))
		}
		if digits[0] == '0' && n > 1 {
			digits[0] = '1'
		}
		text := string(digits)
		if rng.Intn(2) == 0 && n > 1 {
			p := 1 + rng.Intn(n-1)
			text = text[:p] + "." + text[p:]
		}
		if rng.Intn(2) == 0 {
			text += "e" + strconv.Itoa(rng.Intn(700)-350)
		}
		if rng.Intn(3) == 0 {
			text = "-" + text
		}
		out = append(out, text)
	}
	for range 400 {
		// The point halfway between two neighbouring f64, and a hair to
		// either side, where only a correct parser rounds right.
		bits := rng.Uint64() &^ (1 << 63)
		x := math.Float64frombits(bits)
		if math.IsInf(x, 0) || math.IsNaN(x) {
			continue
		}
		next := math.Nextafter(x, math.Inf(1))
		if math.IsInf(next, 0) {
			continue
		}
		mid := new(big.Float).SetPrec(2000).Add(new(big.Float).SetPrec(2000).SetFloat64(x), new(big.Float).SetPrec(2000).SetFloat64(next))
		mid.Quo(mid, big.NewFloat(2))
		text := mid.Text('e', 60)
		out = append(out, text)
		out = append(out, mid.Text('e', 25))
	}
	for range 200 {
		// The same for f32.
		bits := rng.Uint32() &^ (1 << 31)
		x := math.Float32frombits(bits)
		if math.IsInf(float64(x), 0) || math.IsNaN(float64(x)) {
			continue
		}
		next := math.Nextafter32(x, float32(math.Inf(1)))
		mid := (float64(x) + float64(next)) / 2
		out = append(out, strconv.FormatFloat(mid, 'e', -1, 64), strconv.FormatFloat(mid, 'e', 40, 64))
	}
	return out
}

func mutateText(rng *rand.Rand, text string) []string {
	out := []string{}
	if len(text) == 0 {
		return out
	}
	for range 2 {
		out = append(out, text[:rng.Intn(len(text))])
	}
	edits := []string{"", "x", ",", "]", "}", "\"", "\n", " ", "\\", "0", "\x01", "\xff", "{", "["}
	for range 3 {
		i := rng.Intn(len(text))
		out = append(out, text[:i]+edits[rng.Intn(len(edits))]+text[min(len(text), i+1):])
	}
	return out
}

func nested(depth int, open, close string, inner string) string {
	return strings.Repeat(open, depth) + inner + strings.Repeat(close, depth)
}

func TestDiffParseAgainstRust(t *testing.T) {
	oracle.Check(t, "serde_json", "parse", func(yield func(any)) {
		rng := rand.New(rand.NewSource(2))
		modes := []string{"str", "slice", "reader"}
		inputs := append([]string{}, documents...)
		for _, depth := range []int{126, 127, 128, 129, 200} {
			inputs = append(inputs, nested(depth, "[", "]", ""), nested(depth, `{"a":`, "}", "1"), nested(depth, "[", "", ""))
		}
		inputs = append(inputs, strings.Repeat(`{"a":[`, 63)+"1"+strings.Repeat("]}", 63), strings.Repeat(`{"a":[`, 64)+"1"+strings.Repeat("]}", 64))
		for _, document := range documents {
			inputs = append(inputs, mutateText(rng, document)...)
		}
		for _, text := range floatTexts(rand.New(rand.NewSource(3)))[:200] {
			inputs = append(inputs, text, "["+text+"]")
		}
		big := strings.Repeat(`{"key":"value with \"escapes\" and é","n":[1,2.5,-3e7]},`, 2000)
		inputs = append(inputs, "["+big[:len(big)-1]+"]")
		for _, input := range inputs {
			for _, mode := range modes {
				yield(withInput(jsonRequest{Op: "parse", Mode: mode}, input))
			}
		}
	}, answerJSON(t))
}

func TestDiffPrimitivesAgainstRust(t *testing.T) {
	oracle.Check(t, "serde_json", "prim", func(yield func(any)) {
		ints := []string{
			"0", "-0", "1", "-1", "127", "128", "-128", "-129", "255", "256", "32767", "32768", "-32768",
			"-32769", "65535", "65536", "2147483647", "2147483648", "-2147483648", "-2147483649",
			"4294967295", "4294967296", "9223372036854775807", "9223372036854775808",
			"-9223372036854775808", "-9223372036854775809", "18446744073709551615", "18446744073709551616",
			"1.0", "1e2", "-0.0", "1.5", " 7 ", "7 x", "07", `"7"`, "true", "null", "[]", "{}", `"x"`,
		}
		others := []string{
			"true", "false", "null", `"a"`, `""`, `"ab"`, `"é"`, `"😀"`, `"A"`, `"😀"`, "[]", "{}",
			`["a","b"]`, `[1]`, `["a",1]`, `{"a":1}`, `{"a":1,"a":2}`, `{"a":null}`, "1", "-1", "1.5", "[null]",
			`{"a":[{"b":"c"}]}`, ` "x" `, `"x" y`, `[`, `{`, `"`,
		}
		for _, typ := range []string{"i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64"} {
			for _, input := range ints {
				yield(withInput(jsonRequest{Op: "prim", Type: typ, Mode: "str"}, input))
			}
		}
		for _, typ := range []string{"bool", "String", "unit", "OptionU8", "VecString", "Map"} {
			for _, input := range append(append([]string{}, others...), ints[:12]...) {
				for _, mode := range []string{"str", "reader"} {
					yield(withInput(jsonRequest{Op: "prim", Type: typ, Mode: mode}, input))
				}
			}
		}
		for i, text := range floatTexts(rand.New(rand.NewSource(4))) {
			yield(withInput(jsonRequest{Op: "prim", Type: "f64", Mode: "str"}, text))
			yield(withInput(jsonRequest{Op: "prim", Type: "f32", Mode: "str"}, text))
			if i%10 == 0 {
				yield(withInput(jsonRequest{Op: "prim", Type: "f64", Mode: "reader"}, text))
			}
		}
	}, answerJSON(t))
}

func TestDiffFloatFormatAgainstRust(t *testing.T) {
	oracle.Check(t, "serde_json", "float_format", func(yield func(any)) {
		rng := rand.New(rand.NewSource(5))
		special := []float64{
			0, math.Copysign(0, -1), 1, -1, 0.1, 0.5, 1.5, 100, 1e15, 1e16, 1e17, 1e-5, 1e-6, 1e-7, 123456789012345680,
			math.MaxFloat64, math.SmallestNonzeroFloat64, 2.2250738585072014e-308, math.NaN(), math.Inf(1), math.Inf(-1),
			9007199254740992, 9007199254740993, 0.3, 2.5e-5, 9.999999999999999e15, 9.999999999999999e-6, 1e22, 1e21,
		}
		for e := -330; e <= 310; e++ {
			special = append(special, math.Pow(10, float64(e)), 3*math.Pow(10, float64(e)))
		}
		for _, f := range special {
			yield(jsonRequest{Op: "f64_format", Bits: strconv.FormatUint(math.Float64bits(f), 10)})
		}
		for range 2500 {
			yield(jsonRequest{Op: "f64_format", Bits: strconv.FormatUint(rng.Uint64(), 10)})
		}
		for range 500 {
			// Short decimals, which have few digits.
			f := float64(rng.Intn(100000)) * math.Pow(10, float64(rng.Intn(60)-30))
			yield(jsonRequest{Op: "f64_format", Bits: strconv.FormatUint(math.Float64bits(f), 10)})
		}
		specials32 := []float32{0, float32(math.Copysign(0, -1)), 1, 0.1, 16777216, 16777217, math.MaxFloat32, math.SmallestNonzeroFloat32, 1.17549435e-38, 1e12, 1e13, 1e-6, 1e-7}
		for _, f := range specials32 {
			yield(jsonRequest{Op: "f32_format", Bits: strconv.FormatUint(uint64(math.Float32bits(f)), 10)})
		}
		for e := -46; e <= 39; e++ {
			f := float32(math.Pow(10, float64(e)))
			yield(jsonRequest{Op: "f32_format", Bits: strconv.FormatUint(uint64(math.Float32bits(f)), 10)})
		}
		for range 1500 {
			yield(jsonRequest{Op: "f32_format", Bits: strconv.FormatUint(uint64(rng.Uint32()), 10)})
		}
	}, answerJSON(t))
}

func TestDiffStringsAgainstRust(t *testing.T) {
	oracle.Check(t, "serde_json", "strings", func(yield func(any)) {
		rng := rand.New(rand.NewSource(6))
		alphabet := []rune{'a', 'Z', '0', ' ', '"', '\\', '/', '\b', '\f', '\n', '\r', '\t', 0, 0x1f, 0x7f, 0x80, 0xe9, 0x2028, 0xfeff, 0xffff, 0x1f600, 0x10ffff}
		fixed := []string{"", "plain", "\"quoted\"", "back\\slash", "\x00\x01\x1f\x7f", "é😀 ", "</script>", "\u0080\u009f"}
		for _, s := range fixed {
			yield(withInput(jsonRequest{Op: "string"}, s))
		}
		for range 400 {
			n := rng.Intn(20)
			var b strings.Builder
			for range n {
				b.WriteRune(alphabet[rng.Intn(len(alphabet))])
			}
			yield(withInput(jsonRequest{Op: "string"}, b.String()))
		}
	}, answerJSON(t))
}

func TestDiffMiscAgainstRust(t *testing.T) {
	oracle.Check(t, "serde_json", "misc", func(yield func(any)) {
		streams := []string{
			"", " ", "1 2 3", "1 2 [3]x", "[1][2]{}", `"a""b"`, "1x", "null null", "{} 1", "[1, 2] [", "truefalse",
			"1\n2\n\n3 ", "nul", "[1,]", `{"a":1} {"b":2}`, "\"\xff\"", "1 \"\xff\" 2", "-", "1e5 2.5", "[] , []",
		}
		for _, s := range streams {
			for _, mode := range []string{"slice", "reader"} {
				yield(withInput(jsonRequest{Op: "stream", Mode: mode}, s))
			}
		}
		raws := []string{
			"null", " true ", `{"a" : [1,  2]}`, " 1.50 ", `"s"`, "", " ", "1 2", "[1,]", `{"a":1}x`, "\t[\n]\n",
			`{"$serde_json::private::RawValue": 1}`, "-0", "1e400",
		}
		for _, s := range raws {
			yield(withInput(jsonRequest{Op: "raw"}, s))
		}
		numbers := []string{
			"0", "-0", "1", "-1", "1.5", "-1.5", "1e5", "1E5", "1e-5", "18446744073709551615", "18446744073709551616",
			"-9223372036854775808", "-9223372036854775809", "", " 1", "1 ", "01", "1.", "-", "+1", "1e400", "abc", "0x1",
		}
		for _, s := range numbers {
			yield(withInput(jsonRequest{Op: "number"}, s))
		}
		probes := []struct{ doc, pointer, key string }{
			{`{"a":[1,{"b":2}]}`, "/a/1/b", "a"},
			{`{"a":[1,{"b":2}]}`, "/a/0", "b"},
			{`{"a":[1,{"b":2}]}`, "", "a"},
			{`{"a":[1,{"b":2}]}`, "a", "a"},
			{`{"a":[1,{"b":2}]}`, "/a/01", "a"},
			{`{"a":[1,{"b":2}]}`, "/a/+1", "a"},
			{`{"a":[1,{"b":2}]}`, "/a/9", "a"},
			{`{"a/b":1,"c~d":2,"":3}`, "/a~1b", "a/b"},
			{`{"a/b":1,"c~d":2,"":3}`, "/c~0d", "c~d"},
			{`{"a/b":1,"c~d":2,"":3}`, "/", ""},
			{`[1.5, -2, 18446744073709551615, "s", true, null]`, "/0", "x"},
			{`[1.5, -2, 18446744073709551615, "s", true, null]`, "/1", "x"},
			{`[1.5, -2, 18446744073709551615, "s", true, null]`, "/2", "x"},
			{`[1.5, -2, 18446744073709551615, "s", true, null]`, "/3", "x"},
			{`[1.5, -2, 18446744073709551615, "s", true, null]`, "/4", "x"},
			{`[1.5, -2, 18446744073709551615, "s", true, null]`, "/5", "x"},
			{`{"z":1,"a":{"y":[2,3],"b":{}}}`, "/a", "z"},
			{`"text"`, "", "x"},
			{`9223372036854775807`, "", "x"},
			{`-0.0`, "", "x"},
		}
		for i, p := range probes {
			index := i % 3
			yield(jsonRequest{Op: "value_probe", JSON: p.doc, Pointer: p.pointer, Key: p.key, Index: &index})
		}
		fromValues := []string{
			"true", "1", "-1", "255", "256", "1.5", "-0.0", "18446744073709551615", `"s"`, `"é"`, `"ab"`, "null",
			"[]", "[1,2]", `[1,"a"]`, `[1,"a",2]`, `{"a":1}`, `{"1":true}`, `{"x":true}`, `{"a":256}`, "{}",
			`[256]`, `"\u0000"`, "3.4028235e38", "3.5e38", "1e-50",
		}
		for _, typ := range []string{"bool", "u8", "i64", "u64", "f32", "f64", "String", "unit", "VecU8", "OptionString", "MapStringU8", "MapU8Bool", "Tuple2"} {
			for _, doc := range fromValues {
				yield(jsonRequest{Op: "value_from_value", JSON: doc, Type: typ})
			}
		}
		indexMuts := []struct {
			doc   string
			key   string
			index *int
		}{
			{`{"a":2}`, "a", nil}, {`{"a":2}`, "b", nil}, {`null`, "a", nil}, {`[1]`, "a", nil}, {`1`, "a", nil},
			{`"s"`, "a", nil}, {`true`, "a", nil}, {`{"a":1,"b":2}`, "a", nil},
		}
		zero, five := 0, 5
		indexMuts = append(indexMuts, struct {
			doc   string
			key   string
			index *int
		}{`[0,0]`, "", &zero}, struct {
			doc   string
			key   string
			index *int
		}{`[0,0]`, "", &five}, struct {
			doc   string
			key   string
			index *int
		}{`{}`, "", &zero}, struct {
			doc   string
			key   string
			index *int
		}{`null`, "", &zero})
		for _, m := range indexMuts {
			yield(jsonRequest{Op: "value_index_mut", JSON: m.doc, Key: m.key, Index: m.index})
		}
	}, answerJSON(t))
}

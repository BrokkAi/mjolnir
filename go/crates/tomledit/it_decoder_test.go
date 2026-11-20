// Port of toml_edit 0.20.2 tests/decoder.rs

package tomledit_test

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// decoderDecode is `impl toml_test_harness::Decoder for Decoder`: the
// document as toml-test's decoded form, or the error as text.
func decoderDecode(data []byte) (*decoded, string) {
	text, err := strs.FromUtf8(data)
	if err != nil {
		return nil, err.Error()
	}
	document, err := tomledit.DocumentFromStr(text)
	if err != nil {
		return nil, err.Error()
	}
	return documentToDecoded(&document), ""
}

func documentToDecoded(value *tomledit.Document) *decoded {
	return tableToDecoded(value.AsTable())
}

func itemToDecoded(value tomledit.Item) *decoded {
	switch v := value.(type) {
	case *tomledit.Item_None:
		panic(rs.Unreachable("No nones"))
	case *tomledit.Item_Value:
		return valueToDecoded(v.F0)
	case *tomledit.Item_Table:
		return tableToDecoded(&v.F0)
	case *tomledit.Item_ArrayOfTables:
		array := []*decoded{}
		for table := range v.F0.Iter() {
			array = append(array, tableToDecoded(table))
		}
		return &decoded{array: array}
	default:
		panic(rs.Unreachable(value))
	}
}

func valueToDecoded(value tomledit.Value) *decoded {
	switch v := value.(type) {
	case *tomledit.Value_Integer:
		return &decoded{value: &decodedValue{kind: "integer", value: strconv.FormatInt(v.F0.Value(), 10)}}
	case *tomledit.Value_String:
		return &decoded{value: &decodedValue{kind: "string", value: v.F0.Value()}}
	case *tomledit.Value_Float:
		return &decoded{value: decodedFloat(v.F0.Value())}
	case *tomledit.Value_Datetime:
		d := v.F0.Value()
		text := d.String()
		var kind string
		switch {
		case d.Date.IsSome() && d.Time.IsSome() && d.Offset.IsSome():
			kind = "datetime"
		case d.Date.IsSome() && d.Time.IsSome() && d.Offset.IsNone():
			kind = "datetime-local"
		case d.Date.IsSome() && d.Time.IsNone() && d.Offset.IsNone():
			kind = "date-local"
		case d.Date.IsNone() && d.Time.IsSome() && d.Offset.IsNone():
			kind = "time-local"
		default:
			panic(rs.Unreachable("Unsupported case"))
		}
		return &decoded{value: &decodedValue{kind: kind, value: text}}
	case *tomledit.Value_Boolean:
		return &decoded{value: &decodedValue{kind: "bool", value: strconv.FormatBool(v.F0.Value())}}
	case *tomledit.Value_Array:
		array := []*decoded{}
		for element := range v.F0.Iter() {
			array = append(array, valueToDecoded(element))
		}
		return &decoded{array: array}
	case *tomledit.Value_InlineTable:
		return inlineTableToDecoded(&v.F0)
	default:
		panic(rs.Unreachable(value))
	}
}

func tableToDecoded(value *tomledit.Table) *decoded {
	table := map[string]*decoded{}
	for k, v := range value.Iter() {
		table[k] = itemToDecoded(v)
	}
	return &decoded{table: table}
}

func inlineTableToDecoded(value *tomledit.InlineTable) *decoded {
	table := map[string]*decoded{}
	for k, v := range value.Iter() {
		table[k] = valueToDecoded(v)
	}
	return &decoded{table: table}
}

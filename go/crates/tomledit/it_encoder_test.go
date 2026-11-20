// Port of toml_edit 0.20.2 tests/encoder.rs

package tomledit_test

import (
	"errors"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// encoderEncode is `impl toml_test_harness::Encoder for Encoder`.
func encoderEncode(data *decoded) (string, error) {
	doc, err := decodedToDocument(data)
	if err != nil {
		return "", err
	}
	return doc.String(), nil
}

func decodedToDocument(data *decoded) (tomledit.Document, error) {
	item, err := rootFromDecoded(data)
	if err != nil {
		return tomledit.Document{}, err
	}
	doc := tomledit.DocumentNew()
	*doc.AsTableMut() = item
	return doc, nil
}

func rootFromDecoded(data *decoded) (tomledit.Table, error) {
	switch {
	case data.value != nil:
		return tomledit.Table{}, errors.New("Root cannot be a value")
	case data.table != nil:
		var pairs []rs.Tuple2[any, any]
		for k, v := range data.table {
			value, err := fromDecoded(v)
			if err != nil {
				return tomledit.Table{}, err
			}
			pairs = append(pairs, rs.T2[any, any](k, value))
		}
		return tomledit.TableFromIter(pairs...), nil
	default:
		return tomledit.Table{}, errors.New("Root cannot be an array")
	}
}

func fromDecoded(data *decoded) (tomledit.Value, error) {
	switch {
	case data.value != nil:
		return fromDecodedValue(data.value)
	case data.table != nil:
		table, err := fromTable(data.table)
		if err != nil {
			return nil, err
		}
		return &tomledit.Value_InlineTable{F0: table}, nil
	default:
		array, err := fromArray(data.array)
		if err != nil {
			return nil, err
		}
		return &tomledit.Value_Array{F0: array}, nil
	}
}

func fromDecodedValue(data *decodedValue) (tomledit.Value, error) {
	switch data.kind {
	case "string":
		return tomledit.ValueFromString(data.value), nil
	case "integer":
		i, err := strs.ParseI64(data.value)
		if err != nil {
			return nil, err
		}
		return tomledit.ValueFromInt64(i), nil
	case "float":
		f, err := strs.ParseF64(data.value)
		if err != nil {
			return nil, err
		}
		return tomledit.ValueFromFloat64(f), nil
	case "bool":
		b, err := strs.ParseBool(data.value)
		if err != nil {
			return nil, err
		}
		return tomledit.ValueFromBool(b), nil
	default:
		// datetime, datetime-local, date-local and time-local
		d, err := tomledit.DatetimeFromStr(data.value)
		if err != nil {
			return nil, err
		}
		return tomledit.ValueFromDatetime(d), nil
	}
}

func fromTable(data map[string]*decoded) (tomledit.InlineTable, error) {
	var pairs []rs.Tuple2[any, any]
	for k, v := range data {
		value, err := fromDecoded(v)
		if err != nil {
			return tomledit.InlineTable{}, err
		}
		pairs = append(pairs, rs.T2[any, any](k, value))
	}
	return tomledit.InlineTableFromIter(pairs...), nil
}

func fromArray(data []*decoded) (tomledit.Array, error) {
	var values []any
	for _, element := range data {
		value, err := fromDecoded(element)
		if err != nil {
			return tomledit.Array{}, err
		}
		values = append(values, value)
	}
	return tomledit.ArrayFromIter(values...), nil
}

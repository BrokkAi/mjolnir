// Port of toml_edit 0.20.2 src/de/table_enum.rs

package tomledit

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// tableEnumDeserializer is TableEnumDeserializer: deserializes table values
// into enum variants.
type tableEnumDeserializer struct {
	value Item
}

func tableEnumDeserializerNew(value Item) *tableEnumDeserializer {
	return &tableEnumDeserializer{value: value}
}

func (d *tableEnumDeserializer) UnitVariant() error {
	switch values := d.value.(type) {
	case *Item_ArrayOfTables:
		if values.F0.IsEmpty() {
			return nil
		}
		return deErrorCustom("expected empty array", values.F0.span)
	case *Item_Value:
		switch inner := values.F0.(type) {
		case *Value_Array:
			if inner.F0.IsEmpty() {
				return nil
			}
			return deErrorCustom("expected empty table", inner.F0.span)
		case *Value_InlineTable:
			if inner.F0.IsEmpty() {
				return nil
			}
			return deErrorCustom("expected empty table", inner.F0.span)
		}
	case *Item_Table:
		if values.F0.IsEmpty() {
			return nil
		}
		return deErrorCustom("expected empty table", values.F0.span)
	}
	e := d.value
	return deErrorCustom("expected table, found "+e.TypeName(), e.span())
}

func (d *tableEnumDeserializer) NewtypeVariantSeed(seed serde.DeDeserializeSeed) (any, error) {
	value, err := seed.DeserializeSeed(deValueDeserializerNew(d.value))
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *tableEnumDeserializer) TupleVariant(length int, visitor serde.DeVisitor) (any, error) {
	switch values := d.value.(type) {
	case *Item_ArrayOfTables:
		valuesSpan := values.F0.span
		tupleValues := values.F0.values

		if len(tupleValues) == length {
			return arrayDeserializerNew(tupleValues, valuesSpan).DeserializeSeq(visitor)
		}
		return nil, deErrorCustom("expected tuple with length "+strconv.Itoa(length), valuesSpan)
	case *Item_Value:
		switch inner := values.F0.(type) {
		case *Value_Array:
			valuesSpan := inner.F0.span
			tupleValues := inner.F0.values

			if len(tupleValues) == length {
				return arrayDeserializerNew(tupleValues, valuesSpan).DeserializeSeq(visitor)
			}
			return nil, deErrorCustom("expected tuple with length "+strconv.Itoa(length), valuesSpan)
		case *Value_InlineTable:
			valuesSpan := inner.F0.span
			tupleValues, err := tupleValuesOfTable(&inner.F0.items, length)
			if err != nil {
				return nil, err
			}

			if len(tupleValues) == length {
				return arrayDeserializerNew(tupleValues, valuesSpan).DeserializeSeq(visitor)
			}
			return nil, deErrorCustom("expected tuple with length "+strconv.Itoa(length), valuesSpan)
		}
	case *Item_Table:
		valuesSpan := values.F0.span
		tupleValues, err := tupleValuesOfTable(&values.F0.items, length)
		if err != nil {
			return nil, err
		}

		if len(tupleValues) == length {
			return arrayDeserializerNew(tupleValues, valuesSpan).DeserializeSeq(visitor)
		}
		return nil, deErrorCustom("expected tuple with length "+strconv.Itoa(length), valuesSpan)
	}
	e := d.value
	return nil, deErrorCustom("expected table, found "+e.TypeName(), e.span())
}

// tupleValuesOfTable is the fold the Table and InlineTable arms of
// tuple_variant share: the values of a table whose keys are `0`, `1`, ... in
// order, or the error for the first key that is not its index.
func tupleValuesOfTable(items *keyValuePairs, length int) ([]Item, error) {
	tupleValues := make([]Item, 0, length)
	for index, value := range items.snapshot() {
		keyIndex, err := strs.ParseUsize(value.key.Get())
		if err != nil || keyIndex != index {
			return nil, deErrorCustom(
				"expected table key `"+strconv.Itoa(index)+"`, but was `"+value.key.Get()+"`",
				value.key.span(),
			)
		}
		tupleValues = append(tupleValues, value.value)
	}
	return tupleValues, nil
}

func (d *tableEnumDeserializer) StructVariant(fields []string, visitor serde.DeVisitor) (any, error) {
	return deValueDeserializerNew(d.value).withStructKeyValidation().DeserializeStruct(
		"", // TODO: this should be the variant name
		fields,
		visitor,
	)
}

var _ serde.DeVariantAccess = (*tableEnumDeserializer)(nil)

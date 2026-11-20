// Port of toml_edit 0.20.2 src/de/value.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// DeValueDeserializer is toml_edit::de::ValueDeserializer: deserialization
// implementation for TOML values.
//
// Can be created either directly from TOML strings, using
// DeValueDeserializerFromStr, or from parsed values using
// ValueIntoDeserializer.
//
//	#[derive(Deserialize)]
//	struct Config {
//	    title: String,
//	    owner: Owner,
//	}
//
//	#[derive(Deserialize)]
//	struct Owner {
//	    name: String,
//	}
//
//	let value = r#"{ title = 'TOML Example', owner = { name = 'Lisa' } }"#;
//	let deserializer = value.parse::<toml_edit::de::ValueDeserializer>().unwrap();
//	let config = Config::deserialize(deserializer).unwrap();
//	assert_eq!(config.title, "TOML Example");
//	assert_eq!(config.owner.name, "Lisa");
type DeValueDeserializer struct {
	deForward
	input              Item
	validateStructKeys bool
}

func deValueDeserializerNew(input Item) *DeValueDeserializer {
	d := &DeValueDeserializer{deForward: deForward{deserializeAny: nil}, input: input, validateStructKeys: false}
	d.deserializeAny = d.DeserializeAny
	return d
}

func (d *DeValueDeserializer) withStructKeyValidation() *DeValueDeserializer {
	d.validateStructKeys = true
	return d
}

// Note: this is wrapped by `toml::de::ValueDeserializer` and any trait methods
// implemented here need to be wrapped there

// DeserializeAny is `Deserializer::deserialize_any`.
func (d *DeValueDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	span := d.input.span()
	var value any
	var err error
	switch item := d.input.(type) {
	case *Item_None:
		value, err = serde.DeVisitorVisitNone(visitor)
	case *Item_Value:
		switch v := item.F0.(type) {
		case *Value_String:
			value, err = serde.DeVisitorVisitString(visitor, v.F0.IntoValue())
		case *Value_Integer:
			value, err = serde.DeVisitorVisitI64(visitor, v.F0.IntoValue())
		case *Value_Float:
			value, err = serde.DeVisitorVisitF64(visitor, v.F0.IntoValue())
		case *Value_Boolean:
			value, err = serde.DeVisitorVisitBool(visitor, v.F0.IntoValue())
		case *Value_Datetime:
			value, err = serde.DeVisitorVisitMap(visitor, datetimeDeserializerNew(v.F0.IntoValue()))
		case *Value_Array:
			value, err = arrayIntoDeserializer(&v.F0).DeserializeAny(visitor)
		case *Value_InlineTable:
			value, err = inlineTableIntoDeserializer(&v.F0).DeserializeAny(visitor)
		default:
			panic(rs.Unreachable(item.F0))
		}
	case *Item_Table:
		value, err = tableIntoDeserializer(&item.F0).DeserializeAny(visitor)
	case *Item_ArrayOfTables:
		value, err = arrayOfTablesIntoDeserializer(&item.F0).DeserializeAny(visitor)
	default:
		panic(rs.Unreachable(d.input))
	}
	if err != nil {
		return nil, spanIfNone(err, span)
	}
	return value, nil
}

// DeserializeOption is `Deserializer::deserialize_option`.
//
// `None` is interpreted as a missing field so be sure to implement `Some`
// as a present field.
func (d *DeValueDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	span := d.input.span()
	value, err := serde.DeVisitorVisitSome(visitor, d)
	if err != nil {
		return nil, spanIfNone(err, span)
	}
	return value, nil
}

// DeserializeNewtypeStruct is `Deserializer::deserialize_newtype_struct`.
func (d *DeValueDeserializer) DeserializeNewtypeStruct(_ string, visitor serde.DeVisitor) (any, error) {
	span := d.input.span()
	value, err := serde.DeVisitorVisitNewtypeStruct(visitor, d)
	if err != nil {
		return nil, spanIfNone(err, span)
	}
	return value, nil
}

// DeserializeStruct is `Deserializer::deserialize_struct`.
func (d *DeValueDeserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
	if SpannedIsSpanned(name, fields) {
		if span, ok := d.input.span().Get(); ok {
			value, err := serde.DeVisitorVisitMap(visitor, spannedDeserializerNew(d, span))
			if err != nil {
				return nil, deErrorFrom(err)
			}
			return value, nil
		}
	}

	if name == DatetimeName && len(fields) == 1 && fields[0] == DatetimeField {
		span := d.input.span()
		if item, ok := d.input.(*Item_Value); ok {
			if datetime, ok := item.F0.(*Value_Datetime); ok {
				value, err := serde.DeVisitorVisitMap(visitor, datetimeDeserializerNew(datetime.F0.IntoValue()))
				if err != nil {
					return nil, spanIfNone(err, span)
				}
				return value, nil
			}
		}
	}

	if d.validateStructKeys {
		span := d.input.span()
		var err error
		switch item := d.input.(type) {
		case *Item_Table:
			err = validateStructKeys(&item.F0.items, fields)
		case *Item_Value:
			if values, ok := item.F0.(*Value_InlineTable); ok {
				err = validateStructKeys(&values.F0.items, fields)
			}
		}
		if err != nil {
			return nil, spanIfNone(err, span)
		}
	}

	return d.DeserializeAny(visitor)
}

// DeserializeEnum is `Deserializer::deserialize_enum`.
//
// Called when the type to deserialize is an enum, as opposed to a field in
// the type.
func (d *DeValueDeserializer) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	span := d.input.span()
	var value any
	var err error
	matched := false
	switch item := d.input.(type) {
	case *Item_Value:
		switch v := item.F0.(type) {
		case *Value_String:
			matched = true
			value, err = serde.DeVisitorVisitEnum(visitor, stringDeserializerNew(v.F0.IntoValue()))
		case *Value_InlineTable:
			matched = true
			if v.F0.IsEmpty() {
				err = deErrorCustom("wanted exactly 1 element, found 0 elements", v.F0.span)
			} else if v.F0.Len() != 1 {
				err = deErrorCustom("wanted exactly 1 element, more than 1 element", v.F0.span)
			} else {
				value, err = inlineTableIntoDeserializer(&v.F0).DeserializeEnum(name, variants, visitor)
			}
		}
	case *Item_Table:
		matched = true
		value, err = tableIntoDeserializer(&item.F0).DeserializeEnum(name, variants, visitor)
	}
	if !matched {
		err = deErrorCustom("wanted string or table", d.input.span())
	}
	if err != nil {
		return nil, spanIfNone(err, span)
	}
	return value, nil
}

// IntoDeserializer is `impl IntoDeserializer for ValueDeserializer`: itself.
func (d *DeValueDeserializer) IntoDeserializer() *DeValueDeserializer {
	return d
}

// ValueIntoDeserializer is `impl IntoDeserializer for Value`.
func ValueIntoDeserializer(value Value) *DeValueDeserializer {
	return deValueDeserializerNew(&Item_Value{F0: value})
}

// itemIntoDeserializer is `Item::into_deserializer`.
func itemIntoDeserializer(item Item) *DeValueDeserializer {
	return deValueDeserializerNew(item)
}

// DeValueDeserializerFromStr is `impl FromStr for ValueDeserializer`: parses
// a value from a &str.
func DeValueDeserializerFromStr(s string) (*DeValueDeserializer, error) {
	v, err := parseValue(s)
	if err != nil {
		return nil, DeErrorFromTomlError(err.(*TomlError))
	}
	return ValueIntoDeserializer(v), nil
}
